//! Best-effort GPU detection at daemon start (R44).
//!
//! v1 baseline strategy: shell out to vendor tools and parse their
//! human/JSON output rather than linking native SDKs (NVML, ROCm,
//! Metal). This keeps the build portable across CUDA / ROCm /
//! Apple Silicon machines without conditional native deps. Future
//! follow-up: replace the shell-out with `nvml-wrapper` on Linux
//! for accurate per-PID VRAM attribution.
//!
//! Detection order, per the plan:
//! 1. NVIDIA via `nvidia-smi --query-gpu=...` (Linux + Windows).
//! 2. AMD via `rocm-smi --showmeminfo vram --json` (Linux).
//! 3. Apple Silicon Metal via `system_profiler SPDisplaysDataType
//!    -json` (macOS).
//! 4. Fallback: `CpuOnly` — the supervisor still runs, just without
//!    a GPU memory line in `status`.

pub mod amd;
pub mod metal;
pub mod nvidia;
pub mod vulkan;

use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde::Serialize;

/// Wall-clock budget for a single vendor probe. A wedged GPU driver
/// (nvidia-smi hang, ROCm reset, locked Vulkan loader) would otherwise
/// pin the blocking pool thread indefinitely. Five seconds is well
/// above any normal vendor-tool invocation on healthy hardware.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run an external probe with a wall-clock deadline. On expiry the
/// child is killed; the call returns `None` so the probe chain can
/// fall through to the next backend instead of stalling the daemon.
pub(crate) fn run_with_timeout(mut cmd: Command) -> Option<Output> {
  // `Command::spawn()` inherits the parent's stdio by default, which
  // means `wait_with_output()` would return empty buffers — and
  // every probe would silently fall through to `CpuOnly` despite a
  // working vendor binary. Pipe stdout/stderr explicitly so the
  // child's output is captured the way `Command::output()` would.
  let mut child = cmd
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .stdin(Stdio::null())
    .spawn()
    .ok()?;
  let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
  loop {
    match child.try_wait() {
      Ok(Some(_status)) => {
        return child.wait_with_output().ok();
      }
      Ok(None) => {
        if std::time::Instant::now() >= deadline {
          let _ = child.kill();
          let _ = child.wait();
          log::warn!(
            "gpu probe `{:?}` exceeded {:?}; killed and falling through",
            cmd.get_program(),
            PROBE_TIMEOUT
          );
          return None;
        }
        std::thread::sleep(Duration::from_millis(25));
      }
      Err(_) => return None,
    }
  }
}

/// What detection found. Always a complete snapshot — no
/// "partial" / "unknown" middle ground — so the IPC handler can
/// serialise it directly into `status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum GpuInfo {
  /// No GPU detected (or detection failed). The daemon still runs;
  /// `llama-server` falls back to CPU inference.
  CpuOnly,
  /// NVIDIA card(s) found. Multi-GPU machines surface as a list of
  /// devices.
  Nvidia { devices: Vec<GpuDevice> },
  /// AMD card(s) found.
  Amd { devices: Vec<GpuDevice> },
  /// Apple Silicon — unified-memory GPU. Reports the system memory
  /// available to the GPU since Metal doesn't separate VRAM.
  AppleMetal { total_memory_bytes: u64 },
  /// `vulkaninfo` found a device but neither NVIDIA nor ROCm probes
  /// succeeded, so the vendor is unknown. The supervisor still hints
  /// that the user can attempt `-ngl > 0`; the host pane renders
  /// `backend  unknown` rather than mislabelling the card.
  Unknown { devices: Vec<GpuDevice> },
}

/// One discrete GPU device (NVIDIA / AMD path).
///
/// `utilization_pct` and `temperature_c` are best-effort: the per-tick
/// host-metrics sampler reads them from vendor tools that may or may
/// not expose them on a given platform / driver version. When a probe
/// can't surface them they stay `None`; the host stats pane renders
/// `—` in place of a numeric reading rather than dropping the row.
///
/// Note: this struct intentionally does not derive `Eq` because the
/// `f32` fields don't satisfy `Eq` (NaN-not-equal-to-itself). The
/// `PartialEq` derive is sufficient for the only equality use case
/// today — round-tripping in tests. Downstream consumers needing a
/// hashable / `Eq`-bound view should compare a projection (e.g. the
/// `name` + `total_memory_bytes` fields) rather than the whole struct.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuDevice {
  pub name: String,
  pub total_memory_bytes: u64,
  pub used_memory_bytes: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub utilization_pct: Option<f32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub temperature_c: Option<f32>,
}

impl GpuInfo {
  pub fn label(&self) -> &'static str {
    match self {
      Self::CpuOnly => "cpu_only",
      Self::Nvidia { .. } => "nvidia",
      Self::Amd { .. } => "amd",
      Self::AppleMetal { .. } => "apple_metal",
      Self::Unknown { .. } => "unknown",
    }
  }

  pub fn is_gpu(&self) -> bool {
    !matches!(self, Self::CpuOnly)
  }
}

/// Run the full detection chain. Best-effort — every probe failure
/// just falls through to the next backend, then to `CpuOnly`.
/// Suitable for daemon startup; not called per-launch.
pub fn probe() -> GpuInfo {
  if let Some(info) = nvidia::probe() {
    return info;
  }
  if let Some(info) = amd::probe() {
    return info;
  }
  if let Some(info) = metal::probe() {
    return info;
  }
  // Vulkan check is a last-resort "is *anything* there?" signal —
  // it can't give us memory numbers, but the supervisor uses it to
  // hint that the user can probably set `-ngl > 0` even though we
  // don't know how much VRAM they have. Returns CpuOnly when even
  // Vulkan can't find a device.
  vulkan::probe().unwrap_or(GpuInfo::CpuOnly)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cpu_only_is_not_gpu() {
    assert!(!GpuInfo::CpuOnly.is_gpu());
    assert_eq!(GpuInfo::CpuOnly.label(), "cpu_only");
  }

  #[test]
  fn nvidia_is_gpu() {
    let info = GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "RTX 4090".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
      }],
    };
    assert!(info.is_gpu());
    assert_eq!(info.label(), "nvidia");
  }

  #[test]
  fn json_carries_tag_field() {
    let v = GpuInfo::AppleMetal {
      total_memory_bytes: 64 * 1024 * 1024 * 1024,
    };
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("\"backend\":\"apple_metal\""));
    assert!(s.contains("\"total_memory_bytes\":"));
  }

  #[test]
  fn gpu_device_omits_optional_fields_when_absent() {
    let dev = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 0,
      utilization_pct: None,
      temperature_c: None,
    };
    let s = serde_json::to_string(&dev).unwrap();
    assert!(!s.contains("utilization_pct"));
    assert!(!s.contains("temperature_c"));
  }

  #[test]
  fn gpu_device_emits_optional_fields_when_present() {
    let dev = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 12 * 1024 * 1024 * 1024,
      utilization_pct: Some(84.0),
      temperature_c: Some(68.0),
    };
    let s = serde_json::to_string(&dev).unwrap();
    assert!(s.contains("\"utilization_pct\":84"));
    assert!(s.contains("\"temperature_c\":68"));
  }
}
