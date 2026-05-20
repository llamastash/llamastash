//! Reference GGUF + fallback identity (Unit 4).
//!
//! Pinned by HuggingFace commit SHA so the byte-stream the UAT
//! exercises is reproducible across runs and across the maintainer's
//! four backends. If the primary fetch fails on a given run, the
//! orchestrator retries with the fallback; both failing trips an
//! exit-1 with `failure_summary.message` listing both attempts.
//!
//! Constraint envelope (from origin §Reference model contract):
//!
//! * ≤ 1 GB on disk (post-Q4 quantization).
//! * Loads in ≤ 3 GB of usable GPU memory at `--ctx 2048`, including
//!   unified-memory and iGPU configurations.
//! * Supports OpenAI-compatible chat completion meaningfully.
//!
//! The `commit_sha` fields below ship as a `<TBD-locked-on-first-dry-run>`
//! sentinel — the maintainer locks in real SHAs during the first warm-mode
//! dry run, captured here as a constant update. Until the SHA is locked,
//! hf-hub resolves the repo's default branch (`main`) — the precise
//! behavior `--revision` would short-circuit. Documented because a
//! latent placeholder is more honest than a fabricated SHA.

/// Sentinel value stored in `commit_sha` until the maintainer's first
/// warm-mode dry-run locks in real SHAs. `is_unlocked()` is the
/// single match site so a future rename doesn't drift across the
/// declaration and the detection logic.
pub const PLACEHOLDER_SHA: &str = "<TBD-locked-on-first-dry-run>";

/// Identity of a pinned reference model. Reused for both primary and
/// fallback so the orchestrator can iterate over a fixed pair.
#[derive(Debug, Clone)]
pub struct ReferenceModel {
  /// `owner/repo` HF id passed to `llamastash init --model`.
  pub repo: &'static str,
  /// The single `.gguf` shard the UAT loads. Resolved by hf-hub
  /// from the snapshot at `commit_sha`.
  pub filename: &'static str,
  /// HuggingFace commit SHA (or branch / tag) passed to
  /// `llamastash init --revision`. [`PLACEHOLDER_SHA`] until the
  /// maintainer's first real warm-mode dry-run locks the SHA in.
  /// While placeholder, the UAT falls back to the repo's default
  /// branch — fine for development, not for release verification.
  pub commit_sha: &'static str,
  /// Expected on-disk size in bytes after Q4 quantization. Advisory
  /// only — used in the report's `host.warnings` if the actual
  /// download size deviates by more than ±10% so silent file
  /// substitutions surface during outcome review.
  pub expected_size_bytes: u64,
}

/// Primary reference model. Qwen2.5-0.5B-Instruct-GGUF Q4_K_M sits at
/// ~400 MB on disk; loads comfortably in ≤ 3 GB on Metal / iGPU /
/// discrete GPU; Apache 2.0 license keeps redistribution clear of
/// audit work the plan explicitly de-scoped.
pub const PRIMARY: ReferenceModel = ReferenceModel {
  repo: "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
  filename: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
  commit_sha: PLACEHOLDER_SHA,
  expected_size_bytes: 400 * 1024 * 1024,
};

/// Fallback when the primary fetch fails (mocked or real HF outage).
/// SmolLM2-360M-Instruct-GGUF Q4_K_M at ~270 MB on disk, also Apache
/// 2.0. Tighter VRAM envelope so a constrained iGPU box still passes
/// when the primary stretches the limit.
pub const FALLBACK: ReferenceModel = ReferenceModel {
  repo: "HuggingFaceTB/SmolLM2-360M-Instruct-GGUF",
  filename: "smollm2-360m-instruct-q4_k_m.gguf",
  commit_sha: PLACEHOLDER_SHA,
  expected_size_bytes: 270 * 1024 * 1024,
};

/// True iff the `commit_sha` field is still the documented sentinel.
/// The orchestrator records this as a warning in the report's
/// `host.warnings` so the maintainer's first dry run produces an
/// obvious "lock the SHA" todo.
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
    assert!(is_unlocked(&PRIMARY));
    assert!(is_unlocked(&FALLBACK));
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
  fn expected_size_bytes_satisfies_one_gb_disk_envelope() {
    // Origin §Reference model contract — ≤ 1 GB on disk after Q4.
    let one_gb = 1024 * 1024 * 1024;
    assert!(PRIMARY.expected_size_bytes <= one_gb);
    assert!(FALLBACK.expected_size_bytes <= one_gb);
  }
}
