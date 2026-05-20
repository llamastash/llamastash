//! Release-blocking 16/20 corpus check for the recommender (R57).
//!
//! Each fixture row is a (GPU class, VRAM, task) tuple plus the
//! maintainer's expected pick(s). The recommender must surface one of
//! the expected picks in its top-3 for at least 16 of the 20 cases.
//! Below the threshold the gate fails the release, surfacing the
//! mis-classified rows so the snapshot regen flow (Unit 7) can
//! recalibrate weights or trim entries.

use llamastash::gpu::{GpuDevice, GpuInfo};
use llamastash::init::benchmark::load_bundled;
use llamastash::init::detection::{CpuArch, HardwareSnapshot, OsFamily};
use llamastash::init::recommender::{
  recommend, RecommendOptions, Recommendation, RecommendationKind,
};

/// One row of the maintainer-curated corpus. `expected` is the set
/// of model ids any of which is an acceptable top-3 hit.
struct Case {
  label: &'static str,
  hardware: HardwareSnapshot,
  task: Option<&'static str>,
  ctx: u32,
  expected: &'static [&'static str],
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
      expected: &["qwen2.5-14b-q4_k_m", "qwen2.5-7b-q4_k_m"],
    },
    Case {
      label: "24 GB Nvidia + code @ 16k",
      hardware: nvidia(24.0, 64.0),
      task: Some("code"),
      ctx: 16384,
      expected: &["qwen2.5-coder-14b-q4_k_m", "qwen2.5-coder-7b-q4_k_m"],
    },
    Case {
      label: "16 GB Nvidia + general @ 16k",
      hardware: nvidia(16.0, 32.0),
      task: Some("general"),
      ctx: 16384,
      expected: &["qwen2.5-7b-q4_k_m", "llama-3.1-8b-q4_k_m"],
    },
    Case {
      label: "16 GB Nvidia + code @ 16k",
      hardware: nvidia(16.0, 32.0),
      task: Some("code"),
      ctx: 16384,
      expected: &["qwen2.5-coder-7b-q4_k_m"],
    },
    Case {
      label: "12 GB Nvidia + reasoning @ 4k",
      hardware: nvidia(12.0, 32.0),
      task: Some("reasoning"),
      ctx: 4096,
      expected: &["qwen2.5-14b-q4_k_m", "mistral-nemo-12b-q4_k_m"],
    },
    Case {
      label: "8 GB Nvidia + general @ 8k",
      hardware: nvidia(8.0, 16.0),
      task: Some("general"),
      ctx: 8192,
      expected: &["qwen2.5-7b-q4_k_m", "llama-3.2-3b-q4_k_m"],
    },
    Case {
      label: "8 GB Nvidia + code @ 8k",
      hardware: nvidia(8.0, 16.0),
      task: Some("code"),
      ctx: 8192,
      expected: &["qwen2.5-coder-7b-q4_k_m"],
    },
    Case {
      label: "6 GB Nvidia + general @ 4k",
      hardware: nvidia(6.0, 16.0),
      task: Some("general"),
      ctx: 4096,
      expected: &[
        "qwen2.5-7b-q4_k_m",
        "qwen2.5-3b-q4_k_m",
        "llama-3.2-3b-q4_k_m",
      ],
    },
    Case {
      label: "48 GB Nvidia + general @ 16k",
      hardware: nvidia(48.0, 128.0),
      task: Some("general"),
      ctx: 16384,
      expected: &["qwen2.5-32b-q4_k_m", "qwen2.5-14b-q4_k_m"],
    },
    Case {
      label: "48 GB Nvidia + code @ 16k",
      hardware: nvidia(48.0, 128.0),
      task: Some("code"),
      ctx: 16384,
      expected: &["qwen2.5-coder-32b-q4_k_m", "qwen2.5-coder-14b-q4_k_m"],
    },
    Case {
      label: "80 GB Nvidia (A100) + general @ 16k",
      hardware: nvidia(80.0, 256.0),
      task: Some("general"),
      ctx: 16384,
      expected: &["llama-3.3-70b-q4_k_m", "qwen2.5-32b-q4_k_m"],
    },
    Case {
      label: "24 GB AMD + general @ 16k",
      hardware: amd(24.0, 64.0),
      task: Some("general"),
      ctx: 16384,
      expected: &["qwen2.5-14b-q4_k_m", "qwen2.5-7b-q4_k_m"],
    },
    Case {
      label: "12 GB AMD + code @ 16k",
      hardware: amd(12.0, 32.0),
      task: Some("code"),
      ctx: 16384,
      expected: &["qwen2.5-coder-7b-q4_k_m"],
    },
    Case {
      label: "M3 Pro 18 GB unified + general @ 16k",
      hardware: apple(18.0),
      task: Some("general"),
      ctx: 16384,
      expected: &["qwen2.5-7b-q4_k_m", "llama-3.2-3b-q4_k_m"],
    },
    Case {
      label: "M2 Max 32 GB unified + general @ 16k",
      hardware: apple(32.0),
      task: Some("general"),
      ctx: 16384,
      expected: &["qwen2.5-14b-q4_k_m", "qwen2.5-7b-q4_k_m"],
    },
    Case {
      label: "M3 Max 64 GB unified + code @ 16k",
      hardware: apple(64.0),
      task: Some("code"),
      ctx: 16384,
      expected: &["qwen2.5-coder-32b-q4_k_m", "qwen2.5-coder-14b-q4_k_m"],
    },
    Case {
      label: "M3 Max 96 GB unified + reasoning @ 8k",
      hardware: apple(96.0),
      task: Some("reasoning"),
      ctx: 8192,
      expected: &["qwen2.5-32b-q4_k_m", "qwen2.5-14b-q4_k_m"],
    },
    Case {
      label: "CPU-only 16 GB RAM + general @ 4k",
      hardware: cpu(16.0),
      task: Some("general"),
      ctx: 4096,
      expected: &[
        "qwen2.5-3b-q4_k_m",
        "llama-3.2-3b-q4_k_m",
        "qwen2.5-coder-1.5b-q4_k_m",
      ],
    },
    Case {
      label: "CPU-only 32 GB RAM + general @ 4k",
      hardware: cpu(32.0),
      task: Some("general"),
      ctx: 4096,
      expected: &[
        "qwen2.5-7b-q4_k_m",
        "llama-3.1-8b-q4_k_m",
        "qwen2.5-3b-q4_k_m",
      ],
    },
    Case {
      label: "CPU-only 8 GB RAM + code @ 4k",
      hardware: cpu(8.0),
      task: Some("code"),
      ctx: 4096,
      expected: &["qwen2.5-coder-1.5b-q4_k_m"],
    },
  ]
}

fn top_n_ids(recs: &[Recommendation], n: usize) -> Vec<&str> {
  recs
    .iter()
    .filter_map(|r| match &r.kind {
      RecommendationKind::Curated { entry } => Some(entry.id.as_str()),
      _ => None,
    })
    .take(n)
    .collect()
}

#[test]
fn corpus_passes_release_threshold() {
  let snapshot = load_bundled();
  let mut hits = 0;
  let mut misses: Vec<(String, Vec<String>)> = Vec::new();
  let cases = corpus();
  let n_cases = cases.len();
  for case in cases {
    let opts = RecommendOptions {
      task: case.task.map(str::to_string),
      ctx: case.ctx,
      ..RecommendOptions::default()
    };
    let recs = recommend(&snapshot, &case.hardware, &[], &opts);
    let top_ids = top_n_ids(&recs, 3);
    if top_ids.iter().any(|id| case.expected.contains(id)) {
      hits += 1;
    } else {
      misses.push((
        case.label.to_string(),
        top_ids.iter().map(|s| s.to_string()).collect(),
      ));
    }
  }
  assert_eq!(n_cases, 20, "corpus must have exactly 20 cases");
  // Release-blocking threshold: 16/20 hits in the top-3.
  assert!(
    hits >= 16,
    "recommender corpus regression: only {hits}/20 cases matched, threshold 16. Misses: {misses:?}"
  );
}
