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

/// Decode the canonical model path out of a daemon row's nested
/// `id.path` shape. Centralised so the five CLI subcommands that
/// project status / list_models / favorites / last_params rows
/// stop respelling the same two-level `get` (audit §1.1 #7).
pub fn row_path(v: &Value) -> Option<&str> {
  v.get("id")
    .and_then(|id| id.get("path"))
    .and_then(Value::as_str)
}

/// Render `list_models` rows as TSV. Columns: id, path, arch, quant,
/// native_ctx (one line per model, header line first).
pub fn list_human(rows: &[CatalogRow]) -> String {
  if rows.is_empty() {
    return format!("{}\n", crate::cli::colors::dim("(no models discovered)"));
  }
  let mut out = String::new();
  out.push_str(&format!(
    "{}\n",
    crate::cli::colors::bold("NAME\tARCH\tQUANT\tCTX\tPATH")
  ));
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
/// against this, so column drift requires deliberate intent. Wrapped
/// in `{"models": [...]}` so every CLI `--json` surface lives behind
/// the same "always object at the root" rule.
pub fn list_json(rows: &[CatalogRow]) -> Value {
  let arr: Vec<Value> = rows
    .iter()
    .map(|r| {
      // Emit `model_id` only when populated. The IPC `list_models`
      // doesn't currently include it (the catalog has no scan-time
      // BLAKE3 yet), so leaving the field present as `null` would
      // mislead agents into thinking a stable handle exists.
      let mut row = serde_json::json!({
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
      });
      if let Some(id) = &r.model_id {
        row["model_id"] = serde_json::Value::String(id.clone());
      }
      row
    })
    .collect();
  serde_json::json!({"models": arr})
}

/// JSON projection of `favorite_list` rows. Wrapped in
/// `{"favorites": [...]}` (matches the rest of the CLI surface).
/// Each row carries `path` + `name` at the root for symmetry with
/// `list_json` so consumers don't need to descend two levels via
/// `id.path`.
pub fn favorites_json(rows: &[Value]) -> Value {
  let arr: Vec<Value> = rows
    .iter()
    .map(|r| {
      let path = row_path(r).unwrap_or("");
      let name = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
      serde_json::json!({
        "name": name,
        "path": path,
        "id": r.get("id").cloned().unwrap_or(Value::Null),
      })
    })
    .collect();
  serde_json::json!({"favorites": arr})
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
  if let Some(d) = &snap.daemon {
    out.push_str(&format!(
      "daemon: pid={} uptime={}s connections={}\n",
      d.pid, d.uptime_seconds, d.active_connections,
    ));
  }
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
  // GpuInfo serialises as `{"backend": "<name>", ...}` — see
  // `gpu::GpuInfo`'s `#[serde(tag = "backend", rename_all = "snake_case")]`
  // attribute. Earlier versions of this function pattern-matched on
  // PascalCase variant keys (`Nvidia`, `Amd`, `Metal`, `Vulkan`)
  // which the current wire shape never emits, so every non-CpuOnly
  // backend silently fell through to the JSON-blob branch. Match on
  // the tagged-enum shape instead.
  use crate::daemon::host_metrics::GpuFlavor;
  if gpu.is_null() {
    return None;
  }
  let obj = gpu.as_object()?;
  let raw = obj.get("backend").and_then(Value::as_str)?;
  let count = || {
    obj
      .get("devices")
      .and_then(Value::as_array)
      .map(|a| a.len())
      .unwrap_or(0)
  };
  match GpuFlavor::from_label(raw) {
    GpuFlavor::CpuOnly => Some("CPU only".to_string()),
    GpuFlavor::Nvidia => Some(format!("NVIDIA GPU(s): {}", count())),
    GpuFlavor::Amd => Some(format!("AMD GPU(s): {}", count())),
    GpuFlavor::AppleMetal => {
      let total = obj
        .get("total_memory_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
      let gib = total as f64 / (1024.0 * 1024.0 * 1024.0);
      Some(format!("Apple Silicon: {gib:.0}G unified"))
    }
    GpuFlavor::Unknown => Some(format!(
      "Unknown GPU vendor (Vulkan-only): {} device(s)",
      count()
    )),
    GpuFlavor::Unsampled => Some(serde_json::to_string(gpu).unwrap_or_else(|_| "?".to_string())),
  }
}

/// JSON projection of `status` (preserves the daemon's wire shape so
/// agents that already parse `daemon status` keep working).
///
/// Per the api-contract review:
/// * Each row preserves the daemon's `id` object (`{path,
///   header_blake3}`) so consumers can key by the canonical
///   fingerprint, not just the path string. `model_path` survives as
///   a convenience alongside.
/// * Each row also carries `params` (ctx / port / reasoning / mode /
///   advanced) so an agent can answer "how was this launch
///   configured" without a separate `last_params` round-trip.
/// * External rows synthesise `launch_id: "ext-<pid>"` to match the
///   TUI's identifier shape, so `stop ext-<pid>` lines up across
///   surfaces.
pub fn status_json(snap: &StatusSnapshot) -> Value {
  let models: Vec<Value> = snap
    .models
    .iter()
    .map(|r| {
      let mut obj = serde_json::Map::new();
      obj.insert("launch_id".into(), serde_json::json!(r.launch_id));
      // Preserve full `id` object alongside the flat path.
      if let Some(id) = r.id.as_ref() {
        obj.insert("id".into(), id.clone());
      }
      obj.insert("model_path".into(), serde_json::json!(r.model_path));
      obj.insert("port".into(), serde_json::json!(r.port));
      obj.insert("mode".into(), serde_json::json!(r.mode));
      obj.insert("state".into(), serde_json::json!(r.state));
      obj.insert("pid".into(), serde_json::json!(r.pid));
      obj.insert("ready_at".into(), serde_json::json!(r.ready_at));
      if let Some(params) = r.params.as_ref() {
        obj.insert("params".into(), params.clone());
      }
      // Per-PID resource snapshot from the supervisor sampler. `None`
      // before the sampler primes (one tick after launch).
      obj.insert(
        "latest_rss_bytes".into(),
        serde_json::json!(r.latest_rss_bytes),
      );
      obj.insert("latest_cpu_pct".into(), serde_json::json!(r.latest_cpu_pct));
      Value::Object(obj)
    })
    .collect();
  let external: Vec<Value> = snap
    .external
    .iter()
    .map(|r| {
      serde_json::json!({
        "launch_id": format!("ext-{}", r.pid),
        "pid": r.pid,
        "cmdline": r.cmdline,
        "model_path": r.model_path,
      })
    })
    .collect();
  let daemon = snap.daemon.as_ref().map(|d| {
    serde_json::json!({
      "pid": d.pid,
      "uptime_seconds": d.uptime_seconds,
      "active_connections": d.active_connections,
      "build": d.build,
      "server_path": d.server_path,
      "socket_path": d.socket_path,
    })
  });
  serde_json::json!({
    "models": models,
    "external": external,
    "gpu": snap.gpu,
    "host": snap.host,
    "daemon": daemon,
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
      model_id: Some(format!("{name:.8}")),
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
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.starts_with("NAME\tARCH"));
    assert!(plain.contains("qwen.gguf"));
    assert!(plain.contains("8192"));
  }

  #[test]
  fn list_human_handles_empty_catalog() {
    let s = list_human(&[]);
    assert!(console::strip_ansi_codes(&s).contains("no models"));
  }

  #[test]
  fn list_json_wraps_rows_in_models_object_with_documented_keys() {
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let v = list_json(&rows);
    let arr = v
      .get("models")
      .and_then(|m| m.as_array())
      .expect("models array");
    assert_eq!(arr.len(), 1);
    let r = &arr[0];
    for key in [
      "name",
      "path",
      "model_id",
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
  fn list_json_empty_catalog_returns_empty_models_array() {
    let v = list_json(&[]);
    assert_eq!(v, serde_json::json!({"models": []}));
  }

  #[test]
  fn list_json_omits_model_id_when_none() {
    // The IPC `list_models` response doesn't currently include
    // `model_id`. `list_json` must drop the field entirely rather
    // than serialise it as `null`, so agents that test
    // `row.model_id != null` don't have to special-case a missing
    // BLAKE3 column.
    let mut r = row("qwen", "qwen2", "Q4_K", 8192);
    r.model_id = None;
    let v = list_json(&[r]);
    let row_v = v["models"][0].clone();
    assert!(
      row_v.get("model_id").is_none(),
      "model_id must be absent (not null) when CatalogRow.model_id is None"
    );
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
      host: Value::Null,
      daemon: None,
    };
    let s = status_human(&snap);
    assert!(s.contains("no managed"));
  }

  #[test]
  fn status_human_includes_gpu_label_when_present() {
    // The live wire shape is `{"backend": "cpu_only"}` (snake_case
    // tagged enum); the test feeds the same shape the daemon emits,
    // not the legacy PascalCase variant key the function used to
    // match against.
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: serde_json::json!({"backend": "cpu_only"}),
      host: Value::Null,
      daemon: None,
    };
    let s = status_human(&snap);
    assert!(s.contains("CPU only"), "got: {s}");
  }

  #[test]
  fn gpu_label_matches_tagged_enum_shape_for_each_backend() {
    // The daemon serialises GpuInfo with `tag = "backend",
    // rename_all = "snake_case"`. Each backend must produce a
    // human-readable label rather than falling through to the JSON
    // blob branch.
    let nv = serde_json::json!({
      "backend": "nvidia",
      "devices": [
        {"name": "RTX 4090", "total_memory_bytes": 24, "used_memory_bytes": 0},
      ],
    });
    assert_eq!(gpu_label(&nv).as_deref(), Some("NVIDIA GPU(s): 1"));
    let amd = serde_json::json!({
      "backend": "amd",
      "devices": [
        {"name": "RX 7900", "total_memory_bytes": 24, "used_memory_bytes": 0},
        {"name": "RX 7800", "total_memory_bytes": 16, "used_memory_bytes": 0},
      ],
    });
    assert_eq!(gpu_label(&amd).as_deref(), Some("AMD GPU(s): 2"));
    let metal = serde_json::json!({
      "backend": "apple_metal",
      "total_memory_bytes": 64u64 * 1024 * 1024 * 1024,
    });
    assert_eq!(
      gpu_label(&metal).as_deref(),
      Some("Apple Silicon: 64G unified")
    );
    let unknown = serde_json::json!({
      "backend": "unknown",
      "devices": [{"name": "Vulkan device", "total_memory_bytes": 0, "used_memory_bytes": 0}],
    });
    assert_eq!(
      gpu_label(&unknown).as_deref(),
      Some("Unknown GPU vendor (Vulkan-only): 1 device(s)")
    );
  }

  #[test]
  fn status_human_emits_daemon_preamble_when_present() {
    use crate::cli::resolve::DaemonHealth;
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 4242,
        uptime_seconds: 90,
        active_connections: 3,
        build: None,
        server_path: None,
        socket_path: None,
      }),
    };
    let s = status_human(&snap);
    assert!(s.contains("daemon"), "preamble missing: {s}");
    assert!(s.contains("pid=4242"), "pid missing: {s}");
    assert!(s.contains("connections=3"), "conn count missing: {s}");
  }

  #[test]
  fn status_json_round_trips_documented_keys() {
    let snap = StatusSnapshot {
      models: vec![RunningRow {
        launch_id: "L1".into(),
        model_path: "/m/a.gguf".into(),
        id: Some(serde_json::json!({"path": "/m/a.gguf", "header_blake3": "deadbeef"})),
        port: 41100,
        mode: "chat".into(),
        state: "ready".into(),
        pid: Some(123),
        ready_at: Some(1_700_000_000),
        params: None,
        latest_rss_bytes: Some(4_500_000_000),
        latest_cpu_pct: Some(312.0),
      }],
      external: vec![ExternalRow {
        pid: 999,
        cmdline: "llama-server".into(),
        model_path: Some("/m/b.gguf".into()),
      }],
      gpu: Value::String("CpuOnly".into()),
      host: serde_json::json!({"gpu_backend": "amd", "cpu_pct": 12.5}),
      daemon: None,
    };
    let v = status_json(&snap);
    let model = &v["models"][0];
    assert_eq!(model["launch_id"], serde_json::json!("L1"));
    assert_eq!(model["state"], serde_json::json!("ready"));
    assert_eq!(model["port"], serde_json::json!(41100));
    assert_eq!(
      model["latest_rss_bytes"],
      serde_json::json!(4_500_000_000_u64)
    );
    assert_eq!(model["latest_cpu_pct"], serde_json::json!(312.0));
    let ext = &v["external"][0];
    assert_eq!(ext["pid"], serde_json::json!(999));
  }

  #[test]
  fn status_json_includes_daemon_block_when_present() {
    use crate::cli::resolve::DaemonHealth;
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 11,
        uptime_seconds: 7,
        active_connections: 1,
        build: Some("0.1.0".into()),
        server_path: Some("/usr/bin/llama-server".into()),
        socket_path: Some("/run/user/1000/llamastash/daemon.sock".into()),
      }),
    };
    let v = status_json(&snap);
    assert_eq!(v["daemon"]["pid"], serde_json::json!(11));
    assert_eq!(v["daemon"]["uptime_seconds"], serde_json::json!(7));
    assert_eq!(v["daemon"]["active_connections"], serde_json::json!(1));
    assert_eq!(v["daemon"]["build"], serde_json::json!("0.1.0"));
    assert_eq!(
      v["daemon"]["server_path"],
      serde_json::json!("/usr/bin/llama-server")
    );
  }

  #[test]
  fn status_json_preserves_host_block_verbatim() {
    // AGENTS.md guarantees `host` is always an object on the wire.
    // The CLI surface must surface the same shape so agents that
    // parse `status --json` see the same fields as raw IPC clients.
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: serde_json::json!({
        "cpu_pct": 12.5,
        "ram_used_bytes": 1_000_000_u64,
        "ram_total_bytes": 64_000_000_u64,
        "gpu_backend": "amd",
        "gpu_util_pct": 73.0,
        "gpu_temp_c": 62.0,
        "gpu_mem_used_bytes": 3_000_000_000_u64,
        "gpu_mem_total_bytes": 64_000_000_000_u64,
        "gpu_device_count": 1,
      }),
      daemon: None,
    };
    let v = status_json(&snap);
    assert!(v.get("host").is_some(), "host key must appear: {v}");
    assert_eq!(v["host"]["gpu_backend"], serde_json::json!("amd"));
    assert_eq!(v["host"]["cpu_pct"], serde_json::json!(12.5));
    assert_eq!(v["host"]["gpu_device_count"], serde_json::json!(1));
  }
}
