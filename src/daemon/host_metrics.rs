//! Host-level resource sampler.
//!
//! Distinct from [`super::resources`], which is per-PID and attached
//! to each supervised launch. This sampler is a daemon-wide singleton
//! that runs at a configurable cadence (`daemon.metrics_interval_secs`,
//! factory 1 Hz), capturing the host's system-wide CPU%, RAM, and
//! (when a GPU backend is available) GPU utilization, temperature,
//! and VRAM aggregates.
//!
//! Multi-GPU strategy (matches the dashboard plan): mean util%, max
//! temperature, summed VRAM. Per-card detail isn't surfaced here.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::{Components, CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tokio::sync::RwLock;

use super::shutdown::ShutdownToken;
use crate::gpu::{self, ClassSource, GpuDevice, GpuInfo};

/// One detected GPU device — name + total VRAM + util + temp.
/// Carried in the IPC status response so the TUI device picker can
/// list cards by name and the host stats pane can render per-GPU rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRow {
  /// Backend-prefixed display label: `Nvidia0`, `Amd0`, `Vulkan0`,
  /// `Metal0`. Used by the host stats pane to tag per-device rows.
  /// This is *not* the `--device` value — launch selection draws from
  /// the `llama-server --list-devices` catalog (`crate::backend::llama_cpp::list_devices`),
  /// whose selectors (`CUDA0`, `ROCm0`) are the only ones the binary accepts.
  pub selector: String,
  /// Backend the probe used to detect this device: `"nvidia"`,
  /// `"amd"`, `"apple_metal"`, or `"unknown"` (Vulkan fallback).
  pub backend: String,
  /// Human-readable device name (e.g. `"NVIDIA GeForce RTX 3080"`).
  pub name: String,
  /// Total memory bytes for this device.
  pub total_memory_bytes: u64,
  /// Currently-used memory bytes for this device.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub used_memory_bytes: Option<u64>,
  /// Current utilization %, when the vendor tool surfaces it.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub utilization_pct: Option<f32>,
  /// Current temperature °C, when the vendor tool surfaces it.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub temperature_c: Option<f32>,
}

/// Aggregated host snapshot. All GPU fields are `Option` because not
/// every backend exposes them — Vulkan and CpuOnly don't have GPU
/// numbers at all; Apple Silicon reports unified memory total but no
/// per-card util or temp via `system_profiler`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HostMetricsSnapshot {
  /// Mean CPU utilisation across all logical cores, 0..=100.
  pub cpu_pct: f32,
  /// CPU package temperature in °C, when sysinfo's component sensor
  /// surfaces a `coretemp`/`k10temp`/`Tdie`/etc. reading. `None`
  /// when the platform has no readable sensor (containers, BSDs).
  #[serde(default)]
  pub cpu_temp_c: Option<f32>,
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
  /// Per-device rows from all backends. Each entry carries a
  /// backend-prefixed selector (`Nvidia0`, `Amd0`, `Vulkan0`,
  /// `Metal0`), device name, VRAM (used/total), utilization%, and
  /// temperature when the vendor tool surfaces them. Used by the
  /// TUI host stats pane to render one row per GPU instead of a
  /// single aggregate row.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub gpu_devices: Option<Vec<DeviceRow>>,
  /// Portion of GPU memory that physically lives in the system RAM
  /// pool (AMD GTT on UMA APUs like Strix Halo, the shared pool on
  /// Windows UMA adapters). Informational — surfaced for clients that
  /// want the split. The RAM gauge does NOT subtract it: those bytes
  /// are real system RAM and `sysinfo`'s used/total already account
  /// for them; the `unified` flag is what flags the shared pool to the
  /// user. `None` on discrete cards and other backends.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_shared_total_bytes: Option<u64>,
  /// Currently-allocated portion of `uma_shared_total_bytes`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_shared_used_bytes: Option<u64>,
  /// Whether the GPU shares one physical memory pool with the CPU
  /// (Apple Silicon, or an AMD/Intel UMA APU). Derived from
  /// [`crate::gpu::GpuInfo::is_unified`] — the same helper the init
  /// banner uses — so the TUI host pane and init never disagree on the
  /// `RAM*` / "unified" marker. The pane reads this directly instead
  /// of re-deriving it from the backend string + UMA fields.
  #[serde(default)]
  pub unified: bool,
  /// How the unified-vs-discrete verdict was reached — surfaced
  /// in the `doctor` hardware section so a misclassification is
  /// inspectable. `None` on Apple Metal (unified by construction),
  /// NVIDIA, Vulkan, and CPU-only.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_class_source: Option<ClassSource>,
}

impl HostMetricsSnapshot {
  /// `gpu_backend` value the daemon emits before the first sample
  /// lands — distinguishes "never sampled" from "sampled and found
  /// cpu only".
  pub const UNINITIALIZED_BACKEND: &'static str = "unsampled";

  /// `gpu_backend` value emitted when no GPU was detected. Stable
  /// wire string consumed by the TUI host pane and clients gating
  /// behavior on backend kind.
  pub const BACKEND_CPU_ONLY: &'static str = "cpu_only";
  /// `gpu_backend` value emitted when NVIDIA NVML/nvidia-smi probing
  /// succeeded.
  pub const BACKEND_NVIDIA: &'static str = "nvidia";
  /// `gpu_backend` value emitted when AMD rocm-smi probing succeeded.
  pub const BACKEND_AMD: &'static str = "amd";
  /// `gpu_backend` value emitted on Apple Silicon (Metal).
  pub const BACKEND_APPLE_METAL: &'static str = "apple_metal";
  /// `gpu_backend` value emitted when vulkaninfo found a device but
  /// neither NVIDIA nor ROCm probes succeeded. The vendor is unknown.
  pub const BACKEND_UNKNOWN: &'static str = "unknown";
  /// `gpu_backend` value emitted when two or more backends each
  /// found one or more GPUs.
  pub const BACKEND_MULTI: &'static str = "multi";

  /// Classify the raw `gpu_backend` string into a typed variant.
  /// Render layers should branch on this enum instead of comparing
  /// the wire string at the callsite.
  pub fn flavor(&self) -> GpuFlavor {
    GpuFlavor::from_label(self.gpu_backend.as_str())
  }
}

/// Typed view of `HostMetricsSnapshot::gpu_backend`. The wire string
/// stays the source of truth (clients pin against it); this enum is
/// purely a render-layer convenience so the TUI / CLI don't each
/// open-code the same `match string` ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuFlavor {
  /// Sampler hasn't run yet.
  Unsampled,
  CpuOnly,
  Nvidia,
  Amd,
  AppleMetal,
  /// Vulkan saw a device but vendor classification didn't resolve.
  Unknown,
  /// Two or more backends each found one or more GPUs.
  Multi,
}

impl GpuFlavor {
  pub fn from_label(label: &str) -> Self {
    match label {
      HostMetricsSnapshot::UNINITIALIZED_BACKEND => GpuFlavor::Unsampled,
      HostMetricsSnapshot::BACKEND_CPU_ONLY => GpuFlavor::CpuOnly,
      HostMetricsSnapshot::BACKEND_NVIDIA => GpuFlavor::Nvidia,
      HostMetricsSnapshot::BACKEND_AMD => GpuFlavor::Amd,
      HostMetricsSnapshot::BACKEND_APPLE_METAL => GpuFlavor::AppleMetal,
      HostMetricsSnapshot::BACKEND_MULTI => GpuFlavor::Multi,
      _ => GpuFlavor::Unknown,
    }
  }
}

/// One-shot synchronous sample. The first refresh of `cpu_usage`
/// returns 0% (sysinfo needs two refreshes to compute a delta), so
/// this primes once + sleeps + refreshes again before reading.
/// Suitable for tests; the daemon should prefer [`spawn`] for
/// continuous readings.
pub async fn sample_priming(prime_delay: Duration) -> HostMetricsSnapshot {
  let mut sys = System::new_with_specifics(host_refresh_kind());
  // Prime the CPU delta.
  sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
  tokio::time::sleep(prime_delay).await;
  sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
  sys.refresh_memory();
  let components = Components::new_with_refreshed_list();
  build_snapshot(&sys, &components, probe_gpu_full().await)
}

/// Spawn the long-running sampler. Returns an `Arc<RwLock<…>>` whose
/// contents the daemon's IPC `status` handler reads to populate the
/// `host` field. The task exits when `shutdown` is triggered.
///
/// First reading lands `interval` after spawn (so CPU% has a real
/// delta to report). The shared snapshot starts at `Default::default()`
/// with `gpu_backend == "unsampled"`; callers should treat that
/// sentinel as "no reading yet."
/// Cells produced by [`spawn`]. The aggregated [`HostMetricsSnapshot`]
/// powers the host stats pane; the live [`GpuInfo`] keeps `status.gpu`
/// from diverging from `status.host` on driver hotplug or a driver
/// loaded after daemon start.
#[derive(Clone)]
pub struct SamplerHandles {
  pub snapshot: Arc<RwLock<HostMetricsSnapshot>>,
  pub gpu: Arc<RwLock<GpuInfo>>,
}

/// Floor on the sampler tick. `config.daemon.metrics_interval_secs`
/// already clamps to `1..=60 s`; this only stops a direct caller
/// passing `Duration::ZERO` and busy-looping the task, so it sits well
/// below the config floor to leave sub-second intervals usable in tests.
const MIN_TICK: Duration = Duration::from_millis(1);

/// Counts full vendor re-probes so tests can assert the re-probe period
/// tracks wall-clock rather than the tick count.
#[cfg(test)]
static FULL_REPROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Serialises the tests that read [`FULL_REPROBES`] — it is one global
/// counter and the test harness runs them concurrently.
#[cfg(test)]
static REPROBE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Is a full vendor re-probe due, given how long it's been since the
/// last one?
///
/// Keyed on elapsed wall-clock, never on a tick count: the tick cadence
/// is user-configurable (`daemon.metrics_interval_secs`), so counting
/// ticks would silently scale the re-probe period by it. `None` means
/// the user disabled periodic re-probes.
fn due_for_reprobe(since_last: Duration, every: Option<Duration>) -> bool {
  every.is_some_and(|every| since_last >= every)
}

/// Spawn the host-metrics sampler.
///
/// `interval` is the tick cadence; `reprobe` is how often to re-run the
/// full vendor probe chain, or `None` to only probe once at startup.
pub fn spawn(
  shutdown: ShutdownToken,
  interval: Duration,
  reprobe: Option<Duration>,
) -> SamplerHandles {
  let interval = interval.max(MIN_TICK);
  let snapshot = Arc::new(RwLock::new(HostMetricsSnapshot {
    gpu_backend: HostMetricsSnapshot::UNINITIALIZED_BACKEND.into(),
    ..HostMetricsSnapshot::default()
  }));
  let gpu = Arc::new(RwLock::new(GpuInfo::CpuOnly));
  let snapshot_for_task = Arc::clone(&snapshot);
  let gpu_for_task = Arc::clone(&gpu);
  // Route through `spawn_supervised` so a panic in the loop body (a
  // misbehaving sysinfo refresh, a GPU probe parser bug) surfaces in
  // the daemon log instead of silently freezing the snapshot for the
  // lifetime of the daemon.
  super::supervisor::spawn_supervised("host_metrics_sampler", async move {
    let mut sys = System::new_with_specifics(host_refresh_kind());
    // Prime sysinfo's CPU delta on first refresh.
    sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
    // Hoist `Components` out of the per-tick path. The previous shape
    // allocated a fresh `Components::new_with_refreshed_list()` every
    // second *and* then called `components.refresh(true)` — the
    // constructor already calls `refresh(true)` internally, so the
    // second call was a redundant double-refresh. Keeping one
    // long-lived instance and refreshing in place each tick avoids
    // the allocation churn and the duplicate /sys/class/hwmon walk.
    let mut components = Components::new_with_refreshed_list();
    // Cache the detected GPU backend so we only spawn the matching
    // vendor tool each tick instead of running the full
    // nvidia→amd→metal→vulkan chain every second. Hotplug / late
    // driver loads are caught by the periodic full re-probe below.
    let mut info = tokio::select! {
      _ = shutdown.wait_until_triggered() => return,
      result = probe_gpu_full() => result,
    };
    // Elapsed-time based, not tick-counted: the tick cadence is user
    // configurable, so counting ticks would scale the re-probe period
    // by `metrics_interval_secs`.
    let mut last_full_probe = tokio::time::Instant::now();
    loop {
      tokio::select! {
        _ = shutdown.wait_until_triggered() => return,
        _ = tokio::time::sleep(interval) => {}
      }
      sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
      sys.refresh_memory();
      // `refresh(true)` updates values and prunes sensors that
      // vanished since the last tick (hot-removed cards / drivers).
      components.refresh(true);
      // Periodic full re-probe: catches hotplug, a late driver load,
      // and CpuOnly → detected once the vendor tool appears. Users who
      // don't want the spawns set `gpu.reprobe_interval_secs: 0`; there
      // is deliberately no extra per-backend skip, since every backend
      // (CpuOnly included) can transition.
      let now = tokio::time::Instant::now();
      if due_for_reprobe(now.duration_since(last_full_probe), reprobe) {
        last_full_probe = now;
        info = probe_gpu_full().await;
        #[cfg(test)]
        FULL_REPROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      }
      if let Some(refreshed) = refresh_active_gpu(info.clone()).await {
        // Fast path: only the active vendor's tool is spawned. No-op
        // for backends without live metrics (CpuOnly, AppleMetal,
        // Unknown/Vulkan).
        info = refreshed;
      }
      let next = build_snapshot(&sys, &components, info.clone());
      *snapshot_for_task.write().await = next;
      // Mirror the live GpuInfo so `status.gpu` follows hotplug /
      // late driver loads. Without this, `ctx.gpu` would stay
      // pinned to the startup snapshot forever even after the
      // host pane has detected the new device.
      *gpu_for_task.write().await = info.clone();
    }
  });
  SamplerHandles { snapshot, gpu }
}

fn host_refresh_kind() -> RefreshKind {
  RefreshKind::nothing()
    .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
    .with_memory(MemoryRefreshKind::everything())
}

/// Full vendor-chain probe on the blocking pool. Used at sampler
/// startup and on the periodic hotplug pass; the per-tick fast path
/// is [`refresh_active_gpu`].
async fn probe_gpu_full() -> GpuInfo {
  tokio::task::spawn_blocking(gpu::probe)
    .await
    .unwrap_or(GpuInfo::CpuOnly)
}

/// Per-tick GPU refresh on the blocking pool. Spawns only the
/// vendor tool matching the previously-detected backend (or none,
/// for CpuOnly / AppleMetal / Unknown). Returns `None` when the
/// active backend has no live metrics — the caller keeps the prior
/// snapshot rather than overwriting with a fresh probe result.
async fn refresh_active_gpu(prev: GpuInfo) -> Option<GpuInfo> {
  tokio::task::spawn_blocking(move || gpu::refresh_active(&prev))
    .await
    .ok()
    .flatten()
}

fn build_snapshot(sys: &System, components: &Components, info: GpuInfo) -> HostMetricsSnapshot {
  // Debug-build-only multi-GPU simulator; compiled out of release.
  #[cfg(debug_assertions)]
  let info = debug_fake_multi_gpu(info);
  let cpu_pct = host_cpu_pct(sys);
  let cpu_temp_c = host_cpu_temp_c(components);
  let agg = aggregate_gpu(&info);
  let gpu_devices = build_device_rows(&info);
  HostMetricsSnapshot {
    cpu_pct,
    cpu_temp_c,
    ram_used_bytes: sys.used_memory(),
    ram_total_bytes: sys.total_memory(),
    gpu_util_pct: agg.util,
    gpu_mem_used_bytes: agg.mem_used,
    gpu_mem_total_bytes: agg.mem_total,
    gpu_temp_c: agg.temp,
    gpu_backend: info.label().to_string(),
    gpu_device_count: agg.device_count,
    gpu_devices,
    uma_shared_total_bytes: agg.uma_shared_total,
    uma_shared_used_bytes: agg.uma_shared_used,
    unified: info.is_unified(),
    uma_class_source: info.uma_class_source(),
  }
}

/// Debug-only multi-GPU simulator, **compiled out of release builds**
/// (`#[cfg(debug_assertions)]`). When `LLAMASTASH_DEBUG_FAKE_GPUS=N`
/// (N >= 2) is set on the **daemon** process of a debug build, the
/// detected GPU is fanned out into N synthetic cards so the multi-GPU
/// host-pane rendering (the combined `GPU*` row + legend) can be
/// exercised on a single-GPU machine. Each clone gets a distinct name
/// (so `build_device_rows`' name-dedup keeps it) and stepped util/temp
/// so the mean-usage / hottest-temp aggregation is actually visible.
/// No-op unless the var is set — strictly a local testing aid.
#[cfg(debug_assertions)]
fn debug_fake_multi_gpu(info: GpuInfo) -> GpuInfo {
  let n: usize = match std::env::var("LLAMASTASH_DEBUG_FAKE_GPUS")
    .ok()
    .and_then(|v| v.parse().ok())
  {
    Some(n) if n >= 2 => n,
    _ => return info,
  };
  let base: Vec<GpuDevice> = match &info {
    GpuInfo::Nvidia { devices }
    | GpuInfo::Amd { devices }
    | GpuInfo::Unknown { devices }
    | GpuInfo::Multi { devices } => devices.clone(),
    // CpuOnly / AppleMetal carry no per-device list to fan out.
    _ => return info,
  };
  let Some(seed) = base.into_iter().next() else {
    return info;
  };
  let base_util = seed.utilization_pct.unwrap_or(20.0);
  let base_temp = seed.temperature_c.unwrap_or(45.0);
  let devices: Vec<GpuDevice> = (0..n)
    .map(|i| {
      let mut d = seed.clone();
      d.name = format!("{} (fake #{i})", seed.name);
      d.utilization_pct = Some((base_util + i as f32 * 20.0).min(100.0));
      d.temperature_c = Some(base_temp + i as f32 * 9.0);
      d
    })
    .collect();
  GpuInfo::Multi { devices }
}

/// Read the highest CPU package temperature from sysinfo Components.
/// Hardware exposes sensors under varied labels (`coretemp`,
/// `k10temp`, `Tctl`, `Tdie`, `CPU Package`, …); rather than match
/// the platform, take the max reading across any component whose
/// label hints at CPU. `None` when no matching sensor exists.
///
/// The caller hands in a refreshed `Components` — the sampler keeps
/// one long-lived instance and refreshes it in place each tick, so
/// no per-call allocation or /sys/class/hwmon re-walk happens here.
fn host_cpu_temp_c(components: &Components) -> Option<f32> {
  let mut max: Option<f32> = None;
  for c in components.iter() {
    let label_lc = c.label().to_ascii_lowercase();
    let cpu_hint = label_lc.contains("cpu")
      || label_lc.contains("coretemp")
      || label_lc.contains("k10temp")
      || label_lc.contains("tctl")
      || label_lc.contains("tdie")
      || label_lc.contains("package");
    if !cpu_hint {
      continue;
    }
    let Some(t) = c.temperature() else {
      continue;
    };
    if !t.is_finite() || t <= 0.0 {
      continue;
    }
    max = Some(max.map_or(t, |m| m.max(t)));
  }
  max
}

fn host_cpu_pct(sys: &System) -> f32 {
  let cpus = sys.cpus();
  if cpus.is_empty() {
    return 0.0;
  }
  let sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
  sum / cpus.len() as f32
}

#[derive(Default)]
struct GpuAggregate {
  util: Option<f32>,
  mem_used: Option<u64>,
  mem_total: Option<u64>,
  temp: Option<f32>,
  device_count: u32,
  uma_shared_total: Option<u64>,
  uma_shared_used: Option<u64>,
}

/// Per the plan's multi-GPU strategy: mean util%, summed VRAM, max temp.
fn aggregate_gpu(info: &GpuInfo) -> GpuAggregate {
  let all_devices: &[GpuDevice] = match info {
    GpuInfo::CpuOnly => return GpuAggregate::default(),
    GpuInfo::AppleMetal { total_memory_bytes } => {
      // Apple Silicon unified memory: no per-card "used" attribution
      // available via system_profiler. Surface total only so the UI
      // can render a `RAM (unified)` row.
      return GpuAggregate {
        mem_total: Some(*total_memory_bytes),
        device_count: 1,
        ..GpuAggregate::default()
      };
    }
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } => devices,
    GpuInfo::Unknown { devices } | GpuInfo::Multi { devices } => devices,
  };
  if all_devices.is_empty() {
    return GpuAggregate::default();
  }
  let count = all_devices.len() as u32;
  let total: u64 = all_devices.iter().map(|d| d.total_memory_bytes).sum();
  let used: u64 = all_devices.iter().map(|d| d.used_memory_bytes).sum();
  // Treat all-zero totals as "no VRAM data" — the Vulkan fallback
  // emits this and the host pane should show `—/—` instead of a
  // bar pinned to 0%.
  let (used_opt, total_opt) = if total == 0 {
    (None, None)
  } else {
    (Some(used), Some(total))
  };
  let util_readings: Vec<f32> = all_devices
    .iter()
    .filter_map(|d| d.utilization_pct)
    .collect();
  let util = if util_readings.is_empty() {
    None
  } else {
    Some(util_readings.iter().sum::<f32>() / util_readings.len() as f32)
  };
  let temp = all_devices.iter().filter_map(|d| d.temperature_c).fold(
    None,
    |acc: Option<f32>, t| match acc {
      None => Some(t),
      Some(prev) => Some(prev.max(t)),
    },
  );
  // Sum UMA-shared (GTT-on-AMD-APU) memory across devices so the
  // host pane can subtract it from the RAM gauge. `None` when no
  // device flagged a shared portion (discrete cards / non-UMA).
  let uma_shared_total: u64 = all_devices
    .iter()
    .filter_map(|d| d.uma_shared_total_bytes)
    .sum();
  let uma_shared_used: u64 = all_devices
    .iter()
    .filter_map(|d| d.uma_shared_used_bytes)
    .sum();
  let (uma_shared_total_opt, uma_shared_used_opt) = if uma_shared_total == 0 {
    (None, None)
  } else {
    (Some(uma_shared_total), Some(uma_shared_used))
  };
  GpuAggregate {
    util,
    mem_used: used_opt,
    mem_total: total_opt,
    temp,
    device_count: count,
    uma_shared_total: uma_shared_total_opt,
    uma_shared_used: uma_shared_used_opt,
  }
}

/// Build per-device rows from the probe result for the host-stats
/// pane. Each row carries the device name, VRAM (used/total),
/// utilization, and temperature when the vendor tool surfaces them.
/// The `selector` field is a display label only — the launch device
/// list comes from [`crate::backend::llama_cpp::list_devices`], not from here.
pub fn build_device_rows(info: &GpuInfo) -> Option<Vec<DeviceRow>> {
  let all: &[GpuDevice] = match info {
    GpuInfo::CpuOnly => return None,
    GpuInfo::AppleMetal { total_memory_bytes } => {
      return Some(vec![DeviceRow {
        selector: "Metal0".into(),
        backend: "apple_metal".into(),
        name: "Apple Silicon (unified)".into(),
        total_memory_bytes: *total_memory_bytes,
        used_memory_bytes: None,
        utilization_pct: None,
        temperature_c: None,
      }]);
    }
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } => devices,
    GpuInfo::Unknown { devices } | GpuInfo::Multi { devices } => devices,
  };
  if all.is_empty() {
    return None;
  }

  // Deduplicate by device name, preferring native backends (nvidia /
  // amd / apple_metal) over Vulkan fallback (unknown). Since native
  // probes run before Vulkan, the first occurrence of each name is
  // from a native backend — keep it, skip later Vulkan matches.
  let native_names: Vec<String> = all
    .iter()
    .filter(|d| matches!(d.backend.as_str(), "nvidia" | "amd" | "apple_metal"))
    .map(|d| d.name.clone())
    .collect();
  let deduped: Vec<&GpuDevice> = all
    .iter()
    .filter(|d| {
      let is_native = matches!(d.backend.as_str(), "nvidia" | "amd" | "apple_metal");
      is_native || !native_names.contains(&d.name)
    })
    .collect();
  let picker: Vec<DeviceRow> = deduped
    .iter()
    .enumerate()
    .map(|(i, d)| {
      let dev_prefix = match d.backend.as_str() {
        "nvidia" => "Nvidia",
        "amd" => "Amd",
        "unknown" => "Vulkan",
        "apple_metal" => "Metal",
        _ => "GPU",
      };
      DeviceRow {
        selector: format!("{}{}", dev_prefix, i),
        backend: d.backend.clone(),
        name: d.name.clone(),
        total_memory_bytes: d.total_memory_bytes,
        used_memory_bytes: if d.used_memory_bytes > 0 {
          Some(d.used_memory_bytes)
        } else {
          None
        },
        utilization_pct: d.utilization_pct,
        temperature_c: d.temperature_c,
      }
    })
    .collect();

  Some(picker)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::GpuDevice;

  #[test]
  fn gpu_flavor_classifies_each_documented_label() {
    // The wire string set is pinned (P2-16); the typed `flavor()`
    // view collapses three open-coded `match string` ladders into
    // one. Round-trip every documented label.
    assert_eq!(
      GpuFlavor::from_label(HostMetricsSnapshot::UNINITIALIZED_BACKEND),
      GpuFlavor::Unsampled
    );
    assert_eq!(
      GpuFlavor::from_label(HostMetricsSnapshot::BACKEND_CPU_ONLY),
      GpuFlavor::CpuOnly
    );
    assert_eq!(
      GpuFlavor::from_label(HostMetricsSnapshot::BACKEND_NVIDIA),
      GpuFlavor::Nvidia
    );
    assert_eq!(
      GpuFlavor::from_label(HostMetricsSnapshot::BACKEND_AMD),
      GpuFlavor::Amd
    );
    assert_eq!(
      GpuFlavor::from_label(HostMetricsSnapshot::BACKEND_APPLE_METAL),
      GpuFlavor::AppleMetal
    );
    assert_eq!(
      GpuFlavor::from_label(HostMetricsSnapshot::BACKEND_UNKNOWN),
      GpuFlavor::Unknown
    );
    // Any string outside the documented set classifies as `Unknown`
    // so the daemon never panics on a wire-side addition the TUI
    // hasn't been rebuilt with yet.
    assert_eq!(GpuFlavor::from_label("future-backend"), GpuFlavor::Unknown);
  }

  #[test]
  fn aggregate_cpu_only_returns_all_none_and_zero_devices() {
    let agg = aggregate_gpu(&GpuInfo::CpuOnly);
    let (util, used, total, temp, count) = (
      agg.util,
      agg.mem_used,
      agg.mem_total,
      agg.temp,
      agg.device_count,
    );
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
    let agg = aggregate_gpu(&info);
    let (util, used, total, temp, count) = (
      agg.util,
      agg.mem_used,
      agg.mem_total,
      agg.temp,
      agg.device_count,
    );
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
        ..Default::default()
      }],
    };
    let agg = aggregate_gpu(&info);
    let (util, used, total, temp, count) = (
      agg.util,
      agg.mem_used,
      agg.mem_total,
      agg.temp,
      agg.device_count,
    );
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
          ..Default::default()
        },
        GpuDevice {
          name: "1".into(),
          total_memory_bytes: 20,
          used_memory_bytes: 5,
          utilization_pct: Some(80.0),
          temperature_c: Some(72.0),
          ..Default::default()
        },
      ],
    };
    let agg = aggregate_gpu(&info);
    let (util, used, total, temp, count) = (
      agg.util,
      agg.mem_used,
      agg.mem_total,
      agg.temp,
      agg.device_count,
    );
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
          ..Default::default()
        },
        GpuDevice {
          name: "1".into(),
          total_memory_bytes: 10,
          used_memory_bytes: 2,
          utilization_pct: None,
          temperature_c: Some(55.0),
          ..Default::default()
        },
      ],
    };
    let agg = aggregate_gpu(&info);
    let (util, temp, count) = (agg.util, agg.temp, agg.device_count);
    assert_eq!(util, Some(60.0));
    assert_eq!(temp, Some(55.0));
    assert_eq!(count, 2);
  }

  #[test]
  fn aggregate_empty_device_vec_returns_all_none() {
    let info = GpuInfo::Nvidia { devices: vec![] };
    let agg = aggregate_gpu(&info);
    let (util, used, total, temp, count) = (
      agg.util,
      agg.mem_used,
      agg.mem_total,
      agg.temp,
      agg.device_count,
    );
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
  async fn spawn_task_exits_when_shutdown_token_fires() {
    // Triggering the shutdown token must terminate the sampler task
    // so the `Arc<RwLock<...>>` clone it held is released — once the
    // task drops, the strong count falls back to 1 (the local
    // variable). A regression that removes the
    // `wait_until_triggered` select arm would leave the task looping
    // forever and the strong count stuck at 2.
    let token = ShutdownToken::new();
    let handles = spawn(
      token.clone(),
      Duration::from_millis(20),
      Some(Duration::from_secs(60)),
    );
    // Give the task a moment to enter its loop and clone both Arcs.
    tokio::time::sleep(Duration::from_millis(30)).await;
    token.trigger();
    // The task may be parked inside spawn_blocking when the token
    // fires; poll the strong count for up to ~1s. Both cells (host
    // snapshot + live GpuInfo) should be released — checking either
    // is sufficient since the sampler owns clones of both.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while (Arc::strong_count(&handles.snapshot) > 1 || Arc::strong_count(&handles.gpu) > 1)
      && std::time::Instant::now() < deadline
    {
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
      Arc::strong_count(&handles.snapshot),
      1,
      "sampler task must release its snapshot Arc clone after shutdown",
    );
    assert_eq!(
      Arc::strong_count(&handles.gpu),
      1,
      "sampler task must release its live-gpu Arc clone after shutdown",
    );
  }

  #[tokio::test]
  async fn spawn_updates_snapshot_after_first_tick() {
    let token = ShutdownToken::new();
    let handles = spawn(
      token.clone(),
      Duration::from_millis(50),
      Some(Duration::from_secs(60)),
    );
    // First tick lands after the initial GPU probe + `interval`. On
    // macOS the probe shells out to system_profiler which can take
    // 1-3s; allow 5s total headroom for CI and loaded machines.
    for _ in 0..100 {
      tokio::time::sleep(Duration::from_millis(50)).await;
      let read = handles.snapshot.read().await.clone();
      if read.ram_total_bytes > 0 && read.gpu_backend != HostMetricsSnapshot::UNINITIALIZED_BACKEND
      {
        token.trigger();
        return;
      }
    }
    panic!(
      "host-metrics sampler did not produce a snapshot within 5s; \
       snapshot: {:?}",
      handles.snapshot.read().await
    );
  }

  /// Run the sampler for `run_for` and report how many full vendor
  /// re-probes it performed. Serialised by [`REPROBE_TEST_LOCK`] because
  /// [`FULL_REPROBES`] is process-global.
  async fn count_reprobes(
    interval: Duration,
    reprobe: Option<Duration>,
    run_for: Duration,
  ) -> usize {
    let _guard = REPROBE_TEST_LOCK.lock().await;
    FULL_REPROBES.store(0, std::sync::atomic::Ordering::Relaxed);
    let token = ShutdownToken::new();
    let _handles = spawn(token.clone(), interval, reprobe);
    tokio::time::sleep(run_for).await;
    token.trigger();
    FULL_REPROBES.load(std::sync::atomic::Ordering::Relaxed)
  }

  #[tokio::test]
  async fn a_zero_reprobe_interval_disables_periodic_reprobes() {
    let count = count_reprobes(Duration::from_millis(10), None, Duration::from_millis(400)).await;
    assert_eq!(
      count, 0,
      "reprobe_interval_secs: 0 must skip every periodic full probe"
    );
  }

  /// Regression: the re-probe period was once counted in sampler ticks,
  /// so raising `metrics_interval_secs` silently multiplied it. The
  /// decision must read elapsed wall-clock and nothing else — asserted
  /// on the pure helper because a real `probe_gpu_full` costs hundreds
  /// of ms (seconds on macOS), which makes counting live probes a race.
  #[test]
  fn reprobe_is_due_purely_on_elapsed_wall_clock() {
    let every = Some(Duration::from_secs(60));
    assert!(!due_for_reprobe(Duration::ZERO, every));
    assert!(!due_for_reprobe(Duration::from_secs(59), every));
    assert!(due_for_reprobe(Duration::from_secs(60), every));
    assert!(due_for_reprobe(Duration::from_secs(600), every));
  }

  #[test]
  fn no_reprobe_period_is_never_due() {
    for since in [Duration::ZERO, Duration::from_secs(86_400)] {
      assert!(!due_for_reprobe(since, None));
    }
  }

  /// The period is independent of the tick cadence: at a 10 s tick a
  /// 60 s period is still 60 s, not 600 s.
  #[test]
  fn reprobe_period_does_not_scale_with_the_tick_cadence() {
    let every = Some(Duration::from_secs(60));
    let slow_tick = Duration::from_secs(10);
    let ticks_until_due = (1..)
      .find(|n| due_for_reprobe(slow_tick * *n, every))
      .expect("a re-probe must eventually come due");
    assert_eq!(
      slow_tick * ticks_until_due,
      Duration::from_secs(60),
      "re-probe came due after the wrong amount of wall-clock time"
    );
  }

  /// A `Duration::ZERO` tick would busy-loop the task; [`MIN_TICK`]
  /// floors it. The config layer clamps to 1 s, so this only guards
  /// direct callers.
  #[tokio::test]
  async fn a_zero_interval_does_not_busy_loop() {
    let token = ShutdownToken::new();
    let handles = spawn(token.clone(), Duration::ZERO, None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    token.trigger();
    for _ in 0..100 {
      if Arc::strong_count(&handles.snapshot) == 1 {
        return;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("sampler with a zero interval never released its Arc after shutdown");
  }
}
