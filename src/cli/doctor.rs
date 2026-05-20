//! `llamastash doctor` CLI handler. Thin shim into `init::doctor::run`.

use crate::cli::cli_args::{Cli, DoctorArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;

pub async fn handle(args: DoctorArgs, cli: &Cli, config: &Config) -> CliResult {
  crate::init::doctor::run(args, cli, config).await
}
