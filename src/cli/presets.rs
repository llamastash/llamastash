//! `llamastash presets <model-ref> {list|save|delete|show}`.
//!
//! Wraps the daemon's `presets_*` IPC surface. Resolves the model
//! reference once and threads the canonical path to every method;
//! the daemon recomputes `ModelId` from the GGUF header itself.

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, PresetsAction, PresetsArgs, ReasoningFlag};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, USAGE};
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_catalog, resolve_model};
use crate::config::Config;

pub async fn handle(args: PresetsArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let rows = fetch_catalog(&mut client).await?;
  let row = resolve_model(&rows, &args.model)?;

  match args.action {
    PresetsAction::List { json: as_json } => {
      let body = client
        .call("presets_list", Some(json!({"model_path": &row.path})))
        .await
        .map_err(CliExit::from_client_error)?;
      let arr = body
        .get("presets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
      if as_json {
        // Wrapped object — same shape convention as `list --json`
        // (now `{"models": [...]}`) so agent parsers can rely on a
        // single "always object" rule across the CLI surface.
        println!("{}", pretty_json(&serde_json::json!({"presets": arr})));
      } else {
        print!("{}", render_presets_human(&arr, &row.name()));
      }
      Ok(())
    }
    PresetsAction::Show {
      name,
      json: as_json,
    } => {
      let body = client
        .call(
          "presets_show",
          Some(json!({"model_path": &row.path, "name": name})),
        )
        .await
        .map_err(CliExit::from_client_error)?;
      if body.get("preset").map(Value::is_null).unwrap_or(true) {
        return Err(CliExit::new(
          USAGE,
          format!("preset `{name}` not found for {}", row.name()),
        ));
      }
      if as_json {
        // Same wrapping convention as `presets list --json` /
        // `presets delete --json` — agents key on the `action` field
        // and read the preset body from `preset`.
        let out = json!({
          "action": "show",
          "model": row.name(),
          "name": name,
          "preset": body["preset"].clone(),
        });
        println!("{}", pretty_json(&out));
      } else {
        println!("{}", pretty_json(&body["preset"]));
      }
      Ok(())
    }
    PresetsAction::Delete {
      name,
      json: as_json,
    } => {
      let body = client
        .call(
          "presets_delete",
          Some(json!({"model_path": &row.path, "name": &name})),
        )
        .await
        .map_err(CliExit::from_client_error)?;
      let deleted = !body.get("removed").map(Value::is_null).unwrap_or(true);
      if !deleted {
        return Err(CliExit::new(
          USAGE,
          format!("preset `{name}` not found for {}", row.name()),
        ));
      }
      if as_json {
        let out = json!({
          "action": "delete",
          "name": name,
          "deleted": deleted,
          "model": row.name(),
        });
        println!("{}", pretty_json(&out));
      } else if !cli.quiet {
        println!(
          "{}",
          crate::cli::colors::success(&format!("removed preset `{name}` for {}", row.name()))
        );
      }
      Ok(())
    }
    PresetsAction::Save {
      name,
      ctx,
      port,
      reasoning,
      mode,
      extra,
      json: as_json,
    } => {
      if name.trim().is_empty() {
        return Err(CliExit::new(USAGE, "preset name must not be empty"));
      }
      let mut payload = serde_json::Map::new();
      payload.insert("model_path".into(), json!(&row.path));
      payload.insert("name".into(), json!(name));
      if let Some(c) = ctx {
        payload.insert("ctx".into(), json!(c));
      }
      if let Some(p) = port {
        payload.insert("port".into(), json!(p));
      }
      if let Some(r) = reasoning {
        payload.insert("reasoning".into(), json!(matches!(r, ReasoningFlag::On)));
      }
      if let Some(m) = mode {
        payload.insert("mode".into(), json!(m.as_label()));
      }
      let extras: Vec<String> = extra
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
      if !extras.is_empty() {
        payload.insert("advanced".into(), json!(extras));
      }
      let body = client
        .call("presets_save", Some(Value::Object(payload)))
        .await
        .map_err(CliExit::from_client_error)?;
      let replaced = body.get("replaced").map(|v| !v.is_null()).unwrap_or(false);
      let verb = if replaced { "replaced" } else { "saved" };
      if as_json {
        let out = json!({
          "action": "save",
          "name": name,
          "replaced": replaced,
          "model": row.name(),
        });
        println!("{}", pretty_json(&out));
      } else if !cli.quiet {
        println!("{verb} preset `{name}` for {}", row.name());
      }
      Ok(())
    }
  }
}

/// Pure renderer for `presets list` human output. Composes the empty
/// sentinel, the padded TTY table, and the byte-stable TSV branch in
/// one function so tests can drive both branches without an IPC stub.
/// `model_name` flows into the empty-state line; it's display-only.
fn render_presets_human(arr: &[Value], model_name: &str) -> String {
  use crate::cli::{colors, format};
  if arr.is_empty() {
    return format!(
      "{}\n",
      colors::dim(&format!("(no presets for {model_name})"))
    );
  }
  let tty = console::colors_enabled();
  let header = ["NAME", "CTX", "REASONING", "EXTRA"];
  let table_rows: Vec<Vec<String>> = arr
    .iter()
    .map(|preset| {
      let name = preset.get("name").and_then(Value::as_str).unwrap_or("?");
      let p = preset.get("params");
      let ctx = p
        .and_then(|p| p.get("ctx"))
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".into());
      let reasoning_raw = p
        .and_then(|p| p.get("reasoning"))
        .and_then(Value::as_bool)
        .map(|b| if b { "on" } else { "off" }.to_string())
        .unwrap_or_else(|| "-".into());
      let reasoning = colors::reasoning_cell(&reasoning_raw);
      let extra = p
        .and_then(|p| p.get("advanced"))
        .and_then(Value::as_array)
        .map(|a| {
          a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join(" ")
        })
        .unwrap_or_default();
      vec![name.to_string(), ctx, reasoning, extra]
    })
    .collect();
  let mut out = format::table(&header, &table_rows);
  if tty {
    out.push_str(&colors::count(arr.len(), "presets"));
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

  fn preset(name: &str, ctx: Option<u64>, reasoning: Option<bool>, advanced: &[&str]) -> Value {
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
    json!({"name": name, "params": Value::Object(params)})
  }

  #[test]
  fn render_presets_human_empty_returns_dim_sentinel() {
    let _g = ColorGuard::set(false);
    let out = render_presets_human(&[], "qwen-coder");
    assert_eq!(out, "(no presets for qwen-coder)\n");
  }

  #[test]
  fn render_presets_human_tsv_branch_is_byte_stable() {
    let _g = ColorGuard::set(false);
    let arr = vec![
      preset("coding", Some(32768), Some(true), &["--threads", "8"]),
      preset("default", None, Some(false), &[]),
    ];
    let out = render_presets_human(&arr, "qwen-coder");
    assert_eq!(
      out,
      "NAME\tCTX\tREASONING\tEXTRA\n\
       coding\t32768\ton\t--threads 8\n\
       default\t-\toff\t\n"
    );
  }

  #[test]
  fn render_presets_human_tty_branch_pads_and_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let arr = vec![preset("coding", Some(32768), Some(true), &[])];
    let out = render_presets_human(&arr, "qwen-coder");
    let plain = console::strip_ansi_codes(&out);
    assert!(plain.starts_with("NAME"), "header missing: {plain:?}");
    assert!(
      !plain.contains("NAME\t"),
      "padded layout must not contain tabs: {plain:?}"
    );
    assert!(plain.contains("(1 presets)"), "footer missing: {plain:?}");
  }
}
