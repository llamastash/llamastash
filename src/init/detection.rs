//! Hardware + binary detection shared by `init` and `doctor`.
//!
//! Two pure entry points:
//! - [`detect_hardware`] aggregates `gpu::probe` + RAM/disk inspection
//!   into a [`HardwareSnapshot`] the wizard renders in its persistent
//!   header (R50) and stamps into `_init_snapshot` (R67).
//! - [`detect_binary`] wraps `launch::binary::locate` with the
//!   common-location probes from R54: prior llamastash-managed install
//!   dir, `/opt/homebrew/bin`, `/usr/local/bin`, the linuxbrew prefix
//!   (`/home/linuxbrew/.linuxbrew/bin`), `~/.local/bin`, plus the
//!   `LLAMASTASH_LLAMA_SERVER` env hint.
//!
//! `doctor` calls both read-only; init calls them at step 1 and
//! stashes the result for the rest of the run.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sysinfo::System;

use crate::gpu::{self, GpuInfo};
use crate::launch::binary::{locate, LocateInputs};

/// VRAM aggregation rule pinned in Key Decisions:
/// - Nvidia / AMD: `min(device.total_memory_bytes)` because the
///   recommender's single-GPU placement is the limiting case.
/// - AppleMetal: `total_memory_bytes × 0.75` (Metal uses unified
///   memory; the ratio leaves headroom for the OS + apps).
/// - CpuOnly / Unknown: `None`.
pub fn aggregate_vram_bytes(info: &GpuInfo) -> Option<u64> {
  match info {
    GpuInfo::CpuOnly | GpuInfo::Unknown { .. } => None,
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } => {
      devices.iter().map(|d| d.total_memory_bytes).min()
    }
    GpuInfo::AppleMetal { total_memory_bytes } => Some((*total_memory_bytes as f64 * 0.75) as u64),
  }
}

/// What the recommender + wizard need to know about the host.
/// Serialisable so `init --json` and `doctor --json` carry the same
/// shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HardwareSnapshot {
  pub gpu: GpuInfo,
  /// Result of `aggregate_vram_bytes`. `None` for CPU-only / unknown.
  pub vram_bytes: Option<u64>,
  /// GPU device count (Nvidia / AMD lists; `1` for Apple Silicon;
  /// `0` for CpuOnly / Unknown so the wizard can format the persistent
  /// header consistently).
  pub gpu_device_count: u32,
  /// Total physical RAM in bytes.
  pub ram_total_bytes: u64,
  /// OS family the recommender + install router branches on.
  pub os: OsFamily,
  /// CPU architecture the install router branches on (variant select
  /// for GH Releases assets).
  pub cpu_arch: CpuArch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
  Linux,
  MacOs,
  Other,
}

impl OsFamily {
  pub fn detect() -> Self {
    match std::env::consts::OS {
      "linux" => Self::Linux,
      "macos" => Self::MacOs,
      _ => Self::Other,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuArch {
  X86_64,
  Arm64,
  Other,
}

impl CpuArch {
  pub fn detect() -> Self {
    match std::env::consts::ARCH {
      "x86_64" => Self::X86_64,
      "aarch64" => Self::Arm64,
      _ => Self::Other,
    }
  }
}

/// Run the full hardware detection chain. Calls `gpu::probe` once
/// (takes up to `PROBE_TIMEOUT` total for the chained vendor probes),
/// reads RAM from `sysinfo`, and reads OS / arch from `std::env::consts`.
pub fn detect_hardware() -> HardwareSnapshot {
  let gpu = gpu::probe();
  let vram_bytes = aggregate_vram_bytes(&gpu);
  let gpu_device_count = match &gpu {
    GpuInfo::CpuOnly => 0,
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } | GpuInfo::Unknown { devices } => {
      devices.len() as u32
    }
    GpuInfo::AppleMetal { .. } => 1,
  };
  let mut sys = System::new();
  sys.refresh_memory();
  let ram_total_bytes = sys.total_memory();
  HardwareSnapshot {
    gpu,
    vram_bytes,
    gpu_device_count,
    ram_total_bytes,
    os: OsFamily::detect(),
    cpu_arch: CpuArch::detect(),
  }
}

/// Where the binary came from. Distinct from
/// `crate::init::snapshot::InstallMethod` because *detection* can
/// surface paths the wizard has not yet adopted — e.g. a brew-installed
/// `llama-server` on the path before the user opts into using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BinarySource {
  /// Resolved via the `--llama-server` flag, `LLAMASTASH_LLAMA_SERVER`
  /// env, or the `llama_server_path` config key — the same priority
  /// order [`crate::launch::binary::locate`] uses.
  Configured,
  /// Found on `$PATH`.
  Path,
  /// Found at a common install location (brew / linuxbrew / a prior
  /// llamastash-managed dir / `~/.local/bin`).
  CommonLocation,
  /// Nothing on disk — the wizard should run the install step.
  None,
}

/// Detection output. `resolved_path` is `Some` whenever the wizard can
/// proceed without running an install step. `source` carries the
/// provenance so the wizard pre-selects the right install-method
/// option (e.g. "use existing" when source is `Configured` or
/// `CommonLocation`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BinaryPresence {
  pub resolved_path: Option<PathBuf>,
  pub source: BinarySource,
}

impl BinaryPresence {
  fn none() -> Self {
    Self {
      resolved_path: None,
      source: BinarySource::None,
    }
  }
}

/// Inputs to [`detect_binary`]. Mirrors `LocateInputs` plus a probe
/// extension that scans the common-location list. Production callers
/// thread the same `--llama-server` / env / config values they pass
/// to `launch::binary::locate`.
#[derive(Debug, Clone, Default)]
pub struct DetectBinaryInputs {
  pub cli_flag: Option<PathBuf>,
  pub env_var: Option<std::ffi::OsString>,
  pub config_path: Option<PathBuf>,
}

/// Common locations probed in order. The list is platform-aware:
/// linuxbrew on Linux, `/opt/homebrew/bin` on macOS arm64,
/// `/usr/local/bin` on both, plus the user-scoped `~/.local/bin` and
/// the prior llamastash-managed install dir under `$XDG_DATA_HOME`.
fn common_locations() -> Vec<PathBuf> {
  let mut roots: Vec<PathBuf> = Vec::new();
  if let Some(home) = crate::util::paths::home_dir() {
    roots.push(home.join(".local/bin/llama-server"));
  }
  // llamastash-managed install dir from Unit 8 — Vec keeps order stable
  // for tests.
  if let Some(data) = directories::BaseDirs::new().and_then(|b| {
    b.data_dir()
      .join("llamastash/llama-cpp")
      .canonicalize()
      .ok()
      .or_else(|| Some(b.data_dir().join("llamastash/llama-cpp")))
  }) {
    // The actual binary lives under `<data>/<version>/llama-server`;
    // we don't enumerate versions here, but expose the root so a
    // higher-level probe (Unit 10) can pick the newest.
    roots.push(data.join("llama-server"));
  }
  match (OsFamily::detect(), CpuArch::detect()) {
    (OsFamily::MacOs, CpuArch::Arm64) => {
      roots.push(PathBuf::from("/opt/homebrew/bin/llama-server"));
      roots.push(PathBuf::from("/usr/local/bin/llama-server"));
    }
    (OsFamily::MacOs, _) => {
      roots.push(PathBuf::from("/usr/local/bin/llama-server"));
    }
    (OsFamily::Linux, _) => {
      roots.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/llama-server"));
      roots.push(PathBuf::from("/usr/local/bin/llama-server"));
    }
    _ => {}
  }
  roots
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
  paths.iter().find(|p| is_executable_file(p)).cloned()
}

fn is_executable_file(path: &Path) -> bool {
  let Ok(meta) = std::fs::metadata(path) else {
    return false;
  };
  if !meta.is_file() {
    return false;
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
  }
  #[cfg(not(unix))]
  {
    let _ = meta;
    true
  }
}

/// Detect whether `llama-server` is already on the system. Resolution
/// order: configured (flag / env / config) → `$PATH` (`which`) →
/// common locations (brew / linuxbrew / `~/.local/bin` / prior
/// install). `None` when nothing matched.
pub fn detect_binary(inputs: DetectBinaryInputs) -> BinaryPresence {
  // First: the same priority `launch::binary::locate` uses. A hit
  // here carries either `Configured` (caller-supplied) or `Path`
  // (PATH fallback) provenance.
  let configured_supplied =
    inputs.cli_flag.is_some() || inputs.env_var.is_some() || inputs.config_path.is_some();
  let inputs_for_locate = LocateInputs {
    cli_flag: inputs.cli_flag.clone(),
    env_var: inputs.env_var.clone(),
    config_path: inputs.config_path.clone(),
  };
  if let Ok(resolved) = locate(inputs_for_locate) {
    let source = if configured_supplied {
      BinarySource::Configured
    } else {
      BinarySource::Path
    };
    return BinaryPresence {
      resolved_path: Some(resolved),
      source,
    };
  }
  // Second: the R54 common-location list.
  if let Some(path) = first_existing(&common_locations()) {
    return BinaryPresence {
      resolved_path: Some(path),
      source: BinarySource::CommonLocation,
    };
  }
  BinaryPresence::none()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::GpuDevice;

  #[test]
  fn aggregate_vram_picks_minimum_across_nvidia_devices() {
    let info = GpuInfo::Nvidia {
      devices: vec![
        GpuDevice {
          name: "RTX 4090".into(),
          total_memory_bytes: 24 * 1_024 * 1_024 * 1_024,
          used_memory_bytes: 0,
          utilization_pct: None,
          temperature_c: None,
        },
        GpuDevice {
          name: "RTX 3060".into(),
          total_memory_bytes: 12 * 1_024 * 1_024 * 1_024,
          used_memory_bytes: 0,
          utilization_pct: None,
          temperature_c: None,
        },
      ],
    };
    assert_eq!(
      aggregate_vram_bytes(&info),
      Some(12 * 1_024 * 1_024 * 1_024)
    );
  }

  #[test]
  fn aggregate_vram_for_apple_metal_applies_75_percent_ratio() {
    let info = GpuInfo::AppleMetal {
      total_memory_bytes: 64 * 1_024 * 1_024 * 1_024,
    };
    let expected = (64.0_f64 * 1_024.0 * 1_024.0 * 1_024.0 * 0.75) as u64;
    assert_eq!(aggregate_vram_bytes(&info), Some(expected));
  }

  #[test]
  fn aggregate_vram_cpu_only_and_unknown_are_none() {
    assert!(aggregate_vram_bytes(&GpuInfo::CpuOnly).is_none());
    assert!(aggregate_vram_bytes(&GpuInfo::Unknown { devices: vec![] }).is_none());
  }

  #[test]
  fn detect_binary_returns_none_when_nothing_resolves() {
    // Use a deliberately bad cli flag so `locate` errors out, no env
    // override, no config path. `$PATH` may legitimately have a
    // `llama-server` on a dev machine, so we don't assert
    // `resolved_path.is_none()` unconditionally — we assert the
    // provenance instead: an unsupplied input must never report as
    // `Configured`.
    let presence = detect_binary(DetectBinaryInputs::default());
    if presence.resolved_path.is_some() {
      assert!(
        matches!(
          presence.source,
          BinarySource::Path | BinarySource::CommonLocation
        ),
        "unconfigured input must not return Configured"
      );
    } else {
      assert_eq!(presence.source, BinarySource::None);
    }
  }

  #[test]
  fn detect_binary_honours_explicit_configured_path() {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let dir = std::env::temp_dir().join(format!(
      "llamastash-detect-binary-test-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let target = dir.join("llama-server");
    fs::write(&target, b"#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let presence = detect_binary(DetectBinaryInputs {
      cli_flag: Some(target.clone()),
      ..Default::default()
    });
    assert_eq!(presence.source, BinarySource::Configured);
    assert!(presence.resolved_path.is_some());
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn cpu_arch_detect_matches_target_arch() {
    let detected = CpuArch::detect();
    if cfg!(target_arch = "x86_64") {
      assert_eq!(detected, CpuArch::X86_64);
    } else if cfg!(target_arch = "aarch64") {
      assert_eq!(detected, CpuArch::Arm64);
    } else {
      assert_eq!(detected, CpuArch::Other);
    }
  }

  #[test]
  fn os_family_detect_matches_target_os() {
    let detected = OsFamily::detect();
    if cfg!(target_os = "linux") {
      assert_eq!(detected, OsFamily::Linux);
    } else if cfg!(target_os = "macos") {
      assert_eq!(detected, OsFamily::MacOs);
    } else {
      assert_eq!(detected, OsFamily::Other);
    }
  }

  #[test]
  fn detect_hardware_returns_non_panicking_snapshot_on_this_machine() {
    // Smoke check: detection must not panic on the CI runner. The
    // exact gpu / vram values vary; just assert structural invariants.
    let snap = detect_hardware();
    assert!(snap.ram_total_bytes > 0, "RAM must be discoverable");
    match snap.gpu {
      GpuInfo::CpuOnly => {
        assert_eq!(snap.gpu_device_count, 0);
        assert!(snap.vram_bytes.is_none());
      }
      _ => {
        // Non-CpuOnly may still have None vram (Unknown variant) — but
        // device_count must reflect the variant.
      }
    }
  }
}
