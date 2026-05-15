//! `llamatui logs <target> [--follow] [-n N]`.
//!
//! Polling-based follower: the daemon's `logs_tail` returns a tail
//! snapshot, so `--follow` polls every 250 ms and prints any line that
//! is new (de-duped against the last snapshot's tail). SIGINT lets
//! the caller exit cleanly with code 0; SIGPIPE during a pipe like
//! `... | head` also exits 0 because the user has all the output they
//! asked for.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, LogsArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, DAEMON_UNREACHABLE, SUCCESS};
use crate::cli::resolve::{fetch_status, resolve_running};
use crate::config::Config;
use crate::ipc::{Client, ClientError};

const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);
/// Memory ceiling for the dedupe window — lines older than this drop
/// out of the "have I seen this already?" tracker.
const SEEN_WINDOW: usize = 1024;

pub async fn handle(args: LogsArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let snap = fetch_status(&mut client).await?;
  let row = resolve_running(&snap.models, &args.target)?;

  let initial_lines = args.lines.unwrap_or(200) as usize;
  let body = client
    .call(
      "logs_tail",
      Some(json!({"launch_id": &row.launch_id, "lines": initial_lines})),
    )
    .await
    .map_err(CliExit::from_client_error)?;
  let initial = extract_lines(&body);
  for l in &initial {
    safe_println(l)?;
  }

  if !args.follow {
    return Ok(());
  }

  // Track the last N lines so duplicate rows from the rolling tail
  // don't print twice. A small fixed window is enough — the daemon's
  // ring buffer is 4 K lines and our window only needs to cover the
  // overlap between two polls.
  let mut seen: VecDeque<String> = VecDeque::with_capacity(SEEN_WINDOW);
  for l in initial {
    push_seen(&mut seen, l);
  }

  // SIGINT (Ctrl-C) handling: the tokio signal future returns once,
  // and we treat that as "user asked to detach" — clean exit 0.
  let sigint = tokio::signal::ctrl_c();
  tokio::pin!(sigint);

  loop {
    tokio::select! {
      _ = &mut sigint => {
        return Ok(());
      }
      _ = tokio::time::sleep(FOLLOW_INTERVAL) => {
        match poll_tail(&mut client, &row.launch_id).await {
          Ok(lines) => {
            for l in lines {
              if seen.contains(&l) {
                continue;
              }
              safe_println(&l)?;
              push_seen(&mut seen, l);
            }
          }
          Err(ClientError::Connect(_)) | Err(ClientError::Frame(_)) => {
            // Connect failure = socket missing; Frame failure =
            // peer hung up mid-response. Both mean the daemon is no
            // longer there from the follower's POV, so collapse to
            // DAEMON_UNREACHABLE so scripts can branch reliably.
            return Err(CliExit::new(
              DAEMON_UNREACHABLE,
              format!("daemon disconnected (launch {})", row.launch_id),
            ));
          }
          Err(other) => return Err(CliExit::from_client_error(other)),
        }
      }
    }
  }
}

async fn poll_tail(client: &mut Client, launch_id: &str) -> Result<Vec<String>, ClientError> {
  let body = client
    .call(
      "logs_tail",
      Some(json!({"launch_id": launch_id, "lines": 200})),
    )
    .await?;
  Ok(extract_lines(&body))
}

fn extract_lines(body: &Value) -> Vec<String> {
  body
    .get("lines")
    .and_then(Value::as_array)
    .map(|a| {
      a.iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
    })
    .unwrap_or_default()
}

/// Print one line. A `BrokenPipe` from `print!` / `println!` (i.e.
/// the consumer of `... | head` closed early) is treated as a clean
/// exit: the user has all the output they asked for, exit 0.
fn safe_println(line: &str) -> CliResult {
  use std::io::Write;
  let mut stdout = std::io::stdout().lock();
  match writeln!(stdout, "{line}") {
    Ok(()) => Ok(()),
    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(CliExit::code_only(SUCCESS)),
    Err(e) => Err(CliExit::new(
      crate::cli::exit_codes::UNKNOWN,
      format!("write stdout: {e}"),
    )),
  }
}

fn push_seen(buf: &mut VecDeque<String>, line: String) {
  if buf.len() >= SEEN_WINDOW {
    buf.pop_front();
  }
  buf.push_back(line);
}
