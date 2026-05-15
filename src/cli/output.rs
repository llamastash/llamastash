//! Human + JSON output formatters shared by the non-interactive
//! subcommands.
//!
//! Two surfaces, one source of truth: every command supports `--json`
//! whose shape is the public agent contract; the human-readable form
//! is best-effort prettification.
//!
//! Tab-separated text is the default human format. Agents pin against
//! `--json`; humans get something `column -t` friendly.

use serde_json::Value;

use crate::cli::resolve::{CatalogRow, RunningRow, StatusSnapshot};

/// Render `list_models` rows as TSV. Columns: id, path, arch, quant,
/// native_ctx (one line per model, header line first).
pub fn list_human(rows: &[CatalogRow]) -> String {
  if rows.is_empty() {
    return String::from("(no models discovered)\n");
  }
  let mut out = String::new();
  out.push_str("NAME\tARCH\tQUANT\tCTX\tPATH\n");
  for r in rows {
    let arch = r.arch.as_deref().unwrap_or("?");
    let quant = r.quant.as_deref().unwrap_or("?");
    let ctx = r
      .native_ctx
      .map(|n| n.to_string())
      .unwrap_or_else(|| "?".to_string());
    out.push_str(&format!(
      "{name}\t{arch}\t{quant}\t{ctx}\t{path}\n",
      name = r.name(),
      path = r.path,
    ));
  }
  out
}

/// JSON projection of `list_models` rows. Stable shape — agents pin
/// against this, so column drift requires deliberate intent.
pub fn list_json(rows: &[CatalogRow]) -> Value {
  let arr: Vec<Value> = rows
    .iter()
    .map(|r| {
      serde_json::json!({
        "name": r.name(),
        "path": r.path,
        "parent": r.parent,
        "source": r.source,
        "arch": r.arch,
        "quant": r.quant,
        "native_ctx": r.native_ctx,
        "mode_hint": r.mode_hint,
        "parameter_label": r.parameter_label,
        "parse_error": r.parse_error,
      })
    })
    .collect();
  Value::Array(arr)
}

/// Filter catalog rows by case-insensitive substring against name,
/// path, arch, and quant. Matches the `list --filter` semantics
/// documented in the plan.
pub fn filter_rows(rows: &[CatalogRow], pattern: &str) -> Vec<CatalogRow> {
  let lower = pattern.to_lowercase();
  rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower)
        || r.path.to_lowercase().contains(&lower)
        || r
          .arch
          .as_deref()
          .map(|a| a.to_lowercase().contains(&lower))
          .unwrap_or(false)
        || r
          .quant
          .as_deref()
          .map(|a| a.to_lowercase().contains(&lower))
          .unwrap_or(false)
    })
    .cloned()
    .collect()
}

/// Human rendering of `status`.
pub fn status_human(snap: &StatusSnapshot) -> String {
  let mut out = String::new();
  if snap.models.is_empty() && snap.external.is_empty() {
    out.push_str("(no managed launches)\n");
  } else {
    out.push_str("LAUNCH_ID\tSTATE\tMODE\tPORT\tPID\tPATH\n");
    for r in &snap.models {
      out.push_str(&row_string(r));
    }
    for r in &snap.external {
      out.push_str(&format!(
        "external\texternal\t-\t-\t{}\t{}\n",
        r.pid,
        r.model_path.as_deref().unwrap_or(&r.cmdline),
      ));
    }
  }
  if let Some(label) = gpu_label(&snap.gpu) {
    out.push_str(&format!("\nGPU: {label}\n"));
  }
  out
}

fn row_string(r: &RunningRow) -> String {
  let pid = r
    .pid
    .map(|p| p.to_string())
    .unwrap_or_else(|| "-".to_string());
  format!(
    "{lid}\t{state}\t{mode}\t{port}\t{pid}\t{path}\n",
    lid = r.launch_id,
    state = r.state,
    mode = r.mode,
    port = r.port,
    pid = pid,
    path = r.model_path,
  )
}

fn gpu_label(gpu: &Value) -> Option<String> {
  // GpuInfo serialises with serde's default; surface a one-liner so
  // the human form doesn't dump a JSON blob mid-paragraph.
  if gpu.is_null() {
    return None;
  }
  if gpu == &Value::String("CpuOnly".into()) {
    return Some("CPU only".to_string());
  }
  // Map common shapes; fall back to compact JSON for everything else.
  if let Some(obj) = gpu.as_object() {
    if let Some(nv) = obj.get("Nvidia") {
      let count = nv
        .get("devices")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
      return Some(format!("NVIDIA GPU(s): {count}"));
    }
    if let Some(amd) = obj.get("Amd") {
      let count = amd
        .get("devices")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
      return Some(format!("AMD GPU(s): {count}"));
    }
    if let Some(metal) = obj.get("Metal") {
      let label = metal
        .get("device")
        .and_then(Value::as_str)
        .unwrap_or("Apple Silicon");
      return Some(format!("Metal: {label}"));
    }
    if obj.contains_key("Vulkan") {
      return Some("Vulkan".to_string());
    }
  }
  Some(serde_json::to_string(gpu).unwrap_or_else(|_| "?".to_string()))
}

/// JSON projection of `status` (preserves the daemon's wire shape so
/// agents that already parse `daemon status` keep working).
pub fn status_json(snap: &StatusSnapshot) -> Value {
  let models: Vec<Value> = snap
    .models
    .iter()
    .map(|r| {
      serde_json::json!({
        "launch_id": r.launch_id,
        "model_path": r.model_path,
        "port": r.port,
        "mode": r.mode,
        "state": r.state,
        "pid": r.pid,
        "ready_at": r.ready_at,
      })
    })
    .collect();
  let external: Vec<Value> = snap
    .external
    .iter()
    .map(|r| {
      serde_json::json!({
        "pid": r.pid,
        "cmdline": r.cmdline,
        "model_path": r.model_path,
      })
    })
    .collect();
  serde_json::json!({
    "models": models,
    "external": external,
    "gpu": snap.gpu,
  })
}

/// Pretty-print `serde_json::Value` as the canonical CLI JSON form.
/// Agents pin against the pretty form because it's diffable in CI;
/// keep this consistent across every `--json` exit.
pub fn pretty_json(v: &Value) -> String {
  serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::resolve::ExternalRow;

  fn row(name: &str, arch: &str, quant: &str, ctx: u64) -> CatalogRow {
    CatalogRow {
      path: format!("/m/{name}.gguf"),
      parent: "/m".to_string(),
      source: "user".to_string(),
      arch: Some(arch.to_string()),
      quant: Some(quant.to_string()),
      native_ctx: Some(ctx),
      mode_hint: Some("chat".to_string()),
      parameter_label: Some("7B".to_string()),
      parse_error: None,
    }
  }

  #[test]
  fn list_human_renders_header_and_rows() {
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let s = list_human(&rows);
    assert!(s.starts_with("NAME\tARCH"));
    assert!(s.contains("qwen.gguf"));
    assert!(s.contains("8192"));
  }

  #[test]
  fn list_human_handles_empty_catalog() {
    let s = list_human(&[]);
    assert!(s.contains("no models"));
  }

  #[test]
  fn list_json_is_an_array_with_documented_keys() {
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let v = list_json(&rows);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    let r = &arr[0];
    for key in [
      "name",
      "path",
      "parent",
      "source",
      "arch",
      "quant",
      "native_ctx",
      "mode_hint",
      "parameter_label",
      "parse_error",
    ] {
      assert!(r.get(key).is_some(), "key `{key}` missing in JSON row");
    }
  }

  #[test]
  fn list_json_empty_catalog_returns_empty_array() {
    let v = list_json(&[]);
    assert_eq!(v, serde_json::json!([]));
  }

  #[test]
  fn filter_rows_matches_name_arch_quant() {
    let rows = vec![
      row("qwen", "qwen2", "Q4_K", 8192),
      row("phi", "phi3", "Q5_K", 4096),
    ];
    assert_eq!(filter_rows(&rows, "qwen").len(), 1);
    assert_eq!(filter_rows(&rows, "Q5").len(), 1);
    assert_eq!(filter_rows(&rows, "phi3").len(), 1);
    assert_eq!(filter_rows(&rows, "missing").len(), 0);
  }

  #[test]
  fn status_human_handles_empty_snapshot() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
    };
    let s = status_human(&snap);
    assert!(s.contains("no managed"));
  }

  #[test]
  fn status_human_includes_gpu_label_when_present() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::String("CpuOnly".into()),
    };
    let s = status_human(&snap);
    assert!(s.contains("CPU only"), "got: {s}");
  }

  #[test]
  fn status_json_round_trips_documented_keys() {
    let snap = StatusSnapshot {
      models: vec![RunningRow {
        launch_id: "L1".into(),
        model_path: "/m/a.gguf".into(),
        port: 41100,
        mode: "chat".into(),
        state: "ready".into(),
        pid: Some(123),
        ready_at: Some(1_700_000_000),
      }],
      external: vec![ExternalRow {
        pid: 999,
        cmdline: "llama-server".into(),
        model_path: Some("/m/b.gguf".into()),
      }],
      gpu: Value::String("CpuOnly".into()),
    };
    let v = status_json(&snap);
    let model = &v["models"][0];
    assert_eq!(model["launch_id"], serde_json::json!("L1"));
    assert_eq!(model["state"], serde_json::json!("ready"));
    assert_eq!(model["port"], serde_json::json!(41100));
    let ext = &v["external"][0];
    assert_eq!(ext["pid"], serde_json::json!(999));
  }
}
