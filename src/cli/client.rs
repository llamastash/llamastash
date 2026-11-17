//! Daemon connection helper shared by every non-interactive
//! subcommand.
//!
//! The CLI's promise is "if the daemon isn't already running, start
//! it for me — unless `--no-spawn` is set." That keeps casual
//! invocations friction-free while letting agent scripts opt into
//! deterministic failure. The single-shot `Client` is also returned
//! so callers can chain calls on the same connection.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::cli::cli_args::Cli;
use crate::cli::exit_codes::{CliExit, DAEMON_UNREACHABLE};
use crate::config::Config;
use crate::daemon::{
  existing_daemon_pid,
  restart::{shutdown_and_wait, StopOutcome},
  runtime_file, start_detached, DaemonOptions, StartOutcome,
};
use crate::ipc::{Client, ClientError};
use crate::util::paths::state_dir;

/// Detect a stale daemon: the PID lockfile is held by a live process,
/// but `runtime.json` is missing (or unreadable). Means a daemon is
/// running but didn't publish its HTTP control-plane URL+token — a
/// pre-Phase-A binary, a crash between lock-acquire and runtime-file
/// save, or a manual `rm`. Auto-spawning would just fail the second
/// time, so callers should surface an actionable error instead.
///
/// Returns the PID of the stale daemon, or `None` when no daemon owns
/// the lockfile.
pub(crate) fn detect_stale_daemon(state_dir: &std::path::Path) -> Option<i32> {
  let pid = existing_daemon_pid(state_dir)?;
  match runtime_file::load(state_dir) {
    Ok(Some(_)) => None,
    Ok(None) | Err(_) => Some(pid),
  }
}

/// Error message rendered when `detect_stale_daemon` fires. Centralised
/// so the wording stays consistent between `connect_or_spawn`, the
/// post-spawn polling fallback, and any future call sites.
fn stale_daemon_message(pid: i32, state_dir: &std::path::Path) -> String {
  format!(
    "daemon (pid {pid}) is running but didn't publish runtime.json under {} — \
     likely a stale process from an older version. Run `llamastash daemon stop --force` \
     (or `kill {pid}`) and retry.",
    state_dir.display()
  )
}

/// Connect to the daemon. Auto-spawns it (via `daemon::start_detached`)
/// when the socket isn't connectable and `cli.no_spawn` is false.
/// Returns a `CliExit` shaped to the canonical exit codes so the
/// caller doesn't have to map errors a second time.
pub async fn connect_or_spawn(cli: &Cli, config: &Config) -> Result<Client, CliExit> {
  // The HTTP control-plane client reads `runtime.json` (URL + bearer
  // token) from the daemon's state directory.
  let attach_dir = state_dir()
    .ok_or_else(|| CliExit::new(DAEMON_UNREACHABLE, "could not resolve state directory"))?;
  match Client::connect(&attach_dir).await {
    Ok(client) => {
      // `runtime.json` exists — but the file alone isn't proof of life.
      // An unclean exit (crash, `daemon stop --force`, power loss) leaves
      // it behind pointing at a dead control-plane URL, and
      // `Client::connect` deliberately doesn't probe reachability. The
      // authoritative liveness signal is the PID lock, which the OS
      // releases the instant the daemon dies (Unix `flock`, Windows
      // `LockFileEx`). If nothing holds it, the daemon is gone: clear the
      // stale file and auto-spawn a fresh one rather than handing back a
      // client whose every call fails with a cryptic connect error.
      if existing_daemon_pid(&attach_dir).is_none() {
        runtime_file::remove(&attach_dir);
        return spawn_and_attach(cli, config, &attach_dir).await;
      }
      // Reconcile against the running daemon: it may be from an older
      // build than this client, and the user may have passed
      // `--llama-server` (or any other binary-resolving flag) on
      // *this* invocation while the daemon was started earlier
      // without it, which would silently drop the flag. Restart the
      // daemon when no managed launches are running, so we never kill
      // someone else's in-flight model on a stale daemon; otherwise
      // warn on stderr and keep the connection.
      reconcile_with_running_daemon(client, cli, config, &attach_dir).await
    }
    Err(ClientError::Connect(_)) => {
      // Stale-daemon pre-flight: a live process owns `daemon.pid` but
      // didn't publish `runtime.json`. `start_detached`'s fast path
      // would silently fall through to `AlreadyRunning(pid)` and then
      // `await_socket` would time out with the same cryptic Connect
      // error. Surface an actionable hint instead.
      if let Some(pid) = detect_stale_daemon(&attach_dir) {
        return Err(CliExit::new(
          DAEMON_UNREACHABLE,
          stale_daemon_message(pid, &attach_dir),
        ));
      }
      spawn_and_attach(cli, config, &attach_dir).await
    }
    Err(other) => Err(CliExit::from_client_error(other)),
  }
}

/// Spawn a detached daemon and return a client attached to it. Shared
/// by both the "no `runtime.json`" path and the self-heal path that
/// fires when a stale `runtime.json` outlived its daemon. Honors
/// `--no-spawn` (agent scripts opting into deterministic failure).
async fn spawn_and_attach(
  cli: &Cli,
  config: &Config,
  attach_dir: &std::path::Path,
) -> Result<Client, CliExit> {
  if cli.no_spawn {
    return Err(CliExit::new(
      DAEMON_UNREACHABLE,
      format!(
        "daemon: not running and --no-spawn was passed (state dir: {})",
        attach_dir.display()
      ),
    ));
  }
  let opts = build_spawn_options(cli, config)?;
  // Fail-fast: never auto-spawn a daemon that can't bring up an *indicated*
  // backend (a missing `llama-server` with nothing else to launch). The
  // auto-spawn path has no `--force` knob, so a degraded start is never chosen
  // silently — the user resolves the issue or runs `daemon start --force`. The
  // TUI surfaces this message in its Daemon → server-info pane.
  if let Err(msg) = crate::cli::daemon::precheck_indicated_backends(&opts) {
    return Err(CliExit::new(DAEMON_UNREACHABLE, msg));
  }
  let attach_for_poll = opts.state_dir.clone();
  match start_detached(opts) {
    Ok(StartOutcome::RanToCompletion) | Ok(StartOutcome::AlreadyRunning(_)) => {
      // Brief settle window: start_detached returns once runtime.json
      // appears, but a second client opening at the same instant
      // occasionally trips an EAGAIN. One retry is enough.
      await_socket(&attach_for_poll, Duration::from_secs(2)).await
    }
    Err(e) => Err(CliExit::new(
      DAEMON_UNREACHABLE,
      format!("daemon: auto-spawn failed: {e}"),
    )),
  }
}

/// What the reconcile step decided to do about a live daemon.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileAction {
  /// The daemon answers this client as it is.
  Keep,
  /// Shut the daemon down and spawn one from this build. `reason` is the
  /// stderr line naming what changed.
  Restart { reason: String },
  /// Keep the connection and say why it was not updated.
  Warn { message: String },
}

/// Decide from one `status` reply whether the running daemon serves this
/// client. `client_build` is the client's `CARGO_PKG_VERSION`; `binary` is
/// `None` when the invocation passed no binary-resolving flag, else
/// `(the flag path, whether the daemon already runs it)`.
///
/// A build mismatch outranks a binary mismatch: the restart that fixes the
/// build re-applies the flag too. A `status` with no `daemon.build` has
/// nothing to compare, so only the binary check can act on it.
fn reconcile_action(
  status: &Value,
  client_build: &str,
  binary: Option<(&Path, bool)>,
) -> ReconcileAction {
  let running = status
    .get("models")
    .and_then(|v| v.as_array())
    .map(|a| !a.is_empty())
    .unwrap_or(false);
  if let Some(build) = status.pointer("/daemon/build").and_then(|v| v.as_str()) {
    if build != client_build {
      return if running {
        ReconcileAction::Warn {
          message: format!(
            "daemon: running build {build}, this client is {client_build}; stop the running \
             models and run `llamastash daemon restart` to update it"
          ),
        }
      } else {
        ReconcileAction::Restart {
          reason: format!("daemon: restarted to match this build ({build} → {client_build})"),
        }
      };
    }
  }
  let Some((flag, binary_matches)) = binary else {
    return ReconcileAction::Keep;
  };
  if binary_matches {
    return ReconcileAction::Keep;
  }
  let current = status
    .pointer("/daemon/server_path")
    .and_then(|v| v.as_str())
    .unwrap_or("—");
  if running {
    ReconcileAction::Warn {
      message: format!(
        "daemon already running with a different llama-server ({current}); ignored \
         `--llama-server {}` because managed launches are active. Stop them and re-run to apply.",
        flag.display()
      ),
    }
  } else {
    ReconcileAction::Restart {
      reason: format!(
        "daemon: restarting to adopt `--llama-server {}` (was: {current})",
        flag.display()
      ),
    }
  }
}

/// Reconcile a live daemon with this client: a daemon from an older build, or
/// a `--llama-server` flag it was not started with. One `status` call answers
/// both checks.
///
/// An idle daemon is shut down and re-spawned with this build and flags; with
/// managed launches running the connection stays and the mismatch is reported
/// on stderr, so no in-flight model is killed.
///
/// A `status` that fails passes the original client through: a reconcile
/// problem must never fail the command that triggered it.
async fn reconcile_with_running_daemon(
  mut client: Client,
  cli: &Cli,
  config: &Config,
  attach_dir: &std::path::Path,
) -> Result<Client, CliExit> {
  let status = match client.call("status", None).await {
    Ok(v) => v,
    Err(_) => return Ok(client),
  };
  let binary = match cli.llama_server.as_ref() {
    Some(flag) => Some((flag.as_path(), daemon_binary_matches(&status, flag).await)),
    None => None,
  };
  let reason = match reconcile_action(&status, env!("CARGO_PKG_VERSION"), binary) {
    ReconcileAction::Keep => return Ok(client),
    ReconcileAction::Warn { message } => {
      eprintln!("{}", crate::cli::colors::warning(&message));
      log::warn!("{message}");
      return Ok(client);
    }
    ReconcileAction::Restart { reason } => reason,
  };
  eprintln!("{reason}");
  log::info!("{reason}");
  // Re-spawn with the CLI flags flowing through `build_spawn_options`. The stop
  // is the shared one (`daemon stop` / `daemon restart` / `Ctrl+R`): waiting on
  // the lockfile instead of the socket going quiet is what keeps the
  // replacement's `acquire` off a daemon that has not let go yet. Forgiving by
  // design — a failed reconcile must not fail the command that triggered it.
  drop(client);
  match shutdown_and_wait(attach_dir).await {
    Ok(StopOutcome::Stopped) | Ok(StopOutcome::NoChannel) => {}
    Ok(StopOutcome::StillExiting { pid }) => {
      log::warn!("daemon: pid {pid} still exiting; the re-spawn may keep the old daemon");
    }
    Err(e) => log::warn!("daemon: shutdown during reconcile failed: {e}"),
  }
  let opts = build_spawn_options(cli, config)?;
  let attach_for_poll = opts.state_dir.clone();
  match start_detached(opts) {
    Ok(_) => await_socket(&attach_for_poll, Duration::from_secs(3)).await,
    Err(e) => Err(CliExit::new(
      DAEMON_UNREACHABLE,
      format!("daemon: restart to reconcile the running daemon failed: {e}"),
    )),
  }
}

/// Whether the running daemon already serves `desired`, the value of a
/// binary-resolving flag.
async fn daemon_binary_matches(status: &Value, desired: &Path) -> bool {
  // `tokio::fs::canonicalize` so the reconcile gate doesn't block
  // the async runtime on a slow inode lookup (network mounts,
  // unresponsive filesystems).
  let desired = match tokio::fs::canonicalize(desired).await {
    Ok(p) => p,
    // Let the daemon-side locator surface the path error later;
    // for the reconcile gate, fall back to the raw flag value so
    // a missing file doesn't silently bypass the check.
    Err(_) => desired.to_path_buf(),
  };
  let current = status
    .pointer("/daemon/server_path")
    .and_then(|v| v.as_str())
    .map(PathBuf::from);
  // Canonicalize both sides so the comparison is stable across
  // platforms. Windows `fs::canonicalize` returns a `\\?\C:\…`
  // UNC-prefixed path; the daemon stores `server_path` as the raw
  // input from CLI/env, which lacks the prefix. Comparing them
  // verbatim falsely triggers a restart on every dispatch and the
  // spawned re-exec panics because the test binary has no daemon
  // entry point.
  let current_canon = match current.as_ref() {
    Some(c) => tokio::fs::canonicalize(c).await.ok(),
    None => None,
  };
  match (current_canon.as_ref(), current.as_ref()) {
    (Some(canon), _) => canon == &desired,
    (None, Some(raw)) => raw == &desired,
    (None, None) => false,
  }
}

async fn await_socket(attach_dir: &std::path::Path, total: Duration) -> Result<Client, CliExit> {
  let deadline = std::time::Instant::now() + total;
  let mut last_err: Option<ClientError> = None;
  while std::time::Instant::now() < deadline {
    match Client::connect(attach_dir).await {
      Ok(c) => return Ok(c),
      Err(e) => last_err = Some(e),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  // Defensive: if we got here via `start_detached` and a stale daemon
  // is hoarding the lockfile, surface the actionable message instead of
  // the raw Connect error. `connect_or_spawn` already screens this
  // before spawning, but the same state can develop mid-poll on a
  // pathologically slow lock release.
  if let Some(pid) = detect_stale_daemon(attach_dir) {
    return Err(CliExit::new(
      DAEMON_UNREACHABLE,
      stale_daemon_message(pid, attach_dir),
    ));
  }
  Err(match last_err {
    Some(e) => CliExit::from_client_error(e),
    None => CliExit::new(DAEMON_UNREACHABLE, "daemon: never came up"),
  })
}

fn build_spawn_options(cli: &Cli, config: &Config) -> Result<DaemonOptions, CliExit> {
  // Mirror `daemon start`'s composition (state-dir / socket / discovery
  // roots / binary / port range) so a CLI auto-spawn produces the same
  // daemon a user would have hand-typed.
  super::daemon::build_options(super::daemon::BuildOptionsArgs::new(cli, config))
    .map_err(|e| CliExit::new(DAEMON_UNREACHABLE, format!("daemon: build options: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use std::time::{SystemTime, UNIX_EPOCH};

  /// A `status` reply with the given `daemon.build` (absent when `None`)
  /// and that many managed launches.
  fn status(daemon_build: Option<&str>, launches: usize) -> Value {
    let mut daemon = json!({ "server_path": "/usr/bin/llama-server" });
    if let Some(b) = daemon_build {
      daemon["build"] = json!(b);
    }
    json!({ "daemon": daemon, "models": vec![json!({ "port": 11435 }); launches] })
  }

  #[test]
  fn reconcile_action_keeps_a_matching_build() {
    let s = status(Some("0.11.0"), 0);
    assert_eq!(reconcile_action(&s, "0.11.0", None), ReconcileAction::Keep);
  }

  #[test]
  fn reconcile_action_restarts_an_idle_daemon_from_an_older_build() {
    let s = status(Some("0.10.0"), 0);
    match reconcile_action(&s, "0.11.0", None) {
      ReconcileAction::Restart { reason } => {
        assert!(
          reason.contains("0.10.0") && reason.contains("0.11.0"),
          "{reason}"
        );
      }
      other => panic!("expected a restart, got {other:?}"),
    }
  }

  #[test]
  fn reconcile_action_warns_about_an_older_build_while_launches_run() {
    let s = status(Some("0.10.0"), 1);
    match reconcile_action(&s, "0.11.0", None) {
      ReconcileAction::Warn { message } => {
        assert!(message.contains("daemon restart"), "{message}");
        assert!(message.contains("0.10.0"), "{message}");
      }
      other => panic!("expected a warning, got {other:?}"),
    }
  }

  #[test]
  fn reconcile_action_keeps_when_status_has_no_build() {
    // A daemon old enough to omit `daemon.build` has nothing to compare.
    let s = status(None, 0);
    assert_eq!(reconcile_action(&s, "0.11.0", None), ReconcileAction::Keep);
  }

  #[test]
  fn reconcile_action_still_restarts_for_a_binary_mismatch() {
    let s = status(Some("0.11.0"), 0);
    let flag = Path::new("/opt/llama-server");
    match reconcile_action(&s, "0.11.0", Some((flag, false))) {
      ReconcileAction::Restart { reason } => {
        assert!(
          reason.contains("--llama-server /opt/llama-server"),
          "{reason}"
        );
      }
      other => panic!("expected a restart, got {other:?}"),
    }
  }

  #[test]
  fn reconcile_action_warns_about_a_binary_mismatch_while_launches_run() {
    let s = status(Some("0.11.0"), 2);
    let flag = Path::new("/opt/llama-server");
    match reconcile_action(&s, "0.11.0", Some((flag, false))) {
      ReconcileAction::Warn { message } => {
        assert!(message.contains("managed launches are active"), "{message}");
      }
      other => panic!("expected a warning, got {other:?}"),
    }
  }

  fn temp_state_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-stale-detect-{label}-{}-{nanos}",
      std::process::id()
    ));
    std::fs::create_dir_all(&p).expect("temp");
    p
  }

  #[test]
  fn detect_stale_daemon_returns_none_on_empty_state_dir() {
    // No `daemon.pid` and no `runtime.json` → no daemon at all.
    // Auto-spawn should run; we must NOT misclassify this as stale.
    let dir = temp_state_dir("clean");
    assert!(detect_stale_daemon(&dir).is_none());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn detect_stale_daemon_returns_none_when_lockfile_unowned() {
    // A leftover `daemon.pid` file from a crashed daemon doesn't hold
    // a `flock`, so `existing_daemon_pid` returns None — no live
    // process to blame. The error should NOT mention a stale pid.
    let dir = temp_state_dir("unowned");
    std::fs::write(dir.join("daemon.pid"), b"4242\n").expect("write");
    assert!(detect_stale_daemon(&dir).is_none());
    std::fs::remove_dir_all(&dir).ok();
  }
}
