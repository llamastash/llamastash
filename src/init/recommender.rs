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

/// KV cache scaling: bytes per billion attention-bearing parameters
/// per 1k context tokens. Ported from whichllm's
/// `_KV_BYTES_PER_BPARAM_PER_KCTX` (3.5 MB) — calibrated against
/// published llama.cpp memory reports for Qwen2.5-7B, Qwen3-32B, and
/// Llama-3.1-70B, with a small graph-compute-buffer bump rolled in.
const KV_BYTES_PER_BPARAM_PER_KCTX: f64 = 3_500_000.0;

/// MoE attention-parameter multiplier on `params_active`. Ported from
/// whichllm's `_MOE_ATTENTION_PARAM_MULTIPLIER` (4.0): the attention
/// path on MoE models carries roughly 4× the active-params worth of
/// state because it sees every expert's tokens regardless of routing.
const MOE_ATTENTION_PARAM_MULTIPLIER: f64 = 4.0;

/// CPU RAM-fit fraction (R55 fallback rule). When VRAM isn't
/// available, recommend models whose `weights_bytes` fits under
/// 50% of free RAM.
const CPU_RAM_FRACTION: f64 = 0.50;

/// Number of recommendations to surface. R59's original budget was
/// 3–5; we lifted it to 10 once `--only models --json` started being
/// used as a comparison surface against `whichllm --json` (whose
/// default top-N is also 10). Ten leaves enough variety to span
/// quant / param / MoE tiers without burying the curated picks, and
/// the interactive picker scrolls cleanly past a 10-entry list.
pub const DEFAULT_TOP_N: usize = 10;

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

// `large_enum_variant`: ModelEntry is ~272 bytes after Unit 1's
// schema additions; OnDisk is 56, Escape is 0. Boxing the Curated
// payload would force every Recommendation consumer to dereference
// for no measurable win — top-N is 5-6 entries so the per-list
// overhead is ~1.4 KB. Accept the size asymmetry instead.
#[allow(clippy::large_enum_variant)]
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
    if !profile_admits(entry, options) {
      continue;
    }
    let peak = estimate_peak_bytes_for_entry(entry, options.ctx);
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
  // Dedupe by `source_hf_id` after ranking so different GGUF
  // publishers re-hosting the same upstream model don't take multiple
  // slots in the top-N. The regen flow already dedupes on
  // `(source_hf_id, quant)`, but this is defence-in-depth: a remote
  // snapshot a binary downloads at runtime might have pre-dedup data,
  // and an on-disk model may collide with its remote catalog entry.
  // Empty `source_hf_id` (legacy schema rows, on-disk-only paths)
  // bypasses the dedup to avoid collapsing unrelated entries.
  let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
  scored.retain(|rec| {
    let key = match &rec.kind {
      RecommendationKind::Curated { entry } => entry.source_hf_id.clone(),
      RecommendationKind::OnDisk { .. } | RecommendationKind::Escape => String::new(),
    };
    if key.is_empty() {
      return true;
    }
    seen.insert(key)
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

/// Task-profile filter mirroring whichllm's `_matches_profile`. When
/// the caller asks for a specific task (`code`, `math`, ...) only
/// entries tagged for that task are admitted. When no task is set
/// (default general-purpose listing) any entry whose model id
/// includes specialization markers (`coder`, `math`, `vision`, etc.)
/// is dropped — keeps a coder-tuned 30B from appearing alongside a
/// general-purpose 30B of the same family in the default top-10.
fn profile_admits(entry: &ModelEntry, options: &RecommendOptions) -> bool {
  if let Some(task) = options.task.as_deref() {
    return entry.task_hints.iter().any(|h| h == task);
  }
  // No task specified → general profile. Drop specialization-tagged
  // entries the same way whichllm does for `task_profile="general"`.
  let id_lower = entry.source_hf_id.to_ascii_lowercase();
  let name = id_lower
    .rsplit_once('/')
    .map(|(_, n)| n)
    .unwrap_or(&id_lower);
  const SPECIALIZATION_MARKERS: &[&str] =
    &["coder", "codegen", "starcoder", "program", "coding", "math"];
  for marker in SPECIALIZATION_MARKERS {
    if name.contains(marker) {
      return false;
    }
  }
  true
}

/// Ported from whichllm's `engine/vram.py::estimate_vram` so our fit
/// predicate gates the same models theirs does. Formula:
///
///   peak = weights + kv_cache + activation
///
/// Where:
/// - **weights** = `weights_bytes` (file footprint, no overhead mult)
/// - **kv_cache** = 3.5 MB × params_b × ctx_k, with params_b scaled by
///   the MoE attention multiplier (×4) when an MoE row provides
///   `params_active`. Dense rows use total params directly.
/// - **activation** = 400 MB floor + 0.08 B/active-param +
///   150 MB per 4K of context.
///
/// Per-backend framework overhead (CUDA ≈ 512 MB, Vulkan ≈ 1 GB,
/// etc.) is subtracted from VRAM at the ceiling step, not added here
/// — see `effective_vram_ceiling`. The legacy `w × 1.20` activation
/// multiplier this replaces over-counted by 5-10× for MoE rows (it
/// treated the *entire* weights file as activation) and pushed
/// genuinely-runnable 120B MoE models off the recommender's list.
pub fn estimate_peak_bytes(weights_bytes: u64, ctx: u32) -> u64 {
  estimate_peak_bytes_inner(
    weights_bytes,
    weights_bytes_to_dense_params(weights_bytes),
    false,
    ctx,
  )
}

/// Inverse of "Q4_K_M density" used by the bytes-only entry point as
/// a coarse dense-params estimate. Picks 0.56 B/byte ≈ 0.56 GB/B —
/// the same Q4_K_M density used by `_estimate_weights_bytes` in
/// `hf_discovery.py`. Off by up to 2× for non-Q4 callers (e.g.
/// on-disk Q8_0 file inferring 2× too many params), which only
/// affects the activation term — a 5-10% overshoot.
fn weights_bytes_to_dense_params(weights_bytes: u64) -> u64 {
  ((weights_bytes as f64) / 0.5625) as u64
}

/// MoE-aware peak-memory estimate. For MoE rows, KV + activation
/// scale with `params_active` (the slice that runs per token) plus
/// the ×4 attention multiplier; for dense rows they scale with
/// total params and skip the multiplier. Weights stay fully resident
/// either way.
pub fn estimate_peak_bytes_for_entry(entry: &ModelEntry, ctx: u32) -> u64 {
  let (effective_params, is_moe) = match (entry.is_moe, entry.params_active) {
    (true, Some(active)) => (active, true),
    // MoE flagged but no active-param count: treat as dense for the
    // KV/activation math (no ×4 attention multiplier) since the
    // active slice is unknown. Errs on the side of underestimating
    // KV — the fit gate's 10% margin absorbs the difference.
    (true, None) => (entry.params, false),
    (false, _) => (entry.params, false),
  };
  estimate_peak_bytes_inner(entry.weights_bytes, effective_params, is_moe, ctx)
}

fn estimate_peak_bytes_inner(
  weights_bytes: u64,
  effective_params: u64,
  is_moe: bool,
  ctx: u32,
) -> u64 {
  let w = weights_bytes as f64;
  let kv = kv_cache_bytes(effective_params, is_moe, ctx);
  let activation = activation_bytes(effective_params, ctx);
  (w + kv + activation).max(0.0) as u64
}

/// KV cache bytes. `KV_BYTES_PER_BPARAM_PER_KCTX × params_b × ctx_k`,
/// with the MoE attention multiplier applied only for MoE rows. 3.5
/// MB / B / K is calibrated against published llama.cpp memory
/// reports for Qwen2.5-7B, Qwen3-32B, and Llama-3.1-70B.
fn kv_cache_bytes(params: u64, is_moe: bool, ctx: u32) -> f64 {
  let mut params_b = (params as f64) / 1.0e9;
  if is_moe {
    params_b *= MOE_ATTENTION_PARAM_MULTIPLIER;
  }
  let ctx_k = (ctx as f64) / 1024.0;
  KV_BYTES_PER_BPARAM_PER_KCTX * params_b * ctx_k
}

/// Activation / scratch buffer estimate. Ported from whichllm's
/// `_activation_bytes`: a small floor (400 MB), a per-param term
/// (~0.08 B/param), and a per-context term (~150 MB/4K).
fn activation_bytes(params: u64, ctx: u32) -> f64 {
  let base = 400_000_000.0;
  let param_term = (params as f64) * 0.08;
  let ctx_term = (ctx as f64 / 4096.0) * 150_000_000.0;
  base + param_term + ctx_term
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
  // Task hint boost: when caller asks for a specific task, every
  // task-matched entry outranks every non-matched one (a +1.0 lift
  // exceeds the entire 0.0–1.0 composite scale). Inside each tier the
  // composite score still orders things normally. Without this an OLB
  // general-bench winner outranks a coder-tagged peer with a lower
  // OLB score, which contradicts what `task="code"` is asking for.
  if let Some(t) = options.task.as_deref() {
    if entry.task_hints.iter().any(|h| h == t) {
      score += 1.0;
    }
  }
  score
}

/// 0..1 quality multiplier on parameter count. Log-curve normalised
/// at 80B so the "knowledge capacity" dimension keeps rewarding
/// bigger models all the way up to whichllm's frontier picks instead
/// of saturating at 14B. Calibration points:
///   3B → 0.32, 7B → 0.47, 14B → 0.61, 30B → 0.78, 80B → 1.0, 120B → 1.0
fn params_quality_curve(params: u64) -> f32 {
  let billions = (params as f64) / 1e9;
  let raw = (billions.ln_1p() / 80.0_f64.ln_1p()).clamp(0.0, 1.0);
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
    source_hf_id: String::new(),
    params_active: None,
    is_moe: false,
    gguf_publisher: String::new(),
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
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
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
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
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
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
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
    // After the whichllm-aligned VRAM estimator landed, params is no
    // longer the right proxy for "will this fit". Use the actual peak
    // vs ceiling check — every surfaced pick must have an estimated
    // peak that lands under the 8 GB host's effective ceiling. The
    // recommender's own fit gate already enforces this, so the test
    // is asserting the contract rather than a hand-picked param cap.
    let snap = load_bundled();
    let hw = linux_nvidia(8.0);
    let ceiling = effective_vram_ceiling(&hw, &snap);
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    for rec in &recs {
      if let (RecommendationKind::Curated { entry }, Some(peak)) =
        (&rec.kind, rec.estimated_peak_bytes)
      {
        assert!(
          peak <= ceiling,
          "8 GB Nvidia must not surface a model whose peak exceeds the ceiling \
           ({}: peak={}, ceiling={})",
          entry.id,
          peak,
          ceiling
        );
      }
    }
  }

  #[test]
  fn recommend_cpu_only_picks_small_models_only() {
    // Same shape as the 8GB test: assert the fit contract rather than
    // a parametric threshold. CPU-only 16 GB has an 8 GB effective
    // ceiling (50% RAM fraction); any pick must land under it.
    let snap = load_bundled();
    let hw = cpu_only(16.0);
    let ceiling = effective_vram_ceiling(&hw, &snap);
    let recs = recommend(&snap, &hw, &[], &RecommendOptions::default());
    let curated_count = recs
      .iter()
      .filter(|r| matches!(r.kind, RecommendationKind::Curated { .. }))
      .count();
    assert!(curated_count > 0, "cpu-only must surface at least one pick");
    for rec in &recs {
      if let (RecommendationKind::Curated { entry }, Some(peak)) =
        (&rec.kind, rec.estimated_peak_bytes)
      {
        assert!(
          peak <= ceiling,
          "cpu-only 16 GB must not surface a model whose peak exceeds the ceiling \
           ({}: peak={}, ceiling={})",
          entry.id,
          peak,
          ceiling
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

  /// Tiny in-memory snapshot exercising task-hint + on-disk ranking
  /// without depending on whatever the live bundled catalog happens to
  /// contain. Two entries that fit linux_nvidia(24): a general 14B
  /// with a higher OLB-style score, and a coder 7B with a lower one
  /// — exactly the shape the regen rotation produces.
  fn task_hint_fixture() -> BenchmarkSnapshot {
    let bundled = load_bundled();
    BenchmarkSnapshot {
      schema_version: 1,
      bundle_date: "2026-05-20".into(),
      min_version: "0.0.1".into(),
      remote_url: None,
      recommender_weights: bundled.recommender_weights.clone(),
      models: vec![
        ModelEntry {
          task_hints: vec!["general".into(), "reasoning".into()],
          benchmark_score: crate::init::benchmark::BenchmarkScore {
            value: 62.0,
            source: "test-general".into(),
          },
          ..dense_entry(7_700_000_000, 14_000_000_000) // 14B general
        },
        ModelEntry {
          file: "qwen2.5-coder-7b-instruct-q4_k_m.gguf".into(),
          task_hints: vec!["code".into()],
          benchmark_score: crate::init::benchmark::BenchmarkScore {
            value: 34.0,
            source: "test-coder".into(),
          },
          ..dense_entry(4_283_784_288, 7_000_000_000) // 7B coder
        },
      ],
    }
  }

  #[test]
  fn recommend_task_hint_lifts_matching_models() {
    let snap = task_hint_fixture();
    let hw = linux_nvidia(24.0);
    let opts = RecommendOptions {
      task: Some("code".into()),
      ..RecommendOptions::default()
    };
    let recs = recommend(&snap, &hw, &[], &opts);
    // Top pick must be the coder-tagged 7B even though the general
    // 14B has a higher raw benchmark score.
    match &recs[0].kind {
      RecommendationKind::Curated { entry } => assert!(
        entry.task_hints.iter().any(|h| h == "code"),
        "task='code' must surface a coder-tagged model at top, got {}",
        entry.id
      ),
      other => panic!("expected Curated at position 0, got {other:?}"),
    }
  }

  #[test]
  fn recommend_on_disk_beats_remote_tie() {
    let snap = task_hint_fixture();
    let hw = linux_nvidia(24.0);
    // Match the file:basename of the coder-7B catalog entry so
    // on_disk_score clones its score; then the +ON_DISK_TIE_BREAK
    // pushes the on-disk row above its remote twin.
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
    let first_on_disk = recs
      .iter()
      .position(|r| matches!(r.kind, RecommendationKind::OnDisk { .. }));
    assert!(first_on_disk.is_some(), "on-disk model must appear");
    assert_eq!(
      first_on_disk.unwrap(),
      0,
      "on-disk match must sort above its remote twin",
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
    // From 4k to 16k both KV and activation grow:
    //   KV delta = KV_BYTES_PER_BPARAM_PER_KCTX × params_b × ×4_moe_mult × 12k
    //   activation delta = (12/4096 × 4096) × 150 MB = 450 MB
    // Combined delta is ~1.5-3 GB for a 5 GB weights file.
    let delta = at_16k - at_4k;
    assert!(
      delta > 500_000_000 && delta < 5_000_000_000,
      "ctx growth from 4k→16k should land between 500 MB and 5 GB, got {delta}"
    );
  }

  fn moe_entry(weights_bytes: u64, params: u64, params_active: u64) -> ModelEntry {
    ModelEntry {
      id: "moe-test".into(),
      repo: "test/repo".into(),
      file: "test.gguf".into(),
      architecture: "moe".into(),
      quant: "Q4_K_M".into(),
      params,
      weights_bytes,
      task_hints: vec!["general".into()],
      benchmark_score: crate::init::benchmark::BenchmarkScore {
        value: 50.0,
        source: "synthetic".into(),
      },
      tok_s_factor: 1.0,
      recency: 1.0,
      source_hf_id: "test/moe".into(),
      params_active: Some(params_active),
      is_moe: true,
      gguf_publisher: "test".into(),
    }
  }

  fn dense_entry(weights_bytes: u64, params: u64) -> ModelEntry {
    ModelEntry {
      is_moe: false,
      params_active: None,
      ..moe_entry(weights_bytes, params, params)
    }
  }

  #[test]
  fn moe_estimator_qwen3_next_80b_a3b_scales_with_active_params_not_total() {
    // Qwen3-Next-80B-A3B Q4_K_M: 80B total, 3B active per token. The
    // whole point of MoE is that ctx growth doesn't track total weight
    // count, only the active attention slice — so the peak should
    // sit close to the weights file size plus a small KV/activation
    // band that grows with active params, not with total params.
    let entry = moe_entry(48_000_000_000, 80_000_000_000, 3_000_000_000);

    let peak_4k = estimate_peak_bytes_for_entry(&entry, 4096);
    let peak_16k = estimate_peak_bytes_for_entry(&entry, 16384);
    let peak_32k = estimate_peak_bytes_for_entry(&entry, 32768);

    assert!(
      peak_4k > 48_000_000_000,
      "peak must include weights + some overhead"
    );
    assert!(
      peak_4k < 51_000_000_000,
      "peak shouldn't add multiple GB at 4k for 3B-active MoE"
    );
    assert!(peak_16k > peak_4k);
    assert!(peak_32k > peak_16k);

    // KV growth from 4k to 32k: 3.5 MB × 3 × 4 × (32-4) = ~1.18 GB.
    // Plus activation ctx term ≈ 28/4 × 150 MB = ~1.05 GB. Combined
    // ≈ 2.2 GB.
    let kv_delta = peak_32k - peak_4k;
    assert!(
      kv_delta > 1_000_000_000 && kv_delta < 4_000_000_000,
      "MoE ctx delta from 4k→32k should be ~1-4 GB, got {kv_delta}"
    );
  }

  #[test]
  fn moe_estimator_qwen3_coder_30b_a3b_fits_consumer_vram_at_16k() {
    // Qwen3-Coder-30B-A3B Q4_K_M: 30B total, 3B active. Weights
    // ~18 GB. Should fit a 24 GB Nvidia tier at 16k now that we
    // no longer treat the whole weights file as activation overhead.
    let entry = moe_entry(18_000_000_000, 30_000_000_000, 3_000_000_000);

    let peak_4k = estimate_peak_bytes_for_entry(&entry, 4096);
    let peak_16k = estimate_peak_bytes_for_entry(&entry, 16384);

    assert!(peak_16k > peak_4k);
    assert!(
      peak_16k < 24_000_000_000,
      "30B-A3B at 16k must stay under 24 GB, got {peak_16k}"
    );
  }

  #[test]
  fn moe_estimator_falls_back_to_dense_when_params_active_missing() {
    // A snapshot row marked `is_moe: true` but missing `params_active`
    // is a regen-script bug — the estimator degrades to using
    // `entry.params` rather than panicking so recommendations still
    // surface. With the new whichllm-aligned formula this means the
    // KV + activation terms scale with total params instead of the
    // (missing) active slice — overshooting peak somewhat, which is
    // the right side of conservative for an unannotated MoE row.
    let mut entry = moe_entry(5_000_000_000, 10_000_000_000, 3_000_000_000);
    entry.params_active = None;

    let moe_peak = estimate_peak_bytes_for_entry(&entry, 16384);
    // With params_active=None and is_moe=true, the inner estimator
    // uses entry.params (10B). Dense path with 5 GB weights also
    // uses ~5 GB / 0.5625 ≈ 8.9B as its dense-params estimate.
    // Both should land within the same order of magnitude.
    let dense_peak = estimate_peak_bytes(5_000_000_000, 16384);
    let diff_pct = ((moe_peak as i64 - dense_peak as i64).abs() as f64) / dense_peak as f64;
    assert!(
      diff_pct < 0.15,
      "MoE without params_active should be within 15% of dense bytes-only path, \
       got moe={moe_peak} dense={dense_peak} diff_pct={diff_pct}"
    );
  }

  #[test]
  fn dense_regression_qwen25_7b_at_4k_is_under_weights_plus_2gb() {
    // Pin the dense path: a 7B-class Q4_K_M file (~4.7 GB weights)
    // should peak under ~6.7 GB at 4k context (weights + small KV +
    // activation). The old `× 1.20` formula produced ~5.6 GB +
    // weights-fraction KV; the whichllm-aligned formula keeps total
    // closer to the file size with a per-param activation slice.
    let entry = dense_entry(4_683_960_320, 7_610_000_000);
    let peak = estimate_peak_bytes_for_entry(&entry, 4096);
    assert!(
      peak > 4_683_960_320,
      "peak must exceed weights file ({peak} <= 4.68 GB)"
    );
    assert!(
      peak < 4_683_960_320 + 2_500_000_000,
      "peak should sit within 2.5 GB of weights at 4k, got {peak}"
    );
  }

  #[test]
  fn fits_predicate_rejects_zero_peak() {
    let hw = linux_nvidia(24.0);
    let ceiling = effective_vram_ceiling(&hw, &load_bundled());
    assert!(!fits(0, ceiling, &hw));
  }
}
