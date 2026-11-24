//! Most-recently-used tracker keyed on [`ModelId`].
//!
//! The fallback path needs to answer "which currently-running
//! model did the user touch most recently?" when an auto-start
//! fails. The proxy stamps `last_request_at` *as forwarding starts*
//! (not on completion — long-running streams shouldn't delay the
//! timestamp) so the tracker reflects request arrival, not duration.
//!
//! Map lives in process memory only — a daemon restart wipes the
//! data, which is fine because the supervisor registry restarts
//! empty too. There is no persistence value; the ordering is
//! recovered naturally on the first request after a restart.
//!
//! [`pick_fallback`] embodies the family-MRU policy:
//!   1. Filter the supervisor snapshot to `Ready` entries.
//!   2. If the requested model's `general.architecture` is known,
//!      put entries whose arch matches that value first; within
//!      each group, sort by `last_request_at` descending (newest
//!      first); entries with no recorded timestamp sort last in
//!      each group.
//!   3. If the requested arch is `None` (synthetic GGUF without
//!      metadata), skip the family-prefer step and sort the whole
//!      list by `last_request_at` descending.
//!   4. Pick the head. Empty list → caller emits 503.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::gguf::identity::ModelId;

/// Cheap-to-clone tracker. The inner `Arc<RwLock>` makes per-request
/// access lock-aware: hot path reads (the fallback selector) take a
/// brief shared read; the `touch` writer on the happy path takes a
/// brief exclusive write. Auto-start runs at human cadence; lock
/// contention is not a meaningful axis here.
#[derive(Clone, Default)]
pub(crate) struct MruTracker {
  inner: Arc<RwLock<HashMap<ModelId, Instant>>>,
}

/// One candidate considered by [`pick_fallback`]. Carries enough
/// state to render the family-prefer + MRU sort independently of
/// the [`crate::daemon::supervisor::ManagedModel`] handle the caller
/// used to build it — keeps `pick_fallback` a pure function over
/// data we hand in.
#[derive(Debug, Clone)]
pub(crate) struct FallbackCandidate {
  pub model_id: ModelId,
  pub arch: Option<String>,
  pub last_request_at: Option<Instant>,
  pub port: u16,
  /// Display name used by the response header
  /// `x-llamastash-served-by`. Falls back to the path stem when the
  /// caller doesn't have a better label.
  pub served_model_id: String,
}

impl MruTracker {
  pub(crate) fn new() -> Self {
    Self::default()
  }

  /// Stamp `now` as the latest request time for `id`. Called by the
  /// proxy as forwarding starts (before the streaming body is
  /// piped) so the MRU reflects request arrival, not completion.
  pub(crate) async fn touch(&self, id: &ModelId) {
    let now = Instant::now();
    self.inner.write().await.insert(id.clone(), now);
  }

  /// Look up `last_request_at` for one model. `None` if the model
  /// has never been touched in this daemon's lifetime.
  pub(crate) async fn last_request_at(&self, id: &ModelId) -> Option<Instant> {
    self.inner.read().await.get(id).copied()
  }
}

/// Pure-function fallback picker — no shared state inside. Callers
/// (the proxy's route module) build a `Vec<FallbackCandidate>` from
/// the supervisor snapshot + MRU tracker and hand it in; this fn
/// only sorts and picks.
///
/// `failed_arch` is the requested model's `general.architecture`.
/// `None` means the requested model had no arch metadata; the
/// family-prefer step is skipped in that case.
///
/// Selection rules — see the module docstring.
pub(crate) fn pick_fallback(
  mut candidates: Vec<FallbackCandidate>,
  failed_arch: Option<&str>,
) -> Option<FallbackCandidate> {
  if candidates.is_empty() {
    return None;
  }
  // Compose two sort keys: (a) family rank — 0 if the candidate's
  // arch matches `failed_arch`, 1 otherwise (so 0 < 1 ⇒ matches
  // sort first); (b) MRU rank — `Some(t)` newer beats older; `None`
  // sorts last. Using `sort_by` rather than `sort_by_key` so the
  // `Instant` comparison doesn't need an `Ord` impl (it has one,
  // but the reverse-order for "newest first" is clearer expressed
  // via direct cmp).
  candidates.sort_by(|a, b| {
    let arch_rank = |c: &FallbackCandidate| -> u8 {
      match (failed_arch, c.arch.as_deref()) {
        (Some(want), Some(have)) if want == have => 0,
        (Some(_), _) => 1,
        // No requested arch → don't bias by family.
        (None, _) => 0,
      }
    };
    let a_rank = arch_rank(a);
    let b_rank = arch_rank(b);
    if a_rank != b_rank {
      return a_rank.cmp(&b_rank);
    }
    // Within the family group: newest `last_request_at` wins;
    // `None` is the oldest possible (sorts last).
    match (a.last_request_at, b.last_request_at) {
      (Some(ta), Some(tb)) => tb.cmp(&ta),
      (Some(_), None) => std::cmp::Ordering::Less,
      (None, Some(_)) => std::cmp::Ordering::Greater,
      (None, None) => std::cmp::Ordering::Equal,
    }
  });
  candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::time::Duration;

  use super::*;

  fn id(path: &str, byte: u8) -> ModelId {
    ModelId {
      path: PathBuf::from(path),
      header_blake3: [byte; 32],
    }
  }

  fn cand(path: &str, byte: u8, arch: Option<&str>, secs_ago: Option<u64>) -> FallbackCandidate {
    let now = Instant::now();
    FallbackCandidate {
      model_id: id(path, byte),
      arch: arch.map(str::to_string),
      last_request_at: secs_ago.map(|s| now - Duration::from_secs(s)),
      port: 18000 + u16::from(byte),
      served_model_id: path.to_string(),
    }
  }

  #[tokio::test]
  async fn touch_records_the_id() {
    let m = MruTracker::new();
    let i = id("/m/a.gguf", 1);
    assert!(m.last_request_at(&i).await.is_none());
    m.touch(&i).await;
    assert!(m.last_request_at(&i).await.is_some());
  }

  #[test]
  fn empty_candidates_returns_none() {
    assert!(pick_fallback(Vec::new(), Some("llama")).is_none());
  }

  #[test]
  fn family_match_beats_non_match_regardless_of_mru() {
    // Even though the non-match is fresher, family-prefer wins.
    let cands = vec![
      cand("/m/older-family.gguf", 1, Some("qwen3"), Some(10)),
      cand("/m/newer-other.gguf", 2, Some("llama"), Some(1)),
    ];
    let picked = pick_fallback(cands, Some("qwen3")).unwrap();
    assert_eq!(picked.model_id.path, PathBuf::from("/m/older-family.gguf"));
  }

  #[test]
  fn within_family_newest_wins() {
    let cands = vec![
      cand("/m/older.gguf", 1, Some("qwen3"), Some(10)),
      cand("/m/newer.gguf", 2, Some("qwen3"), Some(1)),
    ];
    let picked = pick_fallback(cands, Some("qwen3")).unwrap();
    assert_eq!(picked.model_id.path, PathBuf::from("/m/newer.gguf"));
  }

  #[test]
  fn no_family_match_falls_through_to_mru_only() {
    let cands = vec![
      cand("/m/old-llama.gguf", 1, Some("llama"), Some(10)),
      cand("/m/new-bert.gguf", 2, Some("bert"), Some(1)),
    ];
    // Requested arch is qwen3 → nothing matches → both fall in the
    // "non-match" bucket → newest wins on MRU.
    let picked = pick_fallback(cands, Some("qwen3")).unwrap();
    assert_eq!(picked.model_id.path, PathBuf::from("/m/new-bert.gguf"));
  }

  #[test]
  fn unknown_arch_skips_family_prefer_falls_to_pure_mru() {
    // synthetic GGUF without arch metadata — pick any-MRU.
    let cands = vec![
      cand("/m/old.gguf", 1, Some("llama"), Some(10)),
      cand("/m/new.gguf", 2, Some("qwen3"), Some(1)),
    ];
    let picked = pick_fallback(cands, None).unwrap();
    assert_eq!(picked.model_id.path, PathBuf::from("/m/new.gguf"));
  }

  #[test]
  fn missing_mru_sorts_last_within_group() {
    let cands = vec![
      cand("/m/never-touched.gguf", 1, Some("qwen3"), None),
      cand("/m/touched.gguf", 2, Some("qwen3"), Some(5)),
    ];
    let picked = pick_fallback(cands, Some("qwen3")).unwrap();
    assert_eq!(picked.model_id.path, PathBuf::from("/m/touched.gguf"));
  }

  #[test]
  fn two_never_touched_same_arch_picks_insertion_order() {
    // Both candidates: same arch, both `last_request_at = None`. The
    // sort key collapses to Ordering::Equal across both factors, so
    // the sort is stable on input order. Locking this so a refactor
    // that swaps to an unstable sort surfaces immediately.
    let cands = vec![
      cand("/m/first.gguf", 1, Some("qwen3"), None),
      cand("/m/second.gguf", 2, Some("qwen3"), None),
    ];
    let picked = pick_fallback(cands, Some("qwen3")).unwrap();
    assert_eq!(picked.model_id.path, PathBuf::from("/m/first.gguf"));
  }
}
