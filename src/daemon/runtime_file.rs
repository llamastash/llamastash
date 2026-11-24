//! Per-daemon runtime info file (`runtime.json` under the state directory):
//! the URL and bearer token a client needs to attach to the running daemon.
//!
//! The file is rewritten on every daemon start and lives only as long as the
//! daemon does, which is why it stays separate from `state.json`. That one
//! survives restarts. It holds the bearer token in plaintext, protected by file
//! permissions only (0o600 on Unix, owner-only DACL on Windows). Writes go
//! through `crate::util::atomic_write::write_secure`, the same secure atomic
//! write every state-dir consumer uses.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version, bumped on a breaking change so a future daemon can refuse
/// to load an older shape.
const RUNTIME_SCHEMA_VERSION: u32 = 1;

/// On-disk shape of `runtime.json`. The control plane writes it at startup, and
/// clients read it to attach.
///
/// `Debug` is manual so `format!("{info:?}")` cannot leak the bearer secret.
/// It mirrors [`crate::daemon::auth::IpcToken`]'s posture.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
  #[serde(default = "current_schema_version")]
  pub schema_version: u32,
  /// Full URL the daemon's control plane bound on, e.g.
  /// `"http://127.0.0.1:48134"`. Clients POST to `{ipc_url}/rpc`.
  pub ipc_url: String,
  /// Per-daemon bearer token (base64url, no padding), sent in
  /// `Authorization: Bearer <token>` on every request except `/health`.
  /// Rotated by daemon restart.
  pub ipc_token: String,
  /// Wall-clock seconds since the Unix epoch when this daemon started, used only
  /// to display the start time. No decision reads it.
  pub started_at_unix: u64,
  /// PID of the daemon that owns this file. Informational, because the lockfile
  /// is the authoritative liveness check.
  pub daemon_pid: i32,
}

fn current_schema_version() -> u32 {
  RUNTIME_SCHEMA_VERSION
}

impl std::fmt::Debug for RuntimeInfo {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RuntimeInfo")
      .field("schema_version", &self.schema_version)
      .field("ipc_url", &self.ipc_url)
      .field("ipc_token_len", &self.ipc_token.len())
      .field("started_at_unix", &self.started_at_unix)
      .field("daemon_pid", &self.daemon_pid)
      .finish()
  }
}

/// Path of the runtime info file under `state_dir`.
pub fn path(state_dir: &Path) -> PathBuf {
  state_dir.join("runtime.json")
}

/// Persist `info` to `state_dir/runtime.json` atomically with mode `0o600` on
/// Unix. Creates `state_dir` if it does not exist.
pub fn save(state_dir: &Path, info: &RuntimeInfo) -> Result<(), SaveError> {
  let final_path = path(state_dir);
  let body = serde_json::to_vec_pretty(info).map_err(|e| SaveError::Serialise(e.to_string()))?;
  crate::util::atomic_write::write_secure(
    state_dir,
    "runtime.json.tmp.",
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

/// Read `state_dir/runtime.json`. Returns `Ok(None)` if the file is
/// absent (no daemon running). A parse failure surfaces as
/// `LoadError::Parse` so the caller can warn the user instead of
/// silently masking a corrupt file.
pub fn load(state_dir: &Path) -> Result<Option<RuntimeInfo>, LoadError> {
  let p = path(state_dir);
  match std::fs::read_to_string(&p) {
    Ok(s) => serde_json::from_str(&s)
      .map(Some)
      .map_err(|e| LoadError::Parse {
        path: p,
        error: e.to_string(),
      }),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(e) => Err(LoadError::Io {
      path: p,
      error: e.to_string(),
    }),
  }
}

/// Best-effort removal at daemon shutdown, silent on any error. The orphan
/// re-adoption path tolerates a stale runtime.json because the lockfile is the
/// authoritative liveness check.
pub fn remove(state_dir: &Path) {
  let p = path(state_dir);
  let _ = std::fs::remove_file(&p);
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
  #[error("runtime-info I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("runtime-info parse at {}: {error}", path.display())]
  Parse { path: PathBuf, error: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
  #[error("runtime-info I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("runtime-info serialise: {0}")]
  Serialise(String),
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_state_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-runtime-file-{label}-{}-{nanos}",
      std::process::id()
    ));
    std::fs::create_dir_all(&p).expect("temp");
    p
  }

  fn sample() -> RuntimeInfo {
    RuntimeInfo {
      schema_version: RUNTIME_SCHEMA_VERSION,
      ipc_url: "http://127.0.0.1:48134".into(),
      ipc_token: "abc123_token".into(),
      started_at_unix: 1_748_534_400,
      daemon_pid: 12345,
    }
  }

  #[test]
  fn load_returns_none_when_file_absent() {
    let dir = temp_state_dir("absent");
    let got = load(&dir).expect("load");
    assert_eq!(got, None);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn round_trip_save_load_is_lossless() {
    let dir = temp_state_dir("round");
    let info = sample();
    save(&dir, &info).expect("save");
    let got = load(&dir).expect("load").expect("file present");
    assert_eq!(got, info);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn save_writes_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_state_dir("mode");
    save(&dir, &sample()).expect("save");
    let mode = std::fs::metadata(path(&dir))
      .expect("meta")
      .permissions()
      .mode()
      & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn remove_is_noop_when_absent() {
    let dir = temp_state_dir("rm-absent");
    remove(&dir); // must not panic
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn remove_clears_existing_file() {
    let dir = temp_state_dir("rm-present");
    save(&dir, &sample()).expect("save");
    assert!(path(&dir).exists());
    remove(&dir);
    assert!(!path(&dir).exists());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn debug_does_not_leak_ipc_token() {
    let info = RuntimeInfo {
      ipc_token: "super-secret-bearer-value".into(),
      ..sample()
    };
    let dbg = format!("{info:?}");
    assert!(!dbg.contains("super-secret-bearer-value"));
    assert!(dbg.contains("ipc_token_len"));
  }

  #[test]
  fn load_surfaces_parse_error_on_corrupt_json() {
    let dir = temp_state_dir("parse-err");
    std::fs::write(path(&dir), b"not-json").expect("write");
    let err = load(&dir).expect_err("must surface parse error");
    assert!(matches!(err, LoadError::Parse { .. }));
    std::fs::remove_dir_all(&dir).ok();
  }
}
