//! `llama-server` install routing (R52 / R53 / R54).
//!
//! Three sources land here, picked by the wizard's install-method
//! prompt:
//! - [`gh_releases`] — download a verified asset from
//!   `github.com/ggml-org/llama.cpp/releases` (the default for any
//!   Linux + GPU combination per the Unit 1 spike).
//! - [`brew`] — `brew install --quiet llama.cpp` (the default for
//!   macOS arm64; CPU-only fallback on linuxbrew).
//! - [`custom_path`] — accept a user-supplied existing binary after
//!   running the same integrity gates the other two paths emit.
//!
//! Every path returns a [`BinaryInstall`] the wizard records in
//! `_init_snapshot` (Unit 2) so `doctor` (Unit 13) can flag drift.
//! Integrity-check failures abort with `INIT_ABORTED` (72) per the
//! non-interactive semantics in the plan.

pub mod brew;
pub mod custom_path;
pub mod gh_releases;
pub mod safe_extract;

use std::path::{Path, PathBuf};

use crate::init::detection::{CpuArch, HardwareSnapshot, OsFamily};
use crate::init::snapshot::InstallMethod;

/// What the user picked from the install-method prompt.
#[derive(Debug, Clone)]
pub enum InstallChoice {
  GhReleases,
  Brew,
  CustomPath(PathBuf),
}

/// Outcome of a successful install. `digest` is the binary's
/// SHA-256 hex string; `version` is the `llama-server --version`
/// output (typically a `bNNNN` commit short hash).
#[derive(Debug, Clone)]
pub struct BinaryInstall {
  pub method: InstallMethod,
  pub path: PathBuf,
  pub digest: String,
  pub version: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
  #[error("integrity check failed: {0}")]
  Integrity(String),
  #[error("network fetch failed: {0}")]
  Fetch(String),
  #[error("checksum mismatch — expected {expected}, got {actual}")]
  ChecksumMismatch { expected: String, actual: String },
  #[error("safe-extract refused entry `{path}`: {reason}")]
  UnsafeArchive { path: String, reason: String },
  #[error("brew install failed: {0}")]
  Brew(String),
  #[error("no GH Releases asset matches this hardware (os={os:?}, arch={arch:?})")]
  NoMatchingAsset { os: OsFamily, arch: CpuArch },
  #[error("rate-limited by GitHub Releases API (status {status}); retry after backoff")]
  RateLimited { status: u16 },
  #[error("I/O: {0}")]
  Io(String),
}

/// Compute the SHA-256 of `path`. Surfaced here so brew + custom-path
/// + GH Releases all share the same digesting routine.
pub fn sha256_file(path: &Path) -> Result<String, InstallError> {
  use sha2::{Digest, Sha256};
  let bytes = std::fs::read(path).map_err(|e| InstallError::Io(e.to_string()))?;
  let mut hasher = Sha256::new();
  hasher.update(&bytes);
  Ok(crate::util::hex::encode(hasher.finalize().as_slice()))
}

/// Plan-time helper exposed for tests: which install method should the
/// wizard pre-select given the host's hardware? Returns the choice the
/// non-interactive `--recommended` mode would accept.
pub fn default_install_method(hw: &HardwareSnapshot) -> InstallChoice {
  use crate::gpu::GpuInfo;
  match (&hw.gpu, hw.os, hw.cpu_arch) {
    (GpuInfo::AppleMetal { .. }, OsFamily::MacOs, CpuArch::Arm64) => InstallChoice::Brew,
    (_, OsFamily::MacOs, _) => InstallChoice::Brew,
    // Linux + Nvidia → GH Releases Vulkan (no CUDA prebuilt exists —
    // see Unit 1 GH-Releases-contract spike's breaking finding).
    (GpuInfo::Nvidia { .. }, OsFamily::Linux, _)
    | (GpuInfo::Amd { .. }, OsFamily::Linux, _)
    | (GpuInfo::Unknown { .. }, OsFamily::Linux, _) => InstallChoice::GhReleases,
    // Linux CPU-only: brew if linuxbrew is on PATH, else GH Releases CPU.
    (GpuInfo::CpuOnly, OsFamily::Linux, _) => InstallChoice::GhReleases,
    _ => InstallChoice::GhReleases,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::{GpuDevice, GpuInfo};

  fn hw(gpu: GpuInfo, os: OsFamily, arch: CpuArch) -> HardwareSnapshot {
    HardwareSnapshot {
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
      gpu,
      os,
      cpu_arch: arch,
    }
  }

  fn nvidia_device() -> GpuInfo {
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

  #[test]
  fn macos_arm64_routes_to_brew() {
    let choice = default_install_method(&hw(
      GpuInfo::AppleMetal {
        total_memory_bytes: 32 * 1024 * 1024 * 1024,
      },
      OsFamily::MacOs,
      CpuArch::Arm64,
    ));
    assert!(matches!(choice, InstallChoice::Brew));
  }

  #[test]
  fn linux_nvidia_routes_to_gh_releases() {
    // Per Unit 1 spike: no ubuntu-cuda asset; routing lands on the
    // Vulkan prebuilt with a downgrade banner Unit 10 displays.
    let choice = default_install_method(&hw(nvidia_device(), OsFamily::Linux, CpuArch::X86_64));
    assert!(matches!(choice, InstallChoice::GhReleases));
  }

  #[test]
  fn linux_cpu_only_routes_to_gh_releases_cpu_asset() {
    let choice = default_install_method(&hw(GpuInfo::CpuOnly, OsFamily::Linux, CpuArch::X86_64));
    assert!(matches!(choice, InstallChoice::GhReleases));
  }

  #[test]
  fn sha256_file_is_deterministic_across_runs() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"deterministic input").unwrap();
    let h1 = sha256_file(tmp.path()).unwrap();
    let h2 = sha256_file(tmp.path()).unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 64, "SHA-256 hex is 64 chars");
  }
}
