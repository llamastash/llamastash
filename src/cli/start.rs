//! `llamatui start <model-ref>` — launch a model.
//!
//! Resolution order (per the plan):
//! 1. resolve `<model-ref>` against the catalog (substring / path / id),
//! 2. layer the named preset's params (if `--preset NAME`) onto the
//!    daemon's last_params for this model,
//! 3. apply per-invocation overrides (`--ctx`, `--port`, `--reasoning`,
//!    `--mode`, and the trailing `-- <extra>` args),
//! 4. send `start_model` to the daemon and report the result.
//!
//! Mode resolution is strict: when the catalog reports `mode_hint =
//! unknown` and the user didn't pass `--mode`, we error out rather
//! than silently default to chat. The plan's `cli_args::StartArgs`
//! comment is the authority.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, LaunchMode as CliLaunchMode, ReasoningFlag, StartArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, BINARY_NOT_FOUND, LAUNCH_FAILED, USAGE};
use crate::cli::resolve::{fetch_catalog, resolve_model, CatalogRow};
use crate::config::Config;
use crate::ipc::Client;

pub async fn handle(args: StartArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let rows = fetch_catalog(&mut client).await?;
  let row = resolve_model(&rows, &args.model)?;

  // Mode: explicit override > catalog hint (unless `unknown`).
  let mode = resolve_mode(&row, args.mode)?;

  // Preset baseline → IPC params. `presets_show` returns the saved
  // params so the daemon doesn't have to re-resolve the model id.
  let mut params = if let Some(preset_name) = args.preset.as_ref() {
    fetch_preset_params(&mut client, &row.path, preset_name).await?
  } else {
    PartialParams::default()
  };

  if let Some(ctx) = args.ctx {
    params.ctx = Some(ctx);
  }
  if let Some(port) = args.port {
    params.port = Some(port);
  }
  if let Some(r) = args.reasoning {
    params.reasoning = Some(matches!(r, ReasoningFlag::On));
  }
  if !args.extra.is_empty() {
    params.advanced = args
      .extra
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
  }

  // Warning surfaces (kept best-effort; never block the launch):
  // ctx > native max should advise rather than refuse, per R12.
  if let (Some(ctx), Some(native)) = (params.ctx, row.native_ctx) {
    if (ctx as u64) > native {
      eprintln!(
        "warning: --ctx {ctx} exceeds native context length {native} for {}; the supervisor will still try",
        row.name()
      );
    }
  }

  let payload = build_payload(&row.path, mode, &params);
  let resp = client
    .call("start_model", Some(payload))
    .await
    .map_err(|e| map_start_error(e, &row))?;
  emit_response(args.preset.as_deref(), &row, &resp, args.json, cli.quiet);
  Ok(())
}

#[derive(Debug, Default, Clone)]
struct PartialParams {
  ctx: Option<u32>,
  port: Option<u16>,
  reasoning: Option<bool>,
  advanced: Vec<String>,
}

fn resolve_mode(
  row: &CatalogRow,
  override_mode: Option<CliLaunchMode>,
) -> Result<&'static str, CliExit> {
  if let Some(m) = override_mode {
    return Ok(mode_label(m));
  }
  match row.mode_hint.as_deref() {
    Some("chat") => Ok("chat"),
    Some("embedding") => Ok("embedding"),
    Some("rerank") => Ok("rerank"),
    Some("unknown") | None => Err(CliExit::new(
      USAGE,
      format!(
        "model `{name}` has no mode hint; pass `--mode chat|embedding|rerank` to disambiguate",
        name = row.name(),
      ),
    )),
    Some(other) => Err(CliExit::new(
      USAGE,
      format!("unrecognised mode hint `{other}` from daemon; please file a bug"),
    )),
  }
}

fn mode_label(m: CliLaunchMode) -> &'static str {
  match m {
    CliLaunchMode::Chat => "chat",
    CliLaunchMode::Embedding => "embedding",
    CliLaunchMode::Rerank => "rerank",
  }
}

async fn fetch_preset_params(
  client: &mut Client,
  model_path: &str,
  preset_name: &str,
) -> Result<PartialParams, CliExit> {
  let body = client
    .call(
      "presets_show",
      Some(json!({"model_path": model_path, "name": preset_name})),
    )
    .await
    .map_err(CliExit::from_client_error)?;
  let preset = body.get("preset");
  if preset.map(Value::is_null).unwrap_or(true) {
    return Err(CliExit::new(
      USAGE,
      format!("preset `{preset_name}` not found for {model_path}"),
    ));
  }
  let preset = preset.unwrap();
  let p = preset.get("params").cloned().unwrap_or(Value::Null);
  Ok(PartialParams {
    ctx: p.get("ctx").and_then(Value::as_u64).map(|n| n as u32),
    port: p.get("port").and_then(Value::as_u64).map(|n| n as u16),
    reasoning: p.get("reasoning").and_then(Value::as_bool),
    advanced: p
      .get("advanced")
      .and_then(Value::as_array)
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(str::to_string))
          .collect()
      })
      .unwrap_or_default(),
  })
}

fn build_payload(model_path: &str, mode: &str, p: &PartialParams) -> Value {
  let mut obj = serde_json::Map::new();
  obj.insert(
    "model_path".into(),
    Value::String(PathBuf::from(model_path).display().to_string()),
  );
  obj.insert("mode".into(), Value::String(mode.to_string()));
  if let Some(ctx) = p.ctx {
    obj.insert("ctx".into(), Value::from(ctx));
  }
  if let Some(port) = p.port {
    obj.insert("port".into(), Value::from(port));
  }
  if let Some(r) = p.reasoning {
    obj.insert("reasoning".into(), Value::from(r));
  }
  if !p.advanced.is_empty() {
    obj.insert(
      "advanced".into(),
      Value::Array(p.advanced.iter().cloned().map(Value::String).collect()),
    );
  }
  Value::Object(obj)
}

fn map_start_error(e: crate::ipc::ClientError, row: &CatalogRow) -> CliExit {
  use crate::ipc::ClientError;
  match e {
    ClientError::Remote(err) => {
      // Daemon distinguishes "binary missing" via the launch
      // environment guard; surface that as BINARY_NOT_FOUND so
      // scripts can react.
      let lower = err.message.to_lowercase();
      if lower.contains("launch environment") || lower.contains("llama-server") {
        CliExit::new(
          BINARY_NOT_FOUND,
          format!(
            "daemon could not launch {name}: {msg}\nhint: pass --llama-server <path> or set LLAMATUI_LLAMA_SERVER",
            name = row.name(),
            msg = err.message,
          ),
        )
      } else {
        CliExit::new(
          LAUNCH_FAILED,
          format!("start_model failed for {}: {}", row.name(), err.message),
        )
      }
    }
    other => CliExit::from_client_error(other),
  }
}

fn emit_response(preset: Option<&str>, row: &CatalogRow, resp: &Value, json: bool, quiet: bool) {
  let port = resp.get("port").and_then(Value::as_u64);
  let lid = resp.get("launch_id").and_then(Value::as_str);
  let pid = resp.get("pid").and_then(Value::as_u64);
  if json {
    let body = serde_json::json!({
      "name": row.name(),
      "launch_id": lid,
      "port": port,
      "pid": pid,
      "preset": preset,
      "path": row.path,
    });
    println!("{}", crate::cli::output::pretty_json(&body));
    return;
  }
  if quiet {
    return;
  }
  let preset_label = preset
    .map(|p| format!(" (preset: {p})"))
    .unwrap_or_default();
  println!(
    "started {name}{preset} → launch_id={lid} port={port} pid={pid}",
    name = row.name(),
    preset = preset_label,
    lid = lid.unwrap_or("?"),
    port = port.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
    pid = pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::resolve::CatalogRow;

  fn row(mode_hint: Option<&str>) -> CatalogRow {
    CatalogRow {
      path: "/m/qwen.gguf".into(),
      model_id: Some("deadbeef".into()),
      parent: "/m".into(),
      source: "user".into(),
      arch: Some("qwen2".into()),
      quant: Some("Q4_K".into()),
      native_ctx: Some(8192),
      mode_hint: mode_hint.map(str::to_string),
      parameter_label: Some("7B".into()),
      parse_error: None,
    }
  }

  #[test]
  fn explicit_mode_wins_even_when_hint_present() {
    let r = row(Some("chat"));
    assert_eq!(
      resolve_mode(&r, Some(CliLaunchMode::Embedding)).unwrap(),
      "embedding"
    );
  }

  #[test]
  fn missing_hint_without_override_errors_with_usage() {
    let r = row(None);
    let err = resolve_mode(&r, None).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--mode"));
  }

  #[test]
  fn unknown_hint_without_override_errors() {
    let r = row(Some("unknown"));
    assert!(resolve_mode(&r, None).is_err());
  }

  #[test]
  fn build_payload_includes_only_set_fields() {
    let p = PartialParams {
      ctx: Some(32768),
      port: None,
      reasoning: Some(true),
      advanced: vec!["--threads".into(), "8".into()],
    };
    let v = build_payload("/m/a.gguf", "chat", &p);
    assert_eq!(v["model_path"], serde_json::json!("/m/a.gguf"));
    assert_eq!(v["mode"], serde_json::json!("chat"));
    assert_eq!(v["ctx"], serde_json::json!(32768));
    assert!(v.get("port").is_none(), "port unset must be absent");
    assert_eq!(v["reasoning"], serde_json::json!(true));
    assert_eq!(v["advanced"], serde_json::json!(["--threads", "8"]));
  }
}
