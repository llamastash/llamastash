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

  pub async fn len(&self) -> usize {
    self.inner.read().await.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.is_empty()
  }

  /// Atomically pick a port and add it to the in-flight reservation
  /// set, given the live-supervisor ports the caller already knows
  /// about. The returned port is held in the reservation set until
  /// [`release_reserved_port`] is called. Use this from
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
          return Err(format!(
            "port {p} is already in use by another launch"
          ));
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
}
