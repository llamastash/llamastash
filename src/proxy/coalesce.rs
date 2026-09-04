//! Single-flight launch coalescing keyed on [`ModelId`].
//!
//! Auto-start can fire from any number of concurrent inbound requests
//! for the same dormant model. Without coalescing, each request would
//! issue its own `compose_and_spawn` call, race the port allocator,
//! and either fail with a port collision or burn a second
//! `llama-server` process on top of the one already launching.
//!
//! Each slot in the map carries (a) a [`Notify`] for fast wakeups and
//! (b) a `Mutex<SlotState>` so the leader's outcome is durable across
//! the gap between `Leader::finish` and a follower's
//! [`Notify::notified`] registration. A pure-`Notify` slot would race:
//! a follower that arrives between `acquire` and the leader's
//! `notify_waiters` call can park *after* the wake fires and hang
//! forever (cf. the ce-review correctness finding R-01).
//!
//! Keyed on [`ModelId`] — the canonical `(path, header_blake3)` pair
//! — rather than the raw `body.model` string, so two requests with
//! different fuzzy spellings of the same model still share one
//! launch.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, Notify};

use crate::gguf::identity::ModelId;

/// Cheap-to-clone shared launch outcome — what the leader stamps on
/// the slot for followers to read. The two variants mirror
/// [`crate::proxy::launch::LaunchOutcome`].
#[derive(Clone, Debug)]
pub(crate) enum SharedOutcome {
  /// Supervisor reached `Ready`; the bound port + canonical id are
  /// what the forward path needs to verify the supervisor is still
  /// the same one we launched (defends the Ready→Stopping→port-reuse
  /// race in [`super::forward`]).
  Ready { port: u16, model_id: ModelId },
  /// Launch hit a terminal error before reaching Ready. `cause`
  /// surfaces in the 503 `launch_failed` body when no fallback exists.
  Failed { cause: String },
}

/// Internal slot state. Lives inside an `Arc<SlotInner>` so leaders
/// and followers share it cheaply.
struct SlotInner {
  notify: Notify,
  state: StdMutex<SlotState>,
}

enum SlotState {
  /// Leader still driving the launch; followers must park.
  Pending,
  /// Leader called [`Leader::finish`]. The outcome is what every
  /// awaiting follower returns from `wait()`.
  Complete(SharedOutcome),
  /// Leader dropped without finishing (cancellation / panic).
  /// Followers wake to `None` and fall through to fallback.
  Cancelled,
}

/// The single-flight key: the canonical model id plus the optional
/// launch name, so two named launches of the same model don't coalesce
/// into one flight.
type FlightKey = (ModelId, Option<String>);
/// The in-flight slot map, keyed on [`FlightKey`].
type FlightMap = HashMap<FlightKey, Arc<SlotInner>>;

/// Single-flight registry. Cheap to clone — every field lives behind
/// an `Arc` so the per-request handle is a refcount bump.
///
/// Keyed on [`FlightKey`] — the canonical model id plus the optional
/// launch name — so two named launches of the same model don't
/// coalesce into one flight.
#[derive(Clone, Default)]
pub(crate) struct Coalesce {
  inner: Arc<Mutex<FlightMap>>,
}

/// Outcome of [`Coalesce::acquire`]. Tells the caller whether to
/// drive the launch itself (the [`Leader`]) or to park on an
/// existing waiter and re-check state when notified ([`Follower`]).
pub(crate) enum AcquireOutcome {
  /// This caller is the first to ask for the launch. It owns the
  /// slot until it calls [`Leader::finish`].
  Leader(Leader),
  /// Another caller is already driving the launch. The follower
  /// awaits [`Follower::wait`] and rejoins the route on completion.
  Follower(Follower),
}

/// Token returned to the request that won the right to drive the
/// launch. Holding this token is the marker that this request's
/// `compose_and_spawn` call is the live one. Dropping it without
/// calling [`Leader::finish`] is a bug; the `Drop` impl stamps
/// `SlotState::Cancelled` and wakes followers so they don't hang.
pub(crate) struct Leader {
  parent: Coalesce,
  key: (ModelId, Option<String>),
  slot: Arc<SlotInner>,
  /// Becomes `true` after [`Self::finish`] runs. The `Drop` impl
  /// uses it to detect the "leader dropped without calling finish"
  /// failure mode and signal waiters anyway.
  finished: bool,
}

impl Leader {
  /// Stamp `outcome` on the slot and wake every parked follower.
  /// Removes the map entry so the next request for the same model
  /// starts fresh.
  pub(crate) async fn finish(mut self, outcome: SharedOutcome) {
    self.complete(SlotState::Complete(outcome)).await;
  }

  /// Internal: shared body of `finish` + the `Drop` safety net.
  async fn complete(&mut self, final_state: SlotState) {
    if self.finished {
      return;
    }
    self.finished = true;
    // Write the outcome FIRST so any follower that wakes due to
    // notify_waiters sees Complete/Cancelled on the re-check. The
    // poisoned-mutex case can't lose data (the inner lock is held
    // only across these straight-line writes).
    *self.slot.state.lock().unwrap_or_else(|e| e.into_inner()) = final_state;
    self.slot.notify.notify_waiters();
    // Then drop the map entry so a future acquire for the same key
    // starts a fresh launch round.
    self.parent.inner.lock().await.remove(&self.key);
  }
}

impl Drop for Leader {
  fn drop(&mut self) {
    if self.finished {
      return;
    }
    // Stamp Cancelled synchronously so any follower already parked
    // on the inner Notify sees the right state when they re-check.
    *self.slot.state.lock().unwrap_or_else(|e| e.into_inner()) = SlotState::Cancelled;
    self.slot.notify.notify_waiters();
    // Detach the map cleanup onto the runtime — Drop can't await.
    // If no runtime is available we leak the entry; the next acquire
    // for the same key creates a new slot, which is acceptable.
    let parent = self.parent.clone();
    let key = self.key.clone();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
      handle.spawn(async move {
        parent.inner.lock().await.remove(&key);
      });
    }
  }
}

/// Token returned to followers that arrived while a launch was
/// already in flight. The caller awaits [`Self::wait`] to receive
/// the leader's outcome (or `None` if the leader was cancelled).
pub(crate) struct Follower {
  slot: Arc<SlotInner>,
}

impl Follower {
  /// Block until the leader signals completion and return the
  /// outcome it stamped on the slot. Returns `None` if the leader
  /// was cancelled mid-launch — callers should treat this the same
  /// as a launch failure (try fallback selection).
  pub(crate) async fn wait(self) -> Option<SharedOutcome> {
    loop {
      // Cheap path: leader already stamped the slot.
      if let Some(out) = self.read_slot() {
        return out;
      }
      // Subscribe to the inner Notify BEFORE re-reading the slot.
      // The future's existence is what makes a subsequent
      // notify_waiters wake us, even if we haven't reached the await
      // point yet. The pin keeps the future in place across the
      // re-check.
      let notified = self.slot.notify.notified();
      tokio::pin!(notified);
      // Re-check after registering interest. If the leader fired
      // notify_waiters in the gap between our first read and the
      // register, the inner-state write happened-before the notify
      // (Leader::complete writes state then notifies), so the
      // second read sees it.
      if let Some(out) = self.read_slot() {
        return out;
      }
      notified.as_mut().await;
      // Loop and re-read. Spurious wakes are harmless; we'll fall
      // back into the cheap path and return.
    }
  }

  /// Read the slot state once, returning `Some(outcome)` for
  /// `Complete`, `Some(None)` for `Cancelled` (translated to outer
  /// `Option::None` by the caller via the second `?`), and `None` for
  /// still-pending. Centralised so the lock acquisition + match is
  /// not duplicated across the two read sites in [`Self::wait`].
  fn read_slot(&self) -> Option<Option<SharedOutcome>> {
    let guard = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
    match &*guard {
      SlotState::Complete(out) => Some(Some(out.clone())),
      SlotState::Cancelled => Some(None),
      SlotState::Pending => None,
    }
  }
}

impl Coalesce {
  pub(crate) fn new() -> Self {
    Self::default()
  }

  /// Try to acquire single-flight rights for `key`. Either a fresh
  /// [`Leader`] or an existing [`Follower`] is returned; the caller
  /// branches on the variant.
  ///
  /// The lookup-and-insert happens under one lock, so two concurrent
  /// `acquire(key)` calls can never both walk away as leaders for
  /// the same `key`.
  pub(crate) async fn acquire(&self, key: (ModelId, Option<String>)) -> AcquireOutcome {
    let mut guard = self.inner.lock().await;
    if let Some(slot) = guard.get(&key).cloned() {
      return AcquireOutcome::Follower(Follower { slot });
    }
    let slot = Arc::new(SlotInner {
      notify: Notify::new(),
      state: StdMutex::new(SlotState::Pending),
    });
    guard.insert(key.clone(), slot.clone());
    AcquireOutcome::Leader(Leader {
      parent: self.clone(),
      key,
      slot,
      finished: false,
    })
  }
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::Duration;

  use super::*;

  fn key(path: &str) -> (ModelId, Option<String>) {
    (
      ModelId {
        path: PathBuf::from(path),
        header_blake3: [1u8; 32],
      },
      None,
    )
  }

  fn ready(port: u16) -> SharedOutcome {
    SharedOutcome::Ready {
      port,
      model_id: ModelId {
        path: PathBuf::from("/m/ready.gguf"),
        header_blake3: [1u8; 32],
      },
    }
  }

  #[tokio::test]
  async fn first_caller_becomes_leader() {
    let c = Coalesce::new();
    let outcome = c.acquire(key("/m/a.gguf")).await;
    assert!(matches!(outcome, AcquireOutcome::Leader(_)));
  }

  #[tokio::test]
  async fn second_caller_becomes_follower() {
    let c = Coalesce::new();
    let leader = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Leader(l) => l,
      _ => panic!("expected leader"),
    };
    let outcome = c.acquire(key("/m/a.gguf")).await;
    assert!(matches!(outcome, AcquireOutcome::Follower(_)));
    leader.finish(ready(18001)).await;
  }

  #[tokio::test]
  async fn different_keys_each_get_a_leader() {
    let c = Coalesce::new();
    let a = c.acquire(key("/m/a.gguf")).await;
    let b = c.acquire(key("/m/b.gguf")).await;
    assert!(matches!(a, AcquireOutcome::Leader(_)));
    assert!(matches!(b, AcquireOutcome::Leader(_)));
  }

  #[tokio::test]
  async fn follower_wakes_when_leader_finishes() {
    let c = Coalesce::new();
    let leader = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Leader(l) => l,
      _ => panic!("leader"),
    };
    let follower = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Follower(f) => f,
      _ => panic!("follower"),
    };
    let woke = Arc::new(AtomicUsize::new(0));
    let woke_for_task = woke.clone();
    let task = tokio::spawn(async move {
      let out = follower.wait().await;
      assert!(matches!(
        out,
        Some(SharedOutcome::Ready { port: 18001, .. })
      ));
      woke_for_task.fetch_add(1, Ordering::SeqCst);
    });
    // Yield to let the follower start awaiting; with the durable
    // slot state we no longer NEED a sleep here, but the test reads
    // more clearly with one.
    tokio::time::sleep(Duration::from_millis(20)).await;
    leader.finish(ready(18001)).await;
    task.await.unwrap();
    assert_eq!(woke.load(Ordering::SeqCst), 1);
  }

  #[tokio::test]
  async fn follower_that_parks_after_finish_still_wakes_with_outcome() {
    // The R-01 race: leader.finish() runs BEFORE the follower has
    // reached its wait() call. With Notify-only slots the follower
    // would park forever on the next notified().await. With the
    // durable SlotState the follower's first read_slot() sees
    // Complete and returns immediately.
    let c = Coalesce::new();
    let leader = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Leader(l) => l,
      _ => panic!("leader"),
    };
    let follower = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Follower(f) => f,
      _ => panic!("follower"),
    };
    // Finish FIRST -- this is the race that previously hung.
    leader.finish(ready(18042)).await;
    // ... then wait. Should return immediately.
    let out = follower.wait().await;
    assert!(matches!(
      out,
      Some(SharedOutcome::Ready { port: 18042, .. })
    ));
  }

  #[tokio::test]
  async fn follower_sees_failed_outcome_from_leader() {
    let c = Coalesce::new();
    let leader = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Leader(l) => l,
      _ => panic!("leader"),
    };
    let follower = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Follower(f) => f,
      _ => panic!("follower"),
    };
    leader
      .finish(SharedOutcome::Failed {
        cause: "probe timeout".to_string(),
      })
      .await;
    let out = follower.wait().await;
    match out {
      Some(SharedOutcome::Failed { cause }) => assert_eq!(cause, "probe timeout"),
      other => panic!("expected Failed; got {other:?}"),
    }
  }

  #[tokio::test]
  async fn follower_wakes_to_none_on_leader_cancellation() {
    let c = Coalesce::new();
    let leader = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Leader(l) => l,
      _ => panic!("leader"),
    };
    let follower = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Follower(f) => f,
      _ => panic!("follower"),
    };
    // Drop the leader without calling finish — simulates a cancelled
    // future. The Drop impl stamps Cancelled and notifies waiters.
    drop(leader);
    let out = follower.wait().await;
    assert!(out.is_none());
  }

  #[tokio::test]
  async fn finish_clears_the_slot_so_next_acquire_is_a_fresh_leader() {
    let c = Coalesce::new();
    let leader = match c.acquire(key("/m/a.gguf")).await {
      AcquireOutcome::Leader(l) => l,
      _ => panic!("leader"),
    };
    leader.finish(ready(18000)).await;
    let again = c.acquire(key("/m/a.gguf")).await;
    assert!(matches!(again, AcquireOutcome::Leader(_)));
  }
}
