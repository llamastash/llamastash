//! Per-`ModelId` recent-failure tracker for proxy auto-start.
//!
//! Without this, every inbound `/v1/...` request for a model with no
//! Ready supervisor kicks off a fresh `compose_and_spawn` — and if
//! the model can't load (VRAM contention, broken GGUF, missing CUDA),
//! every retry pays the full GGUF read + child fork + probe wait
//! before failing again. Observed in the wild: a single
//! agent looped on Qwen3.6-27B-Q4_K_M for ~30 s and produced 10+
//! identical failed launches in `~/.cache/llamastash/logs/` before
//! the user noticed.
//!
//! Cap: [`MAX_FAILURES`] failures within [`WINDOW_SECS`] short-circuits
//! further auto-starts of the same id with a `cause` string the proxy
//! surfaces to the client. The window slides — failures older than
//! `WINDOW_SECS` are pruned on every check so a recovered model is
//! launchable again without restarting the daemon.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::gguf::identity::ModelId;

/// Number of failures within [`WINDOW_SECS`] that suppresses further
/// auto-start attempts.
pub const MAX_FAILURES: usize = 3;

/// Sliding window applied to the in-memory failure log. Cleared
/// failures older than this drop off, so a model that recovers
/// (operator stopped a competing process, freed VRAM, fixed a typo
/// in `extras`) becomes launchable again automatically.
pub const WINDOW_SECS: u64 = 60;

/// In-memory per-`ModelId` recent-failure log. Sized for the typical
/// daemon (a few dozen distinct ids at most); a `HashMap` with a
/// `Vec<Instant>` per id keeps the lookup O(1) and the prune O(window).
#[derive(Default)]
pub(crate) struct FailureTracker {
  inner: Mutex<HashMap<ModelId, Vec<Instant>>>,
}

impl FailureTracker {
  pub fn new() -> Self {
    Self::default()
  }

  /// Old entries inside `id`'s bucket are pruned on the same pass so
  /// the bucket can't grow unbounded under a sustained outage.
  pub fn note_failure(&self, id: &ModelId, now: Instant) {
    let cutoff = now - Duration::from_secs(WINDOW_SECS);
    let mut guard = match self.inner.lock() {
      Ok(g) => g,
      Err(poison) => poison.into_inner(),
    };
    let bucket = guard.entry(id.clone()).or_default();
    bucket.retain(|t| *t >= cutoff);
    bucket.push(now);
  }

  /// Returns the suppression `cause` string when `id` has hit
  /// [`MAX_FAILURES`] within [`WINDOW_SECS`], else `None`. Same prune
  /// pass as [`Self::note_failure`] so a long-quiet bucket auto-clears
  /// on first check.
  pub fn over_limit(&self, id: &ModelId, now: Instant) -> Option<String> {
    let cutoff = now - Duration::from_secs(WINDOW_SECS);
    let mut guard = match self.inner.lock() {
      Ok(g) => g,
      Err(poison) => poison.into_inner(),
    };
    let bucket = guard.get_mut(id)?;
    bucket.retain(|t| *t >= cutoff);
    let count = bucket.len();
    if count >= MAX_FAILURES {
      let oldest = bucket.first().copied().unwrap_or(now);
      let elapsed = now.saturating_duration_since(oldest).as_secs();
      Some(format!(
        "auto-start suppressed: {count} failed attempts in the last {elapsed} s (limit {MAX_FAILURES} in {WINDOW_SECS} s); restart the daemon or run `llamastash start` manually to see the underlying error",
      ))
    } else {
      None
    }
  }

  /// Called on a successful launch so a model that recovers
  /// mid-window doesn't carry stale failures into the next outage.
  pub fn clear(&self, id: &ModelId) {
    let mut guard = match self.inner.lock() {
      Ok(g) => g,
      Err(poison) => poison.into_inner(),
    };
    guard.remove(id);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gguf::identity::ModelId;

  fn id(seed: u8) -> ModelId {
    // Stable, distinct fake ids without going through the GGUF parser.
    ModelId {
      path: format!("/fake/model-{seed:02x}.gguf").into(),
      header_blake3: [seed; 32],
    }
  }

  #[test]
  fn under_limit_returns_none() {
    let tracker = FailureTracker::new();
    let now = Instant::now();
    let id = id(1);
    tracker.note_failure(&id, now);
    tracker.note_failure(&id, now);
    assert!(tracker.over_limit(&id, now).is_none());
  }

  #[test]
  fn at_limit_returns_cause() {
    let tracker = FailureTracker::new();
    let now = Instant::now();
    let id = id(2);
    for _ in 0..MAX_FAILURES {
      tracker.note_failure(&id, now);
    }
    let cause = tracker.over_limit(&id, now).expect("limit should trigger");
    assert!(cause.contains("auto-start suppressed"));
    assert!(cause.contains(&format!("limit {MAX_FAILURES}")));
  }

  #[test]
  fn old_failures_prune_off() {
    let tracker = FailureTracker::new();
    let id = id(3);
    let ancient = Instant::now() - Duration::from_secs(WINDOW_SECS * 2);
    // Three ancient failures: would trip the limit if not pruned.
    tracker.note_failure(&id, ancient);
    tracker.note_failure(&id, ancient);
    tracker.note_failure(&id, ancient);
    let now = Instant::now();
    assert!(
      tracker.over_limit(&id, now).is_none(),
      "ancient failures should have pruned"
    );
  }

  #[test]
  fn clear_drops_bucket() {
    let tracker = FailureTracker::new();
    let id = id(4);
    let now = Instant::now();
    for _ in 0..MAX_FAILURES {
      tracker.note_failure(&id, now);
    }
    assert!(tracker.over_limit(&id, now).is_some());
    tracker.clear(&id);
    assert!(tracker.over_limit(&id, now).is_none());
  }

  #[test]
  fn distinct_ids_dont_share_counters() {
    let tracker = FailureTracker::new();
    let now = Instant::now();
    let a = id(5);
    let b = id(6);
    for _ in 0..MAX_FAILURES {
      tracker.note_failure(&a, now);
    }
    assert!(tracker.over_limit(&a, now).is_some());
    assert!(tracker.over_limit(&b, now).is_none());
  }
}
