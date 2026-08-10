//! In-memory registry of every active `ManagedModel`.
//!
//! Lives on the `MethodContext` so IPC handlers can look up
//! supervisors by `LaunchId` and the daemon can iterate the map for
//! `status` / `stop_all`. The registry intentionally keys on a
//! monotonically-increasing `LaunchId` rather than `ModelId` so the
//! same GGUF can be launched twice (different ports, different
//! purposes) without collisions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as TokioMutex, RwLock};

use crate::daemon::supervisor::{ManagedModel, ManagedState};

/// Stable identifier for one launch. Strings on the wire so future
/// schemes (UUID, etc.) don't require an IPC bump.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaunchId(pub String);

impl LaunchId {
  pub fn from_counter(n: u64) -> Self {
    Self(format!("L{n}"))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

/// Shared, cheap-to-clone registry of supervisors. `Arc<RwLock<…>>`
/// inside, mirroring `ModelCatalog`'s pattern so IPC handlers have a
/// consistent shape.
#[derive(Debug, Clone, Default)]
pub struct SupervisorRegistry {
  inner: Arc<RwLock<BTreeMap<LaunchId, ManagedModel>>>,
  counter: Arc<AtomicU64>,
  /// Ports chosen by `reserve_port` but not yet inserted into a
  /// supervisor. Held under a tokio mutex so the
  /// "choose + reserve + spawn" sequence is serialised; without this
  /// two concurrent `start_model` calls would observe the same
  /// `collect_in_use_ports` snapshot, both bind-probe the same free
  /// port, and the second spawn's `llama-server` would fail to bind.
  reserved_ports: Arc<TokioMutex<BTreeSet<u16>>>,
  /// Per-delegated-model state for managed-multiplexer backends
  /// (Lemonade), keyed by registry model name. Delegated models have
  /// no supervisor of their own — the umbrella is the only process —
  /// so the background preload task records its outcome here
  /// (`Loading` → `Ready` / `Error{cause}`) and the `status`
  /// projection reads it instead of blindly mirroring the umbrella's
  /// state. Entries are dropped when the model is stopped/evicted and
  /// cleared wholesale when the umbrella stops.
  delegated: Arc<RwLock<BTreeMap<String, ManagedState>>>,
}

impl SupervisorRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Generate the next launch id. Monotonic per daemon process, so
  /// IDs are unique within one daemon lifetime.
  pub fn next_id(&self) -> LaunchId {
    let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
    LaunchId::from_counter(n)
  }

  pub async fn insert(&self, id: LaunchId, model: ManagedModel) {
    self.inner.write().await.insert(id, model);
  }

  pub async fn get(&self, id: &LaunchId) -> Option<ManagedModel> {
    self.inner.read().await.get(id).cloned()
  }

  pub async fn remove(&self, id: &LaunchId) -> Option<ManagedModel> {
    self.inner.write().await.remove(id)
  }

  /// Snapshot of every (LaunchId, ManagedModel) pair, sorted by id.
  pub async fn snapshot(&self) -> Vec<(LaunchId, ManagedModel)> {
    self
      .inner
      .read()
      .await
      .iter()
      .map(|(k, v)| (k.clone(), v.clone()))
      .collect()
  }

  /// Just the launch ids, for callers that classify launches without
  /// touching the models (e.g. the idle-shutdown poller).
  pub async fn launch_ids(&self) -> Vec<LaunchId> {
    self.inner.read().await.keys().cloned().collect()
  }

  pub async fn len(&self) -> usize {
    self.inner.read().await.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.is_empty()
  }

  /// Atomically pick a port and add it to the in-flight reservation
  /// set, given the live-supervisor ports the caller already knows
  /// about. The returned port is held in the reservation set until
  /// [`Self::release_reserved_port`] is called. Use this from
  /// `start_model` to close the choose-and-reserve race against
  /// concurrent IPC clients.
  pub async fn reserve_port(
    &self,
    requested: Option<u16>,
    live_in_use: &[u16],
    range: &crate::config::loader::PortRange,
  ) -> Result<u16, String> {
    let mut reserved = self.reserved_ports.lock().await;
    let mut combined: Vec<u16> = live_in_use.to_vec();
    combined.extend(reserved.iter().copied());
    let chosen = match requested {
      Some(p) => {
        if reserved.contains(&p) || live_in_use.contains(&p) {
          return Err(format!("port {p} is already in use by another launch"));
        }
        // Probe-before-reserve: the supervisor's reservation set only
        // tracks ports it has handed out. An externally-held port —
        // e.g. a `llama-server` from a previous daemon instance, or
        // any other process bound to a slot inside our configured
        // range — passes the in-set check above but the subsequent
        // child still fails to bind, surfacing as
        // `couldn't bind HTTP server socket` in the launch log. The
        // auto-allocator path (`ports::allocate`) already probes via
        // `try_bind`; explicit / soft-preferred ports must do the
        // same so a `prefer_port: <stale>` from the TUI's last-used
        // memory doesn't lock the user into a broken launch.
        if !crate::daemon::ports::try_bind_probe(p) {
          return Err(format!("port {p} is already in use (external bind)"));
        }
        p
      }
      None => crate::daemon::ports::allocate(range, &combined).map_err(|e| e.to_string())?,
    };
    reserved.insert(chosen);
    Ok(chosen)
  }

  /// Drop a port from the in-flight reservation set, typically called
  /// once the supervisor has been inserted into `inner` (so its port
  /// is visible via the normal supervisor snapshot path) or when a
  /// spawn fails and the reservation should be released for retry.
  pub async fn release_reserved_port(&self, port: u16) {
    self.reserved_ports.lock().await.remove(&port);
  }

  /// Record the state of one delegated (umbrella-served) model.
  pub async fn set_delegated_state(&self, name: &str, state: ManagedState) {
    self.delegated.write().await.insert(name.to_string(), state);
  }

  /// The recorded state of one delegated model, if any. `None` means
  /// no preload outcome is known (e.g. a snapshot re-adopted across a
  /// daemon restart) — callers fall back to mirroring the umbrella.
  pub async fn delegated_state(&self, name: &str) -> Option<ManagedState> {
    self.delegated.read().await.get(name).cloned()
  }

  /// Forget one delegated model (stopped or evicted).
  pub async fn remove_delegated(&self, name: &str) {
    self.delegated.write().await.remove(name);
  }

  /// Forget every delegated model — the umbrella is gone, so no
  /// recorded state can be honored.
  pub async fn clear_delegated(&self) {
    self.delegated.write().await.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn next_id_is_monotonic_within_one_registry() {
    let r = SupervisorRegistry::new();
    let a = r.next_id();
    let b = r.next_id();
    assert_ne!(a, b);
    // Two registries are independent.
    let other = SupervisorRegistry::new();
    let c = other.next_id();
    assert_eq!(c.as_str(), "L1");
  }

  #[test]
  fn launch_id_round_trips_via_json() {
    let id = LaunchId::from_counter(42);
    let s = serde_json::to_string(&id).unwrap();
    assert_eq!(s, "\"L42\"");
    let back: LaunchId = serde_json::from_str(&s).unwrap();
    assert_eq!(back, id);
  }

  use crate::config::loader::PortRange;
  use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

  #[tokio::test]
  async fn explicit_port_held_externally_is_rejected() {
    // Simulate an external process holding a port inside our range:
    // bind it and hold the socket open for the duration of the test.
    // The supervisor doesn't know about this port (live_in_use +
    // reserved are both empty), so the only thing that can catch it
    // is the bind probe.
    let holder = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
      .expect("OS gives us a free loopback port");
    let busy = holder.local_addr().unwrap().port();
    let range = PortRange {
      start: busy,
      end: busy,
    };
    let r = SupervisorRegistry::new();
    let err = r
      .reserve_port(Some(busy), &[], &range)
      .await
      .expect_err("must refuse a port held by an external process");
    assert!(
      err.contains("external") || err.contains("already in use"),
      "error must name the external-bind failure mode, got: {err}"
    );
  }

  #[tokio::test]
  async fn explicit_port_that_is_actually_free_succeeds() {
    // Probe a port we know is free: bind to grab one, immediately
    // drop the holder, then ask `reserve_port` for the same number.
    // The OS may keep the slot in TIME_WAIT briefly on some hosts,
    // so we retry against a small range until something sticks.
    let r = SupervisorRegistry::new();
    let mut chosen = None;
    for offset in 0..16u16 {
      let holder = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .expect("OS gives us a free loopback port");
      let port = holder.local_addr().unwrap().port().wrapping_add(offset);
      drop(holder);
      let range = PortRange {
        start: port,
        end: port,
      };
      if let Ok(p) = r.reserve_port(Some(port), &[], &range).await {
        chosen = Some(p);
        break;
      }
    }
    chosen.expect("at least one of 16 attempts must land on a probe-clear port");
  }
}
