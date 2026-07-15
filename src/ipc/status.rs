//! Assembly of the daemon's `status` IPC document.
//!
//! `status_response` is the read-only snapshot the `status` method
//! returns: managed + delegated + external model rows, the host/GPU
//! samplers, the backend matrix, and the proxy block. The wire shape is
//! an agent-facing contract — every top-level key and field name here is
//! pinned by `status_top_level_key_set_is_stable` and the surrounding
//! `status_*` tests, so changes must stay byte-stable.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::backend::Backend;
use crate::daemon::context::MethodContext;
use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::ipc::methods::flatten_state;

/// The most recent MTP draft-acceptance figure in `lines`, matching
/// llama-server's `slot print_timing: … draft acceptance = <rate> ( <acc>
/// accepted / <gen> generated )` line. Returns `(rate, accepted, generated)`
/// for the last such line, or `None`. Scans newest-first so an idle model
/// keeps its final rate rather than an early warm-up figure.
fn latest_draft_acceptance(lines: &[String]) -> Option<(f32, u64, u64)> {
  lines
    .iter()
    .rev()
    .find_map(|l| parse_draft_acceptance_line(l))
}

/// Parse one `… draft acceptance = <rate> ( <acc> accepted / <gen> generated )`
/// line. Tolerant of the surrounding log prefix and whitespace.
fn parse_draft_acceptance_line(line: &str) -> Option<(f32, u64, u64)> {
  const MARKER: &str = "draft acceptance = ";
  let rest = &line[line.find(MARKER)? + MARKER.len()..];
  let rate: f32 = rest.split_whitespace().next()?.parse().ok()?;
  let num_before = |kw: &str| -> Option<u64> {
    let head = &rest[..rest.find(kw)?];
    head.split_whitespace().last()?.parse().ok()
  };
  Some((rate, num_before("accepted")?, num_before("generated")?))
}

/// Snapshot every active managed model plus the daemon's GPU info.
/// `status` is read-only; never triggers any state-machine transitions.
pub(crate) async fn status_response(ctx: &MethodContext) -> Value {
  let snap = ctx.supervisors.snapshot().await;
  // Post-launch actuals live on the persisted running snapshot
  // (stamped by the recorder on Ready); the live status row is built
  // from the supervisor, so cross-reference by (id, port) to surface
  // the resolved context.
  let running = ctx.state.snapshot().await.running;
  // Config-preset hint inputs, snapshotted once for the whole model loop.
  let preset_rows = super::methods::catalog_rows(ctx).await;
  let preset_store = ctx.presets.snapshot().await;
  let mut models: Vec<Value> = Vec::with_capacity(snap.len());
  for (launch_id, model) in snap {
    let state = model.state().await;
    let pid = model.pid().await;
    let ready_at = model.ready_at().await;
    // Wrap `ManagedState` in a small `{state, cause?}` object: a
    // lowercase string label plus an optional sibling `cause`, so an
    // `Error{cause}` surfaces its cause as a plain field rather than
    // hidden inside a serde tagged-enum content blob.
    let state_obj = match state.cause() {
      Some(cause) => json!({"state": state.label(), "cause": cause}),
      None => json!({"state": state.label()}),
    };
    // `params` so an agent can reproduce the launch without a
    // separate `last_params_list` call.
    let params = model.params();
    // MTP draft-acceptance telemetry: when MTP resolved on for this launch,
    // scan the recent server log for llama-server's latest `draft acceptance =
    // <rate> ( <acc> accepted / <gen> generated )` line. Read-only over the
    // existing log ring buffer (no new sampler); `None` until the model has
    // served enough tokens to print one.
    let mtp_active = params.mtp_directive.is_some();
    let mtp_acceptance = if mtp_active {
      latest_draft_acceptance(&model.tail(200).await)
    } else {
      None
    };
    let params_json = json!({
      "model_path": params.model_path,
      "mode": model.mode().label(),
      "ctx": params.ctx,
      "port": params.port,
      "reasoning": params.reasoning,
      // The server (build/binary) the launch picked, when one was chosen — so
      // the TUI running view can show which build served the model.
      "server": params.server,
      "knobs": &params.knobs,
      // Native (per-backend) knobs the launch dispatched with, so a client
      // can reproduce a ds4 launch and the TUI can save them into a preset.
      "backend_knobs": &params.backend_knobs,
      "extras": params
        .extras
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>(),
      // MTP (speculative decoding) state for this launch: `enable` is the
      // intent (auto/on/off), `active` is whether MTP actually resolved on
      // (`--spec-type draft-mtp` emitted), and `acceptance` is the latest
      // draft-acceptance rate parsed from the server log (null until printed).
      // Additive.
      "mtp": {
        "enable": params.mtp.label(),
        "active": mtp_active,
        "acceptance": mtp_acceptance.map(|(r, _, _)| r),
        "draft_accepted": mtp_acceptance.map(|(_, a, _)| a),
        "draft_generated": mtp_acceptance.map(|(_, _, g)| g),
      },
    });
    let latest = model.latest_resource().await;
    let latest_rss_bytes = latest.as_ref().map(|r| r.rss_bytes);
    let latest_cpu_pct = latest.as_ref().map(|r| r.cpu_percent);
    let running_snap = running.iter().find(|r| r.port == model.port());
    let actuals = running_snap.map(|r| r.actuals);
    // The backend this launch *resolved* to, stamped on the running snapshot
    // at spawn — the honest signal (respects an explicit `--backend llamacpp`
    // on a compatible file), not the `list_models` ds4 badge prediction.
    // A managed-multiplexer umbrella row keys deterministically on its own
    // backend id: the umbrella shares its port with every delegated model's
    // snapshot, so matching by port would pick an arbitrary snapshot (or the
    // default-backend fallback when none is resident) and the reported backend
    // would flip-flop between calls. Resolved via the registry so no backend is
    // named here.
    let resolved_backend = crate::backend::Backends::all()
      .iter()
      .find(|b| b.umbrella_launch_id().as_ref() == Some(&launch_id))
      .map(|b| b.id().to_string())
      .unwrap_or_else(|| {
        running_snap
          .map(|r| r.resolved_backend.clone())
          .unwrap_or_else(|| crate::backend::DEFAULT_BACKEND_ID.to_string())
      });
    let resolved_ctx = actuals.and_then(|a| a.resolved_ctx);
    let ctx_clamped = actuals.map(|a| a.ctx_clamped).unwrap_or(false);
    let (preset_count, preset_default) = super::methods::preset_hint(
      &params.model_path.display().to_string(),
      &preset_rows,
      &preset_store,
    );
    let row = json!({
      "launch_id": launch_id,
      "id": model.id(),
      "port": model.port(),
      "mode": model.mode().label(),
      "pid": pid,
      "ready_at": ready_at,
      "state": state_obj,
      "params": params_json,
      // Backend this launch actually resolved to (`llamacpp` / `ds4` /
      // `lemonade`) — the TUI keys its ds4 badge / knob panel on this, not on
      // the routing prediction.
      "backend": resolved_backend,
      "latest_rss_bytes": latest_rss_bytes,
      "latest_cpu_pct": latest_cpu_pct,
      // Resolved context window `--fit` chose; null until the
      // post-Ready `/props` fetch lands or when the build omits it.
      "resolved_ctx": resolved_ctx,
      // True when `--fit` had to clamp ctx to the floor under memory
      // pressure (a soft notice); strict mode refuses such launches.
      "ctx_clamped": ctx_clamped,
      // Config-preset hint: how many presets this model resolves and its
      // default name (config-only). The full set lives in `presets_list`.
      "preset_count": preset_count,
      "default": preset_default,
    });
    models.push(row);
  }
  // Delegated Lemonade models — the registry holds only the shared
  // umbrella (one row whose path is the `lemond` binary), but every
  // model made resident via `start_model` persisted a RunningSnapshot
  // at the umbrella's port. Project each as a first-class row: the
  // synthetic `lemonade://<name>` path matches the catalog entry, so
  // the TUI list pane and `llamastash list` show the *model* as
  // running, not just the umbrella. State comes from the preload
  // task's recorded outcome (`Loading` / `Ready` / `Error{cause}`);
  // a snapshot with no recorded outcome (re-adopted across a daemon
  // restart) falls back to mirroring the umbrella. Rows are emitted
  // only while the umbrella is registered; with it gone the
  // snapshots are unreachable leftovers (the boot sweep reaps them).
  if let Some(umbrella) = ctx
    .supervisors
    .get(&crate::backend::lemonade::umbrella_launch_id())
    .await
  {
    let ustate = umbrella.state().await;
    let umbrella_ready = matches!(ustate, crate::daemon::supervisor::ManagedState::Ready);
    let ustate_obj = flatten_state(&ustate);
    // The umbrella's own resource reading, mirrored onto every delegated model
    // row — they run inside this one shared process, so its RSS/CPU is the only
    // honest figure. The TUI marks these as shared (`*`) so they don't read as
    // per-model; nothing sums the per-row rss, so the mirror never double-counts.
    let ulatest = umbrella.latest_resource().await;
    let u_rss = ulatest.as_ref().map(|r| r.rss_bytes);
    let u_cpu = ulatest.as_ref().map(|r| r.cpu_percent);
    for running_snap in ctx.state.snapshot().await.running.iter() {
      let Some(backend_id) = running_snap.delegated_backend_id() else {
        continue;
      };
      // The `L#` stamped at launch (delegated rows have no supervisor to hold
      // it). A lemonade snapshot without one is an unreachable leftover — skip
      // it rather than emit a row the client can't stop.
      let Some(launch_id) = running_snap.launch_id.clone() else {
        continue;
      };
      // The cached per-model state is `Ready` from preload and is never
      // updated when the umbrella dies out-of-band (crash / external kill).
      // Trust it only while the umbrella is actually Ready; otherwise the model
      // can't be resident, so reflect the umbrella's real state instead of a
      // stale green row.
      let state_obj = match ctx.supervisors.delegated_state(&backend_id.name).await {
        Some(s) if umbrella_ready => flatten_state(&s),
        _ => ustate_obj.clone(),
      };
      let synthetic_id = crate::gguf::identity::ModelId {
        path: running_snap.params.model_path.clone(),
        header_blake3: [0u8; 32],
      };
      let params_json = json!({
        "model_path": running_snap.params.model_path,
        "mode": running_snap.params.mode.label(),
        "ctx": running_snap.params.ctx,
        "port": running_snap.params.port,
        "reasoning": running_snap.params.reasoning,
        "server": running_snap.params.server,
        "knobs": &running_snap.params.knobs,
        "backend_knobs": &running_snap.params.backend_knobs,
        "extras": running_snap.params.extras
          .iter()
          .map(|s| s.to_string_lossy().into_owned())
          .collect::<Vec<_>>(),
      });
      let (preset_count, preset_default) = super::methods::preset_hint(
        &running_snap.params.model_path.display().to_string(),
        &preset_rows,
        &preset_store,
      );
      models.push(json!({
        "launch_id": launch_id,
        "id": synthetic_id,
        "port": running_snap.port,
        "mode": running_snap.params.mode.label(),
        // A delegated model has no process of its own — it runs inside the
        // shared umbrella. Report no pid (`-` in the CLI table, `null` in JSON);
        // only the umbrella's own row carries the pid.
        "pid": null,
        "ready_at": running_snap.started_at,
        "state": state_obj,
        "params": params_json,
        "backend": running_snap.resolved_backend.clone(),
        // The shared umbrella's RSS/CPU (see `u_rss`/`u_cpu` above), surfaced
        // per delegated row and flagged shared by the TUI.
        "latest_rss_bytes": u_rss,
        "latest_cpu_pct": u_cpu,
        "preset_count": preset_count,
        "default": preset_default,
      }));
    }
  }
  // External — read-only rows for `llama-server` processes the
  // daemon doesn't own. Populated by the startup orphan sweep.
  // Stable shape: `{pid, cmdline, model_path, port, launched_by_llamastash}`.
  let external_snapshot = ctx.external.read().await.clone();
  let external: Vec<Value> = external_snapshot
    .iter()
    .map(|e| {
      json!({
        "pid": e.pid,
        "cmdline": e.cmdline,
        "model_path": e.model_path,
        // Tier-A orphan-tracking fields. `port` lets agents diff the
        // wire snapshot against `ss`/`lsof` without parsing argv;
        // `launched_by_llamastash` exposes whether the orphan carries
        // the supervisor's spawn marker so operators can spot
        // sibling-instance orphans at a glance.
        "port": e.port,
        "launched_by_llamastash": e.launched_by_llamastash,
      })
    })
    .collect();
  // Host-level metrics (CPU%, RAM, GPU util/temp/VRAM aggregates).
  // Sampled by the daemon's `host_metrics` task at 1 Hz. When no
  // sampler is attached (catalog-only contexts), emit a default
  // snapshot rather than `null` so clients see a stable object
  // shape — `gpu_backend == "unsampled"` already distinguishes the
  // never-sampled case from a real reading.
  //
  // Serialize the snapshot directly under the read lock instead of
  // cloning it out first; `HostMetricsSnapshot` already implements
  // `Serialize` for `&Self`, so this saves one full struct clone
  // (including the `gpu_backend: String`) per status call.
  let host = match &ctx.host_metrics {
    Some(slot) => {
      let host_snap = slot.read().await;
      serde_json::to_value(&*host_snap).unwrap_or(Value::Null)
    }
    None => {
      let default_snap = HostMetricsSnapshot {
        gpu_backend: HostMetricsSnapshot::UNINITIALIZED_BACKEND.into(),
        ..HostMetricsSnapshot::default()
      };
      serde_json::to_value(default_snap).unwrap_or(Value::Null)
    }
  };
  // Prefer the live GpuInfo cell when the sampler is attached so
  // `status.gpu` follows hotplug / late-driver-load events. Falls
  // back to the boot-time `ctx.gpu` snapshot when the sampler is
  // off (catalog-only tests).
  let gpu = match &ctx.gpu_live {
    Some(slot) => serde_json::to_value(&*slot.read().await).unwrap_or(Value::Null),
    None => serde_json::to_value(ctx.gpu.as_ref()).unwrap_or(Value::Null),
  };
  // Proxy block — read-only projection of the listener's shared
  // status cell. Catalog-only tests that never bring the proxy up
  // leave `proxy_status` as `None`; the field is omitted in that
  // case so those fixtures stay byte-identical. The wire shape is:
  //
  // ```
  // "proxy": {
  //   "enabled": bool,
  //   "listen": "127.0.0.1:11434" | null,
  //   "status": "disabled" | "listening" | "port_in_use" | "unbound",
  //   "bind_error": "permission denied" | null,
  // }
  // ```
  let proxy = ctx.proxy_status.as_ref().map(project_proxy_status);
  // Neutral server catalog: every backend's build/binary variants, each with
  // its probed devices + derived id. The single launch-device surface the TUI
  // picker and CLI read — each server carries the `--device` selectors its own
  // binary accepts, sourced from that binary's `--list-devices` (not vendor
  // tools), so what the picker offers is precisely what `llama-server` accepts.
  // Empty array when no binary is configured.
  let servers = match ctx.launch.as_ref() {
    Some(env) => serde_json::to_value(&*env.servers.read().await).unwrap_or(Value::Null),
    None => Value::Array(Vec::new()),
  };
  let backends = backends_status(ctx).await;
  let mut body = json!({
    "models": models,
    "external": external,
    "gpu": gpu,
    "host": host,
    "servers": servers,
    "backends": backends,
    "daemon": {
      "pid": std::process::id(),
      "uptime_seconds": ctx.started_at.elapsed().as_secs(),
      "active_connections": ctx.active_connections.load(Ordering::Relaxed),
      "build": env!("CARGO_PKG_VERSION"),
      "server_path": ctx
        .launch
        .as_ref()
        .map(|env| env.binary.display().to_string()),
      "ipc_url": ctx.ipc_url,
    },
  });
  if let Some(proxy) = proxy {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("proxy".into(), proxy);
    }
  }
  body
}

/// Build the `status.backends` array: one row per backend with
/// whether its binary is installed on this host and which accelerators it
/// can run on. llama.cpp's accelerator set unions its CPU floor with the
/// GPU backends the live device catalog reveals; Lemonade reports its
/// static cpu+npu (a live `lemond` system-info probe is deferred).
async fn backends_status(ctx: &MethodContext) -> Value {
  use crate::backend::{Backend, Backends};

  // Live GPU classes the host exposes (from each configured binary's
  // `--list-devices`). The default `status_accelerators` unions these into a
  // backend's static floor; a backend that probes its own installed
  // accelerators live (a managed multiplexer) overrides and ignores them.
  let device_accels: Vec<crate::backend::Accelerator> = match ctx.launch.as_ref() {
    Some(env) => env
      .servers
      .read()
      .await
      .iter()
      .flat_map(|s| s.devices.iter())
      .filter_map(|d| accelerator_from_selector(&d.selector))
      .collect(),
    None => Vec::new(),
  };

  // One row per registered backend, assembled generically from the backend's
  // status hooks (installed / enabled / accelerators / binary / extra). This
  // names no backend, so a new backend surfaces in `status` from its
  // registration alone — no edit here.
  let mut rows = Vec::new();
  for b in Backends::all() {
    let mut row = backend_row(
      b.id(),
      b.lifecycle().label(),
      b.installed(ctx),
      b.status_accelerators(ctx, &device_accels).await,
    );
    if let Some(obj) = row.as_object_mut() {
      if let Some(enabled) = b.status_enabled(ctx) {
        obj.insert("enabled".into(), json!(enabled));
      }
      for (k, v) in b.status_extra(ctx).await {
        obj.insert(k, v);
      }
    }
    set_backend_binary(&mut row, b.binary_path(ctx));
    rows.push(row);
  }
  Value::Array(rows)
}

/// Attach a backend row's resolved `binary` path; absent when no binary
/// resolves so clients can tell "backend known" from "executable found".
fn set_backend_binary(row: &mut Value, binary: Option<String>) {
  if let (Some(obj), Some(bin)) = (row.as_object_mut(), binary) {
    obj.insert("binary".into(), json!(bin));
  }
}

fn backend_row(id: &str, lifecycle: &str, installed: bool, accelerators: Vec<String>) -> Value {
  json!({
    "id": id,
    "lifecycle": lifecycle,
    "installed": installed,
    "accelerators": accelerators,
  })
}

/// Map a llama.cpp `--device` selector prefix to an accelerator class.
fn accelerator_from_selector(selector: &str) -> Option<crate::backend::Accelerator> {
  use crate::backend::Accelerator;
  let s = selector.to_ascii_lowercase();
  if s.starts_with("cuda") {
    Some(Accelerator::Cuda)
  } else if s.starts_with("rocm") {
    Some(Accelerator::Rocm)
  } else if s.starts_with("vulkan") {
    Some(Accelerator::Vulkan)
  } else if s.starts_with("metal") {
    Some(Accelerator::Metal)
  } else {
    None
  }
}

/// Project the proxy listener's status cell into the wire shape
/// surfaced under `status.proxy`. The cell is the single source of
/// truth — the listener task writes every transition
/// (Disabled / Listening / PortInUse / Unbound) and this is the
/// read side.
///
/// `listen` is the *attempted* address: `Disabled` emits `null`
/// (no bind was attempted), every other variant carries the address
/// the daemon tried to bind. `bind_error` is non-null only when the
/// variant is `Unbound` — `PortInUse` is its own discriminator and
/// callers shouldn't need a parallel string to recognise it.
fn project_proxy_status(cell: &crate::proxy::StatusCell) -> Value {
  use crate::proxy::ProxyStatus;
  let snapshot = cell.read().unwrap_or_else(|e| e.into_inner()).clone();
  match snapshot {
    ProxyStatus::Disabled => json!({
      "enabled": false,
      "listen": Value::Null,
      "host": Value::Null,
      "status": "disabled",
      "auth": "none",
      "bind_error": Value::Null,
      "ui_url": Value::Null,
    }),
    ProxyStatus::Listening {
      addr,
      auth_enforced,
    } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "listening",
      "auth": if auth_enforced { "enforced" } else { "none" },
      "bind_error": Value::Null,
      // The stock web UI rides this same listener at a port-stable origin.
      // Populated only while listening (the only state that actually serves it).
      "ui_url": format!("http://{addr}/ui/"),
    }),
    ProxyStatus::PortInUse { addr } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "port_in_use",
      "auth": "none",
      "bind_error": Value::Null,
      "ui_url": Value::Null,
    }),
    ProxyStatus::Unbound { addr, bind_error } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "unbound",
      "auth": "none",
      "bind_error": bind_error,
      "ui_url": Value::Null,
    }),
    // Refused to expose a non-loopback proxy without auth. The daemon
    // is healthy; the proxy just didn't bind. `auth` reports
    // `"required"` so a client can distinguish this from a plain bind
    // failure and surface the fix.
    ProxyStatus::RefusedInsecure { addr } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "refused_insecure",
      "auth": "required",
      "bind_error":
        "refused to bind a non-loopback proxy without authentication; \
         set proxy.api_key or pass --insecure-no-auth",
      "ui_url": Value::Null,
    }),
  }
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::{latest_draft_acceptance, parse_draft_acceptance_line};
  use serde_json::{json, Value};

  #[test]
  fn parses_llama_server_draft_acceptance_line() {
    // The exact shape a real llama-server slot emits (captured 2026-07-14).
    let line = "0.06.893 I slot print_timing: id  3 | task 0 | draft acceptance = 0.65217 (  105 accepted /   161 generated ), mean len =  2.94";
    let (rate, acc, gen) = parse_draft_acceptance_line(line).expect("parses");
    assert!((rate - 0.65217).abs() < 1e-4);
    assert_eq!(acc, 105);
    assert_eq!(gen, 161);
    // A non-matching line yields None.
    assert!(parse_draft_acceptance_line("srv load_model: loading model").is_none());
  }

  #[test]
  fn latest_draft_acceptance_takes_the_newest_line() {
    let lines = vec![
      "draft acceptance = 0.10 ( 1 accepted / 10 generated )".to_string(),
      "some other log line".to_string(),
      "draft acceptance = 0.90 ( 9 accepted / 10 generated )".to_string(),
    ];
    let (rate, acc, gen) = latest_draft_acceptance(&lines).unwrap();
    assert!((rate - 0.90).abs() < 1e-4);
    assert_eq!((acc, gen), (9, 10));
    // No acceptance lines → None.
    assert!(latest_draft_acceptance(&["nothing here".to_string()]).is_none());
  }

  use crate::daemon::context::MethodContext;
  use crate::daemon::shutdown::ShutdownToken;
  use crate::ipc::methods::dispatch_request;
  use crate::ipc::protocol::Request;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;

  fn ctx() -> MethodContext {
    MethodContext::new(ShutdownToken::new())
  }

  /// A delegated-lemonade snapshot the way `start_delegated_lemonade`
  /// persists one: Backend identity + the synthetic `lemonade://` path +
  /// the registry-assigned `L#` handle.
  fn lemonade_running_snapshot(
    name: &str,
    port: u16,
    launch_id: &str,
  ) -> crate::daemon::state_store::RunningSnapshot {
    let path = PathBuf::from(format!("lemonade://{name}"));
    let (id, resolved_backend) = crate::backend::synthetic_identity_for_path(&path)
      .expect("a lemonade:// path mints a synthetic backend identity");
    crate::daemon::state_store::RunningSnapshot {
      id,
      pid: 0,
      port,
      started_at: 0,
      launch_id: Some(crate::daemon::registry::LaunchId(launch_id.to_string())),
      params: LaunchParams::new(path, LaunchMode::Chat),
      actuals: Default::default(),
      resolved_backend,
    }
  }

  #[tokio::test]
  async fn status_includes_daemon_health_block() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let daemon = body
      .get("daemon")
      .expect("status must include daemon health block");
    assert!(daemon["pid"].is_number());
    assert!(daemon["uptime_seconds"].is_number());
    assert_eq!(daemon["active_connections"], json!(0));
  }

  /// Agent-facing contract guard: pins the exact top-level key set of a
  /// catalog-only `status` response so the status-assembly extraction
  /// stays byte-stable (`proxy` is omitted when the cell is absent).
  #[tokio::test]
  async fn status_top_level_key_set_is_stable() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let mut keys: Vec<&str> = body
      .as_object()
      .expect("status is a JSON object")
      .keys()
      .map(String::as_str)
      .collect();
    keys.sort_unstable();
    assert_eq!(
      keys,
      vec!["backends", "daemon", "external", "gpu", "host", "models", "servers",],
      "catalog-only status top-level keys drifted"
    );
  }

  #[tokio::test]
  async fn status_includes_backends_block() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let backends = body
      .get("backends")
      .and_then(|v| v.as_array())
      .expect("status must include a backends array");
    let ids: Vec<&str> = backends
      .iter()
      .filter_map(|b| b.get("id").and_then(|v| v.as_str()))
      .collect();
    assert!(
      ids.contains(&"llamacpp"),
      "backends must list llamacpp: {ids:?}"
    );
    assert!(
      ids.contains(&"lemonade"),
      "backends must list lemonade: {ids:?}"
    );
    // Each row carries the installed/lifecycle/accelerator fields;
    // llama.cpp always offers CPU.
    let llama = backends
      .iter()
      .find(|b| b["id"] == "llamacpp")
      .expect("llamacpp row");
    assert!(llama["installed"].is_boolean());
    assert_eq!(llama["lifecycle"], json!("process_per_model"));
    let accel: Vec<&str> = llama["accelerators"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|v| v.as_str())
      .collect();
    assert!(accel.contains(&"cpu"), "llama.cpp floor is cpu: {accel:?}");
    // The Lemonade row is a managed-multiplexer offering cpu+npu.
    let lemon = backends
      .iter()
      .find(|b| b["id"] == "lemonade")
      .expect("lemonade row");
    assert!(lemon["installed"].is_boolean());
    assert_eq!(lemon["lifecycle"], json!("managed_multiplexer"));
    let lacc: Vec<&str> = lemon["accelerators"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|v| v.as_str())
      .collect();
    assert!(lacc.contains(&"npu"), "lemonade offers npu: {lacc:?}");
  }

  #[tokio::test]
  async fn status_omits_proxy_block_when_cell_is_absent() {
    // Catalog-only contexts (`MethodContext::new`) leave
    // `proxy_status` as `None`. The wire shape must omit the
    // `proxy` field entirely so such fixtures stay byte-identical —
    // callers that don't surface a proxy don't get a confusing
    // `"proxy": null` blob either.
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    assert!(
      body.get("proxy").is_none(),
      "proxy block must be absent when no cell is attached: {body}"
    );
  }

  #[tokio::test]
  async fn status_emits_proxy_listening_block() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "127.0.0.1:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::Listening {
      addr,
      auth_enforced: false,
    };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["listen"], json!("127.0.0.1:11434"));
    assert_eq!(proxy["host"], json!("127.0.0.1"));
    assert_eq!(proxy["status"], json!("listening"));
    assert_eq!(proxy["auth"], json!("none"));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[tokio::test]
  async fn status_listening_reports_auth_enforced_and_lan_host() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "0.0.0.0:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::Listening {
      addr,
      auth_enforced: true,
    };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let proxy = resp.result.expect("status result");
    let proxy = proxy.get("proxy").expect("proxy block present");
    assert_eq!(proxy["host"], json!("0.0.0.0"));
    assert_eq!(proxy["auth"], json!("enforced"));
  }

  #[tokio::test]
  async fn status_omits_delegated_lemonade_rows_without_umbrella() {
    // A snapshot with no registered umbrella is an unreachable leftover
    // (umbrella crashed / was stopped): emitting a row for it would
    // offer a stop affordance against nothing. The happy path (umbrella
    // up → rows emitted) is covered in `lemonade_umbrella_test.rs`.
    let c = ctx();
    c.state
      .mutate(|s| {
        s.running
          .push(lemonade_running_snapshot("Qwen-X", 13305, "L1"))
      })
      .await;
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let models = body["models"].as_array().expect("models array");
    assert!(
      !models
        .iter()
        .any(|m| m["backend"] == crate::backend::lemonade::LEMONADE_BACKEND_ID),
      "no delegated rows without a registered umbrella: {models:?}"
    );
  }

  #[tokio::test]
  async fn status_emits_proxy_refused_insecure_block() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "0.0.0.0:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::RefusedInsecure { addr };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let proxy = resp.result.expect("status result");
    let proxy = proxy.get("proxy").expect("proxy block present");
    assert_eq!(proxy["status"], json!("refused_insecure"));
    assert_eq!(proxy["auth"], json!("required"));
    assert_eq!(proxy["host"], json!("0.0.0.0"));
    assert!(
      proxy["bind_error"]
        .as_str()
        .unwrap()
        .contains("--insecure-no-auth"),
      "refused_insecure must explain the fix: {proxy}"
    );
  }

  #[tokio::test]
  async fn status_emits_proxy_disabled_block() {
    use crate::proxy;
    let cell = proxy::server::new_status_cell();
    // `new_status_cell` already seeds `Disabled`.
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(false));
    assert_eq!(proxy["listen"], Value::Null);
    assert_eq!(proxy["status"], json!("disabled"));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[tokio::test]
  async fn status_emits_proxy_port_in_use_block() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "127.0.0.1:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::PortInUse { addr };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["listen"], json!("127.0.0.1:11434"));
    assert_eq!(proxy["status"], json!("port_in_use"));
    // PortInUse is its own discriminator; no parallel bind_error
    // string — the wire shape pins this so parsers don't have to
    // double-check.
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[tokio::test]
  async fn status_emits_proxy_unbound_block_with_bind_error() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::Unbound {
      addr,
      bind_error: "permission denied".to_string(),
    };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["listen"], json!("127.0.0.1:80"));
    assert_eq!(proxy["status"], json!("unbound"));
    assert_eq!(proxy["bind_error"], json!("permission denied"));
  }

  #[test]
  fn accelerator_from_selector_maps_known_backend_prefixes() {
    use super::accelerator_from_selector;
    use crate::backend::Accelerator;
    // Each `--device` selector prefix maps to its accelerator class,
    // case-insensitively (the device catalog reports mixed case).
    assert_eq!(accelerator_from_selector("CUDA0"), Some(Accelerator::Cuda));
    assert_eq!(accelerator_from_selector("rocm1"), Some(Accelerator::Rocm));
    assert_eq!(
      accelerator_from_selector("Vulkan0"),
      Some(Accelerator::Vulkan)
    );
    assert_eq!(accelerator_from_selector("metal"), Some(Accelerator::Metal));
    // An unrecognised selector contributes no accelerator class.
    assert_eq!(accelerator_from_selector("sycl0"), None);
    assert_eq!(accelerator_from_selector(""), None);
  }
}
