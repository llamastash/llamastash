//! Daemon connection helper shared by every non-interactive
//! subcommand (Unit 8).
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
use crate::daemon::{start_detached, DaemonOptions, StartOutcome};
use crate::ipc::{Client, ClientError};
use crate::util::paths::runtime_socket_path;

/// Connect to the daemon. Auto-spawns it (via `daemon::start_detached`)
/// when the socket isn't connectable and `cli.no_spawn` is false.
/// Returns a `CliExit` shaped to the canonical exit codes so the
/// caller doesn't have to map errors a second time.
pub async fn connect_or_spawn(cli: &Cli, config: &Config) -> Result<Client, CliExit> {
  let socket = runtime_socket_path();
  match Client::connect(&socket).await {
    Ok(client) => {
      // Reconcile against the running daemon: if the user passed
      // `--llama-server` (or any other binary-resolving flag) on
      // *this* invocation but the daemon was started earlier
      // without it, the flag would be silently dropped. Detect the
      // mismatch and restart the daemon with the new args — but
      // only when no managed launches are running, so we never kill
      // someone else's in-flight model on a stale daemon.
      reconcile_binary_with_running_daemon(client, cli, config, &socket).await
    }
    Err(ClientError::Connect(_)) => {
      if cli.no_spawn {
        return Err(CliExit::new(
          DAEMON_UNREACHABLE,
          format!(
            "daemon: not running and --no-spawn was passed (socket: {})",
            socket.display()
          ),
        ));
      }
      let opts = build_spawn_options(cli, config)?;
      let socket_for_poll = opts.socket_path.clone();
      match start_detached(opts) {
        Ok(StartOutcome::RanToCompletion) | Ok(StartOutcome::AlreadyRunning(_)) => {
          // Brief settle window: start_detached returns once the
          // socket is connectable, but a second client opening at
          // the same instant occasionally trips an EAGAIN. One
          // retry is enough.
          await_socket(&socket_for_poll, Duration::from_secs(2)).await
        }
        Err(e) => Err(CliExit::new(
          DAEMON_UNREACHABLE,
          format!("daemon: auto-spawn failed: {e}"),
        )),
      }
    }
    Err(other) => Err(CliExit::from_client_error(other)),
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
  socket: &std::path::Path,
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
  let matches = current.as_ref().map(|c| c == &desired).unwrap_or(false);
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
  await_socket_gone(socket, Duration::from_secs(3)).await;
  let opts = build_spawn_options(cli, config)?;
  let socket_for_poll = opts.socket_path.clone();
  match start_detached(opts) {
    Ok(_) => await_socket(&socket_for_poll, Duration::from_secs(3)).await,
    Err(e) => Err(CliExit::new(
      DAEMON_UNREACHABLE,
      format!("daemon: restart for --llama-server failed: {e}"),
    )),
  }
}

/// Poll until the socket file disappears (or `total` elapses).
/// Used after `shutdown` so the follow-up `start_detached` doesn't
/// race with the old daemon's socket teardown.
async fn await_socket_gone(socket: &std::path::Path, total: Duration) {
  let deadline = std::time::Instant::now() + total;
  while std::time::Instant::now() < deadline {
    if Client::connect(socket).await.is_err() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

async fn await_socket(socket: &std::path::Path, total: Duration) -> Result<Client, CliExit> {
  let deadline = std::time::Instant::now() + total;
  let mut last_err: Option<ClientError> = None;
  while std::time::Instant::now() < deadline {
    match Client::connect(socket).await {
      Ok(c) => return Ok(c),
      Err(e) => last_err = Some(e),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  Err(match last_err {
    Some(e) => CliExit::from_client_error(e),
    None => CliExit::new(DAEMON_UNREACHABLE, "daemon: socket never appeared"),
  })
}

fn build_spawn_options(cli: &Cli, config: &Config) -> Result<DaemonOptions, CliExit> {
  // Mirror `daemon start`'s composition (state-dir / socket / discovery
  // roots / binary / port range) so a CLI auto-spawn produces the same
  // daemon a user would have hand-typed.
  super::daemon::build_options(None, None, cli, config)
    .map_err(|e| CliExit::new(DAEMON_UNREACHABLE, format!("daemon: build options: {e}")))
}
