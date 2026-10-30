//! Assembly of the daemon's `status` IPC document.
//!
//! `status_response` is the read-only snapshot the `status` method
//! returns: managed and external model rows, the host/GPU
//! samplers, the backend matrix, and the proxy block. The wire shape is
//! an agent-facing contract — every top-level key and field name here is
//! pinned by `status_top_level_key_set_is_stable` and the surrounding
//! `status_*` tests, so changes must stay byte-stable.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::backend::Backend;
use crate::daemon::context::MethodContext;
use crate::daemon::host_metrics::HostMetricsSnapshot;

/// Snapshot every active managed model plus the daemon's GPU info.
/// `status` is read-only; never triggers any state-machine transitions.
pub(crate) async fn status_response(ctx: &MethodContext) -> Value {
  let snap = ctx.supervisors.snapshot().await;
  // Post-launch actuals live on the persisted running snapshot
  // (stamped by the recorder on Ready); the live status row is built
  // from the supervisor, so cross-reference by **launch id** to surface
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
    // Keyed on the launch id, not the port. A port is reused the moment its
    // launch stops, so a snapshot that outlived its process was matched to
    // whatever launched next — and that row's `backend` and `resolved_ctx`
    // (both read from the snapshot below) then described the *previous* model.
    // The launch id is unique per launch, so it cannot alias.
    let running_snap = running
      .iter()
      .find(|r| r.launch_id.as_ref() == Some(&launch_id))
      // A row adopted before launch ids were stamped carries none; fall back to
      // the port so its actuals are not lost.
      .or_else(|| {
        running
          .iter()
          .find(|r| r.launch_id.is_none() && r.port == model.port())
      });
    // The backend this launch *resolved* to, stamped on the running snapshot
    // at spawn — the honest signal (respects an explicit backend override on a
    // compatible file), not the routing prediction.
    let owner = running_snap
      .and_then(|r| crate::backend::Backends::from_id(&r.resolved_backend))
      .unwrap_or_else(crate::backend::default_backend);
    let resolved_backend = owner.id().to_string();
    // MTP telemetry comes from the owning backend: whether it dispatched with
    // MTP, and — only then — the acceptance figure it prints in its own log
    // format. Read-only over the existing log ring buffer (no new sampler).
    let mtp_active = owner.mtp_active(params);
    let mtp_acceptance = if mtp_active {
      owner.draft_acceptance(&model.tail(200).await)
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
      "extras": params
        .extras
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>(),
      // MTP (speculative decoding) state for this launch: `enable` is the
      // intent (auto/on/off), `active` is whether the owning backend actually
      // dispatched with MTP, and `acceptance` is the latest draft-acceptance
      // rate it reported (null until printed, or on a backend that publishes
      // none). Additive.
      "mtp": {
        "enable": params.mtp_intent().label(),
        "active": mtp_active,
        "acceptance": mtp_acceptance.map(|a| a.rate),
        "draft_accepted": mtp_acceptance.map(|a| a.accepted),
        "draft_generated": mtp_acceptance.map(|a| a.generated),
      },
    });
    let latest = model.latest_resource().await;
    let latest_rss_bytes = latest.as_ref().map(|r| r.rss_bytes);
    let latest_cpu_pct = latest.as_ref().map(|r| r.cpu_percent);
    let actuals = running_snap.map(|r| r.actuals);
    let resolved_ctx = actuals.and_then(|a| a.resolved_ctx);
    let ctx_clamped = actuals.map(|a| a.ctx_clamped).unwrap_or(false);
    let (preset_count, preset_default) = super::methods::preset_hint(
      &params.model_path.display().to_string(),
      &preset_rows,
      &preset_store,
    );
    let mut row = json!({
      "launch_id": launch_id,
      "id": model.id(),
      "port": model.port(),
      "mode": model.mode().label(),
      "pid": pid,
      "ready_at": ready_at,
      "state": state_obj,
      "params": params_json,
      // Backend this launch actually resolved to — the
      // TUI keys its backend badge / knob panel on this, not on the routing
      // prediction.
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
      // Totals of the proxy requests this launch served: counts, average
      // latency, tokens and tok/s. Zeros until the first request.
      "request_stats": ctx.requests.launch_summary(launch_id.as_str()),
    });
    // Omitted when unset — the same convention `state.json` and the CLI's
    // `launch_status_json` use — so the unnamed-row shape stays byte-stable
    // instead of alternating between `null` and a value across surfaces.
    if let Some(name) = running_snap.and_then(|r| r.name.clone()) {
      row["name"] = json!(name);
    }
    // The preset this launch resolved (explicit pick / `@name` address /
    // config `default:`), omitted when none was in play — the same
    // convention `name` uses, and distinct from the config-only `default`
    // hint above.
    if let Some(preset) = running_snap.and_then(|r| r.preset.clone()) {
      row["preset"] = json!(preset);
    }
    // The backend's floor under every stop grace, omitted when there is none
    // so ordinary rows keep their shape.
    let min_grace = model.min_stop_grace().as_secs();
    if min_grace > 0 {
      row["stop_grace_secs"] = json!(min_grace);
    }
    models.push(row);
  }
  // External — read-only rows for `llama-server` processes the
  // daemon doesn't own. Populated by the boot sweep, then refreshed every
  // `orphans::RESWEEP_INTERVAL` by `orphans::spawn_resweep`.
  // Stable shape: `{pid, cmdline, model_path, port, launched_by_llamastash}`,
  // plus `name` on a row demoted from a named launch.
  let external_snapshot = ctx.external.read().await.clone();
  let external: Vec<Value> = external_snapshot
    .iter()
    .map(|e| {
      let mut obj = json!({
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
      });
      // Only-when-set, the same convention the managed rows use, so a
      // scan-found row stays byte-identical to the pre-name shape.
      if let Some(n) = e.name.as_deref() {
        obj["name"] = json!(n);
      }
      obj
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
  let proxy = ctx.proxy_status.as_ref().map(|cell| {
    let mut block = project_proxy_status(cell);
    // Where the request log is also written and how many finished rows
    // did not get there. `null` unless `proxy.request_log_file` is on.
    block["request_log_file"] = json!(ctx.requests.file_status());
    block
  });
  // Neutral server catalog: every backend's build/binary variants, each with
  // its probed devices + derived id. The single launch-device surface the TUI
  // picker and CLI read — each server carries the `--device` selectors its own
  // binary accepts, sourced from that binary's `--list-devices` (not vendor
  // tools), so what the picker offers is precisely what `llama-server` accepts.
  // Empty array when no binary is configured.
  let servers = match ctx.launch.as_ref() {
    Some(env) => serde_json::to_value(&*env.servers.current().await).unwrap_or(Value::Null),
    None => Value::Array(Vec::new()),
  };
  let backends = backends_status(ctx).await;
  // Boot preload, as configured: the `daemon.preload:` list and how many preset
  // entries pin `preload: true`. Reported so a caller can tell a daemon that was
  // asked to warm models from one that was not; whether a given entry started is
  // visible in `models`, since a preloaded launch is a normal running row.
  let preload_presets = crate::config::preset_entries(&ctx.presets.snapshot().await)
    .filter(|entry| entry.preload)
    .count();
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
        .and_then(|env| env.binary.as_ref())
        .map(|b| b.display().to_string()),
      "ipc_url": ctx.ipc_url,
      "memory": ctx.memory_policy.budget(ctx.host_metrics.as_ref()).await,
      "preload": {
        "entries": ctx.preload.iter().cloned().collect::<Vec<_>>(),
        "presets": preload_presets,
      },
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
/// GPU backends the live device catalog reveals.
async fn backends_status(ctx: &MethodContext) -> Value {
  use crate::backend::{Backend, Backends};

  // Live GPU classes the host exposes (from each configured binary's
  // `--list-devices`). The default `status_accelerators` unions these into a
  // backend's static floor.
  let catalog: Vec<crate::backend::Server> = match ctx.launch.as_ref() {
    Some(env) => env.servers.current().await.to_vec(),
    None => Vec::new(),
  };
  let device_accels: Vec<crate::backend::Accelerator> = catalog
    .iter()
    .flat_map(|s| s.devices.iter())
    .filter_map(|d| accelerator_from_selector(&d.selector))
    .collect();

  // One row per registered backend, assembled generically from the backend's
  // status hooks (installed / enabled / accelerators / binary). This
  // names no backend, so a new backend surfaces in `status` from its
  // registration alone — no edit here.
  let mut rows = Vec::new();
  for b in Backends::all() {
    let accelerators = b.status_accelerators(ctx, &device_accels).await;
    let mut labels = server_labels(b.id(), &catalog, &accelerators);
    labels.extend(b.status_server_labels());
    let mut row = backend_row(b.id(), b.installed(ctx), accelerators);
    if let Some(obj) = row.as_object_mut() {
      obj.insert("server_labels".into(), json!(labels));
      if let Some(enabled) = b.status_enabled(ctx) {
        obj.insert("enabled".into(), json!(enabled));
      }
    }
    set_backend_binary(&mut row, b.binary_path(ctx));
    rows.push(row);
  }
  Value::Array(rows)
}

/// One display label per catalog server of `backend_id`, in catalog order.
/// A server with no label of its own (no name, no device probe) reads as the
/// backend's accelerators joined with `|`, else its binary's file name.
fn server_labels(
  backend_id: &str,
  catalog: &[crate::backend::Server],
  accelerators: &[String],
) -> Vec<String> {
  catalog
    .iter()
    .filter(|s| s.backend_id == backend_id)
    .map(|s| match &s.label {
      Some(l) => l.clone(),
      None if !accelerators.is_empty() => accelerators.join("|"),
      None => s
        .binary
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| s.id.clone()),
    })
    .collect()
}

/// Attach a backend row's resolved `binary` path; absent when no binary
/// resolves so clients can tell "backend known" from "executable found".
fn set_backend_binary(row: &mut Value, binary: Option<String>) {
  if let (Some(obj), Some(bin)) = (row.as_object_mut(), binary) {
    obj.insert("binary".into(), json!(bin));
  }
}

fn backend_row(id: &str, installed: bool, accelerators: Vec<String>) -> Value {
  json!({
    "id": id,
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

  use serde_json::{json, Value};

  use crate::daemon::context::MethodContext;
  use crate::daemon::shutdown::ShutdownToken;
  use crate::ipc::methods::dispatch_request;
  use crate::ipc::protocol::Request;

  fn ctx() -> MethodContext {
    MethodContext::new(ShutdownToken::new())
  }

  #[test]
  fn server_labels_fall_back_from_label_to_accelerators_to_binary_name() {
    let srv = |backend: &str, binary: &str, label: Option<&str>| crate::backend::Server {
      id: format!("{backend}-{binary}"),
      backend_id: backend.into(),
      binary: PathBuf::from(binary),
      name: String::new(),
      devices: Vec::new(),
      caps: Default::default(),
      label: label.map(String::from),
    };
    let catalog = [
      srv("alpha", "/a/rocm/server", Some("rocm")),
      srv("beta", "/b/engine", None),
      srv("alpha", "/a/cpu/server", Some("cpu")),
    ];
    let accels = vec!["cpu".to_string(), "npu".to_string()];
    assert_eq!(
      super::server_labels("alpha", &catalog, &accels),
      ["rocm", "cpu"]
    );
    assert_eq!(super::server_labels("beta", &catalog, &accels), ["cpu|npu"]);
    assert_eq!(super::server_labels("beta", &catalog, &[]), ["engine"]);
    assert!(super::server_labels("gamma", &catalog, &accels).is_empty());
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
    // Preload is always reported, empty included, so "not configured" and
    // "unread" stay distinguishable.
    assert_eq!(daemon["preload"]["entries"], json!([]));
    assert_eq!(daemon["preload"]["presets"], json!(0));
  }

  /// A configured preload list and preset pins both reach `status.daemon`, so an
  /// operator can tell a daemon that was asked to warm models from one that was
  /// not even when every entry failed to start.
  #[tokio::test]
  async fn status_reports_configured_preload() {
    let mut c = ctx();
    c.preload = std::sync::Arc::new(vec!["ghost.gguf".into(), "warm.gguf@long-ctx".into()]);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let daemon = resp.result.expect("status result")["daemon"].clone();
    assert_eq!(
      daemon["preload"]["entries"],
      json!(["ghost.gguf", "warm.gguf@long-ctx"])
    );
    assert_eq!(daemon["preload"]["presets"], json!(0));
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
    // Each row carries the installed/accelerator fields; llama.cpp always
    // offers CPU.
    let llama = backends
      .iter()
      .find(|b| b["id"] == "llamacpp")
      .expect("llamacpp row");
    assert!(llama["installed"].is_boolean());
    assert!(
      llama.get("lifecycle").is_none(),
      "backend rows carry no lifecycle field"
    );
    assert!(
      llama.get("umbrella").is_none(),
      "backend rows carry no umbrella field"
    );
    let accel: Vec<&str> = llama["accelerators"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|v| v.as_str())
      .collect();
    assert!(accel.contains(&"cpu"), "llama.cpp floor is cpu: {accel:?}");
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
