//! Host-level resource sampler.
//!
//! Distinct from [`super::resources`], which is per-PID and attached
//! to each supervised launch. This sampler is a daemon-wide singleton
//! that runs at 1 Hz, capturing the host's system-wide CPU%, RAM, and
//! (when a GPU backend is available) GPU utilization, temperature,
//! and VRAM aggregates.
//!
//! Multi-GPU strategy (matches the dashboard plan): mean util%, max
//! temperature, summed VRAM. Per-card detail isn't surfaced here.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tokio::sync::RwLock;

use super::shutdown::ShutdownToken;
use crate::gpu::{self, GpuInfo};

/// Aggregated host snapshot. All GPU fields are `Option` because not
/// every backend exposes them — Vulkan and CpuOnly don't have GPU
/// numbers at all; Apple Silicon reports unified memory total but no
/// per-card util or temp via `system_profiler`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct HostMetricsSnapshot {
  /// Mean CPU utilisation across all logical cores, 0..=100.
  pub cpu_pct: f32,
  pub ram_used_bytes: u64,
  pub ram_total_bytes: u64,
  /// Mean utilization % across GPUs that surface it. `None` when no
  /// card reports a reading.
  pub gpu_util_pct: Option<f32>,
  /// Sum of `used_memory_bytes` across all GPUs.
  pub gpu_mem_used_bytes: Option<u64>,
  /// Sum of `total_memory_bytes` across all GPUs.
  pub gpu_mem_total_bytes: Option<u64>,
  /// Max temperature across GPUs that surface it. `None` when no card
  /// reports a reading.
  pub gpu_temp_c: Option<f32>,
  /// Backend label: `"cpu_only"`, `"nvidia"`, `"amd"`, `"apple_metal"`.
  pub gpu_backend: String,
  /// Number of GPUs the backend reports. 0 on CpuOnly; 1 on Metal
  /// (unified memory); typically 1 on most NVIDIA/AMD systems.
  pub gpu_device_count: u32,
}

impl HostMetricsSnapshot {
  /// `gpu_backend` value the daemon emits before the first sample
  /// lands — distinguishes "never sampled" from "sampled and found
  /// cpu only".
  pub const UNINITIALIZED_BACKEND: &'static str = "unsampled";
}

/// One-shot synchronous sample. The first refresh of `cpu_usage`
/// returns 0% (sysinfo needs two refreshes to compute a delta), so
/// this primes once + sleeps + refreshes again before reading.
/// Suitable for tests; the daemon should prefer [`spawn`] for
/// continuous readings.
pub async fn sample_priming(prime_delay: Duration) -> HostMetricsSnapshot {
  let mut sys = System::new_with_specifics(host_refresh_kind());
  // Prime the CPU delta.
  sys.refresh_cpu_specifics(CpuRefreshKind::new().with_cpu_usage());
  tokio::time::sleep(prime_delay).await;
  sys.refresh_cpu_specifics(CpuRefreshKind::new().with_cpu_usage());
  sys.refresh_memory();
  build_snapshot(&sys, probe_gpu().await)
}

/// Spawn the long-running sampler. Returns an `Arc<RwLock<…>>` whose
/// contents the daemon's IPC `status` handler reads to populate the
/// `host` field. The task exits when `shutdown` is triggered.
///
/// First reading lands `interval` after spawn (so CPU% has a real
/// delta to report). The shared snapshot starts at `Default::default()`
/// with `gpu_backend == "unsampled"`; callers should treat that
/// sentinel as "no reading yet."
pub fn spawn(shutdown: ShutdownToken, interval: Duration) -> Arc<RwLock<HostMetricsSnapshot>> {
  let shared = Arc::new(RwLock::new(HostMetricsSnapshot {
    gpu_backend: HostMetricsSnapshot::UNINITIALIZED_BACKEND.into(),
    ..HostMetricsSnapshot::default()
  }));
  let shared_for_task = Arc::clone(&shared);
  tokio::spawn(async move {
    let mut sys = System::new_with_specifics(host_refresh_kind());
    // Prime sysinfo's CPU delta on first refresh.
    sys.refresh_cpu_specifics(CpuRefreshKind::new().with_cpu_usage());
    loop {
      tokio::select! {
        _ = shutdown.wait_until_triggered() => return,
        _ = tokio::time::sleep(interval) => {}
      }
      sys.refresh_cpu_specifics(CpuRefreshKind::new().with_cpu_usage());
      sys.refresh_memory();
      let next = build_snapshot(&sys, probe_gpu().await);
      *shared_for_task.write().await = next;
    }
  });
  shared
}

fn host_refresh_kind() -> RefreshKind {
  RefreshKind::new()
    .with_cpu(CpuRefreshKind::new().with_cpu_usage())
    .with_memory(MemoryRefreshKind::everything())
}

/// GPU probe runs on the blocking pool — the existing `gpu::probe()`
/// shells out to `nvidia-smi` / `rocm-smi` / `system_profiler` and
/// would otherwise stall a tokio worker for the duration of the
/// child process.
async fn probe_gpu() -> GpuInfo {
  tokio::task::spawn_blocking(gpu::probe)
    .await
    .unwrap_or(GpuInfo::CpuOnly)
}

fn build_snapshot(sys: &System, info: GpuInfo) -> HostMetricsSnapshot {
  let cpu_pct = host_cpu_pct(sys);
  let (gpu_util_pct, gpu_mem_used_bytes, gpu_mem_total_bytes, gpu_temp_c, gpu_device_count) =
    aggregate_gpu(&info);
  HostMetricsSnapshot {
    cpu_pct,
    ram_used_bytes: sys.used_memory(),
    ram_total_bytes: sys.total_memory(),
    gpu_util_pct,
    gpu_mem_used_bytes,
    gpu_mem_total_bytes,
    gpu_temp_c,
    gpu_backend: info.label().to_string(),
    gpu_device_count,
  }
}

fn host_cpu_pct(sys: &System) -> f32 {
  let cpus = sys.cpus();
  if cpus.is_empty() {
    return 0.0;
  }
  let sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
  sum / cpus.len() as f32
}

/// Per the plan's multi-GPU strategy: mean util%, summed VRAM, max temp.
fn aggregate_gpu(info: &GpuInfo) -> (Option<f32>, Option<u64>, Option<u64>, Option<f32>, u32) {
  match info {
    GpuInfo::CpuOnly => (None, None, None, None, 0),
    GpuInfo::AppleMetal { total_memory_bytes } => {
      // Apple Silicon unified memory: no per-card "used" attribution
      // available via system_profiler. Surface total only so the UI
      // can render a `RAM (unified)` row.
      (None, None, Some(*total_memory_bytes), None, 1)
    }
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } => {
      if devices.is_empty() {
        return (None, None, None, None, 0);
      }
      let count = devices.len() as u32;
      let total: u64 = devices.iter().map(|d| d.total_memory_bytes).sum();
      let used: u64 = devices.iter().map(|d| d.used_memory_bytes).sum();
      let util_readings: Vec<f32> = devices.iter().filter_map(|d| d.utilization_pct).collect();
      let util = if util_readings.is_empty() {
        None
      } else {
        Some(util_readings.iter().sum::<f32>() / util_readings.len() as f32)
      };
      let temp = devices
        .iter()
        .filter_map(|d| d.temperature_c)
        .fold(None, |acc: Option<f32>, t| match acc {
          None => Some(t),
          Some(prev) => Some(prev.max(t)),
        });
      (util, Some(used), Some(total), temp, count)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::GpuDevice;

  #[test]
  fn aggregate_cpu_only_returns_all_none_and_zero_devices() {
    let (util, used, total, temp, count) = aggregate_gpu(&GpuInfo::CpuOnly);
    assert_eq!(util, None);
    assert_eq!(used, None);
    assert_eq!(total, None);
    assert_eq!(temp, None);
    assert_eq!(count, 0);
  }

  #[test]
  fn aggregate_apple_metal_reports_unified_total_only() {
    let info = GpuInfo::AppleMetal {
      total_memory_bytes: 16 * 1024 * 1024 * 1024,
    };
    let (util, used, total, temp, count) = aggregate_gpu(&info);
    assert_eq!(util, None);
    assert_eq!(used, None);
    assert_eq!(total, Some(16 * 1024 * 1024 * 1024));
    assert_eq!(temp, None);
    assert_eq!(count, 1);
  }

  #[test]
  fn aggregate_single_nvidia_card_passes_through() {
    let info = GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "RTX 4090".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 4 * 1024 * 1024 * 1024,
        utilization_pct: Some(73.0),
        temperature_c: Some(68.0),
      }],
    };
    let (util, used, total, temp, count) = aggregate_gpu(&info);
    assert_eq!(util, Some(73.0));
    assert_eq!(used, Some(4 * 1024 * 1024 * 1024));
    assert_eq!(total, Some(24 * 1024 * 1024 * 1024));
    assert_eq!(temp, Some(68.0));
    assert_eq!(count, 1);
  }

  #[test]
  fn aggregate_multi_nvidia_means_util_sums_vram_maxes_temp() {
    let info = GpuInfo::Nvidia {
      devices: vec![
        GpuDevice {
          name: "0".into(),
          total_memory_bytes: 10,
          used_memory_bytes: 1,
          utilization_pct: Some(20.0),
          temperature_c: Some(50.0),
        },
        GpuDevice {
          name: "1".into(),
          total_memory_bytes: 20,
          used_memory_bytes: 5,
          utilization_pct: Some(80.0),
          temperature_c: Some(72.0),
        },
      ],
    };
    let (util, used, total, temp, count) = aggregate_gpu(&info);
    assert_eq!(util, Some(50.0));
    assert_eq!(used, Some(6));
    assert_eq!(total, Some(30));
    assert_eq!(temp, Some(72.0));
    assert_eq!(count, 2);
  }

  #[test]
  fn aggregate_skips_cards_missing_util_or_temp() {
    // One card reports util/temp, the other doesn't (older driver).
    // Mean is taken over cards that *do* report; missing-everywhere
    // collapses to None.
    let info = GpuInfo::Amd {
      devices: vec![
        GpuDevice {
          name: "0".into(),
          total_memory_bytes: 10,
          used_memory_bytes: 1,
          utilization_pct: Some(60.0),
          temperature_c: None,
        },
        GpuDevice {
          name: "1".into(),
          total_memory_bytes: 10,
          used_memory_bytes: 2,
          utilization_pct: None,
          temperature_c: Some(55.0),
        },
      ],
    };
    let (util, _used, _total, temp, count) = aggregate_gpu(&info);
    assert_eq!(util, Some(60.0));
    assert_eq!(temp, Some(55.0));
    assert_eq!(count, 2);
  }

  #[test]
  fn aggregate_empty_device_vec_returns_all_none() {
    let info = GpuInfo::Nvidia { devices: vec![] };
    let (util, used, total, temp, count) = aggregate_gpu(&info);
    assert_eq!(util, None);
    assert_eq!(used, None);
    assert_eq!(total, None);
    assert_eq!(temp, None);
    assert_eq!(count, 0);
  }

  #[tokio::test]
  async fn sample_priming_returns_a_nonzero_ram_total() {
    let snap = sample_priming(Duration::from_millis(50)).await;
    assert!(
      snap.ram_total_bytes > 0,
      "host total memory should be > 0 on any real system"
    );
    assert_ne!(snap.gpu_backend, HostMetricsSnapshot::UNINITIALIZED_BACKEND);
  }

  #[tokio::test]
  async fn spawn_updates_snapshot_after_first_tick() {
    let token = ShutdownToken::new();
    let snap = spawn(token.clone(), Duration::from_millis(50));
    // First tick lands after `interval`; the GPU probe step shells
    // out and may share the blocking pool with other parallel tests,
    // so allow generous headroom for the snapshot to land.
    for _ in 0..40 {
      tokio::time::sleep(Duration::from_millis(25)).await;
      let read = snap.read().await.clone();
      if read.ram_total_bytes > 0
        && read.gpu_backend != HostMetricsSnapshot::UNINITIALIZED_BACKEND
      {
        token.trigger();
        return;
      }
    }
    panic!(
      "host-metrics sampler did not produce a snapshot within 1s; \
       snapshot: {:?}",
      snap.read().await
    );
  }
}
