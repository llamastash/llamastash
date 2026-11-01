//! `llamastash delete <model>` — remove one model's files through the daemon.
//!
//! The daemon plans and does the removal, so this command never touches the
//! files itself. What the prompt shows is the daemon's own plan, and the
//! confirm sends that plan back: if it has grown by the time the delete runs,
//! the daemon refuses and nothing goes.
//!
//! The same TTY guard `stop --all` uses: no terminal without `--yes` is a
//! refusal, not a prompt that reads EOF and quietly does nothing.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, DeleteArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, DELETE_FAILED, USAGE};
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_catalog, resolve_model};
use crate::config::Config;
use crate::ipc::Client;

pub async fn handle(args: DeleteArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let rows = fetch_catalog(&mut client).await?;
  let row = resolve_model(&rows, &args.model)?;
  let path = PathBuf::from(&row.path);

  let mut params = json!({ "path": path });
  if !args.yes {
    if args.json || !io::stdin().is_terminal() {
      return Err(CliExit::new(
        USAGE,
        "delete in a non-interactive context requires --yes",
      ));
    }
    let plan = plan_of(&mut client, &path).await?;
    if !cli.quiet {
      for line in plan_lines(&plan) {
        println!("{line}");
      }
    }
    if !confirm(&row.name())? {
      if !cli.quiet {
        println!("delete: cancelled");
      }
      return Ok(());
    }
    params["expect"] = json!({
      "files": plan["files"],
      "repo_dir": plan["repo_dir"],
    });
  }

  let reply = client
    .call("delete_model", Some(params))
    .await
    .map_err(|e| CliExit::new(DELETE_FAILED, format!("delete: {e}")))?;
  if args.json {
    println!(
      "{}",
      pretty_json(&json!({
        "deleted": true,
        "path": reply["path"],
        "files": reply["files"],
        "repo_dir": reply["repo_dir"],
        "message": reply["message"],
      }))
    );
  } else if !cli.quiet {
    use crate::cli::colors;
    println!(
      "{} {}",
      colors::success("deleted"),
      colors::path(&path.display().to_string())
    );
  }
  Ok(())
}

/// The daemon's answer to `plan_only`, which is what the prompt shows and what
/// the confirm sends back as `expect`.
async fn plan_of(client: &mut Client, path: &Path) -> Result<Value, CliExit> {
  client
    .call(
      "delete_model",
      Some(json!({ "path": path, "plan_only": true })),
    )
    .await
    .map_err(|e| CliExit::new(DELETE_FAILED, format!("delete: {e}")))
}

/// The prompt block: the daemon's own sentence about the plan, then every file
/// it unlinks, then the cache repo when the whole directory goes.
fn plan_lines(plan: &Value) -> Vec<String> {
  let mut lines = Vec::new();
  if let Some(description) = plan.get("description").and_then(Value::as_str) {
    lines.push(description.to_string());
  }
  let files = plan.get("files").and_then(Value::as_array);
  lines.extend(
    files
      .into_iter()
      .flatten()
      .filter_map(Value::as_str)
      .map(|file| format!("  {file}")),
  );
  if let Some(repo_dir) = plan.get("repo_dir").and_then(Value::as_str) {
    lines.push(format!("  {repo_dir} (the whole directory)"));
  }
  lines
}

/// The `[y/N]` prompt, on the shared stdin reader.
fn confirm(name: &str) -> Result<bool, CliExit> {
  crate::cli::picker::confirm_line(
    &mut io::stdin(),
    &mut io::stdout(),
    &format!("delete {name}? [y/N] "),
  )
  .map_err(|e| CliExit::new(DELETE_FAILED, format!("confirm: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  /// What the prompt shows comes from the daemon's reply, so the two shapes it
  /// can hold — a file list, and a whole cache repo — both have to read.
  #[test]
  fn plan_lines_show_the_description_the_files_and_the_repo() {
    let files = plan_lines(&json!({
      "description": "Delete `qwen.gguf` from disk? 3 files go: the model plus its mmproj projector.",
      "files": ["/m/qwen.gguf", "/m/mmproj-qwen-f16.gguf"],
      "repo_dir": null,
    }));
    assert_eq!(
      files,
      vec![
        "Delete `qwen.gguf` from disk? 3 files go: the model plus its mmproj projector.",
        "  /m/qwen.gguf",
        "  /m/mmproj-qwen-f16.gguf",
      ]
    );

    let repo = plan_lines(&json!({
      "description": "Delete `qwen.gguf` from disk?",
      "files": ["/cache/models--o--r/snapshots/rev/qwen.gguf"],
      "repo_dir": "/cache/models--o--r",
    }));
    assert_eq!(
      repo.last().expect("lines"),
      "  /cache/models--o--r (the whole directory)"
    );
  }
}
