//! In-memory registry of every active `ManagedModel`.
//!
//! Lives on the `MethodContext` so IPC handlers can look up
//! supervisors by `LaunchId`. The registry keys on a
//! monotonically-increasing `LaunchId` rather than `ModelId` so the
//! same GGUF can be launched twice (different ports, different
//! purposes) without collisions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as TokioMutex, RwLock};

use crate::daemon::supervisor::ManagedModel;

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

  /// The counter behind an `L<n>` id, or `None` when the id is not that shape.
  pub fn counter(&self) -> Option<u64> {
    self.0.strip_prefix('L')?.parse().ok()
  }
}

/// Shared, cheap-to-clone registry of supervisors. `Arc<RwLock<…>>`
/// inside, mirroring `ModelCatalog`'s pattern.
#[derive(Debug, Clone, Default)]
pub struct SupervisorRegistry {
  inner: Arc<RwLock<BTreeMap<LaunchId, ManagedModel>>>,
  counter: Arc<AtomicU64>,
  /// Ports chosen by `reserve_port` but not yet inserted into a
  /// supervisor. Held under a tokio mutex so the
  /// "choose + reserve + spawn" sequence is serialised; without this
  /// two concurrent `start_model` calls would observe the same
  /// `collect_in_use_ports` snapshot, both bind-probe the same free
  /// port, and the second spawn's engine child would fail to bind.
  reserved_ports: Arc<TokioMutex<BTreeSet<u16>>>,
  /// `(model path, lowercased name)` pairs claimed by a spawn that cleared the
  /// duplicate-name gate but has not stamped its row yet.
  ///
  /// The gate reads a `state` snapshot and the row is pushed much later, after
  /// the child is spawned, so without a claim here two concurrent
  /// `start --name coder` both pass and the only symptom is two launches sharing
  /// one address, with no error. Same serialization rationale as
  /// `reserved_ports`. A plain mutex, not a tokio one: the guard has to release
  /// on `Drop`, which cannot await.
  reserved_names: Arc<StdMutex<BTreeSet<(String, String)>>>,
}

impl SupervisorRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Generate the next launch id, monotonic per daemon process.
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
  /// [`Self::release_reserved_port`] is called. `start_model` uses
  /// this to close the choose-and-reserve race against concurrent
  /// IPC clients.
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
        // Probe before reserving: the reservation set only tracks
        // ports this daemon handed out. A port held by another
        // process, e.g. an engine child from a previous daemon
        // instance, passes the in-set check but the child still
        // fails to bind, surfacing as `couldn't bind HTTP server
        // socket` in the launch log. The auto-allocator path
        // (`ports::allocate`) already probes via `try_bind`, so
        // explicit and soft-preferred ports must do the same.
        // Otherwise a stale `prefer_port` from the TUI's last-used
        // memory locks the user into a broken launch.
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

  /// Drop a port from the in-flight reservation set, once the
  /// supervisor is in `inner` (its port is then visible in the
  /// snapshot) or when a spawn fails and the port can be retried.
  pub async fn release_reserved_port(&self, port: u16) {
    self.reserved_ports.lock().await.remove(&port);
  }

  /// Claim `(model_path, name)` for the duration of one spawn, or
  /// return `None` when the name is already claimed.
  ///
  /// The returned guard releases the claim when it drops, so every
  /// early return between the gate and the spawn hands the name back.
  /// Names are stored lowercased because
  /// [`crate::launch::resolve::name_matches`] compares them that way,
  /// so a capitalization variant collides instead of starting a
  /// second copy.
  pub fn try_reserve_name(&self, model_path: &str, name: &str) -> Option<NameReservation> {
    let key = (model_path.to_string(), name.to_lowercase());
    let mut reserved = self.lock_names();
    if reserved.contains(&key) {
      return None;
    }
    reserved.insert(key.clone());
    Some(NameReservation {
      reserved: self.reserved_names.clone(),
      key,
    })
  }

  /// The reserved-name set, tolerating a poisoned lock: the set holds only
  /// `String` keys, so a panic elsewhere cannot leave it in a state that matters.
  fn lock_names(&self) -> std::sync::MutexGuard<'_, BTreeSet<(String, String)>> {
    self
      .reserved_names
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }
}

/// A live claim on one `(model path, name)` while a spawn is in flight.
/// Releases when dropped.
pub struct NameReservation {
  reserved: Arc<StdMutex<BTreeSet<(String, String)>>>,
  key: (String, String),
}

impl Drop for NameReservation {
  fn drop(&mut self) {
    // Poison-tolerant like `lock_names`: the set holds plain keys, so
    // recovering the guard is safe. Skipping the remove instead would
    // hold the name until the daemon restarts.
    self
      .reserved
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .remove(&self.key);
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
    // reserved are both empty), so only the bind probe can catch it.
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

  /// The name claim is exclusive while held and comes back when the guard
  /// drops. Without this, two concurrent `start --name coder` both clear the
  /// gate, and because the port allocator *is* serialized the only symptom is two
  /// launches sharing one address and no error.
  #[test]
  fn name_claim_is_exclusive_then_released_on_drop() {
    let r = SupervisorRegistry::new();
    let claim = r.try_reserve_name("/m/a.gguf", "coder");
    assert!(claim.is_some(), "first claim on a free name must succeed");
    assert!(
      r.try_reserve_name("/m/a.gguf", "coder").is_none(),
      "a held name must not be claimable twice"
    );
    // A capitalization variant is the same name, so it collides too.
    assert!(
      r.try_reserve_name("/m/a.gguf", "CODER").is_none(),
      "case variant must collide"
    );
    // Different name, or the same name on another model, are separate claims.
    assert!(r.try_reserve_name("/m/a.gguf", "reviewer").is_some());
    assert!(r.try_reserve_name("/m/b.gguf", "coder").is_some());
    drop(claim);
    assert!(
      r.try_reserve_name("/m/a.gguf", "coder").is_some(),
      "dropping the guard must release the name"
    );
  }

  /// A panic elsewhere poisons the registry's mutexes; the claim's `Drop` must
  /// still release (the set holds plain keys, so recovering the guard is safe).
  /// Leaking the claim instead would block that name until the daemon restarts.
  ///
  /// Skipped under `-Cpanic=abort` (the nightly coverage job): poisoning a
  /// mutex means panicking under the lock, and `catch_unwind` cannot catch
  /// what does not unwind, so the process would abort instead.
  #[test]
  #[cfg_attr(panic = "abort", ignore = "poisoning a mutex needs unwinding")]
  fn a_poisoned_lock_does_not_leak_a_held_name() {
    let r = SupervisorRegistry::new();
    let claim = r.try_reserve_name("/m/a.gguf", "coder").unwrap();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = r.reserved_names.lock().unwrap();
      panic!("poison the mutex the way any panic under the lock would");
    }));
    drop(claim);
    assert!(
      r.try_reserve_name("/m/a.gguf", "coder").is_some(),
      "the claim must release even through a poisoned lock"
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
