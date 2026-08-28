//! Hardware + binary detection shared by `init` and `doctor`.
//!
//! Two pure entry points:
//! - [`detect_hardware`] aggregates `gpu::probe` + RAM/disk inspection
//!   into a [`HardwareSnapshot`] the wizard renders in its persistent
//!   header and stamps into `_init_snapshot`.
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
use sysinfo::{Disks, System};

use crate::backend::Backend as _;
use crate::gpu::{self, ClassSource, GpuDevice, GpuInfo};
use crate::launch::binary::{locate, LocateInputs};

/// Canonical gibibyte formatter — bytes ÷ 1024³, one decimal, `GiB`.
/// One implementation so every hardware surface (`init` banner,
/// `doctor`, the recommender, `status`, the TUI) prints memory with the
/// same unit and precision. (Earlier copies disagreed: some printed
/// `GB`, some `GiB`, with different decimal rules.)
pub fn fmt_gib(bytes: u64) -> String {
  format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Canonical spelled-out multi-unit byte formatter: picks the largest
/// binary unit that keeps the number readable and spells the suffix
/// (`B` / `KiB` / `MiB` / `GiB`). One decimal at MiB/GiB, none at KiB,
/// exact bytes under 1 KiB. Shared by `show` (on-disk size, per-shard
/// rows) so a file-size surface and the `GiB`-only `fmt_gib` agree on
/// precision at the GiB boundary.
pub fn fmt_bytes(bytes: u64) -> String {
  const KIB: f64 = 1024.0;
  const MIB: f64 = KIB * 1024.0;
  const GIB: f64 = MIB * 1024.0;
  let b = bytes as f64;
  if b >= GIB {
    format!("{:.1} GiB", b / GIB)
  } else if b >= MIB {
    format!("{:.1} MiB", b / MIB)
  } else if b >= KIB {
    format!("{:.0} KiB", b / KIB)
  } else {
    format!("{bytes} B")
  }
}

/// Canonical display name for a GPU backend label (the `gpu_backend`
/// wire string / [`GpuInfo::label`]). Used wherever a GPU vendor is
/// named so `status` / `doctor` / `init` / the TUI agree on the word.
pub fn gpu_vendor_display(backend: &str) -> &'static str {
  match backend {
    "nvidia" => "NVIDIA",
    "amd" => "AMD",
    "apple_metal" => "Apple",
    "multi" => "Multi-GPU",
    "unknown" => "GPU (Vulkan)",
    // The daemon's pre-first-sample placeholder — not a CPU-only verdict.
    "unsampled" => "detecting",
    // cpu_only and anything unrecognised.
    _ => "CPU only",
  }
}

/// Canonical one-line GPU summary shared by `status` and `doctor`:
/// `<vendor> · <pool total> (<verdict>)`, e.g.
/// `AMD · 124.5 GiB (unified, inferred)`. The parenthetical is the
/// classification *verdict* + confidence ([`ClassSource::label`]);
/// discrete cards carry no suffix. CPU-only and not-yet-sampled hosts
/// render the bare vendor word so the line never claims memory it
/// doesn't have.
pub fn gpu_summary_line(
  backend: &str,
  pool_total_bytes: Option<u64>,
  class_source: Option<ClassSource>,
) -> String {
  let vendor = gpu_vendor_display(backend);
  match (backend, pool_total_bytes) {
    ("cpu_only" | "unsampled", _) | (_, None) => vendor.to_string(),
    (_, Some(total)) => match class_source {
      Some(src) => format!("{vendor} · {} ({})", fmt_gib(total), src.label()),
      None => format!("{vendor} · {}", fmt_gib(total)),
    },
  }
}

/// VRAM aggregation rule pinned in Key Decisions:
/// - Nvidia / AMD / Multi: `min(device.total_memory_bytes)` over the
///   devices that report a non-zero size, because the recommender's
///   single-GPU placement is the limiting case.
/// - AppleMetal: the **raw** unified total. The historical `× 0.75`
///   OS/app headroom moved to [`crate::launch::headroom`] — the
///   one place the budget authority and refusal messages consult, so
///   every display + sizing surface sees raw totals and headroom is
///   applied once, by admission. The recommender keeps its own
///   `SAFETY_MARGIN` on top of the raw value.
/// - CpuOnly / Unknown: `None`.
///
/// Zero-byte devices are excluded from the `min()`. A multi-GPU host
/// can surface a device whose VRAM size a backend can't read — e.g. a
/// Vulkan/`--list-devices` (RADV) duplicate of a ROCm card that the
/// cross-backend dedup failed to collapse. Letting that 0 win the
/// `min()` would collapse the whole host's effective VRAM to zero and
/// make the init smoke probe report a false OOM (peak > ceiling of 0).
/// When *every* device reports 0 we return `None` (the CPU-only path),
/// never `Some(0)`.
pub fn aggregate_vram_bytes(info: &GpuInfo) -> Option<u64> {
  let all_devices: &[GpuDevice] = match info {
    GpuInfo::CpuOnly | GpuInfo::Unknown { .. } => return None,
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } => devices,
    GpuInfo::AppleMetal { total_memory_bytes } => {
      // Raw unified total — the 0.75 OS/app headroom now lives in
      // `launch::headroom` and is applied by admission.
      return Some(*total_memory_bytes);
    }
    GpuInfo::Multi { devices } => devices,
  };
  all_devices
    .iter()
    .map(|d| d.total_memory_bytes)
    .filter(|&bytes| bytes > 0)
    .min()
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
  /// Free disk space on the partition holding the user's home directory
  /// (the same volume model downloads land on). `0` when no disk could
  /// be matched — sysinfo skipped, container without an obvious home
  /// mount, etc. The banner renders this as a hint, not a gate.
  #[serde(default)]
  pub disk_free_bytes: u64,
  /// Human CPU brand string as reported by sysinfo (e.g.
  /// "AMD Ryzen AI MAX+ 395"). Empty when sysinfo couldn't read it.
  #[serde(default)]
  pub cpu_brand: String,
  /// Number of physical cores. Falls back to logical count when
  /// `sysinfo::System::physical_core_count` returns `None`.
  #[serde(default)]
  pub cpu_cores: u32,
  /// Inference-relevant CPU instruction-set names detected at runtime
  /// (subset of `std::arch::is_*_feature_detected!`). Empty on archs
  /// without a meaningful surface (Other) or when no listed feature
  /// is present.
  #[serde(default)]
  pub cpu_features: Vec<String>,
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
  Windows,
  Other,
}

impl OsFamily {
  pub fn detect() -> Self {
    match std::env::consts::OS {
      "linux" => Self::Linux,
      "macos" => Self::MacOs,
      "windows" => Self::Windows,
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
    GpuInfo::Multi { devices } => devices.len() as u32,
  };
  let mut sys = System::new();
  sys.refresh_memory();
  sys.refresh_cpu_all();
  let ram_total_bytes = sys.total_memory();
  let cpu_brand = sys
    .cpus()
    .first()
    .map(|c| c.brand().trim().to_string())
    .unwrap_or_default();
  let cpu_cores = System::physical_core_count()
    .or_else(|| {
      let n = sys.cpus().len();
      if n == 0 {
        None
      } else {
        Some(n)
      }
    })
    .unwrap_or(0) as u32;
  let cpu_features = detect_cpu_features();
  let disk_free_bytes = detect_home_disk_free_bytes();
  HardwareSnapshot {
    gpu,
    vram_bytes,
    gpu_device_count,
    ram_total_bytes,
    disk_free_bytes,
    cpu_brand,
    cpu_cores,
    cpu_features,
    os: OsFamily::detect(),
    cpu_arch: CpuArch::detect(),
  }
}

/// Runtime SIMD detection. We only surface features that meaningfully
/// affect LLM inference (AVX2 / AVX-512 / FMA on x86_64; NEON / SVE on
/// arm64). Anything not listed here is omitted to keep the banner from
/// drowning in flags. Returns an empty vec on `CpuArch::Other`.
fn detect_cpu_features() -> Vec<String> {
  let mut out: Vec<String> = Vec::new();
  #[cfg(target_arch = "x86_64")]
  {
    if std::arch::is_x86_feature_detected!("avx2") {
      out.push("AVX2".into());
    }
    if std::arch::is_x86_feature_detected!("avx512f") {
      out.push("AVX-512".into());
    }
    if std::arch::is_x86_feature_detected!("fma") {
      out.push("FMA".into());
    }
  }
  #[cfg(target_arch = "aarch64")]
  {
    if std::arch::is_aarch64_feature_detected!("neon") {
      out.push("NEON".into());
    }
    if std::arch::is_aarch64_feature_detected!("sve") {
      out.push("SVE".into());
    }
  }
  out
}

/// Free bytes on the volume the user's home directory lives on. That's
/// where model downloads land by default, so it's the number the user
/// actually cares about when sizing a 30B Q4 download. Falls back to
/// the longest-prefix-matching mount on `/` if `$HOME` is unset or
/// nothing matches. Returns `0` on any failure — the banner treats
/// zero as "unknown" and elides the segment.
fn detect_home_disk_free_bytes() -> u64 {
  let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
  let home_path = PathBuf::from(&home);
  let disks = Disks::new_with_refreshed_list();
  let mut best: Option<(usize, u64)> = None;
  for disk in disks.list() {
    let mount = disk.mount_point();
    if !home_path.starts_with(mount) {
      continue;
    }
    let depth = mount.components().count();
    let free = disk.available_space();
    match best {
      Some((d, _)) if depth <= d => continue,
      _ => best = Some((depth, free)),
    }
  }
  best.map(|(_, free)| free).unwrap_or(0)
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
///
/// Every directory is probed for each server basename the default backend
/// declares (the standalone server, then the unified app — the name the
/// official llama.cpp installer drops in `~/.local/bin`), so this list names
/// no backend binary itself.
fn common_locations() -> Vec<PathBuf> {
  let mut dirs: Vec<PathBuf> = Vec::new();
  if let Some(home) = crate::util::paths::home_dir() {
    dirs.push(home.join(".local/bin"));
  }
  // llamastash-managed install dir — Vec keeps order stable
  // for tests.
  if let Some(data) = directories::BaseDirs::new().and_then(|b| {
    let p = b.data_dir().join("llamastash/llama-cpp");
    crate::util::paths::canonicalize(&p).ok().or(Some(p))
  }) {
    // The actual binary lives under `<data>/<version>/<name>`; we don't
    // enumerate versions here, but expose the root so a higher-level probe
    // can pick the newest.
    dirs.push(data);
  }
  match (OsFamily::detect(), CpuArch::detect()) {
    (OsFamily::MacOs, CpuArch::Arm64) => {
      dirs.push(PathBuf::from("/opt/homebrew/bin"));
      dirs.push(PathBuf::from("/usr/local/bin"));
    }
    (OsFamily::MacOs, _) => {
      dirs.push(PathBuf::from("/usr/local/bin"));
    }
    (OsFamily::Linux, _) => {
      dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
      dirs.push(PathBuf::from("/usr/local/bin"));
    }
    _ => {}
  }
  let names = crate::backend::default_backend().process_markers();
  dirs
    .iter()
    .flat_map(|d| names.iter().map(move |n| d.join(n)))
    .collect()
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
  fn common_locations_probe_every_server_binary_name_per_directory() {
    let probed = common_locations();
    let names = crate::backend::default_backend().process_markers();
    assert!(names.len() > 1, "the default backend declares alternates");
    for name in names {
      assert!(
        probed.iter().any(|p| p.file_name() == Some(name.as_ref())),
        "no common location probes `{name}`"
      );
    }
    // Every directory contributes one candidate per name, in name order.
    assert_eq!(probed.len() % names.len(), 0);
  }

  #[test]
  fn fmt_bytes_rolls_through_units_at_threshold_boundaries() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(1023), "1023 B");
    assert_eq!(fmt_bytes(1024), "1 KiB");
    assert_eq!(fmt_bytes(1024 * 1024 - 1), "1024 KiB");
    assert_eq!(fmt_bytes(1024 * 1024), "1.0 MiB");
    assert_eq!(fmt_bytes(2 * 1024 * 1024), "2.0 MiB");
    assert_eq!(fmt_bytes(1024 * 1024 * 1024), "1.0 GiB");
    assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
  }

  #[test]
  fn fmt_bytes_gib_precision_matches_fmt_gib() {
    // Both round 1024³ B to one decimal GiB, so the file-size surface
    // and the canonical memory surface never disagree on the boundary.
    let n = 40_000_000_000u64;
    assert_eq!(fmt_bytes(n), fmt_gib(n));
  }

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
          ..Default::default()
        },
        GpuDevice {
          name: "RTX 3060".into(),
          total_memory_bytes: 12 * 1_024 * 1_024 * 1_024,
          used_memory_bytes: 0,
          utilization_pct: None,
          temperature_c: None,
          ..Default::default()
        },
      ],
    };
    assert_eq!(
      aggregate_vram_bytes(&info),
      Some(12 * 1_024 * 1_024 * 1_024)
    );
  }

  #[test]
  fn aggregate_vram_for_apple_metal_returns_raw_total() {
    // The 0.75 OS/app headroom moved to `launch::headroom`; the
    // aggregate now reports the raw unified total like every other
    // display + sizing surface.
    let total = 64 * 1_024 * 1_024 * 1_024;
    let info = GpuInfo::AppleMetal {
      total_memory_bytes: total,
    };
    assert_eq!(aggregate_vram_bytes(&info), Some(total));
  }

  #[test]
  fn aggregate_vram_cpu_only_and_unknown_are_none() {
    assert!(aggregate_vram_bytes(&GpuInfo::CpuOnly).is_none());
    assert!(aggregate_vram_bytes(&GpuInfo::Unknown { devices: vec![] }).is_none());
  }

  #[test]
  fn aggregate_vram_multi_ignores_zero_byte_device() {
    // Regression: a Vulkan/RADV duplicate of a ROCm card can report 0
    // VRAM. It must not win the min() and collapse the host to 0 (which
    // made the init smoke probe report a false OOM on NVIDIA+AMD hosts).
    let info = GpuInfo::Multi {
      devices: vec![
        GpuDevice {
          name: "NVIDIA GeForce RTX 3080".into(),
          backend: "nvidia".into(),
          total_memory_bytes: 10 * 1_024 * 1_024 * 1_024,
          ..Default::default()
        },
        GpuDevice {
          name: "AMD Radeon AI PRO R9700".into(),
          backend: "amd".into(),
          total_memory_bytes: 32 * 1_024 * 1_024 * 1_024,
          ..Default::default()
        },
        GpuDevice {
          name: "AMD Radeon AI PRO R9700 (RADV GFX1201)".into(),
          backend: "unknown".into(),
          total_memory_bytes: 0,
          ..Default::default()
        },
      ],
    };
    assert_eq!(
      aggregate_vram_bytes(&info),
      Some(10 * 1_024 * 1_024 * 1_024)
    );
  }

  #[test]
  fn aggregate_vram_all_zero_is_none_not_some_zero() {
    // Every device reports 0 → None (CPU-only path), never Some(0).
    let info = GpuInfo::Multi {
      devices: vec![GpuDevice {
        name: "Vulkan0".into(),
        backend: "unknown".into(),
        total_memory_bytes: 0,
        ..Default::default()
      }],
    };
    assert_eq!(aggregate_vram_bytes(&info), None);
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
    } else if cfg!(target_os = "windows") {
      assert_eq!(detected, OsFamily::Windows);
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
