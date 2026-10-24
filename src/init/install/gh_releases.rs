//! `ggml-org/llama.cpp` GitHub Releases install path.
//!
//! SHA-256 lives in the API JSON `digest` field (`sha256:<hex>`). No
//! discrete sidecar files; no body-text parsing.
//!
//! Variant table:
//!
//! | Host | Asset name suffix |
//! |---|---|
//! | linux x86_64 cpu | `ubuntu-x64.tar.gz` |
//! | linux nvidia, driver >= 580 | `ubuntu-cuda-13.*-<arch>.tar.gz` + `cudart-` bundle |
//! | linux x86_64 nvidia, driver >= 525 | `ubuntu-cuda-12.*-x64.tar.gz` + `cudart-` bundle |
//! | linux x86_64 vulkan / older nvidia | `ubuntu-vulkan-x64.tar.gz` |
//! | linux x86_64 amd | `ubuntu-rocm-<ver>-x64.tar.gz` |
//! | linux arm64 cpu | `ubuntu-arm64.tar.gz` |
//! | linux arm64 vulkan | `ubuntu-vulkan-arm64.tar.gz` |
//! | macos arm64 (metal default) | `macos-arm64.tar.gz` |
//! | macos x86_64 | `macos-x64.tar.gz` |
//!
//! Window assets exist but are out of v2 scope per the plan's Scope
//! Boundaries.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::gpu::GpuInfo;
use crate::init::detection::{CpuArch, HardwareSnapshot, OsFamily};
use crate::init::fetch::{FetchClient, FetchError};

use super::safe_extract::{
  existing_install, safe_extract, safe_extract_libs_tar_gz, ExtractLimits,
};
use super::{sha256_file, BinaryInstall, GhBuild, InstallError};
use crate::init::snapshot::InstallMethod;

/// Endpoint the wizard hits to discover the latest asset list. Pinned
/// in source so a hostile env can't redirect us off-org. `per_page=10`
/// lets us walk back when the latest release ships an incomplete asset
/// matrix (observed in `b9352`, which dropped `ubuntu-x64.tar.gz`).
const RELEASES_URL: &str = "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10";

/// API response body's max size cap. The 10-release JSON payload is
/// ~600 KB; 2 MB is generous headroom while still capping a hostile
/// mirror.
const RELEASES_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Per-asset body cap (2 GiB). Assets stream to disk, so this only stops
/// a hostile mirror from sending an unbounded body. The largest asset
/// fetched is the Linux CUDA 12.8 runtime bundle, 594 MB at `b11316`.
const ASSET_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Free-space multiplier per byte of archive: the archive on disk plus
/// what it unpacks to. Measured at `b11804`: a CUDA build and its
/// `cudart-` bundle together need 2.4x, the ROCm builds 5.9x (a 1.09 GiB
/// `libggml-hip.so` inside a 250 MB archive). The flat 3x under-called
/// ROCm, so the precheck passed at 750 MB and extraction then needed
/// 1.4 GiB.
const DISK_SLACK_DEFAULT: u64 = 3;

/// Same, for the ROCm family, which ships one very large HIP blob.
const DISK_SLACK_ROCM: u64 = 7;

/// Free space to hold while downloading and extracting `asset`.
fn disk_need_bytes(asset: &Download) -> u64 {
  let slack = if asset.asset_name.contains("-rocm-") {
    DISK_SLACK_ROCM
  } else {
    DISK_SLACK_DEFAULT
  };
  asset.size.saturating_mul(slack)
}

/// Budget for the first `--list-devices` after a CUDA install, which
/// loads about 1 GB of CUDA libraries from a cold disk.
const CUDA_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Deserialize)]
struct ReleaseRow {
  tag_name: String,
  assets: Vec<AssetRow>,
}

#[derive(Debug, Deserialize, Clone)]
struct AssetRow {
  name: String,
  browser_download_url: String,
  /// `sha256:<hex>`. Optional in the schema; required at use time.
  digest: Option<String>,
  /// Bytes. Required at use time like `digest`: the free-space check
  /// sizes itself from it, and a missing size would pass a check the
  /// extract then fails mid-way.
  size: Option<u64>,
}

/// Pick the (platform, variant, arch) suffix the host wants. Returns
/// `None` for hardware combinations v2 doesn't route to GH Releases
/// (e.g. macOS arm64 — that route goes through brew by default).
pub fn pick_asset_suffix(hw: &HardwareSnapshot) -> Option<String> {
  let arch_suffix = match hw.cpu_arch {
    CpuArch::X86_64 => "x64",
    CpuArch::Arm64 => "arm64",
    CpuArch::Other => return None,
  };
  match (&hw.gpu, hw.os) {
    (_, OsFamily::Other) => None,
    (GpuInfo::AppleMetal { .. }, OsFamily::MacOs) => Some(format!("macos-{arch_suffix}.tar.gz")),
    (_, OsFamily::MacOs) => Some(format!("macos-{arch_suffix}.tar.gz")),
    (GpuInfo::Amd { .. }, OsFamily::Linux) => {
      // ROCm version baked into the asset name (e.g. `rocm-7.2-x64`).
      // We accept any version suffix at match time.
      Some(format!("ubuntu-rocm-*-{arch_suffix}.tar.gz"))
    }
    (GpuInfo::Nvidia { .. } | GpuInfo::Unknown { .. }, OsFamily::Linux) => {
      Some(format!("ubuntu-vulkan-{arch_suffix}.tar.gz"))
    }
    (GpuInfo::CpuOnly, OsFamily::Linux) => Some(format!("ubuntu-{arch_suffix}.tar.gz")),
    // Windows asset naming: `llama-bXXXX-bin-win-<accel>-x64.zip`.
    // AMD on Windows is detected via DXGI (no `rocm-smi.exe` ships), so
    // we can't tell whether the card is in ROCm's narrow Windows-support
    // set (RDNA2/3 only). The HIP build (`win-hip-radeon`) faults during
    // runtime init on unsupported GPUs — RDNA1 (RX 5700 XT, gfx1010) and
    // older — crashing `llama-server` with 0xC0000005 before it prints a
    // line. Vulkan runs on every AMD GPU llama.cpp targets, so it's the
    // correct universal pick here. (Linux AMD keeps ROCm above: there
    // `rocm-smi` only succeeds when a supported ROCm stack is actually
    // installed, so the build matches the hardware.) CUDA wants a `-X.Y`
    // version suffix which the existing `*` glob handles.
    (GpuInfo::Amd { .. }, OsFamily::Windows) => Some(format!("win-vulkan-{arch_suffix}.zip")),
    (GpuInfo::Nvidia { .. }, OsFamily::Windows) => Some(format!("win-cuda-*-{arch_suffix}.zip")),
    (GpuInfo::Unknown { .. }, OsFamily::Windows) => Some(format!("win-vulkan-{arch_suffix}.zip")),
    (GpuInfo::CpuOnly, OsFamily::Windows) => Some(format!("win-cpu-{arch_suffix}.zip")),
    // Multi-GPU hosts — and, crucially, single cards that both the DXGI
    // and Vulkan probes detect: `gpu::probe` counts devices before its
    // cross-probe dedup, so one physical GPU seen twice tips `total` past
    // the single-device fast-paths and surfaces as `Multi`. That makes
    // this arm the common case on any Windows box whose driver ships
    // `vulkaninfo.exe`, not just true multi-card rigs. Pick the build
    // that covers the strongest device present: CUDA when any NVIDIA card
    // is in the set, else Vulkan, which runs on every GPU llama.cpp
    // targets. (Linux CUDA is chosen by [`cuda_asset_suffix`], which
    // also needs the driver version.) (macOS `Multi` already resolves via the `OsFamily::MacOs`
    // catch-all above; `OsFamily::Other` via the top arm.)
    (GpuInfo::Multi { devices }, OsFamily::Windows) => {
      if devices.iter().any(|d| d.backend == "nvidia") {
        Some(format!("win-cuda-*-{arch_suffix}.zip"))
      } else {
        Some(format!("win-vulkan-{arch_suffix}.zip"))
      }
    }
    (GpuInfo::Multi { .. }, OsFamily::Linux) => Some(format!("ubuntu-vulkan-{arch_suffix}.tar.gz")),
    _ => None,
  }
}

/// What the CUDA build choice needs to know about the NVIDIA cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvidiaGpu {
  pub driver_major: u32,
  /// Lowest compute capability across the cards, `(major, minor)`.
  /// `None` when `nvidia-smi` did not report it.
  pub compute_cap: Option<(u32, u32)>,
}

/// Upstream's CUDA 13 builds target compute 7.5 (Turing) and newer: the
/// default arch list in `ggml/src/ggml-cuda/CMakeLists.txt` adds 5.0,
/// 6.1 and 7.0 only below CUDA 13. On an older card the CUDA 13 build
/// still lists the device, then fails on the first kernel launch.
const CUDA13_MIN_COMPUTE_CAP: (u32, u32) = (7, 5);

/// The Linux CUDA build this host's NVIDIA driver and cards can run, or
/// `None` when there is no NVIDIA card on Linux, an AMD card sits beside
/// it, or the driver is too old or unknown.
///
/// CUDA 13.x needs driver 580 or newer; 12.x runs on 525 or newer
/// (NVIDIA's CUDA toolkit release notes, minor version compatibility
/// table). Upstream ships 12.x for x64 only, so an x64 card whose compute
/// capability is older than 7.5, or unknown, gets 12.x. The minor is a
/// glob so a toolkit bump upstream (13.4 → 13.5) still matches.
pub fn cuda_asset_suffix(hw: &HardwareSnapshot, gpu: Option<NvidiaGpu>) -> Option<String> {
  if !wants_cuda(hw) {
    return None;
  }
  let gpu = gpu?;
  let cc = gpu.compute_cap;
  match (hw.cpu_arch, gpu.driver_major) {
    (CpuArch::X86_64, 580..) if cc.is_some_and(|c| c >= CUDA13_MIN_COMPUTE_CAP) => {
      Some("ubuntu-cuda-13.*-x64.tar.gz".into())
    }
    (CpuArch::X86_64, 525..) => Some("ubuntu-cuda-12.*-x64.tar.gz".into()),
    (CpuArch::Arm64, 580..) if cc.is_none_or(|c| c >= CUDA13_MIN_COMPUTE_CAP) => {
      Some("ubuntu-cuda-13.*-arm64.tar.gz".into())
    }
    _ => None,
  }
}

/// Linux with an NVIDIA card and no AMD card. A CUDA build drives only
/// the NVIDIA card, so a mixed NVIDIA + AMD host keeps the Vulkan build,
/// which covers both.
fn wants_cuda(hw: &HardwareSnapshot) -> bool {
  if hw.os != OsFamily::Linux {
    return false;
  }
  match &hw.gpu {
    GpuInfo::Nvidia { .. } => true,
    GpuInfo::Multi { devices } => {
      devices.iter().any(|d| d.backend == "nvidia") && !devices.iter().any(|d| d.backend == "amd")
    }
    _ => false,
  }
}

/// Short label for a CUDA suffix, for the install picker: `CUDA 13` on
/// Linux, `CUDA` on Windows, whose suffix leaves the version open.
pub fn cuda_label(suffix: &str) -> Option<String> {
  if let Some(rest) = suffix.strip_prefix("ubuntu-cuda-") {
    let major = rest.split('.').next()?;
    return Some(format!("CUDA {major}"));
  }
  suffix.starts_with("win-cuda-").then(|| "CUDA".to_string())
}

/// The suffix to fetch for `build`: the CUDA build when asked for the
/// best build and the host can run one, else [`pick_asset_suffix`].
/// `Vulkan` swaps the Windows CUDA zip for the Vulkan one too: that zip
/// carries no CUDA runtime DLLs, so it is the build a failed CUDA check
/// falls back from.
pub fn select_asset_suffix(
  hw: &HardwareSnapshot,
  build: GhBuild,
  gpu: Option<NvidiaGpu>,
) -> Option<String> {
  match build {
    GhBuild::Best => cuda_asset_suffix(hw, gpu).or_else(|| pick_asset_suffix(hw)),
    GhBuild::Vulkan => pick_asset_suffix(hw).map(|s| match s.strip_prefix("win-cuda-*-") {
      Some(arch) => format!("win-vulkan-{arch}"),
      None => s,
    }),
  }
}

/// The NVIDIA driver and cards, read only on a host that could take a
/// CUDA build. `nvidia-smi` gives the driver version and each card's
/// compute capability; without it the driver version comes from
/// `/proc/driver/nvidia/version`. Blocks for up to 5 s.
pub fn nvidia_gpu(hw: &HardwareSnapshot) -> Option<NvidiaGpu> {
  if !wants_cuda(hw) {
    return None;
  }
  let mut cmd = std::process::Command::new("nvidia-smi");
  cmd.args([
    "--query-gpu=driver_version,compute_cap",
    "--format=csv,noheader",
  ]);
  let smi =
    crate::util::process::run_with_drain_and_timeout(cmd, std::time::Duration::from_secs(5))
      .ok()
      .filter(|out| out.status.success())
      .and_then(|out| parse_smi(&String::from_utf8_lossy(&out.stdout)));
  smi.or_else(|| {
    let version = std::fs::read_to_string("/proc/driver/nvidia/version").ok()?;
    Some(NvidiaGpu {
      driver_major: parse_driver_major(&version)?,
      compute_cap: None,
    })
  })
}

/// `nvidia-smi --query-gpu=driver_version,compute_cap` output, one
/// `580.82.07, 8.6` line per card. The compute capability is the lowest
/// card's, and `None` when any card's is unreadable.
fn parse_smi(text: &str) -> Option<NvidiaGpu> {
  let mut lines = text
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty())
    .peekable();
  let driver_major = parse_driver_major(lines.peek()?.split(',').next()?)?;
  let compute_cap = lines
    .map(|l| {
      let (major, minor) = l.split(',').nth(1)?.trim().split_once('.')?;
      Some((major.parse().ok()?, minor.parse().ok()?))
    })
    .collect::<Option<Vec<(u32, u32)>>>()
    .and_then(|caps| caps.into_iter().min());
  Some(NvidiaGpu {
    driver_major,
    compute_cap,
  })
}

/// First `<major>.<minor>[.<patch>]` token in `text`, as its major.
/// Covers both `/proc/driver/nvidia/version` (`NVRM version: NVIDIA
/// UNIX Open Kernel Module for x86_64  580.82.07  Release Build ...`)
/// and `nvidia-smi` output (`580.82.07`).
fn parse_driver_major(text: &str) -> Option<u32> {
  text.split_whitespace().find_map(|tok| {
    let mut parts = tok.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let numeric = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    (numeric(major) && numeric(minor) && parts.all(numeric))
      .then(|| major.parse().ok())
      .flatten()
  })
}

/// Determine whether `asset_name` matches `suffix`. `suffix` may
/// contain a single `*` glob (used for ROCm version drift).
///
/// Only assets whose names start with `llama-` are considered. This
/// excludes supplementary packages shipped alongside the main binary
/// (e.g. `cudart-llama-bin-win-cuda-*-x64.zip`, which contains CUDA
/// runtime DLLs but no `llama-server.exe` and therefore shares the
/// Windows CUDA suffix pattern without being the install target).
pub fn asset_matches(asset_name: &str, suffix: &str) -> bool {
  let lower_name = asset_name.to_ascii_lowercase();
  if !lower_name.starts_with("llama-") {
    return false;
  }
  let lower_suffix = suffix.to_ascii_lowercase();
  if let Some((head, tail)) = lower_suffix.split_once('*') {
    return lower_name.ends_with(&tail) && lower_name.contains(head);
  }
  lower_name.ends_with(&lower_suffix)
}

/// One row of the asset list relevant to the host's hardware.
#[derive(Debug, Clone)]
pub struct AssetPick {
  pub tag: String,
  /// The build archive.
  pub archive: Download,
  /// Shared libraries the build needs beside it: the `cudart-` bundle
  /// (`libcudart`, `libcublas`, `libcublasLt`) for a Linux CUDA build.
  /// The binaries load them through an `$ORIGIN` rpath, so no CUDA
  /// toolkit is needed on the host.
  pub runtime_libs: Option<Download>,
}

/// A verified download: name, URL, expected SHA-256.
#[derive(Debug, Clone)]
pub struct Download {
  pub asset_name: String,
  pub url: String,
  pub sha256: String,
  pub size: u64,
}

impl AssetPick {
  /// A CUDA build, which needs its device list checked after install:
  /// the CUDA backend is a plugin that llama.cpp skips when it cannot
  /// load, so the binary still runs and `--version` still passes.
  pub fn is_cuda(&self) -> bool {
    self.archive.asset_name.contains("-cuda-")
  }

  /// Bytes to download: the build plus its runtime bundle.
  pub fn download_bytes(&self) -> u64 {
    self.archive.size + self.runtime_libs.as_ref().map_or(0, |l| l.size)
  }
}

/// A name from the release feed used as a path component. Anything
/// outside `[A-Za-z0-9._-]` (a `/`, a `..` component) is refused, so a
/// hostile feed cannot point an install outside the install root.
fn safe_component(name: &str) -> Result<&str, InstallError> {
  let ok = !name.is_empty()
    && name != "."
    && name != ".."
    && name
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
  if ok {
    Ok(name)
  } else {
    Err(InstallError::Integrity(format!(
      "release feed name `{name}` is not a safe path component"
    )))
  }
}

/// The name upstream gives a Linux build's runtime bundle:
/// `cudart-` + the build's own asset name.
fn runtime_libs_name(asset_name: &str) -> Option<String> {
  (asset_name.contains("-bin-ubuntu-cuda-")).then(|| format!("cudart-{asset_name}"))
}

fn size_of(asset: &AssetRow) -> Result<u64, InstallError> {
  asset.size.ok_or_else(|| {
    InstallError::Integrity(format!(
      "asset `{}` has no size field on the GH API response",
      asset.name
    ))
  })
}

fn sha256_of(asset: &AssetRow) -> Result<String, InstallError> {
  let digest = asset.digest.as_deref().ok_or_else(|| {
    InstallError::Integrity(format!(
      "asset `{}` has no digest field on the GH API response",
      asset.name
    ))
  })?;
  Ok(
    digest
      .strip_prefix("sha256:")
      .ok_or_else(|| InstallError::Integrity(format!("digest `{digest}` is not sha256:<hex>")))?
      .to_string(),
  )
}

/// Fetch the most recent releases and pick the newest one that has an
/// asset matching the host's variant suffix. Walking back through the
/// page covers the upstream-incomplete-release case (e.g. `b9352`
/// shipped without `ubuntu-x64.tar.gz`); only when no surveyed release
/// matches do we return `NoMatchingAsset`. The wizard layers
/// a user-visible fallback on top for the interactive flow.
/// Backoff between the first and second GH API attempt when the
/// initial call comes back rate-limited. 60 s is the practical floor
/// — GitHub's unauthenticated quota resets in 60-minute windows but
/// queue depth + retry-after headers tend to clear within a minute
/// of the first 429/403. A second failure surfaces immediately as
/// `InstallError::RateLimited` per R71 ("retry once, then fall back").
const RATE_LIMIT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn fetch_latest_asset(
  fetch: &FetchClient,
  hw: &HardwareSnapshot,
  build: GhBuild,
  gpu: Option<NvidiaGpu>,
) -> Result<AssetPick, InstallError> {
  let suffix = select_asset_suffix(hw, build, gpu).ok_or(InstallError::NoMatchingAsset {
    os: hw.os,
    arch: hw.cpu_arch,
  })?;
  // GH Releases API allows 60 unauthenticated requests/hour.
  // On the first 429/403 we sleep briefly and try again; a second
  // rate-limit response is surfaced as-is so the wizard can offer
  // the "point at existing binary" fallback. Any other error is
  // terminal on the first attempt.
  let releases: Vec<ReleaseRow> = match fetch.get_json(RELEASES_URL, RELEASES_MAX_BYTES).await {
    Ok(r) => r,
    Err(FetchError::RateLimited { status: first }) => {
      log::info!(
        "init server: GH Releases API rate-limited (status {first}); \
         retrying once in {}s",
        RATE_LIMIT_RETRY_DELAY.as_secs()
      );
      tokio::time::sleep(RATE_LIMIT_RETRY_DELAY).await;
      fetch
        .get_json(RELEASES_URL, RELEASES_MAX_BYTES)
        .await
        .map_err(translate_fetch)?
    }
    Err(e) => return Err(translate_fetch(e)),
  };
  if releases.is_empty() {
    return Err(InstallError::Fetch("empty releases list".into()));
  }
  let (tag, matched, runtime) =
    pick_release_with_asset(releases, &suffix).ok_or(InstallError::NoMatchingAsset {
      os: hw.os,
      arch: hw.cpu_arch,
    })?;
  Ok(AssetPick {
    tag,
    archive: Download::from_row(matched)?,
    runtime_libs: runtime.map(Download::from_row).transpose()?,
  })
}

impl Download {
  fn from_row(row: AssetRow) -> Result<Self, InstallError> {
    Ok(Self {
      sha256: sha256_of(&row)?,
      size: size_of(&row)?,
      asset_name: row.name,
      url: row.browser_download_url,
    })
  }
}

/// Walk the release list from newest to oldest and return the first
/// `(tag, asset, runtime_libs)` where some asset matches `suffix`, plus
/// its runtime bundle when the build needs one. Skipping a newer
/// release covers the upstream-incomplete-release case (e.g. llama.cpp
/// `b9352` dropped the Linux/Windows asset matrix on publish), and a
/// CUDA build whose `cudart-` bundle is missing is skipped the same
/// way. A clean rejection of every surveyed release is left for the
/// caller to map to `NoMatchingAsset` so the user sees a single
/// canonical error.
fn pick_release_with_asset(
  releases: Vec<ReleaseRow>,
  suffix: &str,
) -> Option<(String, AssetRow, Option<AssetRow>)> {
  let mut skipped: Vec<String> = Vec::new();
  for release in releases {
    let matched = release.assets.iter().find_map(|a| {
      if !asset_matches(&a.name, suffix) {
        return None;
      }
      match runtime_libs_name(&a.name) {
        Some(want) => release
          .assets
          .iter()
          .find(|r| r.name == want)
          .map(|r| (a.clone(), Some(r.clone()))),
        None => Some((a.clone(), None)),
      }
    });
    if let Some((asset, runtime)) = matched {
      if !skipped.is_empty() {
        log::info!(
          "init server: skipping {} newer llama.cpp release(s) without `{}` asset ({}); using {}",
          skipped.len(),
          suffix,
          skipped.join(", "),
          release.tag_name,
        );
      }
      return Some((release.tag_name, asset, runtime));
    }
    skipped.push(release.tag_name);
  }
  None
}

/// Download + verify + safe-extract the picked asset. Returns the
/// resolved binary path + recorded digest the wizard stamps into
/// `_init_snapshot`.
///
/// Assets stream into a temp file under `install_root` (not `/tmp`,
/// which is often RAM-backed) and are hashed on the way, so a CUDA build
/// and its runtime bundle never sit in memory. A build already extracted
/// by an earlier run is reused without a download.
pub async fn install_picked(
  fetch: &FetchClient,
  pick: &AssetPick,
  install_root: &Path,
) -> Result<BinaryInstall, InstallError> {
  std::fs::create_dir_all(install_root).map_err(|e| InstallError::Io(e.to_string()))?;
  remove_stale_temps(install_root, |n| {
    n.starts_with(".download.") || n.contains(".tmp.")
  });
  let dir_name = install_dir_name(pick)?;
  let existing = existing_install(install_root, &dir_name)?;
  let libs = match (&pick.runtime_libs, &existing) {
    (Some(libs), Some(found)) if libs_marker(&found.path, libs)?.exists() => None,
    (libs, _) => libs.as_ref(),
  };
  let mut need = if existing.is_none() {
    disk_need_bytes(&pick.archive)
  } else {
    0
  };
  if let Some(libs) = libs {
    need = need.saturating_add(disk_need_bytes(libs));
  }
  crate::init::download::precheck_disk(install_root, need)
    .map_err(|e| InstallError::Io(e.to_string()))?;
  let extracted = match existing {
    Some(found) => found,
    None => {
      let archive = download_verified(fetch, &pick.archive, install_root).await?;
      safe_extract(
        &pick.archive.asset_name,
        archive.path(),
        install_root,
        &dir_name,
        ExtractLimits::for_asset(Some(pick.archive.size)),
      )?
    }
  };
  if let Some(libs) = libs {
    let dir = bin_dir(&extracted.path)?;
    remove_stale_temps(dir, |n| n.starts_with(".lib") && n.contains(".tmp."));
    let lib_archive = download_verified(fetch, libs, install_root).await?;
    let file =
      std::fs::File::open(lib_archive.path()).map_err(|e| InstallError::Io(e.to_string()))?;
    safe_extract_libs_tar_gz(
      std::io::BufReader::new(file),
      dir,
      ExtractLimits::for_asset(Some(libs.size)),
    )?;
    // Written last, so an interrupted extract is redone on the next run.
    std::fs::write(libs_marker(&extracted.path, libs)?, b"")
      .map_err(|e| InstallError::Io(e.to_string()))?;
  }
  let digest = sha256_file(&extracted.path)?;
  Ok(BinaryInstall {
    method: InstallMethod::GhReleases,
    path: extracted.path,
    digest,
    version: Some(pick.tag.clone()),
  })
}

fn bin_dir(binary: &Path) -> Result<&Path, InstallError> {
  binary
    .parent()
    .ok_or_else(|| InstallError::Io("installed binary has no parent dir".into()))
}

/// Marks a runtime bundle as fully extracted beside `binary`.
fn libs_marker(binary: &Path, libs: &Download) -> Result<PathBuf, InstallError> {
  Ok(bin_dir(binary)?.join(format!(".{}.installed", safe_component(&libs.asset_name)?)))
}

/// A temp entry untouched this long is left from a run that was killed
/// (`init` has no Ctrl-C handler, so its temp files are not cleaned up).
/// A live run writes to its own temp entries far more often.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Best-effort removal of temp files and dirs in `dir` that an
/// interrupted install left behind.
fn remove_stale_temps(dir: &Path, is_temp: impl Fn(&str) -> bool) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let name = entry.file_name();
    if !name.to_str().is_some_and(&is_temp) {
      continue;
    }
    let Ok(meta) = entry.metadata() else {
      continue;
    };
    let stale = meta
      .modified()
      .ok()
      .and_then(|m| m.elapsed().ok())
      .is_some_and(|age| age >= STALE_TEMP_AGE);
    if !stale {
      continue;
    }
    let path = entry.path();
    let removed = if meta.is_dir() {
      std::fs::remove_dir_all(&path)
    } else {
      std::fs::remove_file(&path)
    };
    match removed {
      Ok(()) => log::info!(
        "init server: removed {} left by an interrupted install",
        path.display()
      ),
      Err(e) => log::warn!("init server: could not remove {}: {e}", path.display()),
    }
  }
}

/// Stream `download` into a temp file under `dir` and check its SHA-256.
/// The file is deleted when the returned handle drops.
async fn download_verified(
  fetch: &FetchClient,
  download: &Download,
  dir: &Path,
) -> Result<tempfile::NamedTempFile, InstallError> {
  let mut tmp = tempfile::Builder::new()
    .prefix(".download.")
    .tempfile_in(dir)
    .map_err(|e| InstallError::Io(e.to_string()))?;
  let actual = fetch
    .download_to(&download.url, ASSET_MAX_BYTES, tmp.as_file_mut())
    .await
    .map_err(translate_fetch)?;
  if actual != download.sha256 {
    return Err(InstallError::ChecksumMismatch {
      expected: download.sha256.clone(),
      actual,
    });
  }
  Ok(tmp)
}

/// Directory under the install root for this build. A CUDA build gets
/// its own (`b11302-cuda-13.4-x64`) so a fallback to the Vulkan build of
/// the same tag does not find the CUDA one already in `b11302/`.
fn install_dir_name(pick: &AssetPick) -> Result<String, InstallError> {
  let tag = safe_component(&pick.tag)?;
  let name = &pick.archive.asset_name;
  let variant = pick
    .is_cuda()
    .then(|| {
      let rest = name
        .split_once("-bin-ubuntu-")
        .or_else(|| name.split_once("-bin-win-"))?
        .1;
      rest
        .strip_suffix(".tar.gz")
        .or_else(|| rest.strip_suffix(".zip"))
    })
    .flatten();
  match variant {
    Some(v) => Ok(format!("{tag}-{}", safe_component(v)?)),
    None => Ok(tag.to_string()),
  }
}

/// Whether a freshly installed CUDA build actually loaded its CUDA
/// backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CudaCheck {
  /// `--list-devices` lists a `CUDA<n>` device.
  Loaded,
  /// `--list-devices` ran cleanly and listed no CUDA device.
  NotLoaded,
  /// The probe itself failed (spawn error, timeout, non-zero exit), so
  /// it says nothing about CUDA.
  Unknown(String),
}

/// Run `binary --list-devices` and look for a CUDA device. llama.cpp
/// builds its CUDA backend as a plugin and skips it when it cannot load
/// (no driver, missing runtime libraries), so the binary still runs and
/// passes `--version`; only the device list tells.
pub fn check_cuda_device(binary: &Path) -> CudaCheck {
  let mut cmd = std::process::Command::new(binary);
  cmd.arg("--list-devices");
  match crate::util::process::run_with_drain_and_timeout(cmd, CUDA_CHECK_TIMEOUT) {
    Ok(out) if out.status.success() => {
      if lists_cuda_device(&String::from_utf8_lossy(&out.stdout)) {
        CudaCheck::Loaded
      } else {
        CudaCheck::NotLoaded
      }
    }
    Ok(out) => CudaCheck::Unknown(format!("`--list-devices` exited with {}", out.status)),
    Err(e) => CudaCheck::Unknown(format!("`--list-devices` failed: {e:?}")),
  }
}

/// A `CUDA<n>: <name>` line in `--list-devices` output.
fn lists_cuda_device(stdout: &str) -> bool {
  stdout.lines().any(|line| {
    line
      .trim()
      .split_once(':')
      .and_then(|(sel, _)| sel.strip_prefix("CUDA"))
      .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
  })
}

/// Lists the CUDA builds whose device check failed on this host, one
/// asset suffix per line, so later runs default to the Vulkan build
/// instead of downloading the CUDA one again.
const CUDA_FAILED_FILE: &str = ".cuda-failed";

pub fn cuda_failed_path(install_root: &Path) -> PathBuf {
  install_root.join(CUDA_FAILED_FILE)
}

/// The CUDA suffix the best build resolves to on this host, if any.
pub fn best_cuda_suffix(hw: &HardwareSnapshot, gpu: Option<NvidiaGpu>) -> Option<String> {
  select_asset_suffix(hw, GhBuild::Best, gpu).filter(|s| s.contains("-cuda-"))
}

/// Whether the CUDA build the best build resolves to failed its device
/// check on an earlier run.
pub fn cuda_failed_before(
  install_root: &Path,
  hw: &HardwareSnapshot,
  gpu: Option<NvidiaGpu>,
) -> bool {
  let Some(suffix) = best_cuda_suffix(hw, gpu) else {
    return false;
  };
  std::fs::read_to_string(cuda_failed_path(install_root))
    .is_ok_and(|s| s.lines().any(|l| l.trim() == suffix))
}

/// Record that the CUDA build the best build resolves to listed no CUDA
/// device.
pub fn record_cuda_failure(
  install_root: &Path,
  hw: &HardwareSnapshot,
  gpu: Option<NvidiaGpu>,
) -> std::io::Result<()> {
  let Some(suffix) = best_cuda_suffix(hw, gpu) else {
    return Ok(());
  };
  if cuda_failed_before(install_root, hw, gpu) {
    return Ok(());
  }
  use std::io::Write as _;
  let mut file = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(cuda_failed_path(install_root))?;
  writeln!(file, "{suffix}")
}

/// Forget a recorded failure once the CUDA build lists a device.
pub fn clear_cuda_failure(
  install_root: &Path,
  hw: &HardwareSnapshot,
  gpu: Option<NvidiaGpu>,
) -> std::io::Result<()> {
  let Some(suffix) = best_cuda_suffix(hw, gpu) else {
    return Ok(());
  };
  let path = cuda_failed_path(install_root);
  let Ok(text) = std::fs::read_to_string(&path) else {
    return Ok(());
  };
  let rest: Vec<&str> = text.lines().filter(|l| l.trim() != suffix).collect();
  if rest.is_empty() {
    std::fs::remove_file(&path)
  } else {
    std::fs::write(&path, rest.join("\n") + "\n")
  }
}

/// Remove the install directory that holds `binary`, the child of
/// `install_root` it sits under. Used when a CUDA build is replaced by
/// the Vulkan one, so its ~1 GB does not stay behind.
pub fn remove_install(install_root: &Path, binary: &Path) -> Result<(), InstallError> {
  let dir = binary
    .ancestors()
    .find(|a| a.parent() == Some(install_root))
    .ok_or_else(|| {
      InstallError::Io(format!(
        "{} is not under {}",
        binary.display(),
        install_root.display()
      ))
    })?;
  std::fs::remove_dir_all(dir)
    .map_err(|e| InstallError::Io(format!("remove {}: {e}", dir.display())))
}

fn translate_fetch(e: FetchError) -> InstallError {
  match e {
    FetchError::RateLimited { status } => InstallError::RateLimited { status },
    FetchError::Offline => InstallError::Fetch("offline mode (LLAMASTASH_OFFLINE)".into()),
    other => InstallError::Fetch(other.to_string()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::GpuDevice;

  fn hw(gpu: GpuInfo, os: OsFamily, arch: CpuArch) -> HardwareSnapshot {
    HardwareSnapshot {
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: 0,
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
      gpu,
      os,
      cpu_arch: arch,
    }
  }

  fn nvidia() -> GpuInfo {
    GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "test".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    }
  }

  fn amd() -> GpuInfo {
    GpuInfo::Amd {
      devices: vec![GpuDevice {
        name: "test".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    }
  }

  #[test]
  fn linux_nvidia_picks_cuda_by_driver_and_vulkan_otherwise() {
    let x64 = hw(nvidia(), OsFamily::Linux, CpuArch::X86_64);
    let arm = hw(nvidia(), OsFamily::Linux, CpuArch::Arm64);
    let best = |h: &HardwareSnapshot, d: Option<u32>| {
      select_asset_suffix(h, GhBuild::Best, d.map(ampere)).unwrap()
    };
    assert_eq!(best(&x64, Some(580)), "ubuntu-cuda-13.*-x64.tar.gz");
    assert_eq!(best(&x64, Some(570)), "ubuntu-cuda-12.*-x64.tar.gz");
    assert_eq!(best(&x64, Some(525)), "ubuntu-cuda-12.*-x64.tar.gz");
    assert_eq!(best(&x64, Some(520)), "ubuntu-vulkan-x64.tar.gz");
    assert_eq!(best(&x64, None), "ubuntu-vulkan-x64.tar.gz");
    // Upstream ships no CUDA 12 build for arm64.
    assert_eq!(best(&arm, Some(590)), "ubuntu-cuda-13.*-arm64.tar.gz");
    assert_eq!(best(&arm, Some(570)), "ubuntu-vulkan-arm64.tar.gz");
    assert_eq!(
      select_asset_suffix(&x64, GhBuild::Vulkan, Some(ampere(580))).unwrap(),
      "ubuntu-vulkan-x64.tar.gz"
    );
  }

  fn ampere(driver_major: u32) -> NvidiaGpu {
    NvidiaGpu {
      driver_major,
      compute_cap: Some((8, 6)),
    }
  }

  #[test]
  fn a_card_older_than_turing_gets_cuda_12_or_vulkan() {
    let x64 = hw(nvidia(), OsFamily::Linux, CpuArch::X86_64);
    let arm = hw(nvidia(), OsFamily::Linux, CpuArch::Arm64);
    let gpu = |cc| NvidiaGpu {
      driver_major: 580,
      compute_cap: cc,
    };
    let pascal = gpu(Some((6, 1)));
    assert_eq!(
      cuda_asset_suffix(&x64, Some(pascal)).as_deref(),
      Some("ubuntu-cuda-12.*-x64.tar.gz")
    );
    assert_eq!(cuda_asset_suffix(&arm, Some(pascal)), None);
    assert_eq!(
      cuda_asset_suffix(&x64, Some(gpu(Some((7, 5))))).as_deref(),
      Some("ubuntu-cuda-13.*-x64.tar.gz")
    );
    // Unknown: CUDA 12 runs on every card x64 drivers 525+ support.
    assert_eq!(
      cuda_asset_suffix(&x64, Some(gpu(None))).as_deref(),
      Some("ubuntu-cuda-12.*-x64.tar.gz")
    );
    assert_eq!(
      cuda_asset_suffix(&arm, Some(gpu(None))).as_deref(),
      Some("ubuntu-cuda-13.*-arm64.tar.gz")
    );
  }

  #[test]
  fn nvidia_smi_output_gives_the_driver_and_the_lowest_compute_cap() {
    assert_eq!(
      parse_smi("580.82.07, 8.9\n580.82.07, 6.1\n"),
      Some(NvidiaGpu {
        driver_major: 580,
        compute_cap: Some((6, 1)),
      })
    );
    assert_eq!(
      parse_smi("575.57.08, [N/A]\n").map(|g| (g.driver_major, g.compute_cap)),
      Some((575, None))
    );
    assert_eq!(parse_smi(""), None);
  }

  #[test]
  fn a_failed_cuda_build_is_remembered_per_suffix() {
    let root = crate::util::test_temp::unique_temp_dir("cuda-failed");
    let x64 = hw(nvidia(), OsFamily::Linux, CpuArch::X86_64);
    let cuda13 = Some(ampere(580));
    assert!(!cuda_failed_before(&root, &x64, cuda13));
    record_cuda_failure(&root, &x64, cuda13).unwrap();
    record_cuda_failure(&root, &x64, cuda13).unwrap();
    assert!(cuda_failed_before(&root, &x64, cuda13));
    assert_eq!(
      std::fs::read_to_string(cuda_failed_path(&root)).unwrap(),
      "ubuntu-cuda-13.*-x64.tar.gz\n",
      "recorded once"
    );
    // A driver that moves the pick to another CUDA build tries it.
    assert!(!cuda_failed_before(&root, &x64, Some(ampere(570))));
    // Nothing is recorded where the best build is not CUDA.
    let amd = hw(amd(), OsFamily::Linux, CpuArch::X86_64);
    record_cuda_failure(&root, &amd, None).unwrap();
    assert!(!cuda_failed_before(&root, &amd, None));
    let win = hw(nvidia(), OsFamily::Windows, CpuArch::X86_64);
    assert!(!cuda_failed_before(&root, &win, None));
    record_cuda_failure(&root, &win, None).unwrap();
    assert!(cuda_failed_before(&root, &win, None));
    // A CUDA build that later lists a device clears its line only.
    clear_cuda_failure(&root, &win, None).unwrap();
    assert!(!cuda_failed_before(&root, &win, None));
    assert!(cuda_failed_before(&root, &x64, cuda13));
    clear_cuda_failure(&root, &x64, cuda13).unwrap();
    assert!(!cuda_failed_path(&root).exists());
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn the_vulkan_build_replaces_the_windows_cuda_zip() {
    let win = hw(nvidia(), OsFamily::Windows, CpuArch::X86_64);
    assert_eq!(
      select_asset_suffix(&win, GhBuild::Best, None).as_deref(),
      Some("win-cuda-*-x64.zip")
    );
    assert_eq!(
      select_asset_suffix(&win, GhBuild::Vulkan, None).as_deref(),
      Some("win-vulkan-x64.zip")
    );
    let multi = GpuInfo::Multi {
      devices: vec![dev("nvidia"), dev("unknown")],
    };
    assert_eq!(
      select_asset_suffix(
        &hw(multi, OsFamily::Windows, CpuArch::X86_64),
        GhBuild::Vulkan,
        None
      )
      .as_deref(),
      Some("win-vulkan-x64.zip")
    );
    // Hosts whose pick is not CUDA keep it.
    assert_eq!(
      select_asset_suffix(
        &hw(amd(), OsFamily::Linux, CpuArch::X86_64),
        GhBuild::Vulkan,
        None
      )
      .as_deref(),
      Some("ubuntu-rocm-*-x64.tar.gz")
    );
  }

  #[test]
  fn cuda_is_only_offered_for_nvidia_on_linux() {
    let g = Some(ampere(580));
    assert!(cuda_asset_suffix(&hw(amd(), OsFamily::Linux, CpuArch::X86_64), g).is_none());
    assert!(cuda_asset_suffix(&hw(nvidia(), OsFamily::Windows, CpuArch::X86_64), g).is_none());
    let multi = GpuInfo::Multi {
      devices: vec![dev("nvidia"), dev("unknown")],
    };
    assert_eq!(
      cuda_asset_suffix(&hw(multi, OsFamily::Linux, CpuArch::X86_64), g).as_deref(),
      Some("ubuntu-cuda-13.*-x64.tar.gz")
    );
    // A CUDA build can't drive the AMD card; Vulkan covers both.
    let mixed = hw(
      GpuInfo::Multi {
        devices: vec![dev("nvidia"), dev("amd")],
      },
      OsFamily::Linux,
      CpuArch::X86_64,
    );
    assert!(cuda_asset_suffix(&mixed, g).is_none());
    assert_eq!(
      select_asset_suffix(&mixed, GhBuild::Best, g).as_deref(),
      Some("ubuntu-vulkan-x64.tar.gz")
    );
    // The driver is not even read where CUDA can't be picked.
    assert_eq!(nvidia_gpu(&mixed), None);
    assert_eq!(
      nvidia_gpu(&hw(amd(), OsFamily::Linux, CpuArch::X86_64)),
      None
    );
  }

  #[test]
  fn cuda_label_names_the_major() {
    assert_eq!(
      cuda_label("ubuntu-cuda-13.*-x64.tar.gz").as_deref(),
      Some("CUDA 13")
    );
    assert_eq!(cuda_label("win-cuda-*-x64.zip").as_deref(), Some("CUDA"));
    assert!(cuda_label("ubuntu-vulkan-x64.tar.gz").is_none());
  }

  #[test]
  fn driver_major_parses_proc_and_nvidia_smi_output() {
    assert_eq!(
      parse_driver_major(
        "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  580.82.07  Release Build  (dvs-builder@U16)  Fri Aug 22 2025\nGCC version:  gcc version 14.2.1 20250207 (GCC)\n"
      ),
      Some(580)
    );
    assert_eq!(
      parse_driver_major(
        "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14  Thu Feb 22 01:44:30 UTC 2024"
      ),
      Some(550)
    );
    assert_eq!(parse_driver_major("575.57.08\n"), Some(575));
    assert_eq!(parse_driver_major("NVIDIA-SMI has failed"), None);
  }

  #[test]
  fn a_cuda_build_takes_its_runtime_bundle_and_is_skipped_without_one() {
    let releases = vec![
      // Newest release is missing the runtime bundle: fall back.
      release("b11303", &["llama-b11303-bin-ubuntu-cuda-13.4-x64.tar.gz"]),
      release(
        "b11302",
        &[
          "cudart-llama-b11302-bin-ubuntu-cuda-13.4-x64.tar.gz",
          "llama-b11302-bin-ubuntu-cuda-12.8-x64.tar.gz",
          "llama-b11302-bin-ubuntu-cuda-13.4-x64.tar.gz",
          "cudart-llama-b11302-bin-ubuntu-cuda-12.8-x64.tar.gz",
        ],
      ),
    ];
    let (tag, asset, runtime) =
      pick_release_with_asset(releases, "ubuntu-cuda-13.*-x64.tar.gz").unwrap();
    assert_eq!(tag, "b11302");
    assert_eq!(asset.name, "llama-b11302-bin-ubuntu-cuda-13.4-x64.tar.gz");
    assert_eq!(
      runtime.expect("runtime bundle").name,
      "cudart-llama-b11302-bin-ubuntu-cuda-13.4-x64.tar.gz"
    );
  }

  #[test]
  fn a_cuda_build_installs_apart_from_the_same_tags_vulkan_build() {
    let dir = |name: &str| install_dir_name(&pick("b11302", name)).expect("safe name");
    assert_eq!(
      dir("llama-b11302-bin-ubuntu-cuda-13.4-x64.tar.gz"),
      "b11302-cuda-13.4-x64"
    );
    assert_eq!(dir("llama-b11302-bin-ubuntu-vulkan-x64.tar.gz"), "b11302");
    assert_eq!(
      dir("llama-b11302-bin-win-cuda-12.4-x64.zip"),
      "b11302-cuda-12.4-x64"
    );
    assert_eq!(dir("llama-b11302-bin-win-vulkan-x64.zip"), "b11302");
  }

  fn pick(tag: &str, name: &str) -> AssetPick {
    AssetPick {
      tag: tag.into(),
      archive: Download {
        asset_name: name.into(),
        url: String::new(),
        sha256: String::new(),
        size: 0,
      },
      runtime_libs: None,
    }
  }

  fn download(name: &str, size: u64) -> Download {
    Download {
      asset_name: name.into(),
      url: String::new(),
      sha256: String::new(),
      size,
    }
  }

  /// Sizes and unpacked totals measured on the real `b11804` assets.
  /// ROCm packs one 1.09 GiB HIP blob, so a flat multiplier sized for
  /// CUDA under-books it and extraction fails after the download.
  #[test]
  fn disk_precheck_budgets_rocm_for_its_hip_blob() {
    let linux = download("llama-b11804-bin-ubuntu-rocm-10.0-x64.tar.gz", 249_908_117);
    // archive 249,908,117 + unpacked 1,229,319,446
    assert!(disk_need_bytes(&linux) >= 1_479_227_563);
    let windows = download("llama-b11804-bin-win-rocm-10.0-x64.zip", 263_465_134);
    // archive 263,465,134 + unpacked 1,217,472,285
    assert!(disk_need_bytes(&windows) >= 1_480_937_419);
  }

  /// The same precheck must not balloon for CUDA, or a disk that fits the
  /// install refuses it: a CUDA 12.8 build plus its bundle costs 1.86 GiB.
  #[test]
  fn disk_precheck_does_not_overbudget_a_cuda_install() {
    let main = download("llama-b11804-bin-ubuntu-cuda-12.8-x64.tar.gz", 177_931_030);
    let libs = download(
      "cudart-llama-b11804-bin-ubuntu-cuda-12.8-x64.tar.gz",
      594_377_688,
    );
    let need = disk_need_bytes(&main) + disk_need_bytes(&libs);
    assert!((1_862_402_516..2_500_000_000).contains(&need), "{need}");
  }

  #[test]
  fn a_feed_name_cannot_point_the_install_outside_its_root() {
    // Matches `ubuntu-cuda-13.*-x64.tar.gz`, so it would be picked.
    let hostile = "llama-b1-bin-ubuntu-cuda-13../../../../tmp/evil-x64.tar.gz";
    assert!(asset_matches(hostile, "ubuntu-cuda-13.*-x64.tar.gz"));
    assert!(install_dir_name(&pick("b1", hostile)).is_err());
    assert!(install_dir_name(&pick("../b1", "llama-b1-bin-ubuntu-x64.tar.gz")).is_err());
    assert!(install_dir_name(&pick("..", "llama-b1-bin-ubuntu-x64.tar.gz")).is_err());
    assert!(safe_component("cudart-llama-b11316-bin-ubuntu-cuda-12.8-x64.tar.gz").is_ok());
  }

  #[test]
  fn a_cuda_device_line_is_told_apart_from_none() {
    assert!(lists_cuda_device(
      "Available devices:\n  CUDA0: NVIDIA GeForce RTX 4090 (24080 MiB, 23700 MiB free)\n"
    ));
    // What the real b11302 CUDA build prints on a host without the driver.
    assert!(!lists_cuda_device("Available devices:\n  (none)\n"));
    assert!(!lists_cuda_device(
      "Available devices:\n  Vulkan0: AMD Radeon (RADV)\n"
    ));
    assert!(!lists_cuda_device("CUDA: not a device line\n"));
  }

  #[cfg(unix)]
  #[test]
  fn a_failed_probe_is_unknown_not_a_missing_device() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::util::test_temp::unique_temp_dir("cuda-check");
    let script = |name: &str, body: &str| {
      let p = dir.join(name);
      std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
      std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
      p
    };
    let none = script("none", "printf 'Available devices:\\n  (none)\\n'");
    let cuda = script(
      "cuda",
      "printf 'Available devices:\\n  CUDA0: GPU (100 MiB, 90 MiB free)\\n'",
    );
    let crash = script("crash", "exit 3");
    assert_eq!(check_cuda_device(&none), CudaCheck::NotLoaded);
    assert_eq!(check_cuda_device(&cuda), CudaCheck::Loaded);
    assert!(matches!(check_cuda_device(&crash), CudaCheck::Unknown(_)));
    assert!(matches!(
      check_cuda_device(&dir.join("missing")),
      CudaCheck::Unknown(_)
    ));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn remove_install_takes_only_the_dir_under_the_root() {
    let root = crate::util::test_temp::unique_temp_dir("remove-install");
    let bin = root
      .join("b1-cuda-13.4-x64")
      .join("llama-b1")
      .join("llama-server");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"bin").unwrap();
    std::fs::create_dir_all(root.join("b1")).unwrap();
    remove_install(&root, &bin).expect("remove");
    assert!(!root.join("b1-cuda-13.4-x64").exists());
    assert!(root.join("b1").exists(), "the Vulkan build stays");
    let elsewhere = crate::util::test_temp::unique_temp_dir("remove-elsewhere").join("x");
    assert!(remove_install(&root, &elsewhere).is_err());
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn an_asset_without_a_size_is_refused_not_sized_zero() {
    let mut a = asset("llama-b1-bin-ubuntu-x64.tar.gz");
    assert_eq!(size_of(&a).unwrap(), 1);
    a.size = None;
    assert!(matches!(size_of(&a), Err(InstallError::Integrity(_))));
    // The API row deserialises without it rather than defaulting to 0.
    let row: AssetRow = serde_json::from_str(
      r#"{"name":"llama-b1-bin-ubuntu-x64.tar.gz","browser_download_url":"https://x","digest":"sha256:0"}"#,
    )
    .unwrap();
    assert_eq!(row.size, None);
  }

  #[test]
  fn non_cuda_builds_take_no_runtime_bundle() {
    let releases = vec![release("b1", &["llama-b1-bin-ubuntu-vulkan-x64.tar.gz"])];
    let (_, _, runtime) = pick_release_with_asset(releases, "ubuntu-vulkan-x64.tar.gz").unwrap();
    assert!(runtime.is_none());
  }

  #[test]
  fn linux_amd_x64_picks_rocm_suffix_with_glob() {
    let s = pick_asset_suffix(&hw(amd(), OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-rocm-*-x64.tar.gz");
  }

  #[test]
  fn windows_amd_picks_vulkan_not_hip() {
    // Regression: AMD on Windows is DXGI-detected, so we can't confirm
    // the GPU is in ROCm's narrow Windows-support set. The HIP build
    // crashes on init for unsupported cards (RDNA1 / RX 5700 XT); Vulkan
    // runs everywhere. Must NOT route to `win-hip-radeon`.
    let s = pick_asset_suffix(&hw(amd(), OsFamily::Windows, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "win-vulkan-x64.zip");
  }

  #[test]
  fn linux_amd_still_picks_rocm_after_windows_fix() {
    // Guard: the Windows-AMD→Vulkan fix must not touch the Linux AMD
    // path, where `rocm-smi` detection implies a working ROCm stack.
    let s = pick_asset_suffix(&hw(amd(), OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-rocm-*-x64.tar.gz");
  }

  #[test]
  fn linux_cpu_only_picks_plain_ubuntu_suffix() {
    let s = pick_asset_suffix(&hw(GpuInfo::CpuOnly, OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-x64.tar.gz");
  }

  #[test]
  fn macos_arm64_picks_macos_arm_suffix() {
    let s = pick_asset_suffix(&hw(
      GpuInfo::AppleMetal {
        total_memory_bytes: 32 * 1024 * 1024 * 1024,
      },
      OsFamily::MacOs,
      CpuArch::Arm64,
    ))
    .unwrap();
    assert_eq!(s, "macos-arm64.tar.gz");
  }

  #[test]
  fn asset_matches_handles_glob_for_rocm() {
    let suffix = "ubuntu-rocm-*-x64.tar.gz";
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-rocm-7.2-x64.tar.gz",
      suffix
    ));
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-rocm-6.4-x64.tar.gz",
      suffix
    ));
    assert!(!asset_matches(
      "llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
      suffix
    ));
  }

  #[test]
  fn asset_matches_rejects_cudart_supplementary_package() {
    // Regression: `cudart-llama-bin-win-cuda-12.4-x64.zip` ends with the
    // same suffix pattern as the real Windows CUDA binary but is a CUDA
    // runtime DLL bundle that does not contain `llama-server.exe`.
    // asset_matches must reject it so pick_release_with_asset never
    // hands it to safe_extract_zip.
    let suffix = "win-cuda-*-x64.zip";
    assert!(!asset_matches(
      "cudart-llama-bin-win-cuda-12.4-x64.zip",
      suffix
    ));
    assert!(asset_matches(
      "llama-b9553-bin-win-cuda-12.4-x64.zip",
      suffix
    ));
  }

  #[test]
  fn asset_matches_exact_suffix_for_vulkan() {
    let suffix = "ubuntu-vulkan-x64.tar.gz";
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
      suffix
    ));
    assert!(!asset_matches("llama-b9219-bin-ubuntu-x64.tar.gz", suffix));
  }

  fn dev(backend: &str) -> GpuDevice {
    GpuDevice {
      name: "card".into(),
      backend: backend.into(),
      total_memory_bytes: 8 * 1024 * 1024 * 1024,
      used_memory_bytes: 0,
      utilization_pct: None,
      temperature_c: None,
      ..Default::default()
    }
  }

  #[test]
  fn windows_single_gpu_double_detected_picks_vulkan() {
    // Regression for the v0.0.4 Windows `init` failure: a single AMD/Intel
    // APU is found by BOTH the DXGI probe (→ amd device) and the Vulkan
    // fallback (→ unknown device). `gpu::probe` counts devices before its
    // dedup, so it surfaces `GpuInfo::Multi` for what is one physical card.
    // pick_asset_suffix must resolve the universal Vulkan build instead of
    // returning None → NoMatchingAsset.
    let gpu = GpuInfo::Multi {
      devices: vec![dev("amd"), dev("unknown")],
    };
    let s = pick_asset_suffix(&hw(gpu, OsFamily::Windows, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "win-vulkan-x64.zip");
  }

  #[test]
  fn windows_multi_gpu_with_nvidia_prefers_cuda() {
    let gpu = GpuInfo::Multi {
      devices: vec![dev("nvidia"), dev("unknown")],
    };
    let s = pick_asset_suffix(&hw(gpu, OsFamily::Windows, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "win-cuda-*-x64.zip");
  }

  #[test]
  fn linux_multi_gpu_picks_vulkan() {
    // The base route. CUDA for an NVIDIA-bearing set on Linux is layered
    // on by `select_asset_suffix`, which also needs the driver version.
    let gpu = GpuInfo::Multi {
      devices: vec![dev("nvidia"), dev("unknown")],
    };
    let s = pick_asset_suffix(&hw(gpu, OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-vulkan-x64.tar.gz");
  }

  #[test]
  fn cpu_only_macos_x86_picks_macos_x64() {
    let s = pick_asset_suffix(&hw(GpuInfo::CpuOnly, OsFamily::MacOs, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "macos-x64.tar.gz");
  }

  fn asset(name: &str) -> AssetRow {
    AssetRow {
      name: name.into(),
      browser_download_url: format!("https://example.test/{name}"),
      digest: Some("sha256:0".into()),
      size: Some(1),
    }
  }

  fn release(tag: &str, names: &[&str]) -> ReleaseRow {
    ReleaseRow {
      tag_name: tag.into(),
      assets: names.iter().map(|n| asset(n)).collect(),
    }
  }

  #[test]
  fn pick_release_uses_latest_when_match_present() {
    let releases = vec![
      release("b9352", &["llama-b9352-bin-ubuntu-x64.tar.gz"]),
      release("b9351", &["llama-b9351-bin-ubuntu-x64.tar.gz"]),
    ];
    let (tag, asset, _) = pick_release_with_asset(releases, "ubuntu-x64.tar.gz").unwrap();
    assert_eq!(tag, "b9352");
    assert_eq!(asset.name, "llama-b9352-bin-ubuntu-x64.tar.gz");
  }

  #[test]
  fn pick_release_walks_back_when_latest_missing_target_asset() {
    // Reproduces the `b9352` regression: the latest release lacks the
    // Linux CPU asset but `b9351` has it. The picker should fall back.
    let releases = vec![
      release(
        "b9352",
        &[
          "llama-b9352-bin-macos-arm64.tar.gz",
          "llama-b9352-bin-macos-x64.tar.gz",
          "llama-b9352-bin-ubuntu-arm64.tar.gz",
        ],
      ),
      release(
        "b9351",
        &[
          "llama-b9351-bin-ubuntu-x64.tar.gz",
          "llama-b9351-bin-ubuntu-vulkan-x64.tar.gz",
        ],
      ),
    ];
    let (tag, asset, _) = pick_release_with_asset(releases, "ubuntu-x64.tar.gz").unwrap();
    assert_eq!(tag, "b9351");
    assert_eq!(asset.name, "llama-b9351-bin-ubuntu-x64.tar.gz");
  }

  #[test]
  fn pick_release_returns_none_when_no_release_has_match() {
    let releases = vec![
      release("b9352", &["llama-b9352-bin-macos-arm64.tar.gz"]),
      release("b9351", &["llama-b9351-bin-macos-arm64.tar.gz"]),
    ];
    assert!(pick_release_with_asset(releases, "ubuntu-x64.tar.gz").is_none());
  }

  #[test]
  fn pick_release_honors_glob_suffix_during_walk_back() {
    let releases = vec![
      release("b9352", &["llama-b9352-bin-macos-arm64.tar.gz"]),
      release("b9219", &["llama-b9219-bin-ubuntu-rocm-7.2-x64.tar.gz"]),
    ];
    let (tag, asset, _) = pick_release_with_asset(releases, "ubuntu-rocm-*-x64.tar.gz").unwrap();
    assert_eq!(tag, "b9219");
    assert_eq!(asset.name, "llama-b9219-bin-ubuntu-rocm-7.2-x64.tar.gz");
  }
}
