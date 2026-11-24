//! Reference GGUF + fallback identity.
//!
//! Pinned by HuggingFace commit SHA so the byte-stream the UAT
//! exercises is reproducible across runs and across the maintainer's
//! maintained UAT lanes. If the primary fetch fails on a given run, the
//! orchestrator retries with the fallback; both failing trips an
//! exit-1 with `failure_summary.message` listing both attempts.
//!
//! Constraint envelope:
//!
//! * ≤ 1 GB on disk (post-Q4 quantization).
//! * Loads in ≤ 3 GB of usable GPU memory at `--ctx 2048`, including
//!   unified-memory and iGPU configurations.
//! * Supports OpenAI-compatible chat completion meaningfully.

/// Sentinel for a `commit_sha` that is pinned to no real commit.
/// `is_unlocked()` is the single match site so a future rename doesn't drift
/// across the declaration and the detection logic.
pub const PLACEHOLDER_SHA: &str = "<TBD-locked-on-first-dry-run>";

/// Reused for both primary and fallback so the orchestrator can iterate over a
/// fixed pair.
#[derive(Debug, Clone)]
pub struct ReferenceModel {
  /// `owner/repo` HF id passed to `llamastash init --model`.
  pub repo: &'static str,
  /// The single `.gguf` shard the UAT loads. Resolved by hf-hub
  /// from the snapshot at `commit_sha`.
  pub filename: &'static str,
  /// HuggingFace commit SHA (or branch / tag) passed to
  /// `llamastash init --revision`. While placeholder, the UAT falls
  /// back to the repo's default branch — fine for development, not for
  /// release verification.
  pub commit_sha: &'static str,
  /// Expected on-disk size in bytes after Q4 quantization. Advisory
  /// only — used in the report's `host.warnings` if the actual
  /// download size deviates by more than ±10% so silent file
  /// substitutions surface during outcome review.
  pub expected_size_bytes: u64,
}

/// Qwen2.5-0.5B-Instruct-GGUF Q4_K_M sits at ~469 MiB on disk; loads
/// comfortably in ≤ 3 GB on Metal / iGPU / discrete GPU; Apache 2.0 license
/// keeps redistribution clear of audit work.
pub const PRIMARY: ReferenceModel = ReferenceModel {
  repo: "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
  filename: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
  commit_sha: "9217f5db79a29953eb74d5343926648285ec7e67",
  expected_size_bytes: 491_400_032,
};

/// Fallback when the primary fetch fails (mocked or real HF outage).
/// SmolLM2-360M-Instruct-GGUF Q8_0 sits at ~369 MiB on disk, also
/// Apache 2.0. Still well inside the 1 GiB / low-VRAM envelope while
/// tracking the only currently-published quant in the upstream repo.
pub const FALLBACK: ReferenceModel = ReferenceModel {
  repo: "HuggingFaceTB/SmolLM2-360M-Instruct-GGUF",
  filename: "smollm2-360m-instruct-q8_0.gguf",
  commit_sha: "593b5a2e04c8f3e4ee880263f93e0bd2901ad47f",
  expected_size_bytes: 386_404_992,
};

/// True iff the `commit_sha` field is still the documented sentinel.
/// The orchestrator records this as a warning in the report's
/// `host.warnings` so a dry run on the placeholder produces an obvious
/// "lock the SHA" marker.
pub fn is_unlocked(model: &ReferenceModel) -> bool {
  model.commit_sha == PLACEHOLDER_SHA
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn primary_and_fallback_have_distinct_repos() {
    assert_ne!(PRIMARY.repo, FALLBACK.repo);
  }

  #[test]
  fn placeholder_sha_is_detected_by_is_unlocked() {
    let unlocked = ReferenceModel {
      commit_sha: PLACEHOLDER_SHA,
      ..PRIMARY
    };
    assert!(is_unlocked(&unlocked));
  }

  #[test]
  fn a_real_sha_is_not_unlocked() {
    let locked = ReferenceModel {
      commit_sha: "deadbeefcafe",
      ..PRIMARY
    };
    assert!(!is_unlocked(&locked));
  }

  #[test]
  fn shipped_reference_models_are_locked() {
    assert!(!is_unlocked(&PRIMARY));
    assert!(!is_unlocked(&FALLBACK));
  }

  #[test]
  fn expected_size_bytes_satisfies_one_gb_disk_envelope() {
    let one_gb = 1024 * 1024 * 1024;
    assert!(PRIMARY.expected_size_bytes <= one_gb);
    assert!(FALLBACK.expected_size_bytes <= one_gb);
  }
}
