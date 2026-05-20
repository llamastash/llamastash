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

use crate::cli::resolve::{CatalogRow, StatusSnapshot};

/// Decode the canonical model path out of a daemon row's nested
/// `id.path` shape. Centralised so the five CLI subcommands that
/// project status / list_models / favorites / last_params rows
/// stop respelling the same two-level `get` (audit §1.1 #7).
pub fn row_path(v: &Value) -> Option<&str> {
  v.get("id")
    .and_then(|id| id.get("path"))
    .and_then(Value::as_str)
}

/// Render `list_models` rows as a padded table on TTY, or
/// tab-separated rows when colors are disabled (piped / `--no-colors` /
/// `NO_COLOR`). Columns: NAME, ARCH, QUANT, CTX, PATH.
///
/// Footer line `(N models)` is appended on TTY only — the piped form
/// stays byte-identical to today's TSV so existing `awk -F\t` /
/// `column -t` pipelines keep working.
pub fn list_human(rows: &[CatalogRow]) -> String {
  use crate::cli::{colors, format};
  if rows.is_empty() {
    return format!("{}\n", colors::dim("(no models discovered)"));
  }
  let header = ["NAME", "ARCH", "QUANT", "CTX", "PATH"];
  let body: Vec<Vec<String>> = rows
    .iter()
    .map(|r| {
      let arch = r.arch.as_deref().unwrap_or("?");
      let quant = r.quant.as_deref().unwrap_or("?");
      let ctx = r
        .native_ctx
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
      vec![
        r.name(),
        arch.to_string(),
        quant.to_string(),
        ctx,
        // Path styling is deliberately a no-op when colors are off,
        // so the TSV branch stays byte-stable for piped consumers.
        colors::path(&r.path),
      ]
    })
    .collect();
  let mut out = format::table(&header, &body);
  if console::colors_enabled() {
    out.push_str(&colors::count(rows.len(), "models"));
    out.push('\n');
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
///
/// Two surfaces share one source:
/// - On TTY (`colors_enabled()`): section header for the daemon, a
///   `kv_block` for its fields, a padded launches table, and a dim
///   GPU footer line.
/// - On non-TTY / `--no-colors`: today's single-line daemon preamble
///   plus byte-stable TSV rows — preserves `awk -F\t` / `column -t`
///   pipelines.
///
/// Columns are stable across modes (LAUNCH_ID, STATE, MODE, PORT, PID,
/// PATH). RSS/CPU% are intentionally not surfaced here even when the
/// per-PID sampler has primed them — they belong in a future
/// `--detail` view rather than always-on columns that would push
/// path truncation onto narrow terminals.
pub fn status_human(snap: &StatusSnapshot) -> String {
  use crate::cli::{colors, format};

  let tty = console::colors_enabled();
  let mut out = String::new();

  // Daemon preamble.
  if let Some(d) = &snap.daemon {
    if tty {
      out.push_str(&format::section_header("daemon", None));
      let pid_styled = console::style(d.pid.to_string()).bold().to_string();
      let uptime = format::format_uptime(d.uptime_seconds);
      out.push_str(&format::kv_block(&[
        ("pid", pid_styled),
        ("uptime", uptime),
        ("connections", d.active_connections.to_string()),
      ]));
      out.push('\n');
    } else {
      out.push_str(&format!(
        "daemon: pid={} uptime={}s connections={}\n",
        d.pid, d.uptime_seconds, d.active_connections,
      ));
    }
  }

  // Launches table.
  if snap.models.is_empty() && snap.external.is_empty() {
    out.push_str(&colors::dim("(no managed launches)"));
    out.push('\n');
  } else {
    let header = ["LAUNCH_ID", "STATE", "MODE", "PORT", "PID", "PATH"];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(snap.models.len() + snap.external.len());
    for r in &snap.models {
      let pid = r
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());
      rows.push(vec![
        if tty {
          colors::launch_id(&r.launch_id)
        } else {
          r.launch_id.clone()
        },
        if tty {
          colors::state(&r.state)
        } else {
          r.state.clone()
        },
        r.mode.clone(),
        if tty {
          colors::port(r.port)
        } else {
          r.port.to_string()
        },
        pid,
        if tty {
          colors::path(&r.model_path)
        } else {
          r.model_path.clone()
        },
      ]);
    }
    for r in &snap.external {
      let path = r.model_path.as_deref().unwrap_or(&r.cmdline);
      // External rows are styled dim end-to-end so they read as
      // observer-only entries vs the bright managed ones.
      let dim_or_plain = |s: &str| if tty { colors::dim(s) } else { s.to_string() };
      rows.push(vec![
        dim_or_plain("external"),
        dim_or_plain("external"),
        dim_or_plain("-"),
        dim_or_plain("-"),
        dim_or_plain(&r.pid.to_string()),
        if tty {
          colors::dim(&colors::path(path))
        } else {
          path.to_string()
        },
      ]);
    }
    out.push_str(&format::table(&header, &rows));
  }

  // GPU footer.
  if let Some(label) = gpu_label(&snap.gpu) {
    out.push('\n');
    if tty {
      out.push_str(&colors::dim(&format!("GPU: {label}")));
      out.push('\n');
    } else {
      out.push_str(&format!("GPU: {label}\n"));
    }
  }
  out
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
  use crate::cli::resolve::{ExternalRow, RunningRow};
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
  fn list_human_tsv_branch_emits_byte_exact_today_shape() {
    // Regression guard: piped consumers see exactly today's TSV bytes.
    // Snapshot string is the exact format the pre-padded-table code
    // produced so awk/cut/column pipelines don't drift.
    let _g = ColorGuard::set(false);
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let s = list_human(&rows);
    assert_eq!(
      s,
      "NAME\tARCH\tQUANT\tCTX\tPATH\nqwen.gguf\tqwen2\tQ4_K\t8192\t/m/qwen.gguf\n"
    );
  }

  #[test]
  fn list_human_tty_branch_pads_columns_and_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let rows = vec![
      row("qwen", "qwen2", "Q4_K", 8192),
      row("phi", "phi3", "Q5_K", 4096),
    ];
    let s = list_human(&rows);
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.starts_with("NAME"), "header missing: {plain:?}");
    assert!(
      !plain.contains("NAME\t"),
      "padded output must not contain tabs in header: {plain:?}"
    );
    assert!(plain.contains("qwen.gguf"));
    assert!(plain.contains("(2 models)"), "footer missing: {plain:?}");
  }

  #[test]
  fn list_human_handles_empty_catalog() {
    // Same dim line in both modes — empty-state message is plain
    // bytes either way.
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let s = list_human(&[]);
      assert!(console::strip_ansi_codes(&s).contains("no models"));
    }
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
    // Both modes produce the same "no managed launches" dim line.
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let snap = StatusSnapshot {
        models: vec![],
        external: vec![],
        gpu: Value::Null,
        host: Value::Null,
        daemon: None,
      };
      let s = status_human(&snap);
      assert!(console::strip_ansi_codes(&s).contains("no managed"));
    }
  }

  #[test]
  fn status_human_includes_gpu_label_when_present() {
    // The live wire shape is `{"backend": "cpu_only"}` (snake_case
    // tagged enum); the test feeds the same shape the daemon emits,
    // not the legacy PascalCase variant key the function used to
    // match against. The label content is the same in both modes;
    // only the surrounding color styling differs.
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let snap = StatusSnapshot {
        models: vec![],
        external: vec![],
        gpu: serde_json::json!({"backend": "cpu_only"}),
        host: Value::Null,
        daemon: None,
      };
      let s = status_human(&snap);
      let plain = console::strip_ansi_codes(&s);
      assert!(plain.contains("CPU only"), "got: {plain}");
    }
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
  fn status_human_tsv_branch_emits_legacy_daemon_preamble_byte_shape() {
    // Piped consumers parsing today's `pid=N` / `connections=N` form
    // stay supported byte-for-byte.
    let _g = ColorGuard::set(false);
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
    assert!(s.starts_with("daemon: pid=4242"), "preamble shape: {s:?}");
    assert!(s.contains("uptime=90s"));
    assert!(s.contains("connections=3"));
  }

  #[test]
  fn status_human_tty_branch_renders_daemon_section_header_and_kv_block() {
    let _g = ColorGuard::set(true);
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
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.starts_with("daemon\n"), "section header: {plain:?}");
    assert!(plain.contains("pid  4242"), "kv pid: {plain:?}");
    assert!(plain.contains("uptime  1m 30s"), "kv uptime: {plain:?}");
    assert!(
      plain.contains("connections  3"),
      "kv connections: {plain:?}"
    );
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

  fn running(launch_id: &str, state: &str, port: u16, path: &str) -> RunningRow {
    RunningRow {
      launch_id: launch_id.into(),
      model_path: path.into(),
      id: None,
      port,
      state: state.into(),
      pid: Some(123),
      mode: "chat".into(),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
    }
  }

  #[test]
  fn status_human_tsv_branch_emits_byte_stable_launches_table() {
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![running("L1", "ready", 41100, "/m/qwen.gguf")],
      external: vec![ExternalRow {
        pid: 9999,
        cmdline: "llama-server".into(),
        model_path: Some("/m/ext.gguf".into()),
      }],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
    };
    let s = status_human(&snap);
    // Regression guard: managed + external rows are exact tabs, no
    // padding, no color, no truncation.
    assert!(
      s.contains("LAUNCH_ID\tSTATE\tMODE\tPORT\tPID\tPATH\n"),
      "header drifted: {s:?}"
    );
    assert!(s.contains("L1\tready\tchat\t41100\t123\t/m/qwen.gguf\n"));
    assert!(s.contains("external\texternal\t-\t-\t9999\t/m/ext.gguf\n"));
  }

  #[test]
  fn status_human_tty_branch_pads_launches_table_and_colors_state() {
    let _g = ColorGuard::set(true);
    let snap = StatusSnapshot {
      models: vec![running("L1", "ready", 41100, "/m/qwen.gguf")],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
    };
    let s = status_human(&snap);
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.contains("LAUNCH_ID"), "header missing: {plain:?}");
    assert!(
      !plain.contains("LAUNCH_ID\t"),
      "padded layout must not contain tabs: {plain:?}"
    );
    assert!(plain.contains("L1"));
    assert!(plain.contains("ready"));
    assert!(plain.contains("41100"));
  }
}
