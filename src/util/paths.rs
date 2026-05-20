//! OS-aware path resolution for state, config, cache, logs, and the runtime
//! socket.
//!
//! Linux follows XDG; macOS follows the Apple convention (everything under
//! `~/Library/...`). `state_dir` falls back to `data_dir` on macOS because
//! the `directories` crate only exposes a distinct state directory on Linux.
//! The runtime socket falls back to `$TMPDIR/llamastash-$USER/daemon.sock`
//! when no `runtime_dir` is available.
//!
//! Env-var overrides (consulted as resolution step #1, all empty values
//! treated as unset):
//!
//! * `LLAMASTASH_STATE_DIR` — verbatim override for `state_dir()`.
//! * `LLAMASTASH_CONFIG_DIR` — verbatim override for `config_dir()`;
//!   `user_config_file()` inherits because it is
//!   `config_dir().join("config.yaml")`.
//! * `LLAMASTASH_CACHE_DIR` — verbatim override for `cache_dir()`; `log_dir()`
//!   inherits because it is `cache_dir().join("logs")`.
//! * `LLAMASTASH_SOCKET` — verbatim override for `runtime_socket_path()`.
//! * `HF_HOME` — honored independently by `init::download::hf_cache_dir()`
//!   per HuggingFace convention; not a `paths::*` concern, but worth
//!   naming here because callers isolating a child process need to set
//!   all five to fully sandbox state, config, cache/logs, socket, and HF cache.

use std::{
  ffi::OsString,
  path::{Path, PathBuf},
};

use directories::{BaseDirs, ProjectDirs};

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "llamastash";

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
  if let Some(p) = env_path_override("LLAMASTASH_STATE_DIR") {
    return Some(p);
  }
  project_dirs().map(|d| {
    d.state_dir()
      .map_or_else(|| d.data_dir().to_path_buf(), PathBuf::from)
  })
}

pub fn config_dir() -> Option<PathBuf> {
  if let Some(p) = env_path_override("LLAMASTASH_CONFIG_DIR") {
    return Some(p);
  }
  project_dirs().map(|d| d.config_dir().to_path_buf())
}

pub fn cache_dir() -> Option<PathBuf> {
  if let Some(p) = env_path_override("LLAMASTASH_CACHE_DIR") {
    return Some(p);
  }
  project_dirs().map(|d| d.cache_dir().to_path_buf())
}

/// Read an env var as a `PathBuf`, treating unset and empty values
/// identically. Shared with `runtime_socket_path` so override semantics
/// stay uniform across `state_dir`, `cache_dir`, and the socket.
fn env_path_override(name: &str) -> Option<PathBuf> {
  let raw = std::env::var_os(name)?;
  let p = PathBuf::from(raw);
  if p.as_os_str().is_empty() {
    None
  } else {
    Some(p)
  }
}

pub fn log_dir() -> Option<PathBuf> {
  cache_dir().map(|d| d.join("logs"))
}

/// Resolve the Unix-socket path for the daemon.
///
/// Always returns a path. Resolution order:
/// 1. `LLAMASTASH_SOCKET` env var (verbatim) — used by tests and by
///    operators who want to point a CLI at a non-default daemon
///    without the `--socket-path` hidden flag dance.
/// 2. `XDG_RUNTIME_DIR/llamastash/daemon.sock` (Linux).
/// 3. `$TMPDIR/llamastash-$USER/daemon.sock` (macOS / no runtime dir).
pub fn runtime_socket_path() -> PathBuf {
  if let Some(p) = env_path_override("LLAMASTASH_SOCKET") {
    return p;
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
  tmp.join(format!("llamastash-{user}"))
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

/// Convenience: init-wizard snapshot file path (R67). Sibling of
/// `state.json` under the state dir; written and consumed only by the
/// init wizard and `llamastash doctor` — the daemon ignores it.
pub fn init_snapshot_file() -> Option<PathBuf> {
  state_dir().map(|d| d.join("init_snapshot.json"))
}

#[cfg(test)]
mod tests {
  use std::{
    path::Path,
    sync::{Mutex, OnceLock},
  };

  use super::*;

  /// Serialize the tests that actually mutate process-global env vars
  /// (a small handful covering the `LLAMASTASH_STATE_DIR` /
  /// `LLAMASTASH_CACHE_DIR` resolution chain). Every other test in this
  /// module reads paths without touching env state so they stay
  /// parallel-safe.
  fn env_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
  }

  /// Save-and-restore guard for a single env var. Restores the previous
  /// value (including "unset") on drop so a panicking test does not
  /// leak state into siblings sharing the env mutex.
  struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
  }

  impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
      let previous = std::env::var_os(key);
      std::env::set_var(key, value);
      Self { key, previous }
    }

    fn unset(key: &'static str) -> Self {
      let previous = std::env::var_os(key);
      std::env::remove_var(key);
      Self { key, previous }
    }
  }

  impl Drop for EnvVarGuard {
    fn drop(&mut self) {
      match self.previous.take() {
        Some(v) => std::env::set_var(self.key, v),
        None => std::env::remove_var(self.key),
      }
    }
  }

  /// Pure helper used in tests so callers can verify the path-joining logic
  /// without manipulating process-wide environment variables.
  fn build_socket_path(runtime_dir: Option<&Path>, fallback_tmp: &Path, user: &str) -> PathBuf {
    if let Some(rt) = runtime_dir {
      return rt.join("daemon.sock");
    }
    fallback_tmp
      .join(format!("llamastash-{user}"))
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
  fn all_dir_helpers_contain_llamastash_segment() {
    // Stronger than `is_some()`: every resolved path must live under
    // a `llamastash/` directory regardless of platform. Catches a
    // regression where the `directories` crate dependency or our
    // `APPLICATION` constant changes and silently re-roots the
    // daemon's files outside of its namespace.
    let llamastash = std::ffi::OsStr::new("llamastash");
    for path in [
      state_dir().unwrap(),
      config_dir().unwrap(),
      cache_dir().unwrap(),
      log_dir().unwrap(),
    ] {
      assert!(
        path.components().any(|c| c.as_os_str() == llamastash),
        "{} does not contain a `llamastash` segment",
        path.display()
      );
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_state_dir_uses_xdg_or_home_dot_local() {
    // On Linux the resolved state_dir must live under either
    // `$XDG_STATE_HOME/llamastash` or the conventional fallback
    // `~/.local/state/llamastash`. We assert by string-contains on the
    // canonical segment, not by re-setting env vars (which would
    // race with parallel tests).
    let path = state_dir().unwrap().display().to_string();
    assert!(
      path.contains(".local/state/llamastash") || path.contains("/llamastash"),
      "unexpected state_dir on linux: {path}"
    );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn macos_state_dir_lives_under_library_application_support() {
    // The `directories` crate maps state_dir to data_dir on macOS;
    // both live under `~/Library/Application Support/llamastash`.
    let path = state_dir().unwrap().display().to_string();
    assert!(
      path.contains("Library/Application Support/llamastash"),
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
  fn init_snapshot_file_lives_under_state_dir() {
    let path = init_snapshot_file().unwrap();
    assert_eq!(path, state_dir().unwrap().join("init_snapshot.json"));
  }

  #[test]
  fn build_socket_path_uses_runtime_dir_when_present() {
    let rt = PathBuf::from("/run/user/1000/llamastash");
    let path = build_socket_path(Some(&rt), Path::new("/tmp"), "ignored");
    assert_eq!(path, PathBuf::from("/run/user/1000/llamastash/daemon.sock"));
  }

  #[test]
  fn build_socket_path_falls_back_to_tmp_dir_with_username() {
    let path = build_socket_path(None, Path::new("/tmp"), "alice");
    assert_eq!(path, PathBuf::from("/tmp/llamastash-alice/daemon.sock"));
  }

  #[test]
  fn fallback_runtime_dir_uses_provided_tmp_and_username() {
    let path = fallback_runtime_dir_from(Some(OsString::from("/var/tmp")), "bob");
    assert_eq!(path, PathBuf::from("/var/tmp/llamastash-bob"));
  }

  #[test]
  fn fallback_runtime_dir_defaults_to_slash_tmp() {
    let path = fallback_runtime_dir_from(None, "default");
    assert_eq!(path, PathBuf::from("/tmp/llamastash-default"));
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

  #[test]
  fn env_path_override_returns_pathbuf_when_set() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_TEST_PATH_OVERRIDE_A", "/tmp/uat-abc");
    assert_eq!(
      env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_A"),
      Some(PathBuf::from("/tmp/uat-abc"))
    );
  }

  #[test]
  fn env_path_override_returns_none_when_unset() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::unset("LLAMASTASH_TEST_PATH_OVERRIDE_B");
    assert_eq!(env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_B"), None);
  }

  #[test]
  fn env_path_override_treats_empty_string_as_unset() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_TEST_PATH_OVERRIDE_C", "");
    assert_eq!(env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_C"), None);
  }

  #[test]
  fn env_path_override_preserves_unicode_and_spaces_verbatim() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let raw = "/tmp/uat 🦙 spaces";
    let _e = EnvVarGuard::set("LLAMASTASH_TEST_PATH_OVERRIDE_D", raw);
    assert_eq!(
      env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_D"),
      Some(PathBuf::from(raw))
    );
  }

  #[test]
  fn state_dir_honors_llamadash_state_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "/tmp/uat-state-override");
    assert_eq!(state_dir(), Some(PathBuf::from("/tmp/uat-state-override")));
  }

  #[test]
  fn cache_dir_honors_llamadash_cache_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "/tmp/uat-cache-override");
    assert_eq!(cache_dir(), Some(PathBuf::from("/tmp/uat-cache-override")));
  }

  #[test]
  fn config_dir_honors_llamadash_config_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CONFIG_DIR", "/tmp/uat-config-override");
    assert_eq!(
      config_dir(),
      Some(PathBuf::from("/tmp/uat-config-override"))
    );
  }

  #[test]
  fn user_config_file_inherits_config_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CONFIG_DIR", "/tmp/uat-config-override");
    assert_eq!(
      user_config_file(),
      Some(PathBuf::from("/tmp/uat-config-override/config.yaml"))
    );
  }

  #[test]
  fn config_dir_empty_override_falls_through_to_project_dirs() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CONFIG_DIR", "");
    let p = config_dir().expect("config_dir() resolves on this platform");
    assert!(
      p.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("llamastash")),
      "{} should contain a `llamastash` segment when override is empty",
      p.display()
    );
  }

  #[test]
  fn log_dir_inherits_cache_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "/tmp/uat-cache-override");
    // log_dir = cache_dir().join("logs"); confirms the chain.
    assert_eq!(
      log_dir(),
      Some(PathBuf::from("/tmp/uat-cache-override/logs"))
    );
  }

  #[test]
  fn state_dir_empty_override_falls_through_to_project_dirs() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "");
    // Empty override is treated as unset; resolution returns the
    // platform-default path which still carries the `llamastash` segment.
    let p = state_dir().expect("state_dir() resolves on this platform");
    assert!(
      p.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("llamastash")),
      "{} should contain a `llamastash` segment when override is empty",
      p.display()
    );
  }

  #[test]
  fn cache_dir_empty_override_falls_through_to_project_dirs() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "");
    let p = cache_dir().expect("cache_dir() resolves on this platform");
    assert!(
      p.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("llamastash")),
      "{} should contain a `llamastash` segment when override is empty",
      p.display()
    );
  }

  #[test]
  fn state_and_cache_overrides_are_independent() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _state = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "/tmp/uat-state");
    let _cache = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "/tmp/uat-cache");
    assert_eq!(state_dir(), Some(PathBuf::from("/tmp/uat-state")));
    assert_eq!(cache_dir(), Some(PathBuf::from("/tmp/uat-cache")));
    // log_dir inherits cache, NOT state.
    assert_eq!(log_dir(), Some(PathBuf::from("/tmp/uat-cache/logs")));
  }

  #[test]
  fn state_file_and_pidfile_follow_state_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "/tmp/uat-state-x");
    assert_eq!(
      state_file(),
      Some(PathBuf::from("/tmp/uat-state-x/state.json"))
    );
    assert_eq!(
      daemon_pidfile(),
      Some(PathBuf::from("/tmp/uat-state-x/daemon.pid"))
    );
    assert_eq!(
      init_snapshot_file(),
      Some(PathBuf::from("/tmp/uat-state-x/init_snapshot.json"))
    );
  }
}
