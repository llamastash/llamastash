//! Daemon connection helper shared by every non-interactive
//! subcommand.
//!
//! The CLI's promise is "if the daemon isn't already running, start
//! it for me — unless `--no-spawn` is set." That keeps casual
//! invocations friction-free while letting agent scripts opt into
//! deterministic failure. The single-shot `Client` is also returned
//! so callers can chain calls on the same connection.

use std::time::Duration;

use anyhow::Result;

use crate::cli::cli_args::Cli;
use crate::cli::exit_codes::{CliExit, DAEMON_UNREACHABLE};
use crate::config::Config;
use crate::daemon::{
  existing_daemon_pid, runtime_file, start_detached, DaemonOptions, StartOutcome,
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
      // Reconcile against the running daemon: if the user passed
      // `--llama-server` (or any other binary-resolving flag) on
      // *this* invocation but the daemon was started earlier
      // without it, the flag would be silently dropped. Detect the
      // mismatch and restart the daemon with the new args — but
      // only when no managed launches are running, so we never kill
      // someone else's in-flight model on a stale daemon.
      reconcile_binary_with_running_daemon(client, cli, config, &attach_dir).await
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
  // backend (missing `llama-server`, missing/blocked Lemonade umbrella). The
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

/// Detect a stale `llama-server` binding on an already-running
/// daemon and reconcile it. When `cli.llama_server` is set:
///  1. Query `status` to find the daemon's current `server_path`.
///  2. Resolve the CLI flag to its canonical path.
///  3. If they differ AND no model is currently managed, send
///     `shutdown` and re-spawn the daemon with the new args.
///  4. If launches are running, log a warning and keep the existing
///     connection — the user can stop the launches and re-run.
///
/// All other states (flag unset, daemon already correct, query
/// failed) pass through with the original client.
async fn reconcile_binary_with_running_daemon(
  mut client: Client,
  cli: &Cli,
  config: &Config,
  attach_dir: &std::path::Path,
) -> Result<Client, CliExit> {
  let Some(cli_binary) = cli.llama_server.as_ref() else {
    return Ok(client);
  };
  // `tokio::fs::canonicalize` so the reconcile gate doesn't block
  // the async runtime on a slow inode lookup (network mounts,
  // unresponsive filesystems).
  let desired = match tokio::fs::canonicalize(cli_binary).await {
    Ok(p) => p,
    Err(_) => {
      // Let the daemon-side locator surface the path error later;
      // for the reconcile gate, fall back to the raw flag value so
      // a missing file doesn't silently bypass the check.
      cli_binary.clone()
    }
  };
  let status = match client.call("status", None).await {
    Ok(v) => v,
    Err(_) => return Ok(client),
  };
  let current = status
    .pointer("/daemon/server_path")
    .and_then(|v| v.as_str())
    .map(std::path::PathBuf::from);
  let running = status
    .get("models")
    .and_then(|v| v.as_array())
    .map(|a| !a.is_empty())
    .unwrap_or(false);
  // Canonicalize both sides so the comparison is stable across
  // platforms. Windows `fs::canonicalize` returns a `\\?\C:\…`
  // UNC-prefixed path; the daemon stores `server_path` as the raw
  // input from CLI/env, which lacks the prefix. Comparing them
  // verbatim falsely triggers a restart on every dispatch and the
  // spawned re-exec panics because the test binary has no daemon
  // entry point.
  //
  // Use `tokio::fs::canonicalize` here too so the reconcile gate
  // doesn't block the async runtime on a slow inode lookup — same
  // reasoning as `desired` above.
  let current_canon = match current.as_ref() {
    Some(c) => tokio::fs::canonicalize(c).await.ok(),
    None => None,
  };
  let matches = match (current_canon.as_ref(), current.as_ref()) {
    (Some(canon), _) => canon == &desired,
    (None, Some(raw)) => raw == &desired,
    (None, None) => false,
  };
  if matches {
    return Ok(client);
  }
  if running {
    log::warn!(
      "daemon already running with a different llama-server ({}); ignored \
       `--llama-server {}` because managed launches are active. Stop them \
       and re-run to apply.",
      current
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "—".into()),
      cli_binary.display()
    );
    return Ok(client);
  }
  log::info!(
    "daemon: restarting to adopt `--llama-server {}` (was: {})",
    cli_binary.display(),
    current
      .as_ref()
      .map(|p| p.display().to_string())
      .unwrap_or_else(|| "—".into())
  );
  // Trigger shutdown on the existing daemon, then re-spawn with the
  // CLI flag flowing through `build_spawn_options`.
  let _ = client.call("shutdown", None).await;
  drop(client);
  await_socket_gone(attach_dir, Duration::from_secs(3)).await;
  let opts = build_spawn_options(cli, config)?;
  let attach_for_poll = opts.state_dir.clone();
  match start_detached(opts) {
    Ok(_) => await_socket(&attach_for_poll, Duration::from_secs(3)).await,
    Err(e) => Err(CliExit::new(
      DAEMON_UNREACHABLE,
      format!("daemon: restart for --llama-server failed: {e}"),
    )),
  }
}

/// Poll until the daemon stops responding (or `total` elapses). Used
/// after `shutdown` so the follow-up `start_detached` doesn't race
/// with the old daemon's runtime.json teardown.
async fn await_socket_gone(attach_dir: &std::path::Path, total: Duration) {
  let deadline = std::time::Instant::now() + total;
  while std::time::Instant::now() < deadline {
    if Client::connect(attach_dir).await.is_err() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
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
  super::daemon::build_options(
    None, None, false, false, None, false, false, false, false, cli, config,
  )
  .map_err(|e| CliExit::new(DAEMON_UNREACHABLE, format!("daemon: build options: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

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
