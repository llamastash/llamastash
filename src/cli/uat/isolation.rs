//! Tempdir isolation + child-process env-var configuration (Unit 4).
//!
//! The UAT runs against a fresh tempdir so its state, config, runtime
//! socket, cache/logs, and the HuggingFace download cache never
//! collide with the maintainer's real daily-driver paths. Five env
//! vars + Linux's XDG equivalents fully fence the child processes:
//!
//! | env var | resolved by |
//! |---|---|
//! | `LLAMASTASH_STATE_DIR`  | `paths::state_dir()` (Unit 1) |
//! | `LLAMASTASH_CONFIG_DIR` | `paths::config_dir()`; `user_config_file()` inherits |
//! | `LLAMASTASH_CACHE_DIR`  | `paths::cache_dir()` (Unit 1); `log_dir()` inherits |
//! | `LLAMASTASH_SOCKET`     | `paths::runtime_socket_path()` (pre-existing) |
//! | `HF_HOME`              | `init::download::hf_cache_dir()` (HF convention) |
//!
//! Plus on Linux, `XDG_STATE_HOME` / `XDG_CONFIG_HOME` / `XDG_CACHE_HOME`
//! / `XDG_RUNTIME_DIR` are set redundantly. `directories` on Linux
//! honors XDG vars independently of the LLAMASTASH_* overrides —
//! without XDG, a child process that calls `ProjectDirs::from(...)`
//! directly (no current call site, but future code might) could leak
//! outside the sandbox. The config slot in particular: `init
//! --recommended` auto-confirms a write to
//! `~/.config/llamastash/config.yaml`, so an absent `LLAMASTASH_CONFIG_DIR`
//! / `XDG_CONFIG_HOME` would clobber the maintainer's real config on
//! every UAT run.
//!
//! ## Cleanup contract
//!
//! `TempdirGuard` holds the tempdir's `PathBuf` plus a `preserve:
//! AtomicBool` initialized to `true`. The orchestrator flips it to
//! `false` only when the entire lifecycle returns success. Drop's
//! behavior:
//!
//! 1. Kill the tracked child `llama-server` (if any) with SIGTERM and a
//!    short grace period, then SIGKILL. Done *before* tempdir teardown
//!    so the child doesn't keep writing to a path that's about to
//!    disappear.
//! 2. If `preserve == false`, remove the tempdir.
//! 3. Otherwise, leave the tempdir in place and emit one line to
//!    stderr with the preserved path so the maintainer can post-mortem.
//!
//! SIGKILL of the orchestrator process itself bypasses Drop entirely;
//! that's an acknowledged tradeoff (R448 §Risks). The natural OS
//! behavior — tempdir survives, the maintainer has the diagnostic
//! tree — is actually desirable in the SIGKILL case.

use std::{
  path::{Path, PathBuf},
  process::Command,
  sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc,
  },
};

use tempfile::TempDir;

/// Names of the env vars `TempdirGuard::env_overrides` writes onto a
/// child process. Exposed for tests that need to assert on the
/// configuration shape without spawning a child.
pub const LLAMASTASH_ENV_KEYS: &[&str] = &[
  "LLAMASTASH_STATE_DIR",
  "LLAMASTASH_CONFIG_DIR",
  "LLAMASTASH_CACHE_DIR",
  "LLAMASTASH_SOCKET",
  "HF_HOME",
];

/// Names of the Linux XDG vars set redundantly so a child that calls
/// `ProjectDirs::from(...)` directly stays inside the sandbox.
pub const XDG_ENV_KEYS: &[&str] = &[
  "XDG_STATE_HOME",
  "XDG_CONFIG_HOME",
  "XDG_CACHE_HOME",
  "XDG_RUNTIME_DIR",
];

/// Drop-guard that owns the per-UAT-run tempdir and the spawned
/// `llama-server` child PID. See module docs for the cleanup contract.
pub struct TempdirGuard {
  /// Root tempdir. `tempfile::TempDir` (mkdtemp with mode 0700) keeps
  /// the path TOCTOU-safe; we keep it inside an `Option` so `Drop` can
  /// either let `TempDir::drop` reap it (success) or move it out via
  /// `into_path` to preserve the directory on failure.
  root: Option<TempDir>,
  /// Cached root path so `Path` accessors don't have to peek into the
  /// `Option<TempDir>` after `Drop` has taken it.
  root_path: PathBuf,
  /// Atomic so the orchestrator can flip this to `false` from another
  /// task / blocking section without rewrapping the guard in a Mutex.
  preserve: Arc<AtomicBool>,
  /// PID of the supervisor's `llama-server` child, if one was tracked.
  /// `Arc<AtomicI32>` lets the lifecycle update the PID after spawn
  /// from any context the guard's Drop can read. `0` means "no child
  /// tracked"; positive value is a Unix PID.
  tracked_child_pid: Arc<AtomicI32>,
}

impl TempdirGuard {
  /// Create a new tempdir under the system temp root. The label is
  /// folded into the prefix so concurrent UAT runs (different shells,
  /// two backends) don't collide. The actual directory creation goes
  /// through `tempfile::Builder::tempdir_in` which uses mkdtemp(3) —
  /// atomic 0700 creation, no TOCTOU race on a predictable name.
  pub fn new(label: &str) -> std::io::Result<Self> {
    let prefix = format!("llamastash-uat-{}-", sanitize_label(label));
    let root = tempfile::Builder::new()
      .prefix(&prefix)
      .tempdir_in(std::env::temp_dir())?;
    let root_path = root.path().to_path_buf();
    // Pre-create the four sub-paths so child processes don't race on
    // directory creation. `hf` is HF_HOME's root; hf-hub will create
    // its own `hub/` underneath. The `config` slot backs
    // `LLAMASTASH_CONFIG_DIR` / `XDG_CONFIG_HOME` so `init
    // --recommended`'s config-write lands inside the sandbox.
    for sub in ["state", "config", "cache", "runtime", "hf"] {
      std::fs::create_dir_all(root_path.join(sub))?;
    }
    Ok(Self {
      root: Some(root),
      root_path,
      preserve: Arc::new(AtomicBool::new(true)),
      tracked_child_pid: Arc::new(AtomicI32::new(0)),
    })
  }

  pub fn root(&self) -> &Path {
    &self.root_path
  }

  /// Path to the daemon's runtime socket inside the sandbox.
  pub fn socket_path(&self) -> PathBuf {
    self.root_path.join("runtime").join("daemon.sock")
  }

  /// Path to the sandbox's HF cache root. Children see this via
  /// `HF_HOME`; hf-hub appends `hub/` internally.
  pub fn hf_home(&self) -> PathBuf {
    self.root_path.join("hf")
  }

  /// Build the env-var map for a child process. Returned as
  /// `Vec<(name, value)>` so a caller can either fold it onto a
  /// `Command` via `.env(k, v)` or render it for a dry-run check.
  pub fn env_overrides(&self) -> Vec<(&'static str, PathBuf)> {
    let state = self.root_path.join("state");
    let config = self.root_path.join("config");
    let cache = self.root_path.join("cache");
    let runtime = self.root_path.join("runtime");
    let socket = self.socket_path();
    let hf = self.hf_home();
    vec![
      ("LLAMASTASH_STATE_DIR", state.clone()),
      ("LLAMASTASH_CONFIG_DIR", config.clone()),
      ("LLAMASTASH_CACHE_DIR", cache.clone()),
      ("LLAMASTASH_SOCKET", socket),
      ("HF_HOME", hf),
      // XDG mirror (no-op on macOS, redundant safety on Linux).
      ("XDG_STATE_HOME", state),
      ("XDG_CONFIG_HOME", config),
      ("XDG_CACHE_HOME", cache),
      ("XDG_RUNTIME_DIR", runtime),
    ]
  }

  /// Apply `env_overrides` to a `Command`. Single render site so
  /// every child the orchestrator spawns sees an identical
  /// environment.
  pub fn configure_command(&self, cmd: &mut Command) {
    for (k, v) in self.env_overrides() {
      cmd.env(k, v);
    }
  }

  /// Hand the orchestrator a Drop-time PID-killer handle. The
  /// lifecycle updates the inner atomic after spawning
  /// `llama-server` so the guard can reap it on Drop.
  pub fn child_pid_handle(&self) -> Arc<AtomicI32> {
    Arc::clone(&self.tracked_child_pid)
  }

  /// Lifecycle calls this on the success path to opt out of the
  /// preserve-on-Drop behavior. Idempotent and atomic.
  pub fn release_on_success(&self) {
    self.preserve.store(false, Ordering::SeqCst);
  }

  /// Snapshot for `preserve`. Tests use it; production calls Drop.
  #[cfg(test)]
  fn would_preserve(&self) -> bool {
    self.preserve.load(Ordering::SeqCst)
  }
}

impl Drop for TempdirGuard {
  fn drop(&mut self) {
    // Step 1: kill the tracked child. Best-effort — a stale PID just
    // means SIGTERM lands on nothing.
    let pid = self.tracked_child_pid.load(Ordering::SeqCst);
    if pid > 0 {
      send_term_then_kill(pid);
    }
    // Step 2 / 3: preserve or remove. On the preserve path we move
    // the `TempDir` out via `into_path`, which deliberately leaks the
    // ownership so the directory survives Drop for post-mortem
    // inspection. On the success path, dropping `TempDir` removes the
    // tree; we ignore errors because Drop can't return them and an
    // unremovable tempdir is itself diagnostic.
    if let Some(td) = self.root.take() {
      if self.preserve.load(Ordering::SeqCst) {
        // `TempDir::keep` (was `into_path`) consumes the guard,
        // returns the `PathBuf`, and prevents the underlying directory
        // from being removed when the guard drops. Exactly the
        // preserve-for-post-mortem behavior the cleanup contract
        // documents.
        let path = td.keep();
        eprintln!(
          "llamastash uat: preserved tempdir at {} (run not successful — leave for post-mortem)",
          path.display()
        );
      }
      // else: TempDir's own Drop removes the tree.
    }
  }
}

/// Strip everything except the lowercase ascii alphabet + digits +
/// `-` / `_` so a maintainer-supplied label can't escape the
/// tempdir-name shape. Empty result falls back to `"run"`.
fn sanitize_label(raw: &str) -> String {
  let cleaned: String = raw
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    .collect();
  if cleaned.is_empty() {
    String::from("run")
  } else {
    cleaned
  }
}

#[cfg(unix)]
fn send_term_then_kill(pid: i32) {
  use std::time::{Duration, Instant};
  // SIGTERM first.
  unsafe {
    libc::kill(pid, libc::SIGTERM);
  }
  // Wait up to 2 seconds for the child to exit. Polling is OK here
  // because Drop is synchronous and we don't have a tokio runtime to
  // lean on; the child is `llama-server`, which terminates cleanly on
  // SIGTERM in under a second on the maintainer's hardware.
  let deadline = Instant::now() + Duration::from_secs(2);
  while Instant::now() < deadline {
    // `kill(pid, 0)` returns 0 if the process is still alive, -1 if not.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if !alive {
      return;
    }
    std::thread::sleep(Duration::from_millis(100));
  }
  // Still alive — escalate.
  unsafe {
    libc::kill(pid, libc::SIGKILL);
  }
}

#[cfg(not(unix))]
fn send_term_then_kill(_pid: i32) {
  // Windows isn't in v1 scope (origin §Out of scope). Stub so the
  // module still compiles on Windows hosts for the doc-build CI lane.
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sanitize_label_keeps_alnum_dash_underscore() {
    assert_eq!(sanitize_label("warm-nvidia_1"), "warm-nvidia_1");
  }

  #[test]
  fn sanitize_label_drops_path_traversal() {
    assert_eq!(sanitize_label("../../etc"), "etc");
    assert_eq!(sanitize_label("rm -rf /"), "rm-rf");
  }

  #[test]
  fn sanitize_label_falls_back_when_all_stripped() {
    assert_eq!(sanitize_label(""), "run");
    assert_eq!(sanitize_label("///"), "run");
  }

  #[test]
  fn new_creates_subdirs() {
    let g = TempdirGuard::new("test-new").expect("create");
    for sub in ["state", "config", "cache", "runtime", "hf"] {
      assert!(g.root().join(sub).exists(), "{sub} should exist");
    }
    // The guard's would-preserve is true by default; release on
    // success to clean up after this test (matches the lifecycle).
    g.release_on_success();
  }

  #[test]
  fn env_overrides_covers_all_documented_keys() {
    let g = TempdirGuard::new("test-env").expect("create");
    let envs = g.env_overrides();
    let keys: Vec<&str> = envs.iter().map(|(k, _)| *k).collect();
    for required in LLAMASTASH_ENV_KEYS {
      assert!(
        keys.contains(required),
        "env_overrides missing {required}: {keys:?}"
      );
    }
    for required in XDG_ENV_KEYS {
      assert!(
        keys.contains(required),
        "env_overrides missing XDG {required}: {keys:?}"
      );
    }
    g.release_on_success();
  }

  #[test]
  fn env_overrides_paths_live_under_root() {
    let g = TempdirGuard::new("test-paths").expect("create");
    let root = g.root().to_path_buf();
    for (_, path) in g.env_overrides() {
      assert!(
        path.starts_with(&root),
        "env path {} should live under root {}",
        path.display(),
        root.display()
      );
    }
    g.release_on_success();
  }

  #[test]
  fn drop_removes_root_when_released_on_success() {
    let path = {
      let g = TempdirGuard::new("test-release").expect("create");
      let p = g.root().to_path_buf();
      g.release_on_success();
      assert!(!g.would_preserve());
      p
    };
    assert!(
      !path.exists(),
      "tempdir should have been removed on Drop, still at {}",
      path.display()
    );
  }

  #[test]
  fn drop_preserves_root_by_default() {
    let path = {
      let g = TempdirGuard::new("test-preserve").expect("create");
      assert!(g.would_preserve());
      g.root().to_path_buf()
      // No `release_on_success()` — Drop runs with preserve=true.
    };
    assert!(
      path.exists(),
      "tempdir should be preserved at {}",
      path.display()
    );
    // Manual cleanup — this test deliberately exercises the preserve
    // path so it has to clean up after itself.
    let _ = std::fs::remove_dir_all(&path);
  }

  #[test]
  fn child_pid_handle_starts_at_zero() {
    let g = TempdirGuard::new("test-pid").expect("create");
    let h = g.child_pid_handle();
    assert_eq!(h.load(Ordering::SeqCst), 0);
    g.release_on_success();
  }

  #[test]
  fn configure_command_sets_all_keys() {
    use std::process::Command;
    let g = TempdirGuard::new("test-cmd").expect("create");
    let mut cmd = Command::new("/usr/bin/true");
    g.configure_command(&mut cmd);
    let env_pairs: Vec<(String, String)> = cmd
      .get_envs()
      .filter_map(|(k, v)| {
        let v = v?;
        Some((
          k.to_string_lossy().into_owned(),
          v.to_string_lossy().into_owned(),
        ))
      })
      .collect();
    let keys: Vec<&str> = env_pairs.iter().map(|(k, _)| k.as_str()).collect();
    for required in LLAMASTASH_ENV_KEYS {
      assert!(
        keys.contains(required),
        "configure_command must set {required}: keys={keys:?}"
      );
    }
    g.release_on_success();
  }
}
