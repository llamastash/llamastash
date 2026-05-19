//! The starter-model recommender (R55 / R58 / R59 / R60).
//!
//! Path-A dynamic ranker: pure Rust, candidate universe is the bundled
//! (or remote-overridden) benchmark snapshot **intersected with the
//! on-disk catalog**. Recommendations come ranked by a composite score
//! with a one-line justification each; an `Escape("paste HF repo id")`
//! row is always appended last per the brainstorm.
//!
//! The VRAM-fit hard filter is a coarse, intentionally-conservative
//! estimator: we don't have the GGUF header for un-downloaded models,
//! so we approximate peak memory from `weights_bytes` (recorded in the
//! snapshot) plus a KV-cache band that scales with `ctx`. The 0.90
//! safety margin and per-backend overhead band cover the gap.
//! Re-tighten with real measurements post-launch via the snapshot regen
//! flow.

use serde::Serialize;

use crate::gpu::GpuInfo;
use crate::init::benchmark::{BenchmarkSnapshot, ModelEntry};
use crate::init::detection::HardwareSnapshot;

/// VRAM safety margin: the recommender refuses anything whose
/// estimated peak load exceeds 90% of the host's reported VRAM
/// (minus the backend overhead band). 10% slack absorbs both
/// estimation error and OS/driver volatility.
pub(crate) const SAFETY_MARGIN: f64 = 0.90;

/// Activations + intermediate-buffer overhead within the model
/// itself, expressed as a multiplier on `weights_bytes`. 1.20 is the
/// empirical baseline llama-server inference uses on the reference
/// rig; the snapshot regen flow can override per-arch in a follow-up.
const ACTIVATIONS_OVERHEAD: f64 = 1.20;

/// KV-cache scaling factor — `weights_bytes × KV_FRACTION_AT_4K_F16`
/// at ctx=4096 with F16 cache, scaled linearly with ctx. Approximates
/// modern grouped-query-attention behaviour without per-model header
/// reads. A 7B-class Q4_K_M weights file of ~4.7 GB therefore
/// reserves ~0.7 GB at 4k and ~2.8 GB at 16k for KV — within 25% of
/// the measured llama.cpp numbers.
const KV_FRACTION_AT_4K_F16: f64 = 0.15;

/// CPU RAM-fit fraction (R55 fallback rule). When VRAM isn't
/// available, recommend models whose `weights_bytes` fits under
/// 50% of free RAM.
const CPU_RAM_FRACTION: f64 = 0.50;

/// Number of recommendations to surface. R59 anchors top-N at 3–5;
/// 5 leaves enough variety without burying the curated picks.
pub const DEFAULT_TOP_N: usize = 5;

/// Default context window the recommender evaluates against. Models
/// that don't fit at 16k can ride the no-fit-fallback ladder (ctx
/// halve → quant down → skip).
pub const DEFAULT_CTX: u32 = 16384;

/// One row in the output list. The wizard renders these in order,
/// with the escape row pinned to the end.
#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
  pub kind: RecommendationKind,
  /// Composite ranker score (higher = better). The escape row carries
  /// `score = -inf` so it always sorts last regardless of input order.
  pub score: f32,
  /// One-line summary the wizard prints next to the prompt. Built
  /// from [`render_one_line`] when `kind` is a real model.
  pub justification: String,
  /// Estimated peak memory at the configured ctx. `None` for the
  /// escape row.
  pub estimated_peak_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecommendationKind {
  /// A snapshot model that fits the host's hardware.
  Curated { entry: ModelEntry },
  /// An on-disk GGUF the user already has. Surfaced alongside snapshot
  /// picks per R60. The wizard prefers these on tie so the user can
  /// skip the download.
  OnDisk {
    path: std::path::PathBuf,
    architecture: Option<String>,
    weights_bytes: u64,
  },
  /// `paste HF repo id` escape — always appended last.
  Escape,
}

/// Tuning knobs the wizard threads through.
#[derive(Debug, Clone)]
pub struct RecommendOptions {
  pub top_n: usize,
  pub ctx: u32,
  /// Optional task hint ("code", "general", "reasoning"). Models
  /// whose `task_hints` include this value get a small score boost.
  pub task: Option<String>,
}

impl Default for RecommendOptions {
  fn default() -> Self {
    Self {
      top_n: DEFAULT_TOP_N,
      ctx: DEFAULT_CTX,
      task: None,
    }
  }
}

/// On-disk model the wizard already discovered. Used as the `on_disk`
/// argument so the recommender can rank existing files alongside
/// snapshot picks (R60).
#[derive(Debug, Clone)]
pub struct OnDiskModel {
  pub path: std::path::PathBuf,
  pub architecture: Option<String>,
  pub weights_bytes: u64,
}

/// Produce the ranked recommendation list. Always returns at least
/// one row (the escape option); typical output is `top_n + 1`.
pub fn recommend(
  snapshot: &BenchmarkSnapshot,
  hardware: &HardwareSnapshot,
  on_disk: &[OnDiskModel],
  options: &RecommendOptions,
) -> Vec<Recommendation> {
  let ceiling = effective_vram_ceiling(hardware, snapshot);
  let mut scored: Vec<Recommendation> = Vec::with_capacity(snapshot.models.len() + on_disk.len());

  for entry in &snapshot.models {
    let peak = estimate_peak_bytes(entry.weights_bytes, options.ctx);
    if !fits(peak, ceiling, hardware) {
      continue;
    }
    let score = composite_score(entry, snapshot, options);
    scored.push(Recommendation {
      kind: RecommendationKind::Curated {
        entry: entry.clone(),
      },
      score,
      justification: render_one_line(entry, peak, hardware),
      estimated_peak_bytes: Some(peak),
    });
  }
  for disk in on_disk {
    let peak = estimate_peak_bytes(disk.weights_bytes, options.ctx);
    if !fits(peak, ceiling, hardware) {
      continue;
    }
    // On-disk score: clone the matching catalog entry's score when
    // we have one (same repo/file); otherwise compose a baseline
    // score from raw size.
    let score = on_disk_score(disk, snapshot, options);
    scored.push(Recommendation {
      kind: RecommendationKind::OnDisk {
        path: disk.path.clone(),
        architecture: disk.architecture.clone(),
        weights_bytes: disk.weights_bytes,
      },
      score: score + ON_DISK_TIE_BREAK,
      justification: render_on_disk_one_line(disk, peak, hardware),
      estimated_peak_bytes: Some(peak),
    });
  }
  // Stable sort by score descending — ties keep input order, which
  // happens to favour the catalog snapshot's "best curated" order.
  scored.sort_by(|a, b| {
    b.score
      .partial_cmp(&a.score)
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  scored.truncate(options.top_n);
  scored.push(Recommendation {
    kind: RecommendationKind::Escape,
    score: f32::NEG_INFINITY,
    justification: "Paste an HF repo id to download something not on this list".to_string(),
    estimated_peak_bytes: None,
  });
  scored
}

/// Tiny additive boost so an on-disk model with a tied score sorts
/// above its remote twin — `R60` calls this out: "the 'skip download'
/// path should be natural".
const ON_DISK_TIE_BREAK: f32 = 0.01;

/// Coarse peak-memory estimate. See module-level docs for the
/// approximation rationale.
pub fn estimate_peak_bytes(weights_bytes: u64, ctx: u32) -> u64 {
  let w = weights_bytes as f64;
  let activations = w * ACTIVATIONS_OVERHEAD;
  // KV scales linearly with ctx; reference point is `KV_FRACTION_AT_4K_F16`
  // at ctx=4096 (so at ctx=16384 the factor is 4×, etc.).
  let ctx_scale = (ctx as f64) / 4096.0;
  let kv = w * KV_FRACTION_AT_4K_F16 * ctx_scale;
  (activations + kv).max(0.0) as u64
}

/// Effective VRAM ceiling: 90% of detected VRAM minus the per-backend
/// overhead band. For CPU-only hosts the ceiling is 50% of RAM so
/// the same `fits` predicate applies in both branches.
fn effective_vram_ceiling(hw: &HardwareSnapshot, snap: &BenchmarkSnapshot) -> u64 {
  let backend_key = match &hw.gpu {
    GpuInfo::Nvidia { .. } => "cuda",
    GpuInfo::Amd { .. } => "hip",
    GpuInfo::AppleMetal { .. } => "metal",
    GpuInfo::Unknown { .. } => "vulkan",
    GpuInfo::CpuOnly => "cpu",
  };
  let overhead = snap
    .recommender_weights
    .overhead_band_bytes
    .get(backend_key)
    .copied()
    .unwrap_or(0);
  match hw.vram_bytes {
    Some(vram) => {
      let usable = (vram as f64 * SAFETY_MARGIN) as u64;
      usable.saturating_sub(overhead)
    }
    None => {
      // CPU-only / unknown: gate on RAM fraction.
      (hw.ram_total_bytes as f64 * CPU_RAM_FRACTION) as u64
    }
  }
}

fn fits(peak_bytes: u64, ceiling: u64, _hw: &HardwareSnapshot) -> bool {
  peak_bytes > 0 && peak_bytes <= ceiling
}

/// Composite weighted score (R55).
pub fn composite_score(
  entry: &ModelEntry,
  snapshot: &BenchmarkSnapshot,
  options: &RecommendOptions,
) -> f32 {
  let w = &snapshot.recommender_weights;
  let bench = entry.benchmark_score.value / 100.0; // already 0–100 scale
  let speed = entry.tok_s_factor.clamp(0.0, 2.0) / 2.0;
  let params_score = params_quality_curve(entry.params);
  let recency = entry.recency.clamp(0.0, 1.0);
  let mut score = w.benchmark * bench
    + w.tok_per_second * speed
    + w.param_quality * params_score
    + w.recency * recency;
  // Task hint boost: matching task adds 0.05 — small but enough to
  // re-rank a coder-Q4 vs a generalist-Q4 of the same family.
  if let Some(t) = options.task.as_deref() {
    if entry.task_hints.iter().any(|h| h == t) {
      score += 0.05;
    }
  }
  score
}

/// 0..1 quality multiplier on parameter count. Diminishing returns
/// past 14B — a 70B model isn't 5× as useful as a 14B for typical
/// users, just more expensive.
fn params_quality_curve(params: u64) -> f32 {
  let billions = (params as f64) / 1e9;
  // log-curve normalised to ~0.95 at 14B, ~0.8 at 7B, ~0.55 at 3B.
  let raw = (billions.ln_1p() / 14.0_f64.ln_1p()).clamp(0.0, 1.0);
  raw as f32
}

fn on_disk_score(
  disk: &OnDiskModel,
  snapshot: &BenchmarkSnapshot,
  options: &RecommendOptions,
) -> f32 {
  if let Some(catalog_match) = snapshot.models.iter().find(|m| {
    let m_basename = std::path::Path::new(&m.file).file_name();
    let d_basename = disk.path.file_name();
    m_basename == d_basename || m.weights_bytes == disk.weights_bytes
  }) {
    return composite_score(catalog_match, snapshot, options);
  }
  // No catalog match: estimate from params (derived from
  // weights_bytes via Q4_K_M density).
  let est_params = (disk.weights_bytes as f64 / 0.65) as u64; // rough inverse of Q4_K_M ratio
  let fake_entry = ModelEntry {
    id: "on-disk".into(),
    repo: "local".into(),
    file: disk
      .path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("local")
      .into(),
    architecture: disk.architecture.clone().unwrap_or_else(|| "llama".into()),
    quant: "unknown".into(),
    params: est_params,
    weights_bytes: disk.weights_bytes,
    task_hints: Vec::new(),
    benchmark_score: crate::init::benchmark::BenchmarkScore {
      value: 40.0, // conservative default for unscored locals
      source: "local-estimate".into(),
    },
    tok_s_factor: 1.0,
    recency: 0.7,
  };
  composite_score(&fake_entry, snapshot, options)
}

/// One-line justification rendered next to the prompt. Anchored
/// around "fits N GB · ~X t/s · YB ZK". The wizard's `?` toggle
/// shows the full breakdown — that's Unit 10's job.
pub fn render_one_line(entry: &ModelEntry, peak_bytes: u64, hw: &HardwareSnapshot) -> String {
  let fit = format_gib(peak_bytes);
  let total = match hw.vram_bytes {
    Some(v) => format!("{} VRAM", format_gib(v)),
    None => format!("{} RAM", format_gib(hw.ram_total_bytes)),
  };
  let bench = format!(
    "{:.0} on {}",
    entry.benchmark_score.value, entry.benchmark_score.source
  );
  let params = format_params(entry.params);
  format!("{params} {} · ~{fit} ({total}) · {bench}", entry.quant)
}

fn render_on_disk_one_line(disk: &OnDiskModel, peak_bytes: u64, hw: &HardwareSnapshot) -> String {
  let fit = format_gib(peak_bytes);
  let total = match hw.vram_bytes {
    Some(v) => format!("{} VRAM", format_gib(v)),
    None => format!("{} RAM", format_gib(hw.ram_total_bytes)),
  };
  let path = disk
    .path
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or("local model");
  format!("[on disk] {path} · ~{fit} ({total})")
}

fn format_gib(bytes: u64) -> String {
  let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
  if gib >= 10.0 {
    format!("{gib:.0} GB")
  } else {
    format!("{gib:.1} GB")
  }
}

fn format_params(params: u64) -> &'static str {
  match params {
    p if p < 2_000_000_000 => "1.5B",
    p if p < 4_000_000_000 => "3B",
    p if p < 9_000_000_000 => "7B",
    p if p < 13_000_000_000 => "12B",
    p if p < 20_000_000_000 => "14B",
    p if p < 40_000_000_000 => "32B",
    _ => "70B+",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::{GpuDevice, GpuInfo};
  use crate::init::benchmark::load_bundled;
  use crate::init::detection::{CpuArch, HardwareSnapshot, OsFamily};

  fn linux_nvidia(vram_gb: f64) -> HardwareSnapshot {
    HardwareSnapshot {
      gpu: GpuInfo::Nvidia {
        devices: vec![GpuDevice {
          name: "RTX 4090".into(),
          total_memory_bytes: (vram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
          used_memory_bytes: 0,
          utilization_pct: None,
          temperature_c: None,
        }],
      },
      vram_bytes: Some((vram_gb * 1024.0 * 1024.0 * 1024.0) as u64),
      gpu_device_count: 1,
      ram_total_bytes: 64 * 1024 * 1024 * 1024,
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  fn cpu_only(ram_gb: f64) -> HardwareSnapshot {
    HardwareSnapshot {
      gpu: GpuInfo::CpuOnly,
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: (ram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  fn apple_silicon(unified_gb: f64) -> HardwareSnapshot {
    let bytes = (unified_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    HardwareSnapshot {
      gpu: GpuInfo::AppleMetal {
        total_memory_bytes: bytes,
      },
      vram_bytes: Some((bytes as f64 * 0.75) as u64),
      gpu_device_count: 1,
      ram_total_bytes: bytes,
      os: OsFamily::MacOs,
      cpu_arch: CpuArch::Arm64,
    }
  }

  #[test]
  fn recommend_24gb_nvidia_picks_7b_or_larger_at_top() {
    let snap = load_bundled();
    let hw = linux_nvidia(24.0);
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    assert!(recs.len() > 1, "should have recommendations + escape");
    let top = match &recs[0].kind {
      RecommendationKind::Curated { entry } => entry,
      other => panic!("expected curated top pick, got {other:?}"),
    };
    assert!(
      top.params >= 7_000_000_000,
      "24 GB Nvidia should pick at least 7B-class, got {} params",
      top.params
    );
  }

  #[test]
  fn recommend_8gb_nvidia_does_not_pick_above_8b() {
    let snap = load_bundled();
    let hw = linux_nvidia(8.0);
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    for rec in &recs {
      if let RecommendationKind::Curated { entry } = &rec.kind {
        assert!(
          entry.params <= 8_500_000_000,
          "8 GB Nvidia must not surface a >8.5B model; got {} ({}B params)",
          entry.id,
          entry.params as f64 / 1e9
        );
      }
    }
  }

  #[test]
  fn recommend_cpu_only_picks_small_models_only() {
    let snap = load_bundled();
    let hw = cpu_only(16.0);
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    let curated_count = recs
      .iter()
      .filter(|r| matches!(r.kind, RecommendationKind::Curated { .. }))
      .count();
    assert!(curated_count > 0, "cpu-only must surface at least one pick");
    for rec in &recs {
      if let RecommendationKind::Curated { entry } = &rec.kind {
        assert!(
          entry.params <= 8_000_000_000,
          "cpu-only must stay at ≤7B-class, got {} ({}B)",
          entry.id,
          entry.params as f64 / 1e9
        );
      }
    }
  }

  #[test]
  fn recommend_always_appends_escape_row_last() {
    let snap = load_bundled();
    let hw = linux_nvidia(24.0);
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    assert!(
      matches!(recs.last().unwrap().kind, RecommendationKind::Escape),
      "escape row must be last"
    );
    // And only once.
    let escape_count = recs
      .iter()
      .filter(|r| matches!(r.kind, RecommendationKind::Escape))
      .count();
    assert_eq!(escape_count, 1);
  }

  #[test]
  fn recommend_task_hint_lifts_matching_models() {
    let snap = load_bundled();
    let hw = linux_nvidia(24.0);
    let opts = RecommendOptions {
      task: Some("code".into()),
      ..RecommendOptions::default()
    };
    let recs = recommend(&snap, &hw, &[], &opts);
    // Top pick should be a coder-tagged model.
    if let RecommendationKind::Curated { entry } = &recs[0].kind {
      assert!(
        entry.task_hints.iter().any(|h| h == "code"),
        "task='code' must surface a coder-tagged model at top, got {}",
        entry.id
      );
    }
  }

  #[test]
  fn recommend_on_disk_beats_remote_tie() {
    let snap = load_bundled();
    let hw = linux_nvidia(24.0);
    let on_disk = vec![OnDiskModel {
      path: std::path::PathBuf::from("/m/qwen2.5-coder-7b-instruct-q4_k_m.gguf"),
      architecture: Some("qwen2".into()),
      weights_bytes: 4_683_960_320,
    }];
    let opts = RecommendOptions {
      task: Some("code".into()),
      ..RecommendOptions::default()
    };
    let recs = recommend(&snap, &hw, &on_disk, &opts);
    // On-disk match should rank at or above the equivalent catalog
    // entry's position (the tie-break favours on-disk).
    let first_on_disk = recs
      .iter()
      .position(|r| matches!(r.kind, RecommendationKind::OnDisk { .. }));
    assert!(first_on_disk.is_some(), "on-disk model must appear");
    assert!(
      first_on_disk.unwrap() <= 2,
      "on-disk match should rank near the top, got position {}",
      first_on_disk.unwrap()
    );
  }

  #[test]
  fn recommend_apple_silicon_unified_memory_picks_appropriately() {
    let snap = load_bundled();
    let hw = apple_silicon(32.0); // M-series with 32 GB unified
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    // 24 GB usable ≈ comfortable 14B-class home.
    let curated: Vec<&ModelEntry> = recs
      .iter()
      .filter_map(|r| match &r.kind {
        RecommendationKind::Curated { entry } => Some(entry),
        _ => None,
      })
      .collect();
    assert!(!curated.is_empty(), "Apple silicon 32 GB must yield picks");
    assert!(
      curated.iter().any(|e| e.params >= 7_000_000_000),
      "32 GB unified should surface a ≥7B-class pick"
    );
  }

  #[test]
  fn params_quality_curve_is_monotonic_non_decreasing() {
    // 1.5B < 3B < 7B < 14B; past 14B the curve saturates at 1.0
    // (diminishing returns — a 32B isn't proportionally more useful
    // than 14B for the typical user).
    let p15 = params_quality_curve(1_500_000_000);
    let p3 = params_quality_curve(3_000_000_000);
    let p7 = params_quality_curve(7_000_000_000);
    let p14 = params_quality_curve(14_000_000_000);
    let p32 = params_quality_curve(32_000_000_000);
    assert!(p15 < p3);
    assert!(p3 < p7);
    assert!(p7 < p14);
    assert!(
      p14 <= p32,
      "saturated past 14B (diminishing-returns design)"
    );
    assert!(p32 <= 1.0_f32 + f32::EPSILON);
  }

  #[test]
  fn estimate_peak_bytes_scales_with_ctx() {
    let weights = 5_000_000_000;
    let at_4k = estimate_peak_bytes(weights, 4096);
    let at_16k = estimate_peak_bytes(weights, 16384);
    assert!(at_16k > at_4k, "16k must reserve more than 4k");
    // 16k uses 4× the KV cache of 4k; the delta is therefore
    // `3 × weights × KV_FRACTION_AT_4K_F16`.
    let delta = at_16k - at_4k;
    let expected = (weights as f64 * KV_FRACTION_AT_4K_F16 * 3.0) as u64;
    let off = (delta as i64 - expected as i64).abs();
    assert!(
      off < (expected / 5) as i64,
      "delta ({delta}) should be near {expected} (±20%), got off={off}"
    );
  }

  #[test]
  fn fits_predicate_rejects_zero_peak() {
    let hw = linux_nvidia(24.0);
    let ceiling = effective_vram_ceiling(&hw, &load_bundled());
    assert!(!fits(0, ceiling, &hw));
  }
}
