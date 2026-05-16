use anyhow::Result;
use clap::Parser;
use llamadash::{
  cli::{self, Cli},
  config::loader,
  util::logging,
};

#[tokio::main]
async fn main() -> Result<()> {
  let cli = Cli::parse();

  // Logger must be initialised BEFORE the panic hook — `log::error!` inside
  // the hook is a silent no-op while no logger is registered, so a panic
  // during CLI parsing/early startup would otherwise leave no trace in the
  // log file. Both calls are best-effort: a missing log dir or an already
  // initialised logger shouldn't block CLI use.
  let _ = logging::init(cli.verbose);
  logging::install_panic_hook();

  let config = loader::load_config(cli.config.clone());
  let code = cli::dispatch(cli, config).await?;
  std::process::exit(code);
}
