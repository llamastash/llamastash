//! `init_snapshot.json` — the init wizard's record of what it
//! detected, installed, and wrote.
//!
//! Lives alongside `state.json` under `$XDG_STATE_HOME/llamastash/`
//! (see [`crate::util::paths::init_snapshot_file`]). The daemon does
//! not read this file; only `llamastash init` and `llamastash doctor`
//! do. Persistence reuses the same hardening as `state.json`:
//! `tempfile`-based atomic write, mode 0600 on Unix, parse-fail
//! quarantine to `init_snapshot.json.broken-<ts>` so a re-run can
//! rebuild without surprising the user.
//!
//! Schema rationale lives in the v2 plan's "Key Technical Decisions"
//! section. Three points anchor every field choice:
//! - `managed_keys` records the dotted path *and* a `blake3` digest
//!   of the value the wizard wrote. On re-run a digest mismatch tells
//!   us the user edited the value by hand → preserve it (R72).
//! - `llama_server_digest` is the binary's sha256, recorded so
//!   `doctor` can flag drift on GH-Releases-installed binaries (brew
//!   installs are carved out per the doctor finding #2 design).
//! - `remote_fetch_failures` is the silent-fallback counter the
//!   `RemoteSnapshotUnreachable` doctor finding consumes (Unit 13).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Install method used to acquire the `llama-server` binary. Pinned
/// at install time so `doctor` knows whether digest drift is a real
/// finding or a routine `brew upgrade` (the carve-out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
  /// Downloaded from `github.com/ggml-org/llama.cpp/releases` and
  /// verified against the per-asset `digest` field
  /// (see Unit 1 spike `2026-05-19-llama-cpp-releases-asset-contract.md`).
  GhReleases,
  /// `brew install llama.cpp` — bottle. macOS arm64 ships
  /// Metal-enabled; Linux bottles are CPU-only.
  Brew,
  /// User pointed the wizard at a pre-existing binary; integrity
  /// checks (parent-dir mode, no cross-UID symlink, +x bit) pass and
  /// the digest is recorded.
  CustomPath,
}

/// One entry in `managed_keys` (R67). Records the dotted path the
/// wizard wrote into `config.yaml`, the digest of the value it
/// wrote, and the wall-clock timestamp. On re-run the wizard
/// compares the on-disk value's digest against `value_digest`:
/// match → wizard still owns the key, may regenerate; mismatch →
/// user edited the value, preserve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedKey {
  /// Dotted YAML path (e.g. `"port_range"`, `"arch_defaults.qwen2"`).
  pub path: String,
  /// `blake3` of the canonical YAML serialisation of the value the
  /// wizard wrote. 32 bytes serialised as a 64-char hex string for
  /// human diffability.
  pub value_digest: String,
  /// ISO-8601 wall-clock when this key was last written. Used by
  /// doctor to surface "wizard wrote this 47 days ago" hints.
  pub wrote_at: String,
}

impl ManagedKey {
  /// Build a `ManagedKey` from a path and a value already canonicalised
  /// to bytes. The caller is responsible for canonicalisation; this
  /// keeps the digest reproducible across runs (`blake3` of the same
  /// bytes is the same regardless of HashMap iteration order).
  pub fn new(path: impl Into<String>, canonical_bytes: &[u8], now: SystemTime) -> Self {
    let digest = blake3::hash(canonical_bytes);
    Self {
      path: path.into(),
      value_digest: hex_encode(digest.as_bytes()),
      wrote_at: iso8601(now),
    }
  }

  /// Check whether the supplied canonical bytes hash matches the
  /// recorded digest. `false` means the user has edited the value
  /// since the wizard last wrote it.
  pub fn digest_matches(&self, canonical_bytes: &[u8]) -> bool {
    let digest = blake3::hash(canonical_bytes);
    self.value_digest == hex_encode(digest.as_bytes())
  }
}

/// Schema version. Bumped on breaking changes; the loader refuses
/// snapshots whose `schema_version` exceeds the build's known max so
/// a downgraded binary doesn't misread a future shape.
const CURRENT_SCHEMA_VERSION: u32 = 1;

fn current_schema_version() -> u32 {
  CURRENT_SCHEMA_VERSION
}

/// What `llamastash init` records after a successful run. Every field
/// is optional in the on-disk shape via `#[serde(default)]` so the
/// wizard can partial-write across `--only X` invocations without
/// losing previously-recorded data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitSnapshot {
  /// `gpu::probe` vendor at last detection — `"nvidia"`, `"amd"`,
  /// `"apple_metal"`, `"cpu_only"`, etc. Used by doctor's
  /// `HardwareDrift` finding (Unit 13).
  #[serde(default)]
  pub gpu_vendor: Option<String>,
  /// Aggregated VRAM in GiB at last detection. Aggregation rule from
  /// Key Decisions: `min(device.total)` for Nvidia/AMD,
  /// `0.75 × total` for Metal, `None` for CpuOnly/Unknown.
  #[serde(default)]
  pub vram_gb: Option<f32>,
  /// Number of GPU devices reported at last probe.
  #[serde(default)]
  pub gpu_device_count: Option<u32>,
  /// `llama-server --version` output at install time (typically a
  /// commit short hash like `b9219`).
  #[serde(default)]
  pub llama_server_version: Option<String>,
  /// How the binary was installed (GH Releases / brew / custom path).
  #[serde(default)]
  pub install_method: Option<InstallMethod>,
  /// ISO-8601 wall-clock timestamp of the most recent successful
  /// `init` run that updated this snapshot.
  #[serde(default)]
  pub init_date: Option<String>,
  /// Absolute canonical path to the `llama-server` binary at install
  /// time. doctor's `BinaryMissing` finding reads this.
  #[serde(default)]
  pub llama_server_path: Option<PathBuf>,
  /// SHA-256 of the `llama-server` binary at install time, hex-encoded.
  /// doctor's `BinaryDigestDrift` finding (GH Releases only) compares
  /// against this. Brew installs record it too for forensic audit but
  /// `doctor` ignores drift on those.
  #[serde(default)]
  pub llama_server_digest: Option<String>,
  /// `bundle_date` of the benchmark snapshot used by the most recent
  /// recommender pass. doctor uses this for the `SnapshotStale`
  /// finding (>14 days).
  #[serde(default)]
  pub snapshot_bundle_date: Option<String>,
  /// Count of consecutive remote-snapshot fetch failures. Reset to
  /// 0 on a successful verified fetch. doctor's
  /// `RemoteSnapshotUnreachable` finding consumes this.
  #[serde(default)]
  pub remote_fetch_failures: u32,
  /// Dotted paths the wizard wrote into `config.yaml`, each with the
  /// digest of the value at write time. See [`ManagedKey`].
  #[serde(default)]
  pub managed_keys: Vec<ManagedKey>,
  /// Schema version; rejected on read if it exceeds the build's
  /// known max.
  #[serde(default = "current_schema_version")]
  pub schema_version: u32,
}

impl Default for InitSnapshot {
  fn default() -> Self {
    Self {
      gpu_vendor: None,
      vram_gb: None,
      gpu_device_count: None,
      llama_server_version: None,
      install_method: None,
      init_date: None,
      llama_server_path: None,
      llama_server_digest: None,
      snapshot_bundle_date: None,
      remote_fetch_failures: 0,
      managed_keys: Vec::new(),
      schema_version: current_schema_version(),
    }
  }
}

/// Hard cap on snapshot file size. Even a hand-edited file should
/// stay well under 64 KiB; refuse anything bigger before parsing to
/// avoid `serde_json`-driven allocation amplification.
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024;

/// Errors that `load` may surface. Missing file is not an error —
/// first-run callers receive `Ok(None)` so they can default cleanly.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
  #[error("init-snapshot I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error(
    "init-snapshot parse at {}: {error}; the file has been quarantined to {}",
    path.display(),
    quarantine.display()
  )]
  Parse {
    path: PathBuf,
    quarantine: PathBuf,
    error: String,
  },
  #[error(
    "init-snapshot at {} declares schema_version {found}, but this build only knows {max}; \
     refusing to load (older binary on newer state file)",
    path.display()
  )]
  SchemaTooNew { path: PathBuf, found: u32, max: u32 },
  #[error("init-snapshot at {} exceeds the {cap}-byte size cap", path.display())]
  TooLarge { path: PathBuf, cap: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
  #[error("init-snapshot I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("init-snapshot serialise: {0}")]
  Serialise(String),
}

/// Load `init_snapshot.json` from `state_dir`. Returns `Ok(None)` when
/// the file is absent so first-run callers don't need to disambiguate
/// "no snapshot yet" from "snapshot is broken". Parse failures move
/// the file to `init_snapshot.json.broken-<ts>` and return `Parse`
/// so the caller can warn before defaulting.
pub fn load(state_dir: &Path) -> Result<Option<InitSnapshot>, LoadError> {
  let p = path_for(state_dir);
  let meta = match std::fs::metadata(&p) {
    Ok(m) => m,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(e) => {
      return Err(LoadError::Io {
        path: p,
        error: e.to_string(),
      })
    }
  };
  if meta.len() > MAX_SNAPSHOT_BYTES {
    return Err(LoadError::TooLarge {
      path: p,
      cap: MAX_SNAPSHOT_BYTES,
    });
  }
  let raw = match std::fs::read_to_string(&p) {
    Ok(s) => s,
    Err(e) => {
      return Err(LoadError::Io {
        path: p,
        error: e.to_string(),
      })
    }
  };
  match serde_json::from_str::<InitSnapshot>(&raw) {
    Ok(snap) => {
      if snap.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(LoadError::SchemaTooNew {
          path: p,
          found: snap.schema_version,
          max: CURRENT_SCHEMA_VERSION,
        });
      }
      Ok(Some(snap))
    }
    Err(e) => {
      let quarantine = quarantine_path(&p);
      // Best-effort move — if the rename fails (read-only fs etc.)
      // we still surface a Parse error so the caller knows the
      // snapshot is unusable.
      let _ = std::fs::rename(&p, &quarantine);
      Err(LoadError::Parse {
        path: p,
        quarantine,
        error: e.to_string(),
      })
    }
  }
}

/// Persist `snapshot` to `state_dir/init_snapshot.json` atomically.
/// Routes through [`crate::util::atomic_write::write_secure`] so the
/// `tempfile + fsync + 0o600 + atomic rename` recipe is shared with
/// `state_store::save` and `config::writer::merge_and_write`.
pub fn save(state_dir: &Path, snapshot: &InitSnapshot) -> Result<(), SaveError> {
  let final_path = path_for(state_dir);
  let body =
    serde_json::to_vec_pretty(snapshot).map_err(|e| SaveError::Serialise(e.to_string()))?;
  crate::util::atomic_write::write_secure(
    state_dir,
    "init_snapshot.json.tmp.",
    &final_path,
    &body,
    Some(0o600),
  )
  .map_err(|e| SaveError::Io {
    path: final_path,
    error: e.to_string(),
  })?;
  Ok(())
}

/// File path under `state_dir`. Exposed for tests; production callers
/// use `crate::util::paths::init_snapshot_file()`.
pub fn path_for(state_dir: &Path) -> PathBuf {
  state_dir.join("init_snapshot.json")
}

fn quarantine_path(p: &Path) -> PathBuf {
  let ts = SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
  let stem = p
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("init_snapshot.json");
  p.with_file_name(format!("{stem}.broken-{ts}"))
}

pub(crate) use crate::util::datetime::iso8601;
use crate::util::hex::encode as hex_encode;

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use std::time::{Duration, UNIX_EPOCH};

  fn temp_state_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("init-snapshot-{label}"))
  }

  fn populated() -> InitSnapshot {
    InitSnapshot {
      gpu_vendor: Some("nvidia".into()),
      vram_gb: Some(24.0),
      gpu_device_count: Some(1),
      llama_server_version: Some("b9219".into()),
      install_method: Some(InstallMethod::GhReleases),
      init_date: Some("2026-05-19T00:00:00Z".into()),
      llama_server_path: Some(PathBuf::from(
        "/opt/llamastash/llama-cpp/b9219/llama-server",
      )),
      llama_server_digest: Some("a".repeat(64)),
      snapshot_bundle_date: Some("2026-05-18".into()),
      remote_fetch_failures: 0,
      managed_keys: vec![ManagedKey::new(
        "port_range",
        b"{start: 41100, end: 41300}",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
      )],
      schema_version: CURRENT_SCHEMA_VERSION,
    }
  }

  #[test]
  fn load_returns_none_when_file_absent() {
    let dir = temp_state_dir("missing");
    let result = load(&dir).expect("absent → Ok(None)");
    assert!(result.is_none());
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn save_then_load_round_trips_every_field() {
    let dir = temp_state_dir("round-trip");
    let snap = populated();
    save(&dir, &snap).expect("save");
    let back = load(&dir).expect("load").expect("Some");
    assert_eq!(back, snap, "every field must round-trip exactly");
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn save_writes_mode_0600_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_state_dir("mode-0600");
    save(&dir, &populated()).expect("save");
    let meta = fs::metadata(path_for(&dir)).expect("stat");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "init_snapshot.json must be 0600, got {mode:o}");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn corrupt_snapshot_is_quarantined_with_typed_error() {
    let dir = temp_state_dir("corrupt");
    fs::write(path_for(&dir), b"{not valid json").unwrap();
    let err = load(&dir).unwrap_err();
    match err {
      LoadError::Parse { quarantine, .. } => {
        assert!(quarantine.exists(), "quarantine file should exist");
        assert!(
          !path_for(&dir).exists(),
          "original file should be moved aside"
        );
      }
      other => panic!("expected Parse, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn future_schema_version_is_refused() {
    let dir = temp_state_dir("future-schema");
    let snap = InitSnapshot {
      schema_version: CURRENT_SCHEMA_VERSION + 1,
      ..Default::default()
    };
    save(&dir, &snap).expect("save");
    let err = load(&dir).unwrap_err();
    match err {
      LoadError::SchemaTooNew { found, max, .. } => {
        assert_eq!(found, CURRENT_SCHEMA_VERSION + 1);
        assert_eq!(max, CURRENT_SCHEMA_VERSION);
      }
      other => panic!("expected SchemaTooNew, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn oversize_snapshot_is_refused() {
    let dir = temp_state_dir("oversize");
    let body = "x".repeat((MAX_SNAPSHOT_BYTES as usize) + 16);
    fs::write(path_for(&dir), body).unwrap();
    let err = load(&dir).unwrap_err();
    assert!(
      matches!(err, LoadError::TooLarge { .. }),
      "expected TooLarge, got {err:?}"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn managed_key_digest_matches_same_bytes() {
    let key = ManagedKey::new(
      "port_range",
      b"canonical",
      UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    assert!(key.digest_matches(b"canonical"));
    assert!(!key.digest_matches(b"edited"));
  }

  #[test]
  fn save_is_atomic_no_tmp_lingers() {
    let dir = temp_state_dir("atomic");
    save(&dir, &InitSnapshot::default()).expect("save");
    for entry in fs::read_dir(&dir).expect("readdir") {
      let entry = entry.expect("dirent");
      let name = entry.file_name();
      let name = name.to_string_lossy();
      assert!(
        !name.starts_with("init_snapshot.json.tmp"),
        ".tmp sibling lingered: {name}"
      );
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn iso8601_renders_expected_format() {
    // 1700000000 = 2023-11-14T22:13:20Z
    let s = iso8601(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    assert_eq!(s, "2023-11-14T22:13:20Z");
  }
}
