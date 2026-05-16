//! `llamatui presets <model-ref> {list|save|delete|show}`.
//!
//! Wraps the daemon's `presets_*` IPC surface. Resolves the model
//! reference once and threads the canonical path to every method;
//! the daemon recomputes `ModelId` from the GGUF header itself.

use serde_json::{json, Value};

use crate::cli::cli_args::{
  Cli, LaunchMode as CliLaunchMode, PresetsAction, PresetsArgs, ReasoningFlag,
};
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
        println!(
          "{}",
          pretty_json(&serde_json::json!({"presets": arr}))
        );
      } else if arr.is_empty() {
        println!("(no presets for {})", row.name());
      } else {
        println!("NAME\tCTX\tREASONING\tEXTRA");
        for preset in &arr {
          let name = preset.get("name").and_then(Value::as_str).unwrap_or("?");
          let p = preset.get("params");
          let ctx = p
            .and_then(|p| p.get("ctx"))
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
          let reasoning = p
            .and_then(|p| p.get("reasoning"))
            .and_then(Value::as_bool)
            .map(|b| if b { "on" } else { "off" }.to_string())
            .unwrap_or_else(|| "-".into());
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
          println!("{name}\t{ctx}\t{reasoning}\t{extra}");
        }
      }
      Ok(())
    }
    PresetsAction::Show {
      name,
      json: _as_json,
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
      println!("{}", pretty_json(&body["preset"]));
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
        println!("removed preset `{name}` for {}", row.name());
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
        payload.insert("mode".into(), json!(mode_label(m)));
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

fn mode_label(m: CliLaunchMode) -> &'static str {
  match m {
    CliLaunchMode::Chat => "chat",
    CliLaunchMode::Embedding => "embedding",
    CliLaunchMode::Rerank => "rerank",
  }
}
