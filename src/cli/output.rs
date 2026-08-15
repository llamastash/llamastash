//! Human + JSON output formatters shared by the non-interactive
//! subcommands.
//!
//! Two surfaces, one source of truth: every command supports `--json`
//! whose shape is the public agent contract; the human-readable form
//! is best-effort prettification.
//!
//! Tab-separated text is the default human format. Agents pin against
//! `--json`; humans get something `column -t` friendly.

use std::collections::HashMap;

use serde_json::Value;

use crate::cli::resolve::{CatalogRow, RunningRow, StatusSnapshot};
use crate::tui::status_icons::{glyph_for, SurfaceState};

/// Decode the canonical model path out of a daemon row's nested
/// `id.path` shape. Centralised so the five CLI subcommands that
/// project status / list_models / favorites / last_params rows
/// stop respelling the same two-level `get`.
pub fn row_path(v: &Value) -> Option<&str> {
  v.get("id")
    .and_then(|id| id.get("path"))
    .and_then(Value::as_str)
    // A row whose identity is a backend registry entry serializes as
    // `{backend, name}` and carries no `id.path`, so keying on that alone
    // dropped every such row: `last-params <model>` reported "no recorded
    // last-params" for a model whose entry the IPC method was returning.
    // `params.model_path` is the shape-agnostic key — every row has one, and
    // it is what the catalog row's `path` is compared against.
    .or_else(|| {
      v.get("params")
        .and_then(|p| p.get("model_path"))
        .and_then(Value::as_str)
    })
}

/// Render `list_models` rows as a padded table on TTY, or
/// tab-separated rows when colors are disabled (piped / `--no-colors` /
/// `NO_COLOR`). Columns: `NAME ARCH PARAMS QUANT CTX SIZE MODE [BACKEND]
/// STATUS [DEVICE]` — the bracketed two are conditional (see below).
///
/// SIZE displays the GGUF weights footprint (matches the TUI list
/// pane's SIZE column) — PATH was dropped because the canonical paths
/// dominated line width on real caches. `--json` still carries `path`
/// for agent consumers.
///
/// MODE shows the catalog row's `mode_hint` (`chat` / `embedding` /
/// `rerank`), matching the TUI list's Mode column.
///
/// DEVICE appears only when `multi_device` is set (some single server
/// offers more than one device — the CLI mirror of the TUI's
/// `App::multi_device` gate). Cell semantics mirror the TUI's
/// `column_value`: an explicit `--device` selector renders verbatim,
/// `all` for a running row on the device-selecting default backend
/// with no override, and the `?` placeholder otherwise.
///
/// STATUS shows the live supervisor state when one exists for the
/// row's path: a TUI-shared glyph (`● ready`, `◐ loading`, …) followed
/// by `:<port>`. The column is empty for rows with no supervisor so
/// the catalog stays uncluttered. `running` is the index produced by
/// [`crate::cli::resolve::running_index`] — pass an empty map to opt out.
///
/// Footer line `(N models)` is appended on TTY only — the piped form
/// stays byte-stable for `awk -F\t` / `column -t` pipelines.
pub fn list_human(
  rows: &[CatalogRow],
  running: &HashMap<String, RunningRow>,
  multi_device: bool,
) -> String {
  use crate::cli::{colors, format};
  if rows.is_empty() {
    return format!("{}\n", colors::dim("(no models discovered)"));
  }
  // Show the BACKEND column when any model surfaces a backend beyond the
  // default `llamacpp` — either a non-default primary or more than one supported
  // backend (a ds4-compatible file lists `ds4|llamacpp`). Matches the TUI's
  // `multi_backend` gate.
  let show_backend = rows.iter().any(|r| {
    r.supported_backends.len() > 1
      || r
        .supported_backends
        .iter()
        .any(|b| b != crate::backend::DEFAULT_BACKEND_ID)
      || r
        .backend
        .as_deref()
        .is_some_and(|b| b != crate::backend::DEFAULT_BACKEND_ID)
  });
  let mut header: Vec<&str> = vec!["NAME", "ARCH", "PARAMS", "QUANT", "CTX", "SIZE", "MODE"];
  if show_backend {
    header.push("BACKEND");
  }
  header.push("STATUS");
  if multi_device {
    header.push("DEVICE");
  }
  let body: Vec<Vec<String>> = rows
    .iter()
    .map(|r| {
      // Shared `list_cell` so a missing Arch/Quant/Params reads as one
      // placeholder (`?`), matching the TUI list — an unknown quant no longer
      // leaks the literal `Unknown` while Arch shows `?`.
      let arch = crate::tui::fmt::list_cell(r.arch.as_deref(), "?");
      let params = crate::tui::fmt::list_cell(r.parameter_label.as_deref(), "?");
      let quant = crate::tui::fmt::list_cell(r.quant.as_deref(), "?");
      let ctx = r
        .native_ctx
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
      // Compute the on-disk total via the shared shard-sizes util so
      // a row's SIZE always reflects shard 1 + every sibling shard,
      // independent of when the daemon last scanned (its cached
      // `weights_bytes` may predate a binary upgrade that fixed the
      // split-shard aggregation). One `stat` per row is cheap.
      let size = display_size(r);
      let mode = crate::tui::fmt::list_cell(r.mode_hint.as_deref(), "?");
      let status = running_status_cell(running.get(&r.path));
      let mut cells = vec![r.name(), arch, params, quant, ctx, size, mode];
      if show_backend {
        cells.push(backend_badge(r, "?"));
      }
      cells.push(status);
      if multi_device {
        cells.push(device_cell(running.get(&r.path)));
      }
      cells
    })
    .collect();
  let mut out = format::table(&header, &body);
  if console::colors_enabled() {
    out.push_str(&colors::count(rows.len(), "models"));
    out.push('\n');
  }
  out
}

/// Render the STATUS cell. Empty for non-running rows; for running
/// rows: `<glyph> <state> :<port>`, reusing the TUI's `glyph_for`
/// mapping so the two surfaces never drift.
fn running_status_cell(row: Option<&RunningRow>) -> String {
  use crate::cli::colors;
  let Some(r) = row else {
    return String::new();
  };
  let surface = SurfaceState::from_wire_label(&r.state);
  let glyph = glyph_for(surface);
  let tty = console::colors_enabled();
  let state_label = if tty {
    colors::state(&r.state)
  } else {
    r.state.clone()
  };
  let port_part = format!(":{port}", port = r.port);
  let port_part = if tty {
    colors::dim(&port_part)
  } else {
    port_part
  };
  format!("{glyph} {state_label} {port_part}")
}

/// Backend badge for a catalog row: every backend that can serve it,
/// priority-ordered (`ds4|llamacpp`), else the daemon's single tag, else
/// `placeholder`. Shared by the `list` BACKEND column and `show`'s header
/// row so the two surfaces can never name different backends for one model.
/// The table formatter clips the cell if it overflows.
pub(crate) fn backend_badge(row: &CatalogRow, placeholder: &str) -> String {
  if !row.supported_backends.is_empty() {
    return row.supported_backends.join("|");
  }
  crate::tui::fmt::list_cell(row.backend.as_deref(), placeholder)
}

/// The explicit `--device` selector a running launch was dispatched with,
/// or `None` when it took the backend's default placement. One reader for
/// the `knobs.device` wire path, shared by the DEVICE cell and the
/// `list --json` per-row `status` object.
fn device_selector(row: &RunningRow) -> Option<&str> {
  row
    .params
    .as_ref()
    .and_then(|p| p.get("knobs"))
    .and_then(|k| k.get("device"))
    .and_then(Value::as_str)
}

/// Render the DEVICE cell, mirroring the TUI list pane's
/// `column_value(ColumnId::Device)`: an explicit `--device` selector
/// renders verbatim; a *running* row on the device-selecting default
/// backend with no override targets every GPU (llama.cpp's default), so
/// it reads `all` instead of a blank; anything else (not running, or a
/// device-less backend) gets the CLI table's `?` placeholder.
fn device_cell(row: Option<&RunningRow>) -> String {
  let Some(r) = row else {
    return "?".into();
  };
  match device_selector(r) {
    Some(d) => d.to_string(),
    None
      if r
        .backend
        .as_deref()
        .unwrap_or(crate::backend::DEFAULT_BACKEND_ID)
        == crate::backend::DEFAULT_BACKEND_ID =>
    {
      "all".into()
    }
    None => "?".into(),
  }
}

/// SIZE column for one row. Tries the shared shard-sizes util first
/// (sums shard 1 + every sibling on disk); falls back to the wire
/// shape's `weights_bytes` when neither path exists yet (a row that
/// surfaced from the catalog but was deleted between scan and
/// render), and finally to `?` when even that is absent.
pub(crate) fn display_size(row: &CatalogRow) -> String {
  use std::path::PathBuf;
  let primary = PathBuf::from(&row.path);
  let siblings: Vec<PathBuf> = row.split_siblings.iter().map(PathBuf::from).collect();
  let total = crate::discovery::shard_sizes::on_disk_total(&primary, &siblings);
  if total > 0 {
    return crate::tui::fmt::format_bytes(total);
  }
  row
    .weights_bytes
    .map(crate::tui::fmt::format_bytes)
    .unwrap_or_else(|| "?".to_string())
}

/// Backend id that serves a catalog row from its `source` label, via
/// [`ModelSource::backend_id`](crate::discovery::ModelSource::backend_id).
/// An unrecognized label falls back to the default backend. Names no backend.
pub(crate) fn backend_for_source(source: &str) -> &'static str {
  crate::discovery::ModelSource::from_label(source)
    .map(|s| s.backend_id())
    .unwrap_or(crate::backend::DEFAULT_BACKEND_ID)
}

/// JSON projection of `list_models` rows. Stable shape — agents pin
/// against this, so column drift requires deliberate intent. Wrapped
/// in `{"models": [...]}` so every CLI `--json` surface lives behind
/// the same "always object at the root" rule.
pub fn list_json(rows: &[CatalogRow], running: &HashMap<String, RunningRow>) -> Value {
  let arr: Vec<Value> = rows
    .iter()
    .map(|r| {
      // One shape: serialise the shared `CatalogRow` — byte-identical to the
      // daemon's `list_models` rows (nested `metadata`, `mtp`, `multimodal`).
      // `status` is the only CLI-side enrichment; it comes from a separate
      // `status` query the catalog itself doesn't carry.
      let mut row = serde_json::to_value(r).unwrap_or_else(|_| serde_json::json!({}));
      // `status` is a small nested object so agents can pin
      // `models[i].status.state` / `.port`. Absent (not `null`) when the
      // model has no live supervisor. `device` carries the raw selector
      // (`null` when the launch took the backend default) so an agent reads
      // the same fact the human table's DEVICE column renders as `all`.
      if let Some(live) = running.get(&r.path) {
        row["status"] = serde_json::json!({
          "state": live.state,
          "port": live.port,
          "launch_id": live.launch_id,
          "device": device_selector(live),
        });
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
      // Scheme-aware: a `lemonade://` synthetic path keeps its full registry
      // name (dotted names like `qwen3.5-4b-FLM` must not stem to `qwen3`);
      // real GGUF files still drop the extension. Same resolver the TUI and
      // `last-params` use, so the name is consistent across every surface.
      let name = crate::util::paths::model_display_name(std::path::Path::new(path));
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
/// NAME). NAME is the file basename so narrow terminals don't get
/// crushed by canonical paths; `--json` keeps the full `model_path`
/// for agents. RSS/CPU% are intentionally not surfaced here even when
/// the per-PID sampler has primed them — they belong in a future
/// `--detail` view rather than always-on columns.
/// Render the `status.backends` array into a concise section:
/// per backend, its install state + the accelerators it can run on this
/// host. Returns `None` when the field is absent (older daemon) or empty so
/// the caller skips the section entirely.
fn backends_human(backends: &serde_json::Value, tty: bool) -> Option<String> {
  use crate::cli::{colors, format};
  let arr = backends.as_array()?;
  if arr.is_empty() {
    return None;
  }
  let mut out = String::new();
  if tty {
    out.push_str(&format::section_header("backends", None));
  }
  for b in arr {
    let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let installed = b
      .get("installed")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let accel: Vec<&str> = b
      .get("accelerators")
      .and_then(|v| v.as_array())
      .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
      .unwrap_or_default();
    let accel_str = if accel.is_empty() {
      "—".to_string()
    } else {
      accel.join(", ")
    };
    // Managed-multiplexer backends carry an `umbrella` health string
    // (running / starting / not running / disabled) that `installed` alone
    // can't convey. Absent for process-per-model backends (llama.cpp).
    let umbrella = b.get("umbrella").and_then(|v| v.as_str());
    if tty {
      let state = if installed {
        colors::success("installed")
      } else {
        colors::dim("not installed")
      };
      let umbrella_str = umbrella
        .map(|u| format!("  {}", colors::dim(&format!("(umbrella: {u})"))))
        .unwrap_or_default();
      out.push_str(&format!(
        "  {} {}  {}{}\n",
        console::style(id).bold(),
        state,
        colors::dim(&accel_str),
        umbrella_str,
      ));
    } else {
      let state = if installed {
        "installed"
      } else {
        "not installed"
      };
      let umbrella_str = umbrella
        .map(|u| format!(" umbrella={u}"))
        .unwrap_or_default();
      out.push_str(&format!(
        "backend {id}: {state} [{accel_str}]{umbrella_str}\n"
      ));
    }
  }
  out.push('\n');
  Some(out)
}

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
      let mut rows: Vec<(&'static str, String)> = vec![
        ("pid", pid_styled),
        ("uptime", uptime),
        ("connections", d.active_connections.to_string()),
      ];
      // Proxy row — surfaced inline in the daemon block when the
      // proxy is enabled. Skipped (per plan §Test scenarios edge
      // case 3) when disabled. The label cycles through the same
      // wire labels the IPC emits so a user grepping `status` text
      // matches the same strings agents key on.
      if let Some(line) = proxy_human_label(&snap.proxy) {
        rows.push(("proxy", line));
      }
      if let Some(url) = proxy_ui_url(&snap.proxy) {
        rows.push(("web ui", url.to_string()));
      }
      out.push_str(&format::kv_block(&rows));
      out.push('\n');
    } else {
      out.push_str(&format!(
        "daemon: pid={} uptime={}s connections={}\n",
        d.pid, d.uptime_seconds, d.active_connections,
      ));
      if let Some(line) = proxy_human_label(&snap.proxy) {
        out.push_str(&format!("proxy: {line}\n"));
      }
      if let Some(url) = proxy_ui_url(&snap.proxy) {
        out.push_str(&format!("web ui: {url}\n"));
      }
    }
  } else if let Some(line) = proxy_human_label(&snap.proxy) {
    // No daemon block (older daemon) but the proxy field is
    // present — surface it on its own line so the user can still
    // see the listener state.
    let ui = proxy_ui_url(&snap.proxy);
    if tty {
      out.push_str(&format::section_header("proxy", None));
      let mut rows: Vec<(&'static str, String)> = vec![("status", line)];
      if let Some(url) = ui {
        rows.push(("web ui", url.to_string()));
      }
      out.push_str(&format::kv_block(&rows));
      out.push('\n');
    } else {
      out.push_str(&format!("proxy: {line}\n"));
      if let Some(url) = ui {
        out.push_str(&format!("web ui: {url}\n"));
      }
    }
  }

  // Backends section: installed state + accelerators per backend.
  if let Some(block) = backends_human(&snap.backends, tty) {
    out.push_str(&block);
  }

  // Launches table.
  if snap.models.is_empty() && snap.external.is_empty() {
    out.push_str(&colors::dim("(no managed launches)"));
    out.push('\n');
  } else {
    let header = ["LAUNCH_ID", "STATE", "MODE", "PORT", "PID", "CTX", "NAME"];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(snap.models.len() + snap.external.len());
    for r in &snap.models {
      let pid = r
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());
      // Resolved context window `--fit` chose; "-" until the
      // post-Ready `/props` fetch lands. A trailing `*` flags a
      // memory-driven clamp to the floor, explained beneath the
      // table so the column width stays stable.
      let ctx = match r.resolved_ctx {
        Some(c) if r.ctx_clamped => format!("{c}*"),
        Some(c) => c.to_string(),
        None => "-".to_string(),
      };
      let name = r.name();
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
        ctx,
        name,
      ]);
    }
    for r in &snap.external {
      let name = r.name();
      // External rows are styled dim end-to-end so they read as
      // observer-only entries vs the bright managed ones.
      let dim_or_plain = |s: &str| if tty { colors::dim(s) } else { s.to_string() };
      rows.push(vec![
        dim_or_plain("external"),
        dim_or_plain("external"),
        dim_or_plain("-"),
        dim_or_plain("-"),
        dim_or_plain(&r.pid.to_string()),
        dim_or_plain("-"),
        if tty { colors::dim(&name) } else { name },
      ]);
    }
    out.push_str(&format::table(&header, &rows));
    // Surface the failure cause for any error-state row beneath the
    // table — long-form, so a `health probe timeout` message and its
    // stderr tail don't trash the column widths. Without this the
    // user has to scrape the log file just to see *why* a launch
    // ended up in `error`.
    let causes: Vec<(&str, &str)> = snap
      .models
      .iter()
      .filter_map(|r| Some((r.launch_id.as_str(), r.state_cause.as_deref()?)))
      .collect();
    if !causes.is_empty() {
      out.push('\n');
      for (lid, cause) in causes {
        let cause_header = if tty {
          colors::dim(&format!("{lid} cause:"))
        } else {
          format!("{lid} cause:")
        };
        out.push_str(&format!("{cause_header} {cause}\n"));
      }
    }
    // Explain the `*` clamp marker for any row whose ctx fit had to
    // squeeze to the floor — same long-form treatment as causes
    // so the table columns stay stable.
    let clamped: Vec<&str> = snap
      .models
      .iter()
      .filter(|r| r.ctx_clamped)
      .map(|r| r.launch_id.as_str())
      .collect();
    if !clamped.is_empty() {
      out.push('\n');
      for lid in clamped {
        let note = format!("{lid} note: * ctx clamped to the fit-ctx floor under memory pressure");
        out.push_str(&format!(
          "{}\n",
          if tty { colors::dim(&note) } else { note }
        ));
      }
    }
  }

  // GPU footer — sourced from the live `host` snapshot (the same source
  // the TUI host pane reads) and formatted with the shared
  // `gpu_summary_line`, so `status`, `doctor`, and the TUI name the GPU
  // identically. The separate `snap.gpu` GpuInfo field is no longer used
  // here (it diverged: it showed "CPU only" during the pre-first-sample
  // window while the TUI already saw the card).
  if let Some(label) = host_gpu_label(&snap.host) {
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

/// One-line GPU summary for the `status` footer, sourced from the live
/// `host` snapshot (the same source the TUI host pane reads) and
/// formatted with the shared [`crate::init::detection::gpu_summary_line`]
/// so `status`, `doctor`, and the TUI name the GPU identically. `None`
/// only when `host` carries no backend at all.
fn host_gpu_label(host: &Value) -> Option<String> {
  let backend = host.get("gpu_backend").and_then(Value::as_str)?;
  let pool = host.get("gpu_mem_total_bytes").and_then(Value::as_u64);
  let class = host
    .get("uma_class_source")
    .and_then(|v| serde_json::from_value::<crate::gpu::ClassSource>(v.clone()).ok());
  Some(crate::init::detection::gpu_summary_line(
    backend, pool, class,
  ))
}

/// Format the proxy block for the human-readable status table.
/// Returns `None` when the proxy is disabled or the block is absent
/// — the row is then skipped entirely so a config that turns the
/// proxy off doesn't add noise to the table (plan §Test scenarios
/// edge case 3).
///
/// Examples:
/// - listening  → `listening 127.0.0.1:11434`
/// - port_in_use → `port_in_use 127.0.0.1:11434 (port taken)`
/// - unbound    → `unbound 127.0.0.1:80 (permission denied)`
fn proxy_human_label(proxy: &Value) -> Option<String> {
  let obj = proxy.as_object()?;
  let status = obj.get("status").and_then(Value::as_str)?;
  if status == "disabled" {
    return None;
  }
  let listen = obj.get("listen").and_then(Value::as_str).unwrap_or("?");
  match status {
    "listening" => Some(format!("listening {listen}")),
    "port_in_use" => Some(format!("port_in_use {listen} (port taken)")),
    "unbound" => {
      let cause = obj
        .get("bind_error")
        .and_then(Value::as_str)
        .unwrap_or("bind failed");
      Some(format!("unbound {listen} ({cause})"))
    }
    other => Some(format!("{other} {listen}")),
  }
}

/// The stock web-UI URL from the proxy block — present only while the proxy is
/// listening (the daemon populates `ui_url` only in that state). Surfaced as a
/// `web ui` row in `status` so users don't have to reconstruct the port-stable
/// `/ui/` origin by hand.
fn proxy_ui_url(proxy: &Value) -> Option<&str> {
  proxy.as_object()?.get("ui_url")?.as_str()
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
      if let Some(cause) = r.state_cause.as_deref() {
        obj.insert("state_cause".into(), serde_json::json!(cause));
      }
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
      // Fit-resolved context window, null until the post-Ready
      // `/props` fetch lands.
      obj.insert("resolved_ctx".into(), serde_json::json!(r.resolved_ctx));
      // Config-preset hint, mirrored byte-for-byte from IPC `status`.
      obj.insert("preset_count".into(), serde_json::json!(r.preset_count));
      obj.insert("default".into(), serde_json::json!(r.preset_default));
      // Resolved backend, mirrored from IPC `status`.
      obj.insert("backend".into(), serde_json::json!(r.backend));
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
        "port": r.port,
        "launched_by_llamastash": r.launched_by_llamastash,
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
      "ipc_url": d.ipc_url,
    })
  });
  let mut body = serde_json::json!({
    "models": models,
    "external": external,
    "gpu": snap.gpu,
    "host": snap.host,
    "daemon": daemon,
  });
  // Proxy block — surfaced byte-for-byte from the IPC `status`
  // response so agents that parse `status --json` see the same
  // shape as raw IPC clients. Pre-Unit-5 daemons emit no
  // block; we mirror that by omitting the key entirely rather than
  // synthesising a placeholder.
  if !snap.proxy.is_null() {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("proxy".into(), snap.proxy.clone());
    }
  }
  // Backends block — same byte-for-byte mirror posture as proxy:
  // present verbatim when the daemon emits it, omitted against older daemons.
  if !snap.backends.is_null() {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("backends".into(), snap.backends.clone());
    }
  }
  // Server catalog — same verbatim mirror; omitted against older daemons.
  if !snap.servers.is_null() {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("servers".into(), snap.servers.clone());
    }
  }
  body
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

  /// `last-params <model>` filters on this. A backend-registry identity has no
  /// `id.path`, so keying on it alone hid every such row behind "no recorded
  /// last-params" while `last_params_list` was returning it.
  #[test]
  fn row_path_falls_back_to_params_model_path_for_a_backend_identity() {
    let gguf = serde_json::json!({
      "id": {"path": "/m/a.gguf", "header_blake3": "ab"},
      "params": {"model_path": "/m/a.gguf"},
    });
    assert_eq!(row_path(&gguf), Some("/m/a.gguf"));

    let backend = serde_json::json!({
      "id": {"backend": "some-backend", "name": "o/r"},
      "params": {"model_path": "/hf/models--o--r/snapshots/rev"},
    });
    assert_eq!(
      row_path(&backend),
      Some("/hf/models--o--r/snapshots/rev"),
      "a row with no id.path still resolves through params.model_path"
    );

    assert_eq!(row_path(&serde_json::json!({})), None);
  }

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
  fn list_human_tsv_branch_emits_byte_exact_today_shape() {
    // Regression guard: piped consumers see exactly today's TSV bytes.
    // Snapshot string is the exact format the pre-padded-table code
    // produced so awk/cut/column pipelines don't drift.
    let _g = ColorGuard::set(false);
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let s = list_human(&rows, &HashMap::new(), false);
    assert_eq!(
      s,
      "NAME\tARCH\tPARAMS\tQUANT\tCTX\tSIZE\tMODE\tSTATUS\nqwen.gguf\tqwen2\t7B\tQ4_K\t8192\t3.9G\tchat\t\n"
    );
  }

  #[test]
  fn list_human_mode_column_always_present() {
    let _g = ColorGuard::set(false);
    let mut embed = row("bge", "bert", "F16", 512);
    embed.mode_hint = Some("embedding".to_string());
    let modes = list_human(
      &[row("qwen", "qwen2", "Q4_K", 8192), embed],
      &HashMap::new(),
      false,
    );
    assert!(
      modes.contains("MODE\tSTATUS"),
      "MODE header missing: {modes:?}"
    );
    assert!(
      modes.contains("\tchat\t"),
      "chat mode cell missing: {modes:?}"
    );
    assert!(
      modes.contains("\tembedding\t"),
      "embedding mode cell missing: {modes:?}"
    );
    // A row with no mode hint renders the same `?` placeholder as the
    // other unknown-metadata cells.
    let mut bare = row("raw", "qwen2", "Q4_K", 8192);
    bare.mode_hint = None;
    let placeholder = list_human(&[bare], &HashMap::new(), false);
    assert!(
      placeholder.contains("\t?\t"),
      "mode placeholder missing: {placeholder:?}"
    );
  }

  #[test]
  fn list_human_device_column_gated_on_multi_device() {
    let _g = ColorGuard::set(false);
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let single = list_human(&rows, &HashMap::new(), false);
    assert!(
      !single.contains("DEVICE"),
      "single-device host hides the column: {single:?}"
    );
    let multi = list_human(&rows, &HashMap::new(), true);
    assert!(
      multi.contains("MODE\tSTATUS\tDEVICE"),
      "multi-device host shows the column after STATUS: {multi:?}"
    );
    // An idle row renders the `?` placeholder.
    assert!(
      multi.contains("\t?\n"),
      "idle row renders the placeholder: {multi:?}"
    );
  }

  #[test]
  fn list_human_device_cell_mirrors_tui_semantics() {
    let _g = ColorGuard::set(false);
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let path = "/m/qwen.gguf";

    // Explicit `--device` selector renders verbatim.
    let mut explicit = running("L1", "ready", 41100, path);
    explicit.params = Some(serde_json::json!({"knobs": {"device": "ROCm0"}}));
    let mut explicit_idx = HashMap::new();
    explicit_idx.insert(path.to_string(), explicit);
    let explicit_out = list_human(&rows, &explicit_idx, true);
    assert!(
      explicit_out.contains("\tROCm0\n"),
      "explicit selector missing: {explicit_out:?}"
    );

    // Running on the default backend with no override → `all`.
    let plain = running("L1", "ready", 41100, path);
    let mut plain_idx = HashMap::new();
    plain_idx.insert(path.to_string(), plain);
    let plain_out = list_human(&rows, &plain_idx, true);
    assert!(
      plain_out.contains("\tall\n"),
      "default-backend row must read `all`: {plain_out:?}"
    );

    // Running on a device-less (non-default) backend with no override → `?`.
    let mut other = running("L1", "ready", 41100, path);
    other.backend = Some("lemonade".to_string());
    let mut other_idx = HashMap::new();
    other_idx.insert(path.to_string(), other);
    let other_out = list_human(&rows, &other_idx, true);
    assert!(
      other_out.contains("\t?\n"),
      "device-less backend renders the placeholder: {other_out:?}"
    );
  }

  #[test]
  fn list_human_backend_column_only_on_multi_backend() {
    let _g = ColorGuard::set(false);
    // All-`llamacpp` (or no prediction) → no BACKEND column, old shape intact.
    let mut llama = row("qwen", "qwen2", "Q4_K", 8192);
    llama.backend = Some("llamacpp".to_string());
    let single = list_human(std::slice::from_ref(&llama), &HashMap::new(), false);
    assert!(
      !single.contains("BACKEND"),
      "single-backend host hides the column: {single:?}"
    );
    // A non-`llamacpp` prediction flips it on.
    let mut ds4 = row("deepseek", "deepseek4", "Q2_K", 4096);
    ds4.backend = Some("ds4".to_string());
    let multi = list_human(&[llama, ds4], &HashMap::new(), false);
    assert!(
      multi.contains("BACKEND"),
      "multi-backend host shows the column"
    );
    assert!(multi.contains("ds4"), "the ds4 value renders: {multi:?}");
  }

  #[test]
  fn list_human_tty_branch_pads_columns_and_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let rows = vec![
      row("qwen", "qwen2", "Q4_K", 8192),
      row("phi", "phi3", "Q5_K", 4096),
    ];
    let s = list_human(&rows, &HashMap::new(), false);
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
      let s = list_human(&[], &HashMap::new(), false);
      assert!(console::strip_ansi_codes(&s).contains("no models"));
    }
  }

  #[test]
  fn list_json_wraps_rows_in_models_object_with_documented_keys() {
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let v = list_json(&rows, &HashMap::new());
    let arr = v
      .get("models")
      .and_then(|m| m.as_array())
      .expect("models array");
    assert_eq!(arr.len(), 1);
    let r = &arr[0];
    // Top-level keys of the unified `list_models` wire shape.
    for key in [
      "name",
      "path",
      "model_id",
      "parent",
      "source",
      "backend",
      "supported_backends",
      "split_siblings",
      "metadata",
      "parse_error",
    ] {
      assert!(r.get(key).is_some(), "key `{key}` missing in JSON row");
    }
    // GGUF-derived fields are nested under `metadata` (not flat top-level).
    let md = r.get("metadata").expect("metadata block");
    for key in [
      "arch",
      "quant",
      "native_ctx",
      "mode_hint",
      "parameter_label",
    ] {
      assert!(
        md.get(key).is_some(),
        "key `metadata.{key}` missing in JSON row"
      );
    }
    // Capability blocks are present as keys (null when not capable).
    assert!(r.get("mtp").is_some(), "mtp key present (null when absent)");
    assert!(
      r.get("multimodal").is_some(),
      "multimodal key present (null when absent)"
    );
  }

  #[test]
  fn list_json_empty_catalog_returns_empty_models_array() {
    let v = list_json(&[], &HashMap::new());
    assert_eq!(v, serde_json::json!({"models": []}));
  }

  #[test]
  fn list_json_status_carries_the_device_selector() {
    // The human table's DEVICE column must not be readable only by humans:
    // an agent gets the raw selector here (`null` when the launch took the
    // backend default, which the table renders as `all`).
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let path = "/m/qwen.gguf";

    let mut pinned = running("L1", "ready", 41100, path);
    pinned.params = Some(serde_json::json!({"knobs": {"device": "ROCm0"}}));
    let mut idx = HashMap::new();
    idx.insert(path.to_string(), pinned);
    assert_eq!(
      list_json(&rows, &idx)["models"][0]["status"]["device"],
      serde_json::json!("ROCm0")
    );

    let mut all_gpu = HashMap::new();
    all_gpu.insert(path.to_string(), running("L1", "ready", 41100, path));
    assert_eq!(
      list_json(&rows, &all_gpu)["models"][0]["status"]["device"],
      Value::Null,
      "no override serialises as null, not the human `all`"
    );

    // No supervisor → no `status` object at all (unchanged).
    assert!(list_json(&rows, &HashMap::new())["models"][0]
      .get("status")
      .is_none());
  }

  #[test]
  fn backend_badge_is_the_one_rule_both_surfaces_render() {
    // `list`'s BACKEND column and `show`'s header row share this helper, so a
    // ds4-compatible file can never read `ds4|llamacpp` on one surface and
    // `llamacpp` on the other.
    let mut multi = row("ds", "deepseek4", "IQ2_XXS", 8192);
    multi.supported_backends = vec!["ds4".into(), "llamacpp".into()];
    multi.backend = Some("llamacpp".into());
    assert_eq!(backend_badge(&multi, "?"), "ds4|llamacpp");

    // Untagged by the daemon → the caller's placeholder, not a guess from
    // the row's `source`.
    let mut untagged = row("qwen", "qwen2", "Q4_K", 8192);
    untagged.supported_backends = Vec::new();
    untagged.backend = None;
    assert_eq!(backend_badge(&untagged, "?"), "?");
    assert_eq!(backend_badge(&untagged, "—"), "—");

    // Only a primary tag → that tag.
    untagged.backend = Some("lemonade".into());
    assert_eq!(backend_badge(&untagged, "?"), "lemonade");
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
    let v = list_json(&[r], &HashMap::new());
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
        proxy: Value::Null,
        backends: Value::Null,
        servers: Value::Null,
      };
      let s = status_human(&snap);
      assert!(console::strip_ansi_codes(&s).contains("no managed"));
    }
  }

  #[test]
  fn status_human_gpu_footer_from_host_snapshot() {
    // The footer reads the live `host` snapshot (same source as the TUI
    // + doctor), not the separate `gpu` GpuInfo field. A genuinely
    // CPU-only host shows "CPU only".
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let snap = StatusSnapshot {
        models: vec![],
        external: vec![],
        gpu: Value::Null,
        host: serde_json::json!({"gpu_backend": "cpu_only"}),
        daemon: None,
        proxy: Value::Null,
        backends: Value::Null,
        servers: Value::Null,
      };
      let s = status_human(&snap);
      let plain = console::strip_ansi_codes(&s);
      assert!(plain.contains("GPU: CPU only"), "got: {plain}");
    }
  }

  #[test]
  fn host_gpu_label_matches_shared_summary_per_backend() {
    // Sourced from the host snapshot's `gpu_backend` + pool total +
    // classification, formatted by the shared `gpu_summary_line` so
    // `status` reads identically to `doctor`.
    let amd = serde_json::json!({
      "gpu_backend": "amd",
      "gpu_mem_total_bytes": 133_680_857_088_u64,
      "uma_class_source": "carve_signature",
    });
    assert_eq!(
      host_gpu_label(&amd).as_deref(),
      Some("AMD · 124.5 GiB (unified, inferred)")
    );
    let nv = serde_json::json!({
      "gpu_backend": "nvidia",
      "gpu_mem_total_bytes": 24u64 * 1024 * 1024 * 1024,
      "uma_class_source": "discrete",
    });
    assert_eq!(
      host_gpu_label(&nv).as_deref(),
      Some("NVIDIA · 24.0 GiB (discrete)")
    );
    // Pre-first-sample window reads "detecting", not "CPU only".
    let unsampled = serde_json::json!({"gpu_backend": "unsampled"});
    assert_eq!(host_gpu_label(&unsampled).as_deref(), Some("detecting"));
    let cpu = serde_json::json!({"gpu_backend": "cpu_only"});
    assert_eq!(host_gpu_label(&cpu).as_deref(), Some("CPU only"));
  }

  #[test]
  fn status_renders_and_serializes_backends_block() {
    let _g = ColorGuard::set(false);
    // Second row is a synthetic managed-multiplexer backend (no concrete
    // one ships on this branch) — proves the renderer handles N backends,
    // the not-installed state, and the managed lifecycle generically.
    let backends = serde_json::json!([
      { "id": "llamacpp", "lifecycle": "process_per_model", "installed": true, "accelerators": ["cpu", "vulkan"] },
      { "id": "example", "lifecycle": "managed_multiplexer", "installed": false, "accelerators": ["cpu", "npu"] },
    ]);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: backends.clone(),
      servers: Value::Null,
    };
    // Human render: each backend's id, install state, and accelerators.
    let human = status_human(&snap);
    assert!(human.contains("llamacpp"), "human backends: {human}");
    assert!(human.contains("installed"));
    assert!(human.contains("not installed"));
    assert!(human.contains("cpu, npu"));
    // JSON render mirrors the daemon shape verbatim.
    let json = status_json(&snap);
    assert_eq!(json["backends"], backends);
  }

  #[test]
  fn status_json_omits_backends_when_absent() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let json = status_json(&snap);
    assert!(
      json.get("backends").is_none(),
      "backends key must be omitted against an older daemon: {json}"
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
        ipc_url: None,
      }),
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
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
        ipc_url: None,
      }),
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
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
  fn status_human_surfaces_web_ui_url_when_proxy_listening() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let daemon = || DaemonHealth {
      pid: 1,
      uptime_seconds: 1,
      active_connections: 0,
      build: None,
      server_path: None,
      ipc_url: None,
    };
    let listening = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(daemon()),
      proxy: serde_json::json!({
        "enabled": true,
        "listen": "127.0.0.1:11435",
        "status": "listening",
        "bind_error": Value::Null,
        "ui_url": "http://127.0.0.1:11435/ui/",
      }),
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&listening);
    assert!(s.contains("web ui"), "web ui row present: {s:?}");
    assert!(
      s.contains("http://127.0.0.1:11435/ui/"),
      "the /ui/ url is shown: {s:?}"
    );
    // A non-listening proxy carries `ui_url: null` → no web ui line.
    let disabled = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(daemon()),
      proxy: serde_json::json!({"enabled": false, "status": "disabled", "ui_url": Value::Null}),
      backends: Value::Null,
      servers: Value::Null,
    };
    assert!(
      !status_human(&disabled).contains("web ui"),
      "no web ui line when the proxy isn't listening"
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
        state_cause: None,
        pid: Some(123),
        ready_at: Some(1_700_000_000),
        params: None,
        latest_rss_bytes: Some(4_500_000_000),
        latest_cpu_pct: Some(312.0),
        resolved_ctx: None,
        ctx_clamped: false,
        preset_count: 0,
        preset_default: None,
        backend: None,
      }],
      external: vec![ExternalRow {
        pid: 999,
        cmdline: "llama-server".into(),
        model_path: Some("/m/b.gguf".into()),
        port: Some(41101),
        launched_by_llamastash: true,
      }],
      gpu: Value::String("CpuOnly".into()),
      host: serde_json::json!({"gpu_backend": "amd", "cpu_pct": 12.5}),
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
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
        ipc_url: Some("http://127.0.0.1:48134".into()),
      }),
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
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
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
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
      state_cause: None,
      pid: Some(123),
      mode: "chat".into(),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
      ctx_clamped: false,
      preset_count: 0,
      preset_default: None,
      backend: None,
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
        port: None,
        launched_by_llamastash: false,
      }],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&snap);
    // Regression guard: managed + external rows are exact tabs, no
    // padding, no color, no truncation.
    assert!(
      s.contains("LAUNCH_ID\tSTATE\tMODE\tPORT\tPID\tCTX\tNAME\n"),
      "header drifted: {s:?}"
    );
    // resolved_ctx unset → "-" in the CTX column.
    assert!(s.contains("L1\tready\tchat\t41100\t123\t-\tqwen.gguf\n"));
    assert!(s.contains("external\texternal\t-\t-\t9999\t-\text.gguf\n"));
  }

  #[test]
  fn status_human_shows_resolved_ctx_and_flags_clamp() {
    let _g = ColorGuard::set(false);
    let mut clamped = running("L1", "ready", 41100, "/m/qwen.gguf");
    clamped.resolved_ctx = Some(16384);
    clamped.ctx_clamped = true;
    let mut healthy = running("L2", "ready", 41101, "/m/gemma.gguf");
    healthy.resolved_ctx = Some(131072);
    let snap = StatusSnapshot {
      models: vec![clamped, healthy],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&snap);
    // Clamped row carries the `*` marker; healthy row is the bare number.
    assert!(s.contains("\t16384*\t"), "clamp marker missing: {s:?}");
    assert!(s.contains("\t131072\t"), "resolved ctx missing: {s:?}");
    // The marker is explained beneath the table, keyed by launch_id.
    assert!(
      s.contains("L1 note: * ctx clamped to the fit-ctx floor under memory pressure"),
      "clamp note missing: {s:?}"
    );
    // The un-clamped row gets no note.
    assert!(
      !s.contains("L2 note:"),
      "spurious note for healthy row: {s:?}"
    );
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
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
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

  fn proxy_value(status: &str, listen: Option<&str>, bind_error: Option<&str>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("enabled".into(), Value::Bool(status != "disabled"));
    obj.insert(
      "listen".into(),
      listen
        .map(|s| Value::String(s.into()))
        .unwrap_or(Value::Null),
    );
    obj.insert("status".into(), Value::String(status.into()));
    obj.insert(
      "bind_error".into(),
      bind_error
        .map(|s| Value::String(s.into()))
        .unwrap_or(Value::Null),
    );
    Value::Object(obj)
  }

  #[test]
  fn status_json_round_trips_proxy_listening_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("listening", Some("127.0.0.1:11434"), None),
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("listening"));
    assert_eq!(proxy["listen"], serde_json::json!("127.0.0.1:11434"));
    assert_eq!(proxy["enabled"], serde_json::json!(true));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[test]
  fn status_json_round_trips_proxy_disabled_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("disabled", None, None),
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("disabled"));
    assert_eq!(proxy["enabled"], serde_json::json!(false));
    assert_eq!(proxy["listen"], Value::Null);
  }

  #[test]
  fn status_json_round_trips_proxy_port_in_use_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("port_in_use", Some("127.0.0.1:11434"), None),
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("port_in_use"));
    assert_eq!(proxy["listen"], serde_json::json!("127.0.0.1:11434"));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[test]
  fn status_json_round_trips_proxy_unbound_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("unbound", Some("127.0.0.1:80"), Some("permission denied")),
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("unbound"));
    assert_eq!(proxy["bind_error"], serde_json::json!("permission denied"));
  }

  #[test]
  fn status_json_omits_proxy_block_when_absent() {
    // Pre-Unit-5 daemons emit no `proxy` field. The CLI surface must
    // mirror that by omitting the key entirely, not synthesising a
    // null placeholder.
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    assert!(
      v.get("proxy").is_none(),
      "proxy key must be absent when StatusSnapshot.proxy is null: {v}"
    );
  }

  #[test]
  fn status_human_skips_proxy_row_when_disabled() {
    // Plan §Test scenarios edge case 3: disabled config doesn't add a
    // row. The label cycle includes `proxy` as a kv label otherwise;
    // its absence is the signal that the proxy is off.
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("disabled", None, None),
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&snap);
    assert!(
      !s.contains("proxy"),
      "disabled proxy must not add a `proxy` row: {s:?}"
    );
  }

  #[test]
  fn status_human_renders_proxy_listening_row_under_daemon() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("listening", Some("127.0.0.1:11434"), None),
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&snap);
    assert!(
      s.contains("proxy: listening 127.0.0.1:11434"),
      "expected proxy row, got: {s:?}"
    );
  }

  #[test]
  fn status_human_renders_proxy_port_in_use_row_with_hint() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("port_in_use", Some("127.0.0.1:11434"), None),
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&snap);
    assert!(
      s.contains("port_in_use"),
      "expected port_in_use label: {s:?}"
    );
  }

  #[test]
  fn display_size_sums_every_shard_on_disk_independent_of_cached_weights() {
    // Regression: `list_human` used to read `weights_bytes` straight
    // from the wire, which on a daemon whose catalog predated the
    // split-shard aggregation fix showed only shard 1's bytes.
    // `display_size` must now stat every shard so the SIZE column
    // is self-correcting independent of the daemon's cached value.
    let dir = tempfile::tempdir().unwrap();
    let primary = dir.path().join("m-00001-of-00002.gguf");
    std::fs::write(&primary, vec![0u8; 1024 * 1024]).unwrap(); // 1 MiB
    let sib = dir.path().join("m-00002-of-00002.gguf");
    std::fs::write(&sib, vec![0u8; 2 * 1024 * 1024]).unwrap(); // 2 MiB
    let row = CatalogRow {
      path: primary.display().to_string(),
      // Pretend the daemon cached a way-too-low value (the bug we
      // are working around).
      weights_bytes: Some(1024 * 1024),
      split_siblings: vec![sib.display().to_string()],
      ..row("split", "qwen3", "Q5_K", 32768)
    };
    let rendered = display_size(&row);
    // 1 MiB + 2 MiB = 3 MiB across both shards. format_bytes renders
    // megabytes without a trailing iB suffix, so just check the
    // leading magnitude + M unit.
    assert!(
      rendered.starts_with('3') && rendered.contains('M'),
      "expected ~3 MiB total across shards, got: {rendered}"
    );
  }

  #[test]
  fn display_size_falls_back_to_cached_weights_when_files_missing() {
    let row = CatalogRow {
      path: "/does/not/exist.gguf".into(),
      weights_bytes: Some(42 * 1024 * 1024),
      ..row("ghost", "llama", "Q4_K", 8192)
    };
    let rendered = display_size(&row);
    assert!(
      rendered.starts_with("42") && rendered.contains('M'),
      "expected fallback to cached 42 MiB, got: {rendered}"
    );
  }

  #[test]
  fn status_human_appends_cause_line_for_error_state_launches() {
    let _g = ColorGuard::set(false);
    let mut row = running("L1", "error", 41100, "/m/qwen.gguf");
    row.state_cause = Some("health probe timeout (last status 503); last stderr lines: …".into());
    let snap = StatusSnapshot {
      models: vec![row],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let rendered = status_human(&snap);
    assert!(
      rendered.contains("L1 cause:") && rendered.contains("health probe timeout"),
      "expected `L1 cause: …` line beneath the launches table, got:\n{rendered}"
    );
  }

  #[test]
  fn status_json_includes_state_cause_when_set() {
    let mut row = running("L1", "error", 41100, "/m/qwen.gguf");
    row.state_cause = Some("health probe timeout (last status 503)".into());
    let snap = StatusSnapshot {
      models: vec![row],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    assert_eq!(
      v["models"][0]["state_cause"],
      serde_json::json!("health probe timeout (last status 503)")
    );
  }

  #[test]
  fn status_json_omits_state_cause_when_none() {
    let snap = StatusSnapshot {
      models: vec![running("L1", "ready", 41100, "/m/qwen.gguf")],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
      backends: Value::Null,
      servers: Value::Null,
    };
    let v = status_json(&snap);
    assert!(
      v["models"][0].get("state_cause").is_none(),
      "ready-state rows must not carry a state_cause field"
    );
  }

  #[test]
  fn status_human_renders_proxy_unbound_row_with_cause() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("unbound", Some("127.0.0.1:80"), Some("permission denied")),
      backends: Value::Null,
      servers: Value::Null,
    };
    let s = status_human(&snap);
    assert!(
      s.contains("unbound") && s.contains("permission denied"),
      "expected unbound row with cause: {s:?}"
    );
  }

  #[test]
  fn favorites_json_keeps_dotted_lemonade_name_but_stems_gguf() {
    let rows = vec![
      serde_json::json!({"id": {"path": "lemonade://qwen3.5-4b-FLM"}}),
      serde_json::json!({"id": {"path": "/models/Llama-3.2-1B-Instruct-Q4_K_M.gguf"}}),
    ];
    let out = favorites_json(&rows);
    let favs = out["favorites"].as_array().unwrap();
    // Regression: a dotted registry name must not stem to `qwen3`.
    assert_eq!(favs[0]["name"], "qwen3.5-4b-FLM");
    assert_eq!(favs[0]["path"], "lemonade://qwen3.5-4b-FLM");
    // Real GGUF files still drop the extension (byte-stable with the old path).
    assert_eq!(favs[1]["name"], "Llama-3.2-1B-Instruct-Q4_K_M");
  }
}
