use anyhow::Result;
use llamastash::{cli, config::loader, util::logging};

#[tokio::main]
async fn main() -> Result<()> {
  // Translate `LLAMASTASH_OFFLINE=1`/`0`/empty into the `true`/unset clap's
  // boolean env binding accepts, before parsing argv (see the fn doc).
  cli::cli_args::normalize_offline_env();

  // Parse by hand so clap's arg-rejection exit code matches our contract:
  // a usage error exits USAGE (64), not clap's default 2. `--help` /
  // `--version` are not errors — clap writes them to stdout and we exit 0.
  // `parse_cli` also wires the `--no-colors` → `ColorChoice::Never` policy
  // for styled help, which has to be decided before clap renders --help.
  let cli = match cli::cli_args::parse_cli() {
    Ok(cli) => cli,
    Err(err) => {
      if !err.use_stderr() {
        let _ = err.print();
        std::process::exit(0);
      }
      if cli::cli_args::argv_wants_json() {
        let text = err.render().to_string();
        // The error is the first paragraph; the `Usage:` block follows a blank line.
        let head = text
          .lines()
          .take_while(|l| !l.trim().is_empty())
          .map(str::trim)
          .collect::<Vec<_>>()
          .join(" ");
        let message = head.strip_prefix("error: ").unwrap_or(&head);
        cli::output::print_json_error(cli::exit_codes::USAGE, message);
      } else {
        let _ = err.print();
      }
      std::process::exit(cli::exit_codes::USAGE);
    }
  };

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
