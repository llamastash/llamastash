//! `ggml-org/llama.cpp` GitHub Releases install path.
//!
//! Two facts from the Unit 1 spike anchor this module:
//! 1. SHA-256 lives in the API JSON `digest` field (`sha256:<hex>`).
//!    No discrete sidecar files; no body-text parsing.
//! 2. Linux + Nvidia has **no** CUDA prebuilt — Vulkan is the
//!    routing default with an actionable downgrade banner Unit 10
//!    prints.
//!
//! Variant table (per spike):
//!
//! | Host | Asset name suffix |
//! |---|---|
//! | linux x86_64 cpu | `ubuntu-x64.tar.gz` |
//! | linux x86_64 vulkan / nvidia | `ubuntu-vulkan-x64.tar.gz` |
//! | linux x86_64 amd | `ubuntu-rocm-<ver>-x64.tar.gz` |
//! | linux arm64 cpu | `ubuntu-arm64.tar.gz` |
//! | linux arm64 vulkan | `ubuntu-vulkan-arm64.tar.gz` |
//! | macos arm64 (metal default) | `macos-arm64.tar.gz` |
//! | macos x86_64 | `macos-x64.tar.gz` |
//!
//! Window assets exist but are out of v2 scope per the plan's Scope
//! Boundaries.

use std::path::Path;

use serde::Deserialize;

use crate::gpu::GpuInfo;
use crate::init::detection::{CpuArch, HardwareSnapshot, OsFamily};
use crate::init::fetch::{FetchClient, FetchError};

use super::safe_extract::safe_extract_tar_gz;
use super::{sha256_file, BinaryInstall, InstallError};
use crate::init::snapshot::InstallMethod;

/// Endpoint the wizard hits to discover the latest asset list. Pinned
/// in source so a hostile env can't redirect us off-org.
const RELEASES_URL: &str = "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=1";

/// API response body's max size cap. The latest-releases-1 JSON
/// payload is ~30 KB on a typical day; 256 KB is generous headroom.
const RELEASES_MAX_BYTES: u64 = 256 * 1024;

/// Per-asset body cap (1 GB). Largest GH Releases asset for the
/// platforms v2 supports is the Vulkan tarball (~30 MB on Linux);
/// the cap keeps a hostile mirror from streaming an unbounded body.
const ASSET_MAX_BYTES: u64 = 1024 * 1024 * 1024;

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
    _ => None,
  }
}

/// Determine whether `asset_name` matches `suffix`. `suffix` may
/// contain a single `*` glob (used for ROCm version drift).
pub fn asset_matches(asset_name: &str, suffix: &str) -> bool {
  let lower_name = asset_name.to_ascii_lowercase();
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
  pub asset_name: String,
  pub url: String,
  pub sha256: String,
}

/// Fetch the most recent release and pick the asset matching the
/// host's variant suffix. Returns `Err(NoMatchingAsset)` when no
/// asset in the most recent release matches — falling back to an
/// older release lives in the wizard (Unit 10) since the choice is
/// user-visible.
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
) -> Result<AssetPick, InstallError> {
  let suffix = pick_asset_suffix(hw).ok_or(InstallError::NoMatchingAsset {
    os: hw.os,
    arch: hw.cpu_arch,
  })?;
  // R71: GH Releases API allows 60 unauthenticated requests/hour.
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
  let release = releases
    .into_iter()
    .next()
    .ok_or_else(|| InstallError::Fetch("empty releases list".into()))?;
  let matched = release
    .assets
    .into_iter()
    .find(|a| asset_matches(&a.name, &suffix))
    .ok_or(InstallError::NoMatchingAsset {
      os: hw.os,
      arch: hw.cpu_arch,
    })?;
  let digest = matched.digest.ok_or_else(|| {
    InstallError::Integrity(format!(
      "asset `{}` has no digest field on the GH API response",
      matched.name
    ))
  })?;
  let sha256 = digest
    .strip_prefix("sha256:")
    .ok_or_else(|| InstallError::Integrity(format!("digest `{digest}` is not sha256:<hex>")))?
    .to_string();
  Ok(AssetPick {
    tag: release.tag_name,
    asset_name: matched.name,
    url: matched.browser_download_url,
    sha256,
  })
}

/// Download + verify + safe-extract the picked asset. Returns the
/// resolved binary path + recorded digest the wizard stamps into
/// `_init_snapshot`.
pub async fn install_picked(
  fetch: &FetchClient,
  pick: &AssetPick,
  install_root: &Path,
) -> Result<BinaryInstall, InstallError> {
  let bytes = fetch
    .get_bytes(&pick.url, ASSET_MAX_BYTES)
    .await
    .map_err(translate_fetch)?;
  // Verify SHA-256 before any extraction.
  let actual = sha256_bytes(&bytes);
  if actual != pick.sha256 {
    return Err(InstallError::ChecksumMismatch {
      expected: pick.sha256.clone(),
      actual,
    });
  }
  let extracted = safe_extract_tar_gz(&bytes, install_root, &pick.tag)?;
  let digest = sha256_file(&extracted.path)?;
  Ok(BinaryInstall {
    method: InstallMethod::GhReleases,
    path: extracted.path,
    digest,
    version: Some(pick.tag.clone()),
  })
}

fn translate_fetch(e: FetchError) -> InstallError {
  match e {
    FetchError::RateLimited { status } => InstallError::RateLimited { status },
    FetchError::Offline => InstallError::Fetch("offline mode (LLAMASTASH_OFFLINE)".into()),
    other => InstallError::Fetch(other.to_string()),
  }
}

fn sha256_bytes(bytes: &[u8]) -> String {
  use sha2::{Digest, Sha256};
  let mut hasher = Sha256::new();
  hasher.update(bytes);
  crate::util::hex::encode(hasher.finalize().as_slice())
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
      }],
    }
  }

  #[test]
  fn linux_nvidia_x64_picks_vulkan_suffix() {
    let s = pick_asset_suffix(&hw(nvidia(), OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-vulkan-x64.tar.gz");
  }

  #[test]
  fn linux_amd_x64_picks_rocm_suffix_with_glob() {
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
  fn asset_matches_exact_suffix_for_vulkan() {
    let suffix = "ubuntu-vulkan-x64.tar.gz";
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
      suffix
    ));
    assert!(!asset_matches("llama-b9219-bin-ubuntu-x64.tar.gz", suffix));
  }

  #[test]
  fn cpu_only_macos_x86_picks_macos_x64() {
    let s = pick_asset_suffix(&hw(GpuInfo::CpuOnly, OsFamily::MacOs, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "macos-x64.tar.gz");
  }
}
