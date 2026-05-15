//! Resolve user-supplied model / launch references against the
//! daemon's catalog or active supervisor list.
//!
//! Model references (used by `start`, `presets`, `favorites`) accept:
//! - an absolute path (matched verbatim against the canonical path),
//! - a canonical model id (the BLAKE3 short fingerprint or full hex),
//! - a substring matched case-insensitively against the file name and
//!   the parent directory.
//!
//! Running-launch references (used by `stop`, `logs`) accept either a
//! `LaunchId` (e.g. `L3`) or a port number. Multiple matches surface
//! as `MODEL_NOT_FOUND` with a disambiguation hint.

use anyhow::Result;
use serde_json::Value;

use crate::cli::exit_codes::{CliExit, MODEL_NOT_FOUND};
use crate::ipc::Client;

/// One row from `list_models`. Lean wrapper kept here so the resolver
/// can stay independent of the catalog's internal `DiscoveredModel`
/// shape.
#[derive(Debug, Clone)]
pub struct CatalogRow {
  /// Canonical absolute path to the launchable file (or shard 1).
  pub path: String,
  pub parent: String,
  pub source: String,
  pub arch: Option<String>,
  pub quant: Option<String>,
  pub native_ctx: Option<u64>,
  pub mode_hint: Option<String>,
  pub parameter_label: Option<String>,
  pub parse_error: Option<String>,
}

impl CatalogRow {
  /// File name (basename) used for human matching and table rendering.
  pub fn name(&self) -> String {
    std::path::Path::new(&self.path)
      .file_name()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| self.path.clone())
  }
}

/// One row from `status`'s `models` array.
#[derive(Debug, Clone)]
pub struct RunningRow {
  pub launch_id: String,
  pub model_path: String,
  pub port: u16,
  pub mode: String,
  pub state: String,
  pub pid: Option<u64>,
  pub ready_at: Option<u64>,
}

/// Fetch every catalog row via `list_models`. Centralised here so
/// resolvers and the `list` handler share parsing.
pub async fn fetch_catalog(client: &mut Client) -> Result<Vec<CatalogRow>, CliExit> {
  let body = client
    .call("list_models", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let arr = body
    .get("models")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default();
  Ok(arr.into_iter().map(parse_catalog_row).collect())
}

fn parse_catalog_row(row: Value) -> CatalogRow {
  let path = row
    .get("path")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let parent = row
    .get("parent")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let source = row
    .get("source")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let metadata = row.get("metadata");
  let parse_error = row
    .get("parse_error")
    .and_then(Value::as_str)
    .map(str::to_string);
  CatalogRow {
    path,
    parent,
    source,
    arch: metadata
      .and_then(|m| m.get("arch"))
      .and_then(Value::as_str)
      .map(str::to_string),
    quant: metadata
      .and_then(|m| m.get("quant"))
      .and_then(Value::as_str)
      .map(str::to_string),
    native_ctx: metadata
      .and_then(|m| m.get("native_ctx"))
      .and_then(Value::as_u64),
    mode_hint: metadata
      .and_then(|m| m.get("mode_hint"))
      .and_then(Value::as_str)
      .map(str::to_string),
    parameter_label: metadata
      .and_then(|m| m.get("parameter_label"))
      .and_then(Value::as_str)
      .map(str::to_string),
    parse_error,
  }
}

/// Find a catalog row that matches `reference`. Disambiguation rules
/// (in order):
/// 1. exact canonical-path match,
/// 2. exact name match (basename),
/// 3. case-insensitive substring of name OR parent dir.
///
/// Returns `MODEL_NOT_FOUND` when zero or many rows match. The error
/// message names every candidate when matches > 1 so callers can
/// re-issue with a tighter reference.
pub fn resolve_model(rows: &[CatalogRow], reference: &str) -> Result<CatalogRow, CliExit> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty model reference; supply a name substring, absolute path, or short id",
    ));
  }

  // Tier 1: exact path / exact name. A full canonical path is
  // unambiguous by construction.
  let exact_path: Vec<&CatalogRow> = rows.iter().filter(|r| r.path == needle).collect();
  if exact_path.len() == 1 {
    return Ok(exact_path[0].clone());
  }
  let exact_name: Vec<&CatalogRow> = rows.iter().filter(|r| r.name() == needle).collect();
  if exact_name.len() == 1 {
    return Ok(exact_name[0].clone());
  }

  // Tier 2: case-insensitive substring of name OR parent.
  let lower = needle.to_lowercase();
  let candidates: Vec<&CatalogRow> = rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower) || r.parent.to_lowercase().contains(&lower)
    })
    .collect();
  match candidates.len() {
    0 => Err(CliExit::new(
      MODEL_NOT_FOUND,
      format!("no model matches `{reference}` ({} known)", rows.len()),
    )),
    1 => Ok(candidates[0].clone()),
    _ => {
      let names: Vec<String> = candidates.iter().map(|r| r.name()).collect();
      Err(CliExit::new(
        MODEL_NOT_FOUND,
        format!(
          "`{reference}` matches {} models: {}\nrefine the reference (full path or unique substring) and retry",
          candidates.len(),
          names.join(", ")
        ),
      ))
    }
  }
}

/// Fetch the supervisor + external snapshot via `status`.
pub async fn fetch_status(client: &mut Client) -> Result<StatusSnapshot, CliExit> {
  let body = client
    .call("status", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let models: Vec<RunningRow> = body
    .get("models")
    .and_then(Value::as_array)
    .map(|a| a.iter().filter_map(parse_running_row).collect())
    .unwrap_or_default();
  let external: Vec<ExternalRow> = body
    .get("external")
    .and_then(Value::as_array)
    .map(|a| a.iter().filter_map(parse_external_row).collect())
    .unwrap_or_default();
  let gpu = body.get("gpu").cloned().unwrap_or(Value::Null);
  let daemon = body.get("daemon").and_then(parse_daemon_health);
  Ok(StatusSnapshot {
    models,
    external,
    gpu,
    daemon,
  })
}

#[derive(Debug, Clone)]
pub struct StatusSnapshot {
  pub models: Vec<RunningRow>,
  pub external: Vec<ExternalRow>,
  pub gpu: Value,
  /// Daemon health preamble (`pid`, `uptime_seconds`,
  /// `active_connections`). Older daemons may omit the field, in
  /// which case this is `None` — the formatter silently skips it.
  pub daemon: Option<DaemonHealth>,
}

#[derive(Debug, Clone)]
pub struct DaemonHealth {
  pub pid: u64,
  pub uptime_seconds: u64,
  pub active_connections: u64,
}

fn parse_daemon_health(v: &Value) -> Option<DaemonHealth> {
  let obj = v.as_object()?;
  Some(DaemonHealth {
    pid: obj.get("pid").and_then(Value::as_u64).unwrap_or(0),
    uptime_seconds: obj
      .get("uptime_seconds")
      .and_then(Value::as_u64)
      .unwrap_or(0),
    active_connections: obj
      .get("active_connections")
      .and_then(Value::as_u64)
      .unwrap_or(0),
  })
}

#[derive(Debug, Clone)]
pub struct ExternalRow {
  pub pid: u64,
  pub cmdline: String,
  pub model_path: Option<String>,
}

fn parse_running_row(v: &Value) -> Option<RunningRow> {
  let launch_id = v.get("launch_id")?.as_str()?.to_string();
  // Path lives under id.path because the `status` handler nests the
  // ModelId; preserve the same shape so callers can show it.
  let model_path = v
    .get("id")
    .and_then(|id| id.get("path"))
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let port = v.get("port")?.as_u64()? as u16;
  let mode = v
    .get("mode")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let state = v
    .get("state")
    .and_then(|s| s.get("state"))
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let pid = v.get("pid").and_then(Value::as_u64);
  let ready_at = v.get("ready_at").and_then(Value::as_u64);
  Some(RunningRow {
    launch_id,
    model_path,
    port,
    mode,
    state,
    pid,
    ready_at,
  })
}

fn parse_external_row(v: &Value) -> Option<ExternalRow> {
  let pid = v.get("pid")?.as_u64()?;
  let cmdline = v
    .get("cmdline")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let model_path = v
    .get("model_path")
    .and_then(Value::as_str)
    .map(str::to_string);
  Some(ExternalRow {
    pid,
    cmdline,
    model_path,
  })
}

/// Resolve a `--id-or-port` reference against the running snapshot.
/// Numeric → port match; otherwise → launch-id match. Multiple
/// matches (e.g. two launches on the same port — shouldn't happen but
/// guard anyway) surface as `MODEL_NOT_FOUND`.
pub fn resolve_running(rows: &[RunningRow], reference: &str) -> Result<RunningRow, CliExit> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty target; supply a launch id (e.g. L3) or a port",
    ));
  }
  if let Ok(port) = needle.parse::<u16>() {
    let by_port: Vec<&RunningRow> = rows.iter().filter(|r| r.port == port).collect();
    return single_or_error(by_port, reference);
  }
  // Case-insensitive launch-id match. The supervisor formats them as
  // `L<n>` so case is fixed in practice; lower-case both sides for
  // forgiveness.
  let lower = needle.to_lowercase();
  let by_id: Vec<&RunningRow> = rows
    .iter()
    .filter(|r| r.launch_id.to_lowercase() == lower)
    .collect();
  single_or_error(by_id, reference)
}

fn single_or_error(matches: Vec<&RunningRow>, reference: &str) -> Result<RunningRow, CliExit> {
  match matches.len() {
    0 => Err(CliExit::new(
      MODEL_NOT_FOUND,
      format!("no running launch matches `{reference}`"),
    )),
    1 => Ok(matches[0].clone()),
    _ => {
      let ids: Vec<String> = matches.iter().map(|r| r.launch_id.clone()).collect();
      Err(CliExit::new(
        MODEL_NOT_FOUND,
        format!(
          "`{reference}` matches {} launches: {}",
          matches.len(),
          ids.join(", ")
        ),
      ))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(path: &str, parent: &str) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      parent: parent.to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: Some("Q4_K".to_string()),
      native_ctx: Some(8192),
      mode_hint: Some("chat".to_string()),
      parameter_label: Some("7B".to_string()),
      parse_error: None,
    }
  }

  #[test]
  fn exact_path_wins_even_with_overlap() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let pick = resolve_model(&rows, "/m/qwen-coder-7b.gguf").unwrap();
    assert_eq!(pick.path, "/m/qwen-coder-7b.gguf");
  }

  #[test]
  fn substring_match_disambiguates_when_unique() {
    let rows = vec![row("/m/qwen-coder.gguf", "/m"), row("/m/llama.gguf", "/m")];
    let pick = resolve_model(&rows, "qwen").unwrap();
    assert!(pick.name().contains("qwen"));
  }

  #[test]
  fn ambiguous_substring_returns_disambiguation_hint() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let err = resolve_model(&rows, "qwen-coder").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
    let msg = err.to_string();
    assert!(msg.contains("qwen-coder-7b.gguf"), "got: {msg}");
    assert!(msg.contains("qwen-coder-13b.gguf"), "got: {msg}");
  }

  #[test]
  fn zero_matches_surfaces_not_found_with_count() {
    let rows = vec![row("/m/qwen-coder.gguf", "/m")];
    let err = resolve_model(&rows, "phi").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
    assert!(err.to_string().contains("phi"));
  }

  #[test]
  fn empty_reference_errors_with_hint() {
    let err = resolve_model(&[], "  ").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
    assert!(err.to_string().to_lowercase().contains("empty"));
  }

  #[test]
  fn parent_dir_substring_matches() {
    let rows = vec![
      row(
        "/cache/lm-studio/models/qwen.gguf",
        "/cache/lm-studio/models",
      ),
      row("/cache/ollama/models/llama.gguf", "/cache/ollama/models"),
    ];
    let pick = resolve_model(&rows, "lm-studio").unwrap();
    assert!(pick.parent.contains("lm-studio"));
  }

  #[test]
  fn resolve_running_by_port_matches() {
    let rows = vec![
      RunningRow {
        launch_id: "L1".into(),
        model_path: "/m/a.gguf".into(),
        port: 41100,
        mode: "chat".into(),
        state: "ready".into(),
        pid: Some(123),
        ready_at: None,
      },
      RunningRow {
        launch_id: "L2".into(),
        model_path: "/m/b.gguf".into(),
        port: 41101,
        mode: "chat".into(),
        state: "ready".into(),
        pid: Some(124),
        ready_at: None,
      },
    ];
    assert_eq!(resolve_running(&rows, "41100").unwrap().launch_id, "L1");
    assert_eq!(resolve_running(&rows, "L2").unwrap().launch_id, "L2");
  }

  #[test]
  fn resolve_running_unknown_port_errors() {
    let rows = vec![RunningRow {
      launch_id: "L1".into(),
      model_path: "/m/a.gguf".into(),
      port: 41100,
      mode: "chat".into(),
      state: "ready".into(),
      pid: None,
      ready_at: None,
    }];
    let err = resolve_running(&rows, "9999").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
  }
}
