//! `llamatui stop <target>` / `llamatui stop --all`.
//!
//! `<target>` is a launch id (`L3`) or a port; `--all` stops every
//! managed launch after a y/n prompt (skipped with `--yes`).

use std::io::{self, Write};

use serde_json::json;

use crate::cli::cli_args::{Cli, StopArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, STOP_FAILED, USAGE};
use crate::cli::resolve::{fetch_status, resolve_running, RunningRow};
use crate::config::Config;

pub async fn handle(args: StopArgs, cli: &Cli, config: &Config) -> CliResult {
  if !args.all && args.target.is_none() {
    return Err(CliExit::new(USAGE, "stop requires <target> or --all"));
  }
  let mut client = connect_or_spawn(cli, config).await?;

  if args.all {
    let snap = fetch_status(&mut client).await?;
    if snap.models.is_empty() {
      println!("stop --all: no managed launches");
      return Ok(());
    }
    if !args.yes && !confirm(&snap.models)? {
      println!("stop --all: cancelled");
      return Ok(());
    }
    let resp = client
      .call("stop_all", None)
      .await
      .map_err(|e| CliExit::new(STOP_FAILED, format!("stop_all: {e}")))?;
    let stopped = resp
      .get("stopped")
      .and_then(|v| v.as_array())
      .map(|a| a.len())
      .unwrap_or(0);
    println!("stop --all: stopped {stopped} launch(es)");
    return Ok(());
  }

  let target = args.target.expect("checked above");
  let snap = fetch_status(&mut client).await?;
  let row = resolve_running(&snap.models, &target)?;
  let resp = client
    .call("stop_model", Some(json!({"launch_id": &row.launch_id})))
    .await
    .map_err(|e| CliExit::new(STOP_FAILED, format!("stop_model {}: {e}", row.launch_id)))?;
  let state = resp
    .get("state")
    .and_then(|s| s.get("state"))
    .and_then(|s| s.as_str())
    .unwrap_or("stopped");
  println!("stopped {} → {state}", row.launch_id);
  Ok(())
}

fn confirm(models: &[RunningRow]) -> Result<bool, CliExit> {
  print!("stop {n} managed launch(es)? [y/N] ", n = models.len());
  io::stdout()
    .flush()
    .map_err(|e| CliExit::new(STOP_FAILED, format!("flush stdout: {e}")))?;
  let mut buf = String::new();
  io::stdin()
    .read_line(&mut buf)
    .map_err(|e| CliExit::new(STOP_FAILED, format!("read stdin: {e}")))?;
  let answer = buf.trim().to_lowercase();
  Ok(matches!(answer.as_str(), "y" | "yes"))
}
