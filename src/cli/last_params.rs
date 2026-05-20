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
  print!("{}", render_last_params_human(&rows));
  Ok(())
}

/// Pure renderer for `last-params` human output. Empty rows surface a
/// dim sentinel; non-empty rows pad on TTY and emit byte-stable TSV
/// when piped. Extracted so unit tests can pin both branches without
/// driving a live IPC client.
fn render_last_params_human(rows: &[Value]) -> String {
  use crate::cli::{colors, format};
  if rows.is_empty() {
    return format!(
      "{}\n",
      colors::dim("(no recorded last-params; launch a model to populate)")
    );
  }
  let tty = console::colors_enabled();
  let header = ["MODEL", "CTX", "REASONING", "ADVANCED"];
  let table_rows: Vec<Vec<String>> = rows
    .iter()
    .map(|r| {
      let path = crate::cli::output::row_path(r).unwrap_or("?");
      let params = r.get("params");
      let ctx = params
        .and_then(|p| p.get("ctx"))
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".into());
      let reasoning_raw = params
        .and_then(|p| p.get("reasoning"))
        .and_then(Value::as_bool)
        .map(|b| if b { "on" } else { "off" }.to_string())
        .unwrap_or_else(|| "-".into());
      let reasoning = colors::reasoning_cell(&reasoning_raw);
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
      vec![
        if tty {
          colors::path(path)
        } else {
          path.to_string()
        },
        ctx,
        reasoning,
        advanced,
      ]
    })
    .collect();
  let mut out = format::table(&header, &table_rows);
  if tty {
    out.push_str(&colors::count(rows.len(), "models"));
    out.push('\n');
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::test_lock::serialize;
  use std::sync::MutexGuard;

  struct ColorGuard {
    _lock: MutexGuard<'static, ()>,
    prior: bool,
  }

  impl ColorGuard {
    fn set(enabled: bool) -> Self {
      let g = Self {
        _lock: serialize(),
        prior: console::colors_enabled(),
      };
      console::set_colors_enabled(enabled);
      g
    }
  }

  impl Drop for ColorGuard {
    fn drop(&mut self) {
      console::set_colors_enabled(self.prior);
    }
  }

  fn row(path: &str, ctx: Option<u64>, reasoning: Option<bool>, advanced: &[&str]) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(c) = ctx {
      params.insert("ctx".into(), json!(c));
    }
    if let Some(r) = reasoning {
      params.insert("reasoning".into(), json!(r));
    }
    if !advanced.is_empty() {
      params.insert(
        "advanced".into(),
        Value::Array(advanced.iter().map(|s| json!(s)).collect()),
      );
    }
    json!({"id": {"path": path}, "params": Value::Object(params)})
  }

  #[test]
  fn render_last_params_human_empty_returns_dim_sentinel() {
    let _g = ColorGuard::set(false);
    let out = render_last_params_human(&[]);
    assert_eq!(
      out,
      "(no recorded last-params; launch a model to populate)\n"
    );
  }

  #[test]
  fn render_last_params_human_tsv_branch_is_byte_stable() {
    let _g = ColorGuard::set(false);
    let rows = vec![
      row("/m/qwen.gguf", Some(32768), Some(true), &["--threads", "8"]),
      row("/m/phi.gguf", None, Some(false), &[]),
    ];
    let out = render_last_params_human(&rows);
    assert_eq!(
      out,
      "MODEL\tCTX\tREASONING\tADVANCED\n\
       /m/qwen.gguf\t32768\ton\t--threads 8\n\
       /m/phi.gguf\t-\toff\t\n"
    );
  }

  #[test]
  fn render_last_params_human_tty_branch_pads_and_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let rows = vec![row("/m/qwen.gguf", Some(32768), Some(true), &[])];
    let out = render_last_params_human(&rows);
    let plain = console::strip_ansi_codes(&out);
    assert!(plain.starts_with("MODEL"), "header missing: {plain:?}");
    assert!(
      !plain.contains("MODEL\t"),
      "padded layout must not contain tabs: {plain:?}"
    );
    assert!(plain.contains("(1 models)"), "footer missing: {plain:?}");
  }
}
