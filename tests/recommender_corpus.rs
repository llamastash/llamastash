//! Release-blocking 16/20 corpus check for the recommender (R57).
//!
//! Each fixture row is a (GPU class, VRAM, task) tuple plus a predicate
//! describing what the maintainer considers a fitting pick for that
//! cell. The recommender must surface at least one top-3 model that
//! satisfies the predicate for ≥16 of the 20 cases. Below the
//! threshold the gate fails the release, surfacing the mis-classified
//! rows so the snapshot regen flow (Unit 7) can recalibrate.
//!
//! Predicate-based `expected` (Unit 5 of plan 2026-05-20-001): the
//! corpus stays meaningful as the auto-regenerated catalog rotates.
//! Previously `expected` was a hard-coded list of model ids, which
//! quietly went stale every time `data/benchmark-snapshot.json` got a
//! new row. With predicates the test now fails loudly when a tier
//! loses every fitting model — exactly the regression we want to
//! catch.

use llamastash::gpu::{GpuDevice, GpuInfo};
use llamastash::init::benchmark::{load_bundled, ModelEntry};
use llamastash::init::detection::{CpuArch, HardwareSnapshot, OsFamily};
use llamastash::init::recommender::{
  recommend, RecommendOptions, Recommendation, RecommendationKind,
};

/// Predicate every corpus row checks against the recommender's top-3.
/// At least one top-3 entry must satisfy *both* clauses:
///
/// 1. `task_hints` contains [`task`]; and
/// 2. the entry fits the size budget:
///    - **Dense** rows: `params ≤ max_params_b * 1e9`.
///    - **MoE** rows: `params_active ≤ max_params_b * 1e9`. A
///      30B-A3B MoE with 3B active is functionally a 3B-class
///      pick for this corpus's "max N billion" rubric — its VRAM
///      and tok/s land near a 3B dense, even though the catalog
///      entry's `params` field reports 30B.
///    - When [`prefer_moe`] is true, only the MoE branch counts —
///      i.e. dense rows can never satisfy a prefer-MoE cell, even
///      if they're small enough to fit.
///
/// `task` is `None` when the cell doesn't care about a specific tag
/// (no current rows use that, but the option exists for future
/// general-purpose cells).
struct ExpectedFit {
  task: Option<&'static str>,
  max_params_b: f32,
  prefer_moe: bool,
}

impl ExpectedFit {
  fn matches(&self, entry: &ModelEntry) -> bool {
    if let Some(want) = self.task {
      if !entry.task_hints.iter().any(|t| t == want) {
        return false;
      }
    }
    let cap = (self.max_params_b as f64 * 1.0e9) as u64;
    let moe_fits = entry.is_moe && entry.params_active.map(|p| p <= cap).unwrap_or(false);
    if self.prefer_moe {
      moe_fits
    } else {
      entry.params <= cap || moe_fits
    }
  }
}

/// One row of the maintainer-curated corpus.
struct Case {
  label: &'static str,
  hardware: HardwareSnapshot,
  task: Option<&'static str>,
  ctx: u32,
  expected: ExpectedFit,
}

fn nvidia(vram_gb: f64, ram_gb: f64) -> HardwareSnapshot {
  HardwareSnapshot {
    gpu: GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "test-gpu".into(),
        total_memory_bytes: (vram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
      }],
    },
    vram_bytes: Some((vram_gb * 1024.0 * 1024.0 * 1024.0) as u64),
    gpu_device_count: 1,
    ram_total_bytes: (ram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
    disk_free_bytes: 0,
    cpu_brand: String::new(),
    cpu_cores: 0,
    cpu_features: Vec::new(),
    os: OsFamily::Linux,
    cpu_arch: CpuArch::X86_64,
  }
}

fn amd(vram_gb: f64, ram_gb: f64) -> HardwareSnapshot {
  HardwareSnapshot {
    gpu: GpuInfo::Amd {
      devices: vec![GpuDevice {
        name: "test-amd".into(),
        total_memory_bytes: (vram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
      }],
    },
    vram_bytes: Some((vram_gb * 1024.0 * 1024.0 * 1024.0) as u64),
    gpu_device_count: 1,
    ram_total_bytes: (ram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
    disk_free_bytes: 0,
    cpu_brand: String::new(),
    cpu_cores: 0,
    cpu_features: Vec::new(),
    os: OsFamily::Linux,
    cpu_arch: CpuArch::X86_64,
  }
}

fn cpu(ram_gb: f64) -> HardwareSnapshot {
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

fn apple(unified_gb: f64) -> HardwareSnapshot {
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

fn corpus() -> Vec<Case> {
  vec![
    Case {
      label: "24 GB Nvidia + general @ 16k",
      hardware: nvidia(24.0, 64.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 16.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "24 GB Nvidia + code @ 16k",
      hardware: nvidia(24.0, 64.0),
      task: Some("code"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 16.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "16 GB Nvidia + general @ 16k",
      hardware: nvidia(16.0, 32.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 10.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "16 GB Nvidia + code @ 16k",
      hardware: nvidia(16.0, 32.0),
      task: Some("code"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 10.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "12 GB Nvidia + reasoning @ 4k",
      hardware: nvidia(12.0, 32.0),
      task: Some("reasoning"),
      ctx: 4096,
      expected: ExpectedFit {
        task: Some("reasoning"),
        max_params_b: 16.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "8 GB Nvidia + general @ 8k",
      hardware: nvidia(8.0, 16.0),
      task: Some("general"),
      ctx: 8192,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 8.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "8 GB Nvidia + code @ 8k",
      hardware: nvidia(8.0, 16.0),
      task: Some("code"),
      ctx: 8192,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 8.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "6 GB Nvidia + general @ 4k",
      hardware: nvidia(6.0, 16.0),
      task: Some("general"),
      ctx: 4096,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 4.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "48 GB Nvidia + general @ 16k",
      hardware: nvidia(48.0, 128.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 36.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "48 GB Nvidia + code @ 16k",
      hardware: nvidia(48.0, 128.0),
      task: Some("code"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 36.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "80 GB Nvidia (A100) + general @ 16k",
      hardware: nvidia(80.0, 256.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 80.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "24 GB AMD + general @ 16k",
      hardware: amd(24.0, 64.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 16.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "12 GB AMD + code @ 16k",
      hardware: amd(12.0, 32.0),
      task: Some("code"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 10.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "M3 Pro 18 GB unified + general @ 16k",
      hardware: apple(18.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 10.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "M2 Max 32 GB unified + general @ 16k",
      hardware: apple(32.0),
      task: Some("general"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 16.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "M3 Max 64 GB unified + code @ 16k",
      hardware: apple(64.0),
      task: Some("code"),
      ctx: 16384,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 36.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "M3 Max 96 GB unified + reasoning @ 8k",
      hardware: apple(96.0),
      task: Some("reasoning"),
      ctx: 8192,
      expected: ExpectedFit {
        task: Some("reasoning"),
        max_params_b: 36.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "CPU-only 16 GB RAM + general @ 4k",
      hardware: cpu(16.0),
      task: Some("general"),
      ctx: 4096,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 4.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "CPU-only 32 GB RAM + general @ 4k",
      hardware: cpu(32.0),
      task: Some("general"),
      ctx: 4096,
      expected: ExpectedFit {
        task: Some("general"),
        max_params_b: 10.0,
        prefer_moe: false,
      },
    },
    Case {
      label: "CPU-only 8 GB RAM + code @ 4k",
      hardware: cpu(8.0),
      task: Some("code"),
      ctx: 4096,
      expected: ExpectedFit {
        task: Some("code"),
        max_params_b: 2.0,
        prefer_moe: false,
      },
    },
  ]
}

fn top_n_entries(recs: &[Recommendation], n: usize) -> Vec<&ModelEntry> {
  recs
    .iter()
    .filter_map(|r| match &r.kind {
      RecommendationKind::Curated { entry } => Some(entry),
      _ => None,
    })
    .take(n)
    .collect()
}

#[test]
fn corpus_passes_release_threshold() {
  let snapshot = load_bundled();
  let mut hits = 0;
  let mut misses: Vec<String> = Vec::new();
  let cases = corpus();
  let n_cases = cases.len();
  for case in cases {
    let opts = RecommendOptions {
      task: case.task.map(str::to_string),
      ctx: case.ctx,
      ..RecommendOptions::default()
    };
    let recs = recommend(&snapshot, &case.hardware, &[], &opts);
    let top = top_n_entries(&recs, 3);
    if top.iter().any(|e| case.expected.matches(e)) {
      hits += 1;
    } else {
      let summary: Vec<String> = top
        .iter()
        .map(|e| {
          format!(
            "{} ({}B params{})",
            e.id,
            e.params / 1_000_000_000,
            if e.is_moe {
              format!(
                ", MoE active={}B",
                e.params_active.unwrap_or(0) / 1_000_000_000
              )
            } else {
              String::new()
            }
          )
        })
        .collect();
      misses.push(format!(
        "{} — expected task={:?} max={}B prefer_moe={} → top-3 was [{}]",
        case.label,
        case.expected.task,
        case.expected.max_params_b,
        case.expected.prefer_moe,
        summary.join(", "),
      ));
    }
  }
  assert_eq!(n_cases, 20, "corpus must have exactly 20 cases");
  // Release-blocking threshold: 16/20 predicate hits in the top-3.
  assert!(
    hits >= 16,
    "recommender corpus regression: only {hits}/20 cases matched, threshold 16.\nMisses:\n  {}",
    misses.join("\n  ")
  );
}
