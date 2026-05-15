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
    Ok(client) => Ok(client),
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

/// Strict variant — never spawns. Used by `daemon stop` / `daemon
/// status` where "not running" is a meaningful state worth surfacing
/// rather than papering over.
pub async fn connect_or_fail(socket: &std::path::Path) -> Result<Client, CliExit> {
  Client::connect(socket).await.map_err(|e| match e {
    ClientError::Connect(_) => CliExit::new(
      DAEMON_UNREACHABLE,
      format!("daemon: not running (socket: {})", socket.display()),
    ),
    other => CliExit::from_client_error(other),
  })
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
