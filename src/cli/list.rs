//! `llamastash list` — enumerate discovered models.
//!
//! TSV-by-default output keeps it pipe-friendly; `--json` emits the
//! stable agent-facing array. `--filter` is a substring matched
//! against name, path, arch, and quant (mirrors the TUI's `/`).

use crate::cli::cli_args::{Cli, ListArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::CliResult;
use crate::cli::output::{filter_rows, list_human, list_json, pretty_json};
use crate::cli::resolve::fetch_catalog;
use crate::config::Config;

pub async fn handle(args: ListArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let mut rows = fetch_catalog(&mut client).await?;
  if let Some(pat) = &args.filter {
    rows = filter_rows(&rows, pat);
  }
  if args.json {
    println!("{}", pretty_json(&list_json(&rows)));
  } else {
    print!("{}", list_human(&rows));
  }
  Ok(())
}
