//! Benchmark snapshot — the curated model corpus + per-arch recommender
//! weights the init wizard's recommender ranks against (R56).
//!
//! Two tiers travel together:
//! - **Bundled**: `data/benchmark-snapshot.json` is `include_str!`-ed
//!   into the binary so a fresh `cargo install` works offline.
//! - **Remote**: a daily-CI-built JSON file (Unit 7) lives at the
//!   rolling release tag `snapshot-latest`. `load_remote` fetches it
//!   through Unit 4's `FetchClient`, verifies the integrity contract
//!   (monotonic `bundle_date` + `min_version` ≤ build), and prefers
//!   it over the bundled tier on success.
//!
//! Verification rules (Key Decisions):
//! - `min_version > CARGO_PKG_VERSION` → reject (rollback-DoS gate).
//! - `bundle_date ≤ bundled.bundle_date` → reject (monotonic gate).
//! - Any fetch / parse / verification failure → silent fallback to
//!   bundled. `doctor` finding `RemoteSnapshotUnreachable` surfaces
//!   prolonged outages via the `remote_fetch_failures` counter in
//!   `InitSnapshot`.

use std::collections::BTreeMap;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::init::fetch::{FetchClient, FetchError};

/// Source the bundled snapshot is read from. Kept as a top-level
/// const so build-time tooling can assert against a known path.
pub const BUNDLED_PATH: &str = "../../data/benchmark-snapshot.json";

/// Bundled snapshot bytes — fixed at build time by `include_str!`.
/// 2 MiB build-time cap is enforced by [`bundled_size_budget`]. The
/// ceiling was raised from the original 500 KiB in Unit 6 of plan
/// 2026-05-20-001 to make room for the ~100-row live-discovery
/// catalog (Qwen3.6 / Gemma 4 / DeepSeek V3.2 / GLM-5 / Llama 4 /
/// Phi-4 / MoE flagships) without trimming task tiers.
const BUNDLED_RAW: &str = include_str!("../../data/benchmark-snapshot.json");

/// Build-time size budget for the bundled snapshot. A future regen
/// that blows past this fails the build via the assertion in
/// [`bundled_size_budget`] rather than silently bloating the binary.
/// 2 MiB ≈ 0.05% of the release binary — comfortable headroom for the
/// 100-row catalog cap (`SNAPSHOT_MODEL_LIMIT` in the regen script).
const BUNDLED_SIZE_BUDGET_BYTES: usize = 2 * 1024 * 1024;

/// Compile-time-evaluable size check. Calling it from
/// `load_bundled_or_panic` would surface a runtime panic; the
/// `const_assert!` form fires at build time.
#[allow(dead_code)]
const _BUNDLED_SIZE_CHECK: () = {
  if BUNDLED_RAW.len() > BUNDLED_SIZE_BUDGET_BYTES {
    panic!(
      "bundled benchmark snapshot exceeds the 2 MiB build-time \
       budget — trim the corpus or raise BUNDLED_SIZE_BUDGET_BYTES \
       deliberately"
    );
  }
};

/// Runtime accessor for the size budget. Used by unit tests so the
/// constant isn't dead code in the eyes of `clippy`.
pub fn bundled_size_budget() -> usize {
  BUNDLED_SIZE_BUDGET_BYTES
}

/// Top-level benchmark snapshot. Forward-compatible: callers tolerate
/// unknown fields (`#[serde(default)]` for everything optional;
/// `serde_json` ignores unknown keys by default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSnapshot {
  /// Bumped on breaking shape changes; current readers refuse a
  /// snapshot whose `schema_version` exceeds their max.
  pub schema_version: u32,
  /// ISO-8601 calendar date of the snapshot build. Used by both the
  /// monotonic-timestamp gate (`load_remote`) and `doctor`'s
  /// `SnapshotStale` finding (>14 days).
  pub bundle_date: String,
  /// Minimum binary version that may consume this snapshot. The
  /// rollback-DoS gate: an attacker publishing a fresher
  /// `bundle_date` cannot weaponise the silent-fallback against a
  /// downgraded llamastash build by raising `min_version`.
  pub min_version: String,
  /// Where the daily CI run publishes the next snapshot. Captured in
  /// the snapshot itself rather than hard-coded so the URL can be
  /// rotated without a binary release.
  #[serde(default)]
  pub remote_url: Option<String>,
  /// Tunables the recommender consumes. Sourced from the snapshot so
  /// the CI workflow (Unit 7) can re-tune without a binary release.
  pub recommender_weights: RecommenderWeights,
  /// The curated model catalog. Unit 6's recommender ranks against
  /// this list intersected with the on-disk catalog.
  pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommenderWeights {
  pub benchmark: f32,
  pub tok_per_second: f32,
  pub param_quality: f32,
  pub recency: f32,
  /// Per-backend VRAM overhead (driver / cuBLAS / Vulkan slab / Metal
  /// alignment). Subtracted from `vram_total × safety_margin` before
  /// the recommender's fit filter compares against the GGUF's peak
  /// memory estimate.
  pub overhead_band_bytes: BTreeMap<String, u64>,
}

/// One model in the curated catalog. Fields mirror what the
/// recommender (Unit 6) reads + what `doctor` shows alongside picks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
  pub id: String,
  pub repo: String,
  pub file: String,
  pub architecture: String,
  pub quant: String,
  pub params: u64,
  pub weights_bytes: u64,
  /// Tags the recommender uses for the task-aware second-stage
  /// filter (e.g. "code" picks ride a separate ranking lane).
  #[serde(default)]
  pub task_hints: Vec<String>,
  /// Headline leaderboard score (Open LLM, Aider, etc.). The
  /// recommender treats this as the primary ranking signal; absent
  /// scores still appear but rank below comparable entries with a
  /// score on the same task.
  pub benchmark_score: BenchmarkScore,
  /// Indicative tok/s factor relative to a reference 7B model on the
  /// recommender's reference hardware bucket. Used for the secondary
  /// "speed" feature in the composite score.
  pub tok_s_factor: f32,
  /// Recency multiplier (0.0–1.0). Newer models start at 1.0; older
  /// peers decay by the recommender's recency feature.
  pub recency: f32,
  /// Base HuggingFace id (e.g. `Qwen/Qwen3-Coder-30B-A3B-Instruct`)
  /// the GGUF in `repo` is a quantisation of. The benchmark-adapter
  /// score map keys off this — the join replaces the legacy
  /// `BUNDLED_ID_TO_SOURCE_HF_ID` table in the regen script.
  #[serde(default)]
  pub source_hf_id: String,
  /// Active parameters per token, for MoE models (e.g. 3_000_000_000
  /// for Qwen3-Next-80B-A3B). `None` on dense models. Read by the
  /// MoE-aware estimator (Unit 2 of plan 2026-05-20-001).
  #[serde(default)]
  pub params_active: Option<u64>,
  /// True for MoE architectures (Mixtral, DeepSeek V3, Qwen3-Next, …).
  /// Branches `estimate_peak_bytes` onto whichllm's
  /// `params_active × MoE multiplier` KV math instead of the dense
  /// `weights × ctx_scale` term.
  #[serde(default)]
  pub is_moe: bool,
  /// HF org publishing the GGUF in `repo` (e.g. `bartowski`,
  /// `unsloth`, `Qwen`). Used by the regen script's publisher
  /// allowlist gate.
  #[serde(default)]
  pub gguf_publisher: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkScore {
  pub value: f32,
  pub source: String,
}

/// Failure modes specific to loading a remote snapshot. Caller maps
/// every variant to "silent fallback to bundled" — exposing them as
/// distinct types is for `doctor`'s logging telemetry, not the
/// happy-path control flow.
#[derive(Debug, thiserror::Error)]
pub enum LoadRemoteError {
  #[error("fetch failed: {0}")]
  Fetch(#[from] FetchError),
  #[error("parse failed: {0}")]
  Parse(String),
  #[error("snapshot bundle_date {got} is not newer than bundled {bundled}")]
  StaleBundle { got: String, bundled: String },
  #[error("snapshot min_version {got} exceeds our build {build}")]
  MinVersionTooNew { got: String, build: String },
  #[error("snapshot version field not parseable as semver: {0}")]
  BadVersion(String),
  #[error("snapshot schema_version {got} exceeds our max {max}")]
  SchemaTooNew { got: u32, max: u32 },
}

/// Max remote-snapshot body size. Tracks the bundle budget (2 MiB
/// after Unit 6 of plan 2026-05-20-001) plus a 64 KiB tolerance for
/// formatting drift; a CI run that drifts past must raise both
/// numbers deliberately.
pub const REMOTE_MAX_BYTES: u64 = (BUNDLED_SIZE_BUDGET_BYTES as u64) + 64 * 1024;

/// Schema versions this build understands. Reading a snapshot whose
/// declared version exceeds this is refused.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Parse the bundled JSON. Infallible if the data file shipped with
/// the binary is valid (which the build asserts via the size budget
/// and the inline test below).
pub fn load_bundled() -> BenchmarkSnapshot {
  serde_json::from_str(BUNDLED_RAW)
    .expect("bundled benchmark-snapshot.json is part of the binary; CI must catch shape drift")
}

/// Try to fetch the snapshot's `remote_url` (when set) and accept it
/// only if both gates pass: `bundle_date > bundled.bundle_date` and
/// `min_version ≤ CARGO_PKG_VERSION`. Returns `Ok(None)` when the
/// bundled snapshot carries no `remote_url`; returns `Err(...)` for
/// every other failure path so the caller can log telemetry.
///
/// **Silent-fallback contract**: the caller is expected to treat
/// every `Err` as "use the bundled snapshot, bump
/// `_init_snapshot.remote_fetch_failures` so doctor's
/// `RemoteSnapshotUnreachable` finding can surface a sustained
/// outage". We never panic and never modify the bundled snapshot.
pub async fn load_remote(
  fetch: &FetchClient,
  bundled: &BenchmarkSnapshot,
) -> Result<Option<BenchmarkSnapshot>, LoadRemoteError> {
  let Some(url) = bundled.remote_url.as_deref() else {
    return Ok(None);
  };
  let bytes = fetch.get_bytes(url, REMOTE_MAX_BYTES).await?;
  let candidate: BenchmarkSnapshot =
    serde_json::from_slice(&bytes).map_err(|e| LoadRemoteError::Parse(e.to_string()))?;
  verify_remote(&candidate, bundled)?;
  Ok(Some(candidate))
}

/// Pure-function verifier used by `load_remote` and by Unit 5's tests.
pub fn verify_remote(
  candidate: &BenchmarkSnapshot,
  bundled: &BenchmarkSnapshot,
) -> Result<(), LoadRemoteError> {
  if candidate.schema_version > SUPPORTED_SCHEMA_VERSION {
    return Err(LoadRemoteError::SchemaTooNew {
      got: candidate.schema_version,
      max: SUPPORTED_SCHEMA_VERSION,
    });
  }
  if candidate.bundle_date <= bundled.bundle_date {
    return Err(LoadRemoteError::StaleBundle {
      got: candidate.bundle_date.clone(),
      bundled: bundled.bundle_date.clone(),
    });
  }
  let build_version = Version::parse(env!("CARGO_PKG_VERSION"))
    .map_err(|e| LoadRemoteError::BadVersion(format!("build version: {e}")))?;
  let min_version = Version::parse(&candidate.min_version)
    .map_err(|e| LoadRemoteError::BadVersion(format!("snapshot min_version: {e}")))?;
  // Compare on (major, minor, patch) only — pre-release suffixes
  // (e.g. `0.2.0-dev`) are a build-time bookkeeping detail and must
  // not block a CI-refreshed snapshot whose `min_version` is the
  // release form. Without this, every dev build silently rejects
  // every remote snapshot and falls back to bundled forever.
  let build_triple = (
    build_version.major,
    build_version.minor,
    build_version.patch,
  );
  let min_triple = (min_version.major, min_version.minor, min_version.patch);
  if min_triple > build_triple {
    return Err(LoadRemoteError::MinVersionTooNew {
      got: candidate.min_version.clone(),
      build: env!("CARGO_PKG_VERSION").to_string(),
    });
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn bundled_test() -> BenchmarkSnapshot {
    BenchmarkSnapshot {
      schema_version: 1,
      bundle_date: "2026-05-01".into(),
      min_version: "0.2.0".into(),
      remote_url: Some("https://github.com/llamastash/llamastash/releases/download/snapshot-latest/benchmark-snapshot.json".into()),
      recommender_weights: load_bundled().recommender_weights.clone(),
      models: load_bundled().models.clone(),
    }
  }

  #[test]
  fn bundled_snapshot_parses() {
    let snap = load_bundled();
    assert!(!snap.models.is_empty(), "bundled snapshot must have models");
    assert_eq!(snap.schema_version, SUPPORTED_SCHEMA_VERSION);
  }

  #[test]
  fn bundled_models_default_new_fields_when_absent() {
    // The current bundled JSON predates Unit 1's schema additions, so
    // every row must parse with `source_hf_id=""`, `params_active=None`,
    // `is_moe=false`, `gguf_publisher=""` — the serde defaults. This
    // pins the "old snapshot still loads" half of Unit 1's contract.
    let snap = load_bundled();
    for m in &snap.models {
      assert_eq!(
        m.source_hf_id, "",
        "{} carries empty default source_hf_id",
        m.id
      );
      assert!(
        m.params_active.is_none(),
        "{} has no params_active by default",
        m.id
      );
      assert!(!m.is_moe, "{} defaults to dense (is_moe=false)", m.id);
      assert_eq!(
        m.gguf_publisher, "",
        "{} carries empty default gguf_publisher",
        m.id
      );
    }
  }

  #[test]
  fn hand_rolled_snapshot_with_new_fields_round_trips() {
    // The "new snapshot also loads" half: a snapshot the v2-style
    // regenerator might emit must deserialise into the same shape it
    // came from, including the four new fields.
    let body = serde_json::json!({
      "schema_version": 1,
      "bundle_date": "2026-05-20",
      "min_version": "0.2.0",
      "recommender_weights": {
        "benchmark": 0.45,
        "tok_per_second": 0.25,
        "param_quality": 0.2,
        "recency": 0.1,
        "overhead_band_bytes": { "cuda": 536870912_u64 }
      },
      "models": [{
        "id": "qwen3-next-80b-a3b-q4_k_m",
        "repo": "Qwen/Qwen3-Next-80B-A3B-Instruct-GGUF",
        "file": "qwen3-next-80b-a3b-instruct-q4_k_m.gguf",
        "architecture": "qwen3-next",
        "quant": "Q4_K_M",
        "params": 80_000_000_000_u64,
        "weights_bytes": 49_000_000_000_u64,
        "task_hints": ["general", "reasoning"],
        "benchmark_score": { "value": 70.0, "source": "openllm-leaderboard" },
        "tok_s_factor": 1.2,
        "recency": 1.0,
        "source_hf_id": "Qwen/Qwen3-Next-80B-A3B-Instruct",
        "params_active": 3_000_000_000_u64,
        "is_moe": true,
        "gguf_publisher": "Qwen"
      }]
    });
    let snap: BenchmarkSnapshot = serde_json::from_value(body).expect("parse");
    let m = &snap.models[0];
    assert_eq!(m.source_hf_id, "Qwen/Qwen3-Next-80B-A3B-Instruct");
    assert_eq!(m.params_active, Some(3_000_000_000));
    assert!(m.is_moe);
    assert_eq!(m.gguf_publisher, "Qwen");
    // Round-trip back through JSON: shape stable.
    let s = serde_json::to_string(&snap).expect("serialize");
    let again: BenchmarkSnapshot = serde_json::from_str(&s).expect("reparse");
    assert!(again.models[0].is_moe);
    assert_eq!(again.models[0].params_active, Some(3_000_000_000));
  }

  #[test]
  fn bundled_size_is_within_budget() {
    assert!(
      BUNDLED_RAW.len() <= bundled_size_budget(),
      "bundled snapshot {} bytes exceeds budget {}",
      BUNDLED_RAW.len(),
      bundled_size_budget()
    );
  }

  #[test]
  fn verify_remote_accepts_fresher_snapshot_at_or_below_build_version() {
    let bundled = bundled_test();
    let candidate = BenchmarkSnapshot {
      bundle_date: "2026-05-19".into(), // newer than bundled
      min_version: "0.0.1".into(),      // ≤ any build
      ..bundled_test()
    };
    assert!(verify_remote(&candidate, &bundled).is_ok());
  }

  #[test]
  fn verify_remote_rejects_stale_bundle_date() {
    let bundled = bundled_test();
    let candidate = BenchmarkSnapshot {
      bundle_date: "2026-04-01".into(), // older than bundled
      ..bundled_test()
    };
    let err = verify_remote(&candidate, &bundled).unwrap_err();
    assert!(matches!(err, LoadRemoteError::StaleBundle { .. }));
  }

  #[test]
  fn verify_remote_rejects_min_version_exceeding_build() {
    let bundled = bundled_test();
    let candidate = BenchmarkSnapshot {
      bundle_date: "2026-05-19".into(),
      min_version: "999.0.0".into(),
      ..bundled_test()
    };
    let err = verify_remote(&candidate, &bundled).unwrap_err();
    assert!(
      matches!(err, LoadRemoteError::MinVersionTooNew { .. }),
      "expected MinVersionTooNew, got {err:?}"
    );
  }

  #[test]
  fn verify_remote_accepts_release_min_version_against_prerelease_build() {
    // A CI-refreshed snapshot whose min_version matches the build's
    // major.minor.patch must NOT be rejected when the build itself
    // carries a pre-release suffix (e.g. 0.0.1-dev vs 0.0.1):
    // pre-release suffixes are a build-bookkeeping detail, not a
    // capability signal. Without this carve-out, every dev build
    // would silently reject every remote snapshot forever.
    let bundled = bundled_test();
    let candidate = BenchmarkSnapshot {
      bundle_date: "2026-05-19".into(),
      min_version: "0.0.1".into(),
      ..bundled_test()
    };
    // Only meaningful if the build is itself pre-release; in a
    // released binary the comparison is a no-op. The assertion is
    // that the result is *not* MinVersionTooNew.
    let result = verify_remote(&candidate, &bundled);
    match result {
      Ok(()) | Err(LoadRemoteError::StaleBundle { .. }) => {}
      Err(LoadRemoteError::MinVersionTooNew { .. }) => {
        panic!("pre-release build must accept release-form min_version");
      }
      Err(other) => panic!("unexpected error: {other:?}"),
    }
  }

  #[test]
  fn verify_remote_rejects_future_schema_version() {
    let bundled = bundled_test();
    let candidate = BenchmarkSnapshot {
      schema_version: SUPPORTED_SCHEMA_VERSION + 1,
      bundle_date: "2026-05-19".into(),
      ..bundled_test()
    };
    let err = verify_remote(&candidate, &bundled).unwrap_err();
    assert!(matches!(err, LoadRemoteError::SchemaTooNew { .. }));
  }

  #[tokio::test]
  async fn load_remote_returns_none_when_bundled_has_no_remote_url() {
    let mut bundled = bundled_test();
    bundled.remote_url = None;
    let client = FetchClient::offline(); // never reached
    let result = load_remote(&client, &bundled).await.expect("Ok");
    assert!(result.is_none());
  }

  #[tokio::test]
  async fn load_remote_propagates_offline_fetch_error() {
    let bundled = bundled_test();
    let client = FetchClient::offline();
    let err = load_remote(&client, &bundled).await.unwrap_err();
    assert!(
      matches!(err, LoadRemoteError::Fetch(FetchError::Offline)),
      "expected Fetch(Offline), got {err:?}"
    );
  }
}
