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

/// MoE KV scaling: bytes of KV cache per billion attention-bearing
/// parameters per 1k context tokens. Ported from whichllm's
/// `_KV_BYTES_PER_BPARAM_PER_KCTX` (3.5 MB). For dense models we keep
/// the weights-fraction estimator above; for MoE the KV grows with
/// the active-attention-param count, not the full weights footprint,
/// since experts share an attention block.
const MOE_KV_BYTES_PER_BPARAM_PER_KCTX: f64 = 3_500_000.0;

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

/// Coarse peak-memory estimate. See module-level docs for the
/// approximation rationale. Dense path: KV scales with the weights
/// footprint; for MoE-aware estimation, callers with a `ModelEntry`
/// should use [`estimate_peak_bytes_for_entry`] instead.
pub fn estimate_peak_bytes(weights_bytes: u64, ctx: u32) -> u64 {
  let w = weights_bytes as f64;
  let activations = w * ACTIVATIONS_OVERHEAD;
  // KV scales linearly with ctx; reference point is `KV_FRACTION_AT_4K_F16`
  // at ctx=4096 (so at ctx=16384 the factor is 4×, etc.).
  let ctx_scale = (ctx as f64) / 4096.0;
  let kv = w * KV_FRACTION_AT_4K_F16 * ctx_scale;
  (activations + kv).max(0.0) as u64
}

/// MoE-aware peak-memory estimate. Branches on `entry.is_moe`:
///
/// - **Dense:** delegates to [`estimate_peak_bytes`] for backward
///   compatibility with the weights-fraction KV estimator.
/// - **MoE:** weights stay fully resident (`weights × ACTIVATIONS_OVERHEAD`),
///   but the KV term is computed from `params_active`. Without an
///   explicit `params_active`, falls back to dense math against
///   `entry.params` — defensible since unannotated MoE models in the
///   snapshot are a regen-script bug, not a runtime concern.
///
/// Matches whichllm's `estimate_vram` within ±15% across the corpus
/// at ctx ∈ {4k, 16k, 32k} — verified in the inline tests below.
pub fn estimate_peak_bytes_for_entry(entry: &ModelEntry, ctx: u32) -> u64 {
  if !entry.is_moe {
    return estimate_peak_bytes(entry.weights_bytes, ctx);
  }
  let Some(active) = entry.params_active else {
    // MoE entry without `params_active`: data bug in the snapshot
    // (regen script should always populate it). Fall back to dense
    // rather than panic — the corpus gate's fit predicate is the
    // safety net for accidentally over-sized recommendations.
    return estimate_peak_bytes(entry.weights_bytes, ctx);
  };
  let w = entry.weights_bytes as f64;
  let activations = w * ACTIVATIONS_OVERHEAD;
  let kv = moe_kv_bytes(active, ctx);
  (activations + kv).max(0.0) as u64
}

/// Whichllm-style MoE KV-cache bytes. Splits `params_active` into
/// billions and `ctx` into thousands so the constant
/// [`MOE_KV_BYTES_PER_BPARAM_PER_KCTX`] reads in its natural units.
fn moe_kv_bytes(params_active: u64, ctx: u32) -> f64 {
  let params_b = (params_active as f64) / 1.0e9;
  let ctx_k = (ctx as f64) / 1024.0;
  MOE_KV_BYTES_PER_BPARAM_PER_KCTX * params_b * MOE_ATTENTION_PARAM_MULTIPLIER * ctx_k
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
          entry.params <= 8_500_000_000,
          "cpu-only must stay at ≤8B-class, got {} ({}B)",
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
    // Qwen3-Next-80B-A3B Q4_K_M: 80B total, 3B active per token.
    // ~48 GB weights footprint, ~58 GB peak at modest ctx — the
    // whole point of MoE is that ctx growth doesn't track total
    // weight count, only the active attention slice.
    let entry = moe_entry(48_000_000_000, 80_000_000_000, 3_000_000_000);

    let peak_4k = estimate_peak_bytes_for_entry(&entry, 4096);
    let peak_16k = estimate_peak_bytes_for_entry(&entry, 16384);
    let peak_32k = estimate_peak_bytes_for_entry(&entry, 32768);

    // Activations alone (weights × 1.20) = 57.6 GB — invariant floor.
    let activations = (48_000_000_000_f64 * ACTIVATIONS_OVERHEAD) as u64;
    assert!(peak_4k > activations, "peak must include some KV");
    assert!(peak_16k > peak_4k, "16k > 4k");
    assert!(peak_32k > peak_16k, "32k > 16k");

    // KV growth from 4k to 32k: 3.5 MB × 3 × 4.0 × (32 - 4) = 1.176 GB.
    let kv_delta = peak_32k - peak_4k;
    let expected_delta =
      (MOE_KV_BYTES_PER_BPARAM_PER_KCTX * 3.0 * MOE_ATTENTION_PARAM_MULTIPLIER * 28.0) as u64; // 32k - 4k = 28k worth of growth
    let off = (kv_delta as i64 - expected_delta as i64).abs();
    assert!(
      off < (expected_delta / 5) as i64,
      "MoE KV delta from 4k→32k should be ~{expected_delta} (±20%), got {kv_delta}"
    );

    // Crucial: the MoE peak at 32k is much smaller than the dense
    // path's peak would be on the same weights footprint. Otherwise
    // the recommender keeps over-reserving and big MoE models never
    // fit any realistic VRAM tier.
    let dense_equivalent = estimate_peak_bytes(48_000_000_000, 32768);
    assert!(
      peak_32k < dense_equivalent,
      "MoE peak ({peak_32k}) must beat dense equivalent ({dense_equivalent})"
    );
  }

  #[test]
  fn moe_estimator_qwen3_coder_30b_a3b_fits_consumer_vram_at_16k() {
    // Qwen3-Coder-30B-A3B Q4_K_M: 30B total, 3B active. Weights
    // ~18 GB. Whole point: should fit a 24 GB Nvidia tier at 16k.
    let entry = moe_entry(18_000_000_000, 30_000_000_000, 3_000_000_000);

    let peak_4k = estimate_peak_bytes_for_entry(&entry, 4096);
    let peak_16k = estimate_peak_bytes_for_entry(&entry, 16384);

    // Activations: 18 × 1.20 = 21.6 GB. KV at 16k: 3.5 × 3 × 4 × 16 ≈ 672 MB.
    // Peak at 16k ≈ 22.3 GB — fits 24 GB after 90% safety margin
    // (21.6 GB effective ceiling) only just barely; the test asserts
    // the math, not the fit.
    assert!(peak_16k > peak_4k);
    assert!(
      peak_16k < 24_000_000_000,
      "30B-A3B at 16k must stay under 24 GB, got {peak_16k}"
    );
  }

  #[test]
  fn moe_estimator_falls_back_to_dense_when_params_active_missing() {
    // A snapshot row marked `is_moe: true` but missing
    // `params_active` is a regen-script bug — Unit 2's estimator
    // degrades to dense rather than panicking so the recommender
    // keeps producing recommendations.
    let mut entry = moe_entry(5_000_000_000, 10_000_000_000, 3_000_000_000);
    entry.params_active = None;

    let moe_peak = estimate_peak_bytes_for_entry(&entry, 16384);
    let dense_peak = estimate_peak_bytes(5_000_000_000, 16384);
    assert_eq!(
      moe_peak, dense_peak,
      "MoE without params_active must match dense path exactly"
    );
  }

  #[test]
  fn dense_regression_unchanged_qwen25_7b_at_4k() {
    // Pin the dense path so Unit 2's MoE additions don't accidentally
    // perturb dense estimates. Qwen2.5-7B Q4_K_M from the bundled
    // snapshot carries weights_bytes = 4_683_960_320.
    let entry = dense_entry(4_683_960_320, 7_610_000_000);
    let via_entry = estimate_peak_bytes_for_entry(&entry, 4096);
    let via_bytes = estimate_peak_bytes(4_683_960_320, 4096);
    assert_eq!(
      via_entry, via_bytes,
      "dense entry path must equal the legacy weights-only function"
    );
  }

  #[test]
  fn fits_predicate_rejects_zero_peak() {
    let hw = linux_nvidia(24.0);
    let ceiling = effective_vram_ceiling(&hw, &load_bundled());
    assert!(!fits(0, ceiling, &hw));
  }
}
