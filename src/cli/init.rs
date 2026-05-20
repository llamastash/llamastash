//! `llamastash init` CLI handler. Thin shim into `init::wizard::run`
//! so the wizard's body can land in Unit 10 without touching the
//! dispatcher again.

use crate::cli::cli_args::{Cli, InitArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;

pub async fn handle(args: InitArgs, cli: &Cli, config: &Config) -> CliResult {
  crate::init::wizard::run(args, cli, config).await
}
