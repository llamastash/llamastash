//! `llamastash last-params [<ref>] [--json]`.
//!
//! Surfaces the daemon's `last_params_list` IPC method on the CLI so
//! an agent can answer "what params did I last successfully start
//! this model with?" without scraping the TUI. Closes the agent-
//! native review finding that `last_params_list` was reachable from
//! the TUI only.

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, LastParamsArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, USAGE};
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_catalog, resolve_model};
use crate::config::Config;

pub async fn handle(args: LastParamsArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let body = client
    .call("last_params_list", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let mut rows: Vec<Value> = body
    .get("last_params")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default();

  if let Some(target) = args.target.as_ref() {
    // Resolve against the catalog so users can pass a name substring
    // / path / canonical id like every other subcommand.
    let catalog = fetch_catalog(&mut client).await?;
    let row = resolve_model(&catalog, target)?;
    rows.retain(|r| {
      crate::cli::output::row_path(r)
        .map(|p| p == row.path)
        .unwrap_or(false)
    });
    if rows.is_empty() {
      return Err(CliExit::new(
        USAGE,
        format!(
          "no recorded last-params for `{}`; launch it once to populate",
          row.name()
        ),
      ));
    }
  }

  if args.json {
    println!("{}", pretty_json(&json!({"last_params": rows})));
    return Ok(());
  }
  // Human form: one row per model. Columns chosen to match what an
  // operator typically needs to relaunch (ctx, reasoning, advanced).
  if rows.is_empty() {
    println!(
      "{}",
      crate::cli::colors::dim("(no recorded last-params; launch a model to populate)")
    );
    return Ok(());
  }
  println!(
    "{}",
    crate::cli::colors::bold("MODEL\tCTX\tREASONING\tADVANCED")
  );
  for r in &rows {
    let path = crate::cli::output::row_path(r).unwrap_or("?");
    let params = r.get("params");
    let ctx = params
      .and_then(|p| p.get("ctx"))
      .and_then(Value::as_u64)
      .map(|n| n.to_string())
      .unwrap_or_else(|| "-".into());
    let reasoning = params
      .and_then(|p| p.get("reasoning"))
      .and_then(Value::as_bool)
      .map(|b| if b { "on" } else { "off" }.to_string())
      .unwrap_or_else(|| "-".into());
    let advanced = params
      .and_then(|p| p.get("advanced"))
      .and_then(Value::as_array)
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(str::to_string))
          .collect::<Vec<_>>()
          .join(" ")
      })
      .unwrap_or_default();
    println!("{path}\t{ctx}\t{reasoning}\t{advanced}");
  }
  Ok(())
}
