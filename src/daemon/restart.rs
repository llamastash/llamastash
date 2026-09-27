//! Client-side stop and restart of a running daemon, shared by `daemon stop`,
//! `daemon restart`, the TUI restart key, and the `--llama-server` re-spawn.

use std::{path::Path, time::Duration};

use anyhow::Result;

use super::{existing_daemon_pid, start_detached, DaemonOptions, StartOutcome};
use crate::ipc::{Client, ClientError};

/// How a [`shutdown_and_wait`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
  /// The daemon released its lockfile.
  Gone,
  /// `shutdown` was accepted but the process outlived the wait.
  StillExiting(i32),
}

/// Ask the daemon at `state_dir` to shut down, then wait until it releases its
/// lockfile. `Err` means the IPC call failed; `ClientError::Connect` there
/// means no daemon answered.
///
/// Waits on the lockfile, not the socket: the daemon drops its lockfile last,
/// after draining connections and stopping its children, so a start fired
/// when the socket goes quiet can still hit `AlreadyRunning`.
pub async fn shutdown_and_wait(state_dir: &Path) -> Result<ShutdownOutcome, ClientError> {
  let mut client = Client::connect(state_dir).await?;
  let resp = client.call("shutdown", None).await?;
  // Close the pooled keep-alive, or the daemon's drain waits on it.
  drop(client);
  // The longest child stop grace the daemon reported, plus a margin for its
  // own teardown, at least 10 s.
  let grace = resp
    .get("stop_grace_secs")
    .and_then(|v| v.as_u64())
    .unwrap_or(0);
  let deadline = std::time::Instant::now() + Duration::from_secs(grace.saturating_add(5).max(10));
  loop {
    match existing_daemon_pid(state_dir) {
      None => return Ok(ShutdownOutcome::Gone),
      Some(pid) if std::time::Instant::now() >= deadline => {
        return Ok(ShutdownOutcome::StillExiting(pid))
      }
      Some(_) => tokio::time::sleep(Duration::from_millis(50)).await,
    }
  }
}

/// Stop the daemon at `opts.state_dir` if one is running, then start a
/// detached one with `opts`. A daemon that did not exit in time comes back as
/// `StartOutcome::AlreadyRunning`.
pub async fn restart_detached(opts: DaemonOptions) -> Result<StartOutcome> {
  match shutdown_and_wait(&opts.state_dir).await {
    Ok(ShutdownOutcome::Gone) | Err(ClientError::Connect(_)) => {}
    Ok(ShutdownOutcome::StillExiting(pid)) => return Ok(StartOutcome::AlreadyRunning(pid)),
    Err(e) => log::warn!("restart: shutdown call failed: {e}"),
  }
  start_detached(opts)
}
