//! Resolve user-supplied model / launch references against the
//! daemon's catalog or active supervisor list.
//!
//! Model references (used by `start`, `presets`, `favorites`) accept:
//! - an absolute path (matched verbatim against the canonical path),
//! - a canonical model id (the BLAKE3 short fingerprint or full hex),
//! - a substring matched case-insensitively against the file name and
//!   the parent directory.
//!
//! Running-launch references (used by `stop`, `logs`) accept a
//! `LaunchId` (e.g. `L3`), a port number, or a case-insensitive
//! substring of the running model's file name / parent directory.
//! Multiple matches surface as `MODEL_NOT_FOUND` with a
//! disambiguation hint.

use anyhow::Result;
use serde_json::Value;

use crate::cli::exit_codes::{CliExit, MODEL_NOT_FOUND};
use crate::ipc::Client;

// The catalog row + fuzzy matcher live in `crate::launch::resolve` (no
// `clap`/IPC/`CliExit` coupling) so the HTTP proxy can share them without
// importing "up" into `cli`. Re-exported here so existing
// `crate::cli::resolve::{CatalogRow, ResolveError, …}` call sites and the
// IPC/`CliExit` wrappers below keep their shape.
pub use crate::launch::resolve::{resolve_model_with_candidates, CatalogRow, ResolveError};

/// One row from `status`'s `models` array.
#[derive(Debug, Clone)]
pub struct RunningRow {
  pub launch_id: String,
  pub model_path: String,
  /// Full canonical `ModelId` object as emitted by the daemon (path
  /// + header_blake3). `None` if the wire shape omits it.
  pub id: Option<Value>,
  pub port: u16,
  pub mode: String,
  pub state: String,
  /// Failure cause from the daemon's `ManagedState::Error { cause }`
  /// payload. Surfaced so users (and agents) can see *why* a launch
  /// landed in the `error` state without having to scrape the log
  /// file separately. `None` for non-error states.
  pub state_cause: Option<String>,
  pub pid: Option<u64>,
  pub ready_at: Option<u64>,
  /// Per-launch params (ctx / reasoning / advanced / mode). Lets
  /// agents reproduce a launch without a separate `last_params`
  /// call.
  pub params: Option<Value>,
  /// Latest resident-set bytes for the supervised process. `None`
  /// before the per-PID sampler has primed (typically one tick after
  /// launch).
  pub latest_rss_bytes: Option<u64>,
  /// Latest CPU usage % (multi-core sum, so >100% is normal for
  /// inference workloads). `None` before the per-PID sampler primes.
  pub latest_cpu_pct: Option<f32>,
  /// Context window `--fit` actually chose, read from the child's
  /// `/props` on Ready. `None` until the fetch lands, or when the
  /// build doesn't expose it. Carried so `status --json` surfaces the
  /// resolved window without re-querying the child.
  pub resolved_ctx: Option<u32>,
  /// True when `--fit` had to clamp the context window down to the floor
  /// under memory pressure. Surfaced as a note on `status` and in
  /// the `show` running section.
  pub ctx_clamped: bool,
  /// Config-preset hint: how many presets this model resolves (per-model
  /// ∪ arch). `0` when none. The full set lives in `presets_list`.
  pub preset_count: u32,
  /// Default preset name (config-only) for this model, or `None`.
  pub preset_default: Option<String>,
  /// Backend this launch resolved to (`llamacpp` / `ds4` / `lemonade`),
  /// mirrored from the IPC `status` row's `backend` field. `None` on a row
  /// the daemon didn't tag; defaults to `llamacpp` semantics downstream.
  pub backend: Option<String>,
}

impl RunningRow {
  /// Human-friendly basename (mirrors [`CatalogRow::name`]). Strips
  /// the parent directory but keeps the extension so split-shard
  /// files stay distinguishable. Falls back to the raw path when it
  /// has no separator.
  pub fn name(&self) -> String {
    basename(&self.model_path)
  }
}

/// Internal basename helper shared by the row types above. Kept in
/// this module so the row impls stay self-contained — callers should
/// use `RunningRow::name()` / `ExternalRow::name()`, not this
/// function directly.
fn basename(path: &str) -> String {
  std::path::Path::new(path)
    .file_name()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| path.to_string())
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
  // One shape, one parser: `CatalogRow`'s `Deserialize` is the mirror of the
  // daemon's serializer. A row that can't be read degrades to skipped rather
  // than aborting the whole list.
  Ok(
    arr
      .into_iter()
      .filter_map(|row| serde_json::from_value(row).ok())
      .collect(),
  )
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
  match resolve_model_with_candidates(rows, reference) {
    Ok(row) => Ok(row),
    Err(ResolveError::Empty) => Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty model reference; supply a name substring, absolute path, or short id",
    )),
    Err(ResolveError::None) => Err(CliExit::new(
      MODEL_NOT_FOUND,
      format!("no model matches `{reference}` ({} known)", rows.len()),
    )),
    Err(ResolveError::Many(candidates)) => {
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

/// Index running rows by canonical model path. Returns the first
/// running row per path (multiple supervisors for one path is rare
/// but possible — the picker uses the most recent ready_at row first
/// when this matters; here the list view just needs *some* live row).
pub fn running_index(rows: &[RunningRow]) -> std::collections::HashMap<String, RunningRow> {
  let mut out = std::collections::HashMap::with_capacity(rows.len());
  for r in rows {
    out.entry(r.model_path.clone()).or_insert_with(|| r.clone());
  }
  out
}

/// Multi-device gate for the `list` DEVICE column — mirrors the TUI's
/// `App::multi_device`: true when **some single server** offers more than
/// one device (a launch targets one server's devices, so cross-build
/// selector variety must not trip this). `servers` is the verbatim
/// `status.servers` wire value; anything else (older daemon, missing
/// field) reads as single-device.
pub fn multi_device(servers: &Value) -> bool {
  servers
    .as_array()
    .map(|ss| {
      ss.iter().any(|s| {
        s.get("devices")
          .and_then(Value::as_array)
          .map(|d| d.len() > 1)
          .unwrap_or(false)
      })
    })
    .unwrap_or(false)
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
  // AGENTS.md: `host` is always an object on the wire; preserve it
  // verbatim so the CLI `status --json` mirrors the IPC contract.
  let host = body.get("host").cloned().unwrap_or(Value::Null);
  let daemon = body.get("daemon").and_then(parse_daemon_health);
  // Preserve the proxy block verbatim — the CLI `status --json` is
  // byte-shape-identical to the IPC wire format per the plan's
  // R161 contract. Older daemons that don't surface the field land
  // as `Value::Null` and the projection in `status_json` drops the
  // key entirely.
  let proxy = body.get("proxy").cloned().unwrap_or(Value::Null);
  // Backends block — verbatim copy of the daemon's `status.backends`
  // array. `Value::Null` when talking to a daemon that predates
  // the field; the formatter then skips the section.
  let backends = body.get("backends").cloned().unwrap_or(Value::Null);
  // Server catalog — verbatim copy of `status.servers`. `Value::Null` on a
  // daemon that predates the field; `status_json` drops the key then.
  let servers = body.get("servers").cloned().unwrap_or(Value::Null);
  Ok(StatusSnapshot {
    models,
    external,
    gpu,
    host,
    daemon,
    proxy,
    backends,
    servers,
  })
}

#[derive(Debug, Clone)]
pub struct StatusSnapshot {
  pub models: Vec<RunningRow>,
  pub external: Vec<ExternalRow>,
  pub gpu: Value,
  /// Host-level metrics (CPU%, RAM, GPU util/temp/VRAM aggregates,
  /// sampler backend). Always an object on the wire per
  /// `AGENTS.md::status IPC fields`; the CLI preserves it verbatim
  /// so `status --json` consumers see the same shape as raw IPC
  /// clients. `Value::Null` only when talking to a daemon that
  /// predates the field.
  pub host: Value,
  /// Daemon health preamble (`pid`, `uptime_seconds`,
  /// `active_connections`). Older daemons may omit the field, in
  /// which case this is `None` — the formatter silently skips it.
  pub daemon: Option<DaemonHealth>,
  /// Proxy listener block — `{enabled, listen, status, bind_error}`.
  /// Verbatim copy of the daemon's wire shape; the
  /// CLI `status --json` rewrites it byte-for-byte so agents that
  /// parse the IPC and the CLI see identical shapes. `Value::Null`
  /// when talking to a daemon that omits the field.
  pub proxy: Value,
  /// Backends block — array of `{id, lifecycle, installed, accelerators}`
  /// Verbatim copy of the daemon's wire shape; `Value::Null`
  /// against a daemon that predates the field.
  pub backends: Value,
  /// Server catalog — array of `{id, backend_id, binary, name, devices}` (a
  /// build/binary of a backend with its probed `--device` targets). Verbatim
  /// copy of the daemon's wire shape; `Value::Null` on an older daemon.
  pub servers: Value,
}

#[derive(Debug, Clone)]
pub struct DaemonHealth {
  pub pid: u64,
  pub uptime_seconds: u64,
  pub active_connections: u64,
  /// Daemon build version (cargo pkg version at compile time).
  /// `None` when an older daemon omits the field.
  pub build: Option<String>,
  /// Path to the `llama-server` binary the daemon resolved at start.
  /// `None` when the daemon doesn't expose it or hasn't resolved one.
  pub server_path: Option<String>,
  /// HTTP control-plane URL the daemon bound on startup
  /// (e.g. `http://127.0.0.1:48134`). `None` when talking to a
  /// pre-Phase-A daemon that doesn't surface the field.
  pub ipc_url: Option<String>,
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
    build: obj.get("build").and_then(Value::as_str).map(str::to_string),
    server_path: obj
      .get("server_path")
      .and_then(Value::as_str)
      .map(str::to_string),
    ipc_url: obj
      .get("ipc_url")
      .and_then(Value::as_str)
      .map(str::to_string),
  })
}

#[derive(Debug, Clone)]
pub struct ExternalRow {
  pub pid: u64,
  pub cmdline: String,
  pub model_path: Option<String>,
  /// Listening port parsed from the orphan's argv on the daemon
  /// side. `None` when the cmdline didn't carry `--port` / `-p`
  /// (rare for llamastash-launched orphans — the supervisor always
  /// emits the long flag). Surfaced into `status --json` so agents
  /// can diff against `ss`/`lsof` without re-parsing argv client-side.
  pub port: Option<u16>,
  /// True when the orphan's environment carried `LLAMASTASH_LAUNCHED=1`
  /// at sweep time — i.e. it was spawned by some llamastash
  /// instance (this daemon's previous run, a sibling UAT daemon,
  /// etc.). Drives `collect_in_use_ports` on the daemon side; here
  /// it lives so the `daemon status` formatter can flag the row.
  pub launched_by_llamastash: bool,
}

impl ExternalRow {
  /// Best-effort label for an external row. Prefers the discovered
  /// model path's basename so the row reads like a managed launch;
  /// falls back to the cmdline's basename when the path is unknown
  /// rather than dumping the full argv into a narrow column.
  pub fn name(&self) -> String {
    self
      .model_path
      .as_deref()
      .map(basename)
      .unwrap_or_else(|| basename(&self.cmdline))
  }
}

fn parse_running_row(v: &Value) -> Option<RunningRow> {
  let launch_id = v.get("launch_id")?.as_str()?.to_string();
  let id = v.get("id").cloned();
  // Path lives under id.path because the `status` handler nests the
  // ModelId; preserve the same shape so callers can show it.
  let model_path = v
    .get("id")
    .and_then(|model_id| model_id.get("path"))
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let port = v.get("port")?.as_u64()? as u16;
  let mode = v
    .get("mode")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let state = parse_running_state_label(v)
    .map(str::to_string)
    .unwrap_or_default();
  let state_cause = v
    .get("state")
    .and_then(|s| s.get("cause"))
    .and_then(Value::as_str)
    .map(str::to_string);
  let pid = v.get("pid").and_then(Value::as_u64);
  let ready_at = v.get("ready_at").and_then(Value::as_u64);
  let params = v.get("params").cloned();
  let latest_rss_bytes = v.get("latest_rss_bytes").and_then(Value::as_u64);
  let latest_cpu_pct = v
    .get("latest_cpu_pct")
    .and_then(Value::as_f64)
    .map(|n| n as f32);
  let resolved_ctx = v
    .get("resolved_ctx")
    .and_then(Value::as_u64)
    .map(|n| n as u32);
  let ctx_clamped = v
    .get("ctx_clamped")
    .and_then(Value::as_bool)
    .unwrap_or(false);
  let preset_count = v.get("preset_count").and_then(Value::as_u64).unwrap_or(0) as u32;
  let preset_default = v.get("default").and_then(Value::as_str).map(str::to_string);
  let backend = v.get("backend").and_then(Value::as_str).map(str::to_string);
  Some(RunningRow {
    launch_id,
    model_path,
    id,
    port,
    mode,
    state,
    state_cause,
    pid,
    ready_at,
    params,
    latest_rss_bytes,
    latest_cpu_pct,
    resolved_ctx,
    ctx_clamped,
    preset_count,
    preset_default,
    backend,
  })
}

fn parse_running_state_label(v: &Value) -> Option<&str> {
  v.get("state").and_then(Value::as_str).or_else(|| {
    v.get("state")
      .and_then(|s| s.get("state"))
      .and_then(Value::as_str)
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
  let port = v
    .get("port")
    .and_then(Value::as_u64)
    .and_then(|p| u16::try_from(p).ok());
  let launched_by_llamastash = v
    .get("launched_by_llamastash")
    .and_then(Value::as_bool)
    .unwrap_or(false);
  Some(ExternalRow {
    pid,
    cmdline,
    model_path,
    port,
    launched_by_llamastash,
  })
}

/// Resolve a reference against the running snapshot. Tiers, first hit wins:
/// 1. numeric → port match;
/// 2. exact (case-insensitive) launch-id (`L<n>`);
/// 3. case-insensitive substring of the model file name or its parent dir —
///    the same reference shape `start` / `show` / `presets` / `favorites`
///    accept against the catalog, here matched against the running launches
///    (usage.md §Concepts "Model references"). So `logs gemma` / `stop qwen`
///    work, not just `logs L3` / `stop 41100`.
///
/// Multiple matches surface as `MODEL_NOT_FOUND` with the launch ids listed.
pub fn resolve_running(rows: &[RunningRow], reference: &str) -> Result<RunningRow, CliExit> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty target; supply a launch id (e.g. L3), a port, or a model name",
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
  if !by_id.is_empty() {
    return single_or_error(by_id, reference);
  }
  // Fall back to a name / parent-dir substring against the running rows.
  let by_name: Vec<&RunningRow> = rows
    .iter()
    .filter(|r| {
      let path = std::path::Path::new(&r.model_path);
      let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
      let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");
      name.to_lowercase().contains(&lower) || parent.to_lowercase().contains(&lower)
    })
    .collect();
  single_or_error(by_name, reference)
}

/// [`resolve_running`], with a lazy catalog fallback for launches whose
/// user-facing name never appears in their path — an Ollama alias like
/// `gemma4:e2b` runs from a `sha256-…` blob, so the path-substring tier
/// can't see it. Only a zero-match miss falls through (ambiguity is
/// final); the fallback maps catalog display labels back to paths and
/// re-matches the running rows, costing one extra `list_models` call
/// only on that miss path.
pub async fn resolve_running_via_catalog(
  client: &mut Client,
  rows: &[RunningRow],
  reference: &str,
) -> Result<RunningRow, CliExit> {
  let miss = match resolve_running(rows, reference) {
    Ok(row) => return Ok(row),
    Err(e) => e,
  };
  // `single_or_error`'s zero-match message — ambiguous matches keep
  // their launch-id listing and never reach the catalog.
  if !miss
    .message
    .as_deref()
    .unwrap_or_default()
    .starts_with("no running launch matches")
  {
    return Err(miss);
  }
  let Ok(catalog) = fetch_catalog(client).await else {
    return Err(miss);
  };
  let lower = reference.trim().to_lowercase();
  let label_paths: Vec<String> = catalog
    .iter()
    .filter(|c| c.name().to_lowercase().contains(&lower))
    .map(|c| c.path.clone())
    .collect();
  let by_label: Vec<&RunningRow> = rows
    .iter()
    .filter(|r| label_paths.contains(&r.model_path))
    .collect();
  if by_label.is_empty() {
    return Err(miss);
  }
  single_or_error(by_label, reference)
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
      model_id: None,
      parent: parent.to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: Some("Q4_K".to_string()),
      native_ctx: Some(8192),
      mode_hint: Some("chat".to_string()),
      parameter_label: Some("7B".to_string()),
      weights_bytes: Some(4_200_000_000),
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
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
        id: None,
        port: 41100,
        mode: "chat".into(),
        state: "ready".into(),
        state_cause: None,
        pid: Some(123),
        ready_at: None,
        params: None,
        latest_rss_bytes: None,
        latest_cpu_pct: None,
        resolved_ctx: None,
        ctx_clamped: false,
        preset_count: 0,
        preset_default: None,
        backend: None,
      },
      RunningRow {
        launch_id: "L2".into(),
        model_path: "/m/b.gguf".into(),
        id: None,
        port: 41101,
        mode: "chat".into(),
        state: "ready".into(),
        state_cause: None,
        pid: Some(124),
        ready_at: None,
        params: None,
        latest_rss_bytes: None,
        latest_cpu_pct: None,
        resolved_ctx: None,
        ctx_clamped: false,
        preset_count: 0,
        preset_default: None,
        backend: None,
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
      id: None,
      port: 41100,
      mode: "chat".into(),
      state: "ready".into(),
      state_cause: None,
      pid: None,
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
      ctx_clamped: false,
      preset_count: 0,
      preset_default: None,
      backend: None,
    }];
    let err = resolve_running(&rows, "9999").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
  }

  #[test]
  fn resolve_running_by_name_substring() {
    let row = |id: &str, path: &str, port: u16| RunningRow {
      launch_id: id.into(),
      model_path: path.into(),
      id: None,
      port,
      mode: "chat".into(),
      state: "ready".into(),
      state_cause: None,
      pid: Some(1),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
      ctx_clamped: false,
      preset_count: 0,
      preset_default: None,
      backend: None,
    };
    let rows = vec![
      row("L1", "/cache/gemma-4-E2B-it-Q4_K_M.gguf", 41100),
      row("L2", "/cache/qwen3-reranker-0.6b-q8_0.gguf", 41101),
    ];
    // File-name substring — the documented model reference — resolves the
    // running launch (this is the F-04 regression: only L<n>/port worked).
    assert_eq!(resolve_running(&rows, "gemma").unwrap().launch_id, "L1");
    assert_eq!(
      resolve_running(&rows, "GEMMA-4-E2B-it-Q4_K_M.gguf")
        .unwrap()
        .launch_id,
      "L1"
    );
    assert_eq!(resolve_running(&rows, "reranker").unwrap().launch_id, "L2");
    // launch-id and port still take precedence and behave as before.
    assert_eq!(resolve_running(&rows, "L2").unwrap().launch_id, "L2");
    assert_eq!(resolve_running(&rows, "41100").unwrap().launch_id, "L1");
    // A substring matching multiple running launches (shared parent dir)
    // disambiguates rather than picking arbitrarily.
    assert_eq!(
      resolve_running(&rows, "cache").unwrap_err().code,
      MODEL_NOT_FOUND
    );
    // No match.
    assert_eq!(
      resolve_running(&rows, "nope").unwrap_err().code,
      MODEL_NOT_FOUND
    );
  }

  #[test]
  fn parse_running_row_accepts_nested_state_object() {
    let row = serde_json::json!({
      "launch_id": "L1",
      "id": {"path": "/m/a.gguf", "header_blake3": "deadbeef"},
      "port": 41100,
      "mode": "chat",
      "state": {"state": "ready"},
      "pid": 123,
      "ready_at": 456,
    });
    let parsed = parse_running_row(&row).expect("row should parse");
    assert_eq!(parsed.state, "ready");
  }

  #[test]
  fn parse_running_row_accepts_flat_state_string() {
    let row = serde_json::json!({
      "launch_id": "L1",
      "id": {"path": "/m/a.gguf", "header_blake3": "deadbeef"},
      "port": 41100,
      "mode": "chat",
      "state": "loading",
      "pid": 123,
      "ready_at": 456,
    });
    let parsed = parse_running_row(&row).expect("row should parse");
    assert_eq!(parsed.state, "loading");
  }
}
