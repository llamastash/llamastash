//! `llamadash pull` — placeholder for the v2 in-app HuggingFace pull
//! worker (R46).
//!
//! The subcommand surface is wired so callers see a stable shape, but
//! every action exits with `PULL_FAILED` (69) and a clear message
//! pointing at v2. The plan's `Scope Boundaries` / "v2 deferrals"
//! section is the authority.

use crate::cli::cli_args::{PullAction, PullArgs};
use crate::cli::exit_codes::{CliExit, CliResult, PULL_FAILED};

pub async fn handle(args: PullArgs) -> CliResult {
  let what = match args.action {
    PullAction::Start { repo, .. } => format!("pull start {repo}"),
    PullAction::Status { job_id, .. } => format!("pull status {job_id}"),
    PullAction::Cancel { job_id } => format!("pull cancel {job_id}"),
  };
  Err(CliExit::new(
    PULL_FAILED,
    format!("{what}: in-app HuggingFace pull is deferred to llamadash v2 (R46)"),
  ))
}
