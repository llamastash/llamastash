//! OS-aware path resolution for state, config, cache, logs, and the runtime
//! socket.
//!
//! Linux follows XDG; macOS follows the Apple convention (everything under
//! `~/Library/...`). `state_dir` falls back to `data_dir` on macOS because
//! the `directories` crate only exposes a distinct state directory on Linux.
//! The runtime socket falls back to `$TMPDIR/llamadash-$USER/daemon.sock`
//! when no `runtime_dir` is available.

use std::{
  ffi::OsString,
  path::{Path, PathBuf},
};

use directories::{BaseDirs, ProjectDirs};

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "llamadash";

pub fn project_dirs() -> Option<ProjectDirs> {
  ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
}

/// Human-friendly short label derived from a GGUF file path. Falls
/// back to `"model"` when the path has no readable `file_stem`. Used
/// by every TUI surface that needs a short tag for the focused model
/// (chat-tab `model` field, right-pane title, launch picker, logs).
pub fn model_display_name(path: &Path) -> String {
  path
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model")
    .to_string()
}

/// Best-effort home directory resolution. Returns `None` only when the
/// platform can't supply one (i.e. broken `$HOME` and no equivalent in
/// the password database) — every realistic developer machine has one.
/// Discovery uses this to anchor `~/.cache/huggingface/hub`,
/// `~/.ollama/models`, and `~/.lmstudio/models`.
pub fn home_dir() -> Option<PathBuf> {
  BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

pub fn state_dir() -> Option<PathBuf> {
  project_dirs().map(|d| {
    d.state_dir()
      .map_or_else(|| d.data_dir().to_path_buf(), PathBuf::from)
  })
}

pub fn config_dir() -> Option<PathBuf> {
  project_dirs().map(|d| d.config_dir().to_path_buf())
}

pub fn cache_dir() -> Option<PathBuf> {
  project_dirs().map(|d| d.cache_dir().to_path_buf())
}

pub fn log_dir() -> Option<PathBuf> {
  cache_dir().map(|d| d.join("logs"))
}

/// Resolve the Unix-socket path for the daemon.
///
/// Always returns a path. Resolution order:
/// 1. `LLAMADASH_SOCKET` env var (verbatim) — used by tests and by
///    operators who want to point a CLI at a non-default daemon
///    without the `--socket-path` hidden flag dance.
/// 2. `XDG_RUNTIME_DIR/llamadash/daemon.sock` (Linux).
/// 3. `$TMPDIR/llamadash-$USER/daemon.sock` (macOS / no runtime dir).
pub fn runtime_socket_path() -> PathBuf {
  if let Some(raw) = std::env::var_os("LLAMADASH_SOCKET") {
    let p = PathBuf::from(raw);
    if !p.as_os_str().is_empty() {
      return p;
    }
  }
  if let Some(dirs) = project_dirs() {
    if let Some(rt) = dirs.runtime_dir() {
      return rt.join("daemon.sock");
    }
  }
  fallback_runtime_dir_from(std::env::var_os("TMPDIR"), &username()).join("daemon.sock")
}

fn username() -> String {
  let raw = std::env::var("USER")
    .or_else(|_| std::env::var("LOGNAME"))
    .unwrap_or_else(|_| String::from("default"));
  sanitize_username(&raw)
}

/// Strip any character not in `[A-Za-z0-9_.-]` from the supplied username.
/// `$USER=../../root` would otherwise let an attacker direct the fallback
/// socket out of the per-user scratch dir; see the Unit 1 review findings.
/// Returns `"default"` if sanitization eats the whole string.
fn sanitize_username(raw: &str) -> String {
  let cleaned: String = raw
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    .collect();
  if cleaned.is_empty() {
    String::from("default")
  } else {
    cleaned
  }
}

/// Build the fallback runtime directory used when `XDG_RUNTIME_DIR` is not
/// set (notably on macOS). Per-user scoped so concurrent users on the same
/// host don't collide. Callers must supply a sanitized username — see
/// `sanitize_username`.
fn fallback_runtime_dir_from(tmpdir: Option<OsString>, user: &str) -> PathBuf {
  let tmp = tmpdir
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("/tmp"));
  tmp.join(format!("llamadash-{user}"))
}

/// Convenience: return the canonical user config-file path.
pub fn user_config_file() -> Option<PathBuf> {
  config_dir().map(|d| d.join("config.yaml"))
}

/// Convenience: state-store path (favorites, presets, last-params, running snapshot).
pub fn state_file() -> Option<PathBuf> {
  state_dir().map(|d| d.join("state.json"))
}

/// Convenience: PID lockfile path used by the daemon to enforce single-instance.
pub fn daemon_pidfile() -> Option<PathBuf> {
  state_dir().map(|d| d.join("daemon.pid"))
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use super::*;

  /// Pure helper used in tests so callers can verify the path-joining logic
  /// without manipulating process-wide environment variables.
  fn build_socket_path(runtime_dir: Option<&Path>, fallback_tmp: &Path, user: &str) -> PathBuf {
    if let Some(rt) = runtime_dir {
      return rt.join("daemon.sock");
    }
    fallback_tmp
      .join(format!("llamadash-{user}"))
      .join("daemon.sock")
  }

  #[test]
  fn project_dirs_resolves_on_this_platform() {
    // Smoke test: directories crate must produce a project root on every
    // platform we support. If this returns None the test environment has no
    // resolvable home directory, which is a broader problem than we want to
    // hide here.
    assert!(
      project_dirs().is_some(),
      "ProjectDirs::from should resolve on a normal developer machine"
    );
  }

  #[test]
  fn all_dir_helpers_return_some() {
    assert!(state_dir().is_some());
    assert!(config_dir().is_some());
    assert!(cache_dir().is_some());
    assert!(log_dir().is_some());
    assert!(user_config_file().is_some());
    assert!(state_file().is_some());
    assert!(daemon_pidfile().is_some());
  }

  #[test]
  fn all_dir_helpers_contain_llamadash_segment() {
    // Stronger than `is_some()`: every resolved path must live under
    // a `llamadash/` directory regardless of platform. Catches a
    // regression where the `directories` crate dependency or our
    // `APPLICATION` constant changes and silently re-roots the
    // daemon's files outside of its namespace.
    let llamadash = std::ffi::OsStr::new("llamadash");
    for path in [
      state_dir().unwrap(),
      config_dir().unwrap(),
      cache_dir().unwrap(),
      log_dir().unwrap(),
    ] {
      assert!(
        path.components().any(|c| c.as_os_str() == llamadash),
        "{} does not contain a `llamadash` segment",
        path.display()
      );
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_state_dir_uses_xdg_or_home_dot_local() {
    // On Linux the resolved state_dir must live under either
    // `$XDG_STATE_HOME/llamadash` or the conventional fallback
    // `~/.local/state/llamadash`. We assert by string-contains on the
    // canonical segment, not by re-setting env vars (which would
    // race with parallel tests).
    let path = state_dir().unwrap().display().to_string();
    assert!(
      path.contains(".local/state/llamadash") || path.contains("/llamadash"),
      "unexpected state_dir on linux: {path}"
    );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn macos_state_dir_lives_under_library_application_support() {
    // The `directories` crate maps state_dir to data_dir on macOS;
    // both live under `~/Library/Application Support/llamadash`.
    let path = state_dir().unwrap().display().to_string();
    assert!(
      path.contains("Library/Application Support/llamadash"),
      "unexpected state_dir on macOS: {path}"
    );
  }

  #[test]
  fn runtime_socket_path_always_resolves() {
    // Infallible by design — see runtime_socket_path's contract.
    let path = runtime_socket_path();
    assert_eq!(
      path.file_name().and_then(|s| s.to_str()),
      Some("daemon.sock")
    );
  }

  #[test]
  fn log_dir_is_logs_under_cache_dir() {
    let log = log_dir().unwrap();
    let cache = cache_dir().unwrap();
    assert_eq!(log, cache.join("logs"));
  }

  #[test]
  fn user_config_file_lives_under_config_dir() {
    let path = user_config_file().unwrap();
    assert_eq!(path, config_dir().unwrap().join("config.yaml"));
  }

  #[test]
  fn state_file_lives_under_state_dir() {
    let path = state_file().unwrap();
    assert_eq!(path, state_dir().unwrap().join("state.json"));
  }

  #[test]
  fn daemon_pidfile_lives_under_state_dir() {
    let path = daemon_pidfile().unwrap();
    assert_eq!(path, state_dir().unwrap().join("daemon.pid"));
  }

  #[test]
  fn build_socket_path_uses_runtime_dir_when_present() {
    let rt = PathBuf::from("/run/user/1000/llamadash");
    let path = build_socket_path(Some(&rt), Path::new("/tmp"), "ignored");
    assert_eq!(path, PathBuf::from("/run/user/1000/llamadash/daemon.sock"));
  }

  #[test]
  fn build_socket_path_falls_back_to_tmp_dir_with_username() {
    let path = build_socket_path(None, Path::new("/tmp"), "alice");
    assert_eq!(path, PathBuf::from("/tmp/llamadash-alice/daemon.sock"));
  }

  #[test]
  fn fallback_runtime_dir_uses_provided_tmp_and_username() {
    let path = fallback_runtime_dir_from(Some(OsString::from("/var/tmp")), "bob");
    assert_eq!(path, PathBuf::from("/var/tmp/llamadash-bob"));
  }

  #[test]
  fn fallback_runtime_dir_defaults_to_slash_tmp() {
    let path = fallback_runtime_dir_from(None, "default");
    assert_eq!(path, PathBuf::from("/tmp/llamadash-default"));
  }

  #[test]
  fn sanitize_username_strips_path_traversal() {
    assert_eq!(sanitize_username("../../root"), "....root");
  }

  #[test]
  fn sanitize_username_strips_slashes_and_specials() {
    assert_eq!(sanitize_username("alice/../bob"), "alice..bob");
    assert_eq!(sanitize_username("alice;rm -rf /"), "alicerm-rf");
  }

  #[test]
  fn sanitize_username_keeps_well_formed_names() {
    assert_eq!(sanitize_username("alice"), "alice");
    assert_eq!(sanitize_username("alice_user-1.0"), "alice_user-1.0");
  }

  #[test]
  fn sanitize_username_falls_back_when_all_chars_stripped() {
    assert_eq!(sanitize_username("///"), "default");
    assert_eq!(sanitize_username(""), "default");
  }
}
