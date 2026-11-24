//! `llamastash status` — managed launches + GPU snapshot.
//!
//! Optional `<target>` filters to a single launch (matched as port
//! number or `LaunchId`). `--json` mirrors the daemon's `status` wire
//! shape so agents that already consume it keep working.

use crate::cli::cli_args::{Cli, StatusArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::CliResult;
use crate::cli::output::{pretty_json, status_human, status_json};
use crate::cli::resolve::{fetch_status, resolve_running_via_catalog, StatusSnapshot};
use crate::config::Config;

pub async fn handle(args: StatusArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let snap = fetch_status(&mut client).await?;
  let scoped = match &args.target {
    Some(t) => StatusSnapshot {
      models: vec![resolve_running_via_catalog(&mut client, &snap.models, t).await?],
      external: vec![],
      gpu: snap.gpu.clone(),
      host: snap.host.clone(),
      daemon: snap.daemon.clone(),
      // `status <target>` filters the launches list but doesn't redact
      // daemon-level surfaces.
      proxy: snap.proxy.clone(),
      backends: snap.backends.clone(),
      servers: snap.servers.clone(),
    },
    None => snap,
  };
  if args.json {
    println!("{}", pretty_json(&status_json(&scoped)));
  } else {
    print!("{}", status_human(&scoped));
  }
  Ok(())
}
