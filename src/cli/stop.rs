//! `llamatui stop <target>` / `llamatui stop --all`.
//!
//! `<target>` is a launch id (`L3`) or a port; `--all` stops every
//! managed launch after a y/n prompt (skipped with `--yes`).
//!
//! When stdin isn't a TTY and `--yes` isn't passed, `--all` refuses
//! up front. An interactive prompt blocked on EOF was the previous
//! shape and silently no-op'd in piped contexts; that drift between
//! caller intent and observed outcome is the trap this TTY guard
//! closes.

use std::io::{self, IsTerminal, Read, Write};

use serde_json::json;

use crate::cli::cli_args::{Cli, StopArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, STOP_FAILED, USAGE};
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_status, resolve_running, ExternalRow, RunningRow};
use crate::config::Config;

pub async fn handle(args: StopArgs, cli: &Cli, config: &Config) -> CliResult {
  if !args.all && args.target.is_none() {
    return Err(CliExit::new(USAGE, "stop requires <target> or --all"));
  }
  let mut client = connect_or_spawn(cli, config).await?;
  let grace = args.grace_secs;

  if args.all {
    let snap = fetch_status(&mut client).await?;
    if snap.models.is_empty() {
      if args.json {
        println!(
          "{}",
          pretty_json(&serde_json::json!({"stopped": [], "count": 0}))
        );
      } else if !cli.quiet {
        println!("stop --all: no managed launches");
      }
      return Ok(());
    }
    if !args.yes && !confirm_or_refuse(&snap.models)? {
      if args.json {
        println!(
          "{}",
          pretty_json(&serde_json::json!({"stopped": [], "cancelled": true}))
        );
      } else if !cli.quiet {
        println!("stop --all: cancelled");
      }
      return Ok(());
    }
    let resp = client
      .call("stop_all", None)
      .await
      .map_err(|e| CliExit::new(STOP_FAILED, format!("stop_all: {e}")))?;
    let stopped_count = resp
      .get("stopped")
      .and_then(|v| v.as_array())
      .map(|a| a.len())
      .unwrap_or(0);
    if args.json {
      let body = serde_json::json!({
        "stopped": resp.get("stopped").cloned().unwrap_or(serde_json::Value::Array(Vec::new())),
        "count": stopped_count,
      });
      println!("{}", pretty_json(&body));
    } else if !cli.quiet {
      println!("stop --all: stopped {stopped_count} launch(es)");
    }
    return Ok(());
  }

  let target = args.target.expect("checked above");
  let snap = fetch_status(&mut client).await?;
  // External processes use `ext-<pid>` identifiers in `status` and
  // accept `stop_external` only (no edit/restart path). Try the
  // external snapshot first so a `stop ext-1234` doesn't get
  // disambiguated against the managed list and miss.
  if let Some(ext) = resolve_external(&snap.external, &target) {
    let mut params = serde_json::json!({ "pid": ext.pid });
    if let Some(g) = grace {
      params["grace_secs"] = serde_json::Value::from(g);
    }
    let resp = client
      .call("stop_external", Some(params))
      .await
      .map_err(|e| CliExit::new(STOP_FAILED, format!("stop_external pid={}: {e}", ext.pid)))?;
    let killed = resp
      .get("killed_with_sigkill")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    if args.json {
      let body = serde_json::json!({
        "pid": ext.pid,
        "killed_with_sigkill": killed,
      });
      println!("{}", pretty_json(&body));
    } else if !cli.quiet {
      println!(
        "stopped external pid {} → {}",
        ext.pid,
        if killed { "SIGKILL" } else { "SIGTERM" },
      );
    }
    return Ok(());
  }
  let row = resolve_running(&snap.models, &target)?;
  let mut params = serde_json::json!({"launch_id": &row.launch_id});
  if let Some(g) = grace {
    params["grace_secs"] = serde_json::Value::from(g);
  }
  let resp = client
    .call("stop_model", Some(params))
    .await
    .map_err(|e| CliExit::new(STOP_FAILED, format!("stop_model {}: {e}", row.launch_id)))?;
  let state = resp
    .get("state")
    .and_then(|s| s.get("state"))
    .and_then(|s| s.as_str())
    .unwrap_or("stopped");
  if args.json {
    let body = serde_json::json!({
      "launch_id": row.launch_id,
      "state": state,
    });
    println!("{}", pretty_json(&body));
  } else if !cli.quiet {
    println!("stopped {} → {state}", row.launch_id);
  }
  Ok(())
}

/// Match `target` against an external row. Accepted forms:
/// - `ext-<pid>` (the format `status` uses for the `launch_id`-like
///   identifier of external rows in the TUI surface),
/// - bare `<pid>` that also doesn't match a managed launch — the
///   caller checks managed first via [`resolve_running`] in the
///   primary path.
fn resolve_external(rows: &[ExternalRow], target: &str) -> Option<ExternalRow> {
  let needle = target.trim();
  if let Some(rest) = needle.strip_prefix("ext-") {
    if let Ok(pid) = rest.parse::<u64>() {
      return rows.iter().find(|r| r.pid == pid).cloned();
    }
  }
  if let Ok(pid) = needle.parse::<u64>() {
    return rows.iter().find(|r| r.pid == pid).cloned();
  }
  None
}

/// TTY-guarded confirmation. Refuses up front when stdin isn't a
/// terminal (the agent / piped / CI case) so the user has to opt in
/// to the destructive `--all` action via `--yes`, instead of seeing
/// the prompt silently no-op on EOF.
fn confirm_or_refuse(models: &[RunningRow]) -> Result<bool, CliExit> {
  if !io::stdin().is_terminal() {
    return Err(CliExit::new(
      USAGE,
      "stop --all in a non-interactive context requires --yes",
    ));
  }
  confirm_from(&mut io::stdin(), models)
}

/// Pure-stdin version of `confirm_or_refuse` so tests can drive both
/// branches without touching the real `stdin`.
pub(crate) fn confirm_from<R: Read>(input: &mut R, models: &[RunningRow]) -> Result<bool, CliExit> {
  print!("stop {n} managed launch(es)? [y/N] ", n = models.len());
  io::stdout()
    .flush()
    .map_err(|e| CliExit::new(STOP_FAILED, format!("flush stdout: {e}")))?;
  let mut buf = String::new();
  let mut reader = io::BufReader::new(input);
  reader
    .read_to_string(&mut buf)
    .map_err(|e| CliExit::new(STOP_FAILED, format!("read stdin: {e}")))?;
  let answer = buf.trim().to_lowercase();
  Ok(matches!(answer.as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  fn dummy_rows() -> Vec<RunningRow> {
    vec![RunningRow {
      launch_id: "L1".into(),
      model_path: "/m/a.gguf".into(),
      port: 41100,
      state: "ready".into(),
      pid: Some(1),
      mode: "chat".into(),
      ready_at: None,
    }]
  }

  #[test]
  fn confirm_from_accepts_y_yes() {
    let rows = dummy_rows();
    let mut input = Cursor::new(b"y\n".to_vec());
    assert!(confirm_from(&mut input, &rows).unwrap());
    let mut input = Cursor::new(b"yes\n".to_vec());
    assert!(confirm_from(&mut input, &rows).unwrap());
  }

  #[test]
  fn confirm_from_rejects_n_empty_or_other() {
    let rows = dummy_rows();
    for raw in [&b""[..], b"\n", b"n\n", b"No\n", b"maybe\n"] {
      let mut input = Cursor::new(raw.to_vec());
      assert!(
        !confirm_from(&mut input, &rows).unwrap(),
        "input {raw:?} should not confirm"
      );
    }
  }
}
