//! CLI surface (clap definitions + dispatcher).
//!
//! The dispatcher is `async` because Unit 2's daemon client speaks
//! Tokio. Each subcommand has its own handler module under
//! `src/cli/`. Handlers return [`exit_codes::CliResult`] so the
//! top-level dispatcher can map structured failure into the
//! documented exit-code table without losing the message.

pub mod cli_args;
pub mod client;
pub mod daemon;
pub mod exit_codes;
pub mod favorites;
pub mod list;
pub mod logs;
pub mod output;
pub mod presets;
pub mod pull;
pub mod resolve;
pub mod start;
pub mod status;
pub mod stop;

use anyhow::Result;

use crate::config::loader::LoadedConfig;

#[allow(unused_imports)]
pub use cli_args::{
  Cli, Command, DaemonAction, FavoritesAction, LaunchMode, PresetsAction, PullAction, ReasoningFlag,
};
pub use exit_codes::{CliExit, CliResult};

/// Dispatch the parsed CLI to its handler. Returns the OS exit code
/// the binary should propagate. `main.rs` calls
/// `std::process::exit(code)` with the result.
pub async fn dispatch(mut cli: Cli, config: LoadedConfig) -> Result<i32> {
  if let Some(warning) = &config.warning {
    log::warn!("{warning}");
  }
  let command = cli.command.take();
  let resolved_config = &config.config;
  let outcome: CliResult = match command {
    None => handle_tui(&cli, resolved_config).await,
    Some(Command::Daemon(action)) => {
      map_anyhow(daemon::handle(action, &cli, resolved_config).await)
    }
    Some(Command::List(args)) => list::handle(args, &cli, resolved_config).await,
    Some(Command::Start(args)) => start::handle(args, &cli, resolved_config).await,
    Some(Command::Stop(args)) => stop::handle(args, &cli, resolved_config).await,
    Some(Command::Status(args)) => status::handle(args, &cli, resolved_config).await,
    Some(Command::Logs(args)) => logs::handle(args, &cli, resolved_config).await,
    Some(Command::Presets(args)) => presets::handle(args, &cli, resolved_config).await,
    Some(Command::Favorites(args)) => favorites::handle(args, &cli, resolved_config).await,
    // `pull` stays scaffolded; handler returns PULL_FAILED + an
    // explanatory message until R46 lands in v2.
    Some(Command::Pull(args)) => pull::handle(args).await,
  };
  Ok(report(outcome))
}

/// Translate an anyhow-bearing handler result into the CliResult
/// shape. The `daemon` subcommand still uses `anyhow::Result` for its
/// internal start/stop/status flow; we treat any anyhow error as a
/// `UNKNOWN` exit unless it's already a `CliExit`.
fn map_anyhow(r: Result<()>) -> CliResult {
  match r {
    Ok(()) => Ok(()),
    Err(e) => match e.downcast::<CliExit>() {
      Ok(exit) => Err(exit),
      Err(other) => Err(CliExit::new(exit_codes::UNKNOWN, format!("{other}"))),
    },
  }
}

/// Print any error message and return the exit code.
fn report(result: CliResult) -> i32 {
  match result {
    Ok(()) => exit_codes::SUCCESS,
    Err(exit) => {
      if let Some(msg) = &exit.message {
        eprintln!("{msg}");
      }
      exit.code
    }
  }
}

/// Entry point for the TUI (`llamatui` with no subcommand). Returns a
/// `CliResult` so the dispatcher's exit-code surface stays uniform;
/// any anyhow failure from the TUI runtime maps to `UNKNOWN`.
async fn handle_tui(_cli: &Cli, config: &crate::config::Config) -> CliResult {
  let socket = crate::util::paths::runtime_socket_path();
  match crate::tui::events::launch(config.theme, &socket).await {
    Ok(()) => Ok(()),
    Err(e) => Err(CliExit::new(exit_codes::UNKNOWN, format!("tui: {e}"))),
  }
}
