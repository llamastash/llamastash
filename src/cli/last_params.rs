//! `llamastash last-params [<ref>] [--json]`.
//!
//! Surfaces the daemon's `last_params_list` IPC method on the CLI so
//! an agent can answer "what params did I last successfully start
//! this model with?" without scraping the TUI.

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

/// Stringify a single knob value for the `ADVANCED` cell. Bools,
/// numbers, and strings render bare (`flash_attn:true`,
/// `threads:2`, `cache_type_k:f16`); fall back to JSON for anything
/// the typed-knob schema doesn't currently emit, so future field
/// additions degrade gracefully instead of producing `null`.
fn knob_value_str(v: &Value) -> String {
  match v {
    Value::String(s) => s.clone(),
    Value::Bool(b) => b.to_string(),
    Value::Number(n) => n.to_string(),
    _ => v.to_string(),
  }
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
  let header = ["MODEL", "CTX", "REASONING", "ADVANCED", "EXTRAS"];
  let table_rows: Vec<Vec<String>> = rows
    .iter()
    .map(|r| {
      let path = crate::cli::output::row_path(r).unwrap_or("?");
      let name = crate::util::paths::model_display_name(std::path::Path::new(path));
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
      // `ADVANCED` collapses every typed knob *except* `ctx` /
      // `reasoning` (those already get dedicated columns) into a
      // compact `k:v, k:v` list so the row stays readable while
      // surfacing the full knob set the user staged.
      let advanced = params
        .and_then(|p| p.get("knobs"))
        .and_then(Value::as_object)
        .map(|m| {
          // Sorted for stable diffs; the map keeps the order it arrived in.
          let mut knobs: Vec<_> = m
            .iter()
            .filter(|(k, v)| !v.is_null() && k.as_str() != "ctx" && k.as_str() != "reasoning")
            .collect();
          knobs.sort_by_key(|(k, _)| k.as_str());
          knobs
            .iter()
            .map(|(k, v)| format!("{k}:{}", knob_value_str(v)))
            .collect::<Vec<_>>()
            .join(", ")
        })
        .unwrap_or_default();
      let extras = params
        .and_then(|p| p.get("extras"))
        .and_then(Value::as_array)
        .map(|a| {
          a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join(" ")
        })
        .unwrap_or_default();
      vec![name, ctx, reasoning, advanced, extras]
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

  fn row(
    path: &str,
    ctx: Option<u64>,
    reasoning: Option<bool>,
    knobs: &[(&str, Value)],
    extras: &[&str],
  ) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(c) = ctx {
      params.insert("ctx".into(), json!(c));
    }
    if let Some(r) = reasoning {
      params.insert("reasoning".into(), json!(r));
    }
    if !knobs.is_empty() {
      let mut k = serde_json::Map::new();
      for (key, val) in knobs {
        k.insert((*key).into(), val.clone());
      }
      params.insert("knobs".into(), Value::Object(k));
    }
    if !extras.is_empty() {
      params.insert(
        "extras".into(),
        Value::Array(extras.iter().map(|s| json!(s)).collect()),
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
      row(
        "/m/qwen.gguf",
        Some(32768),
        Some(true),
        &[("threads", json!(8))],
        &["--foo", "bar"],
      ),
      row("/m/phi.gguf", None, Some(false), &[], &[]),
    ];
    let out = render_last_params_human(&rows);
    assert_eq!(
      out,
      "MODEL\tCTX\tREASONING\tADVANCED\tEXTRAS\n\
       qwen\t32768\ton\tthreads:8\t--foo bar\n\
       phi\t-\toff\t\t\n"
    );
  }

  #[test]
  fn render_last_params_human_advanced_skips_ctx_reasoning_and_joins_with_commas() {
    // String values render bare (no JSON quoting); the column is sorted
    // by key for stable diffs.
    let _g = ColorGuard::set(false);
    let rows = vec![row(
      "/m/qwen.gguf",
      Some(2048),
      Some(true),
      &[
        ("ctx", json!(2048)),
        ("reasoning", json!(true)),
        ("threads", json!(2)),
        ("cache_type_k", json!("f16")),
        ("flash_attn", json!(true)),
      ],
      &[],
    )];
    let out = render_last_params_human(&rows);
    let advanced = out.lines().nth(1).and_then(|l| l.split('\t').nth(3));
    assert_eq!(
      advanced,
      Some("cache_type_k:f16, flash_attn:true, threads:2")
    );
  }

  #[test]
  fn render_last_params_human_uses_file_stem_for_model_column() {
    // First column is the model name (file stem), not the full path —
    // keeps the table readable on terminals that don't wrap.
    let _g = ColorGuard::set(false);
    let rows = vec![row(
      "/home/u/.cache/hf/blobs/abcdef-gemma-3-12b-it-Q4_K_M.gguf",
      None,
      None,
      &[],
      &[],
    )];
    let out = render_last_params_human(&rows);
    let name = out.lines().nth(1).and_then(|l| l.split('\t').next());
    assert_eq!(name, Some("abcdef-gemma-3-12b-it-Q4_K_M"));
  }

  #[test]
  fn render_last_params_human_tty_branch_pads_and_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let rows = vec![row("/m/qwen.gguf", Some(32768), Some(true), &[], &[])];
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
