//! Method dispatch for the daemon's IPC layer.
//!
//! Keeping the registry as a `match` (rather than a
//! `HashMap<&str, fn>`) avoids dynamic-dispatch plumbing for what is,
//! in practice, a small fixed set of methods.

use std::{ffi::OsString, path::PathBuf, sync::atomic::Ordering, time::Duration};

use serde::Deserialize;
use serde_json::{json, Value};

use super::protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
use crate::backend::identity::ModelIdentity;
use crate::daemon::context::MethodContext;
use crate::daemon::launch_service::{compose_and_spawn, LaunchModeWire, StartParams};
use crate::daemon::registry::LaunchId;
use crate::daemon::supervisor::ManagedState;
use crate::gguf::identity::ModelId;
use crate::launch::favorites::FavoriteEntry;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;
use crate::launch::presets::{
  effective_presets, materialize_preset, preset_body_from_launch_params, EffectivePresets,
  NamedPreset,
};
use crate::launch::resolve::CatalogRow;

/// Top-level dispatch. Always returns a `Response` — protocol violations
/// surface as JSON-RPC error responses rather than disconnects.
pub async fn dispatch_request(ctx: &MethodContext, req: Request) -> Response {
  let id = req.id.clone().unwrap_or(Value::Null);

  if req.jsonrpc != JSONRPC_VERSION {
    return Response::err(
      id,
      ErrorObject::new(
        ErrorCode::InvalidRequest,
        format!("jsonrpc must be \"{JSONRPC_VERSION}\""),
      ),
    );
  }

  match req.method.as_str() {
    "ping" => Response::ok(id, json!("pong")),
    "version" => {
      let uptime_secs = ctx.started_at.elapsed().as_secs();
      let connections = ctx.active_connections.load(Ordering::Relaxed);
      Response::ok(
        id,
        json!({
          "name": env!("CARGO_PKG_NAME"),
          "version": env!("CARGO_PKG_VERSION"),
          // Wire protocol version. Bumped only when an existing
          // method's request or response shape changes in a way
          // older clients can't parse. New methods are additive
          // and don't require a bump; callers can feature-detect
          // via `capabilities`.
          "protocol_version": 1u32,
          "pid": std::process::id(),
          "uptime_seconds": uptime_secs,
          "connections": connections,
        }),
      )
    }
    "capabilities" => {
      // Method-set introspection. Returned as a sorted array of the
      // method names this daemon advertises so clients can do a
      // cheap feature-detect before issuing an unknown method call.
      let methods = supported_methods();
      Response::ok(
        id,
        json!({
          "protocol_version": 1u32,
          "methods": methods,
        }),
      )
    }
    "shutdown" => {
      // The longest grace the teardown may take, so `daemon stop` waits for
      // it instead of giving up while a slow engine is still stopping.
      let grace = ctx
        .supervisors
        .snapshot()
        .await
        .iter()
        .map(|(_, m)| m.min_stop_grace())
        .max()
        .unwrap_or_default()
        .max(SHUTDOWN_STOP_GRACE);
      ctx.shutdown.trigger();
      Response::ok(
        id,
        json!({"shutdown": "scheduled", "stop_grace_secs": grace.as_secs()}),
      )
    }
    #[cfg(feature = "test-fixtures")]
    "_test_sleep" => {
      // Test-only seam: holds the connection open for the requested
      // number of milliseconds. Used by drain-timeout tests to model
      // a slow in-flight request. Behind the `test-fixtures` feature
      // so production builds never expose it.
      let ms: u64 = req
        .params
        .as_ref()
        .and_then(|p| p.get("ms"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
      tokio::time::sleep(Duration::from_millis(ms)).await;
      Response::ok(id, json!({"slept_ms": ms}))
    }
    "list_models" => {
      // The ids of backends currently available on this host, so a row badges
      // its routed backend only when that backend can actually serve it.
      // Registry-driven — names no backend.
      use crate::backend::Backend;
      let available_routed: std::collections::BTreeSet<String> = crate::backend::Backends::all()
        .into_iter()
        .filter(|b| b.available(ctx))
        .map(|b| b.id().to_string())
        .collect();
      let body = ctx.catalog.to_list_response(&available_routed).await;
      Response::ok(id, body)
    }
    "status" => Response::ok(id, crate::ipc::status::status_response(ctx).await),
    "start_model" => respond(id, start_model_handler(ctx, req.params).await),
    "stop_model" => respond(id, stop_model_handler(ctx, req.params).await),
    "stop_all" => respond(id, stop_all_handler(ctx, req.params).await),
    "stop_external" => respond(id, stop_external_handler(ctx, req.params).await),
    "logs_tail" => respond(id, logs_tail_handler(ctx, req.params).await),
    "presets_list" => respond(id, presets_list_handler(ctx, req.params).await),
    "presets_save" => respond(id, presets_save_handler(ctx, req.params).await),
    "presets_delete" => respond(id, presets_delete_handler(ctx, req.params).await),
    "presets_show" => respond(id, presets_show_handler(ctx, req.params).await),
    "presets_all" => Response::ok(id, presets_all_handler(ctx).await),
    "favorite_add" => respond(id, favorite_add_handler(ctx, req.params).await),
    "favorite_remove" => respond(id, favorite_remove_handler(ctx, req.params).await),
    "favorite_list" => respond(id, favorite_list_handler(ctx).await),
    "last_params_list" => respond(id, last_params_list_handler(ctx).await),
    other => Response::err(
      id,
      ErrorObject::new(
        ErrorCode::MethodNotFound,
        format!("unknown method: {other}"),
      ),
    ),
  }
}

/// Lift a `Result<Value, ErrorObject>` into a `Response`. Collapses the
/// 14 near-identical `match { Ok(v) => Response::ok(id, v), Err(e) =>
/// Response::err(id, e) }` arms in the dispatcher.
fn respond(id: Value, result: Result<Value, ErrorObject>) -> Response {
  match result {
    Ok(v) => Response::ok(id, v),
    Err(e) => Response::err(id, e),
  }
}

#[derive(Deserialize)]
struct StopParams {
  launch_id: LaunchId,
  #[serde(default = "default_grace_secs")]
  grace_secs: u64,
}

fn default_grace_secs() -> u64 {
  5
}

/// Upper bound on the SIGTERM→SIGKILL grace window. Caps both
/// managed `stop_model` and external `stop_external`. Keeps
/// `Duration::from_secs(grace)` arithmetic safe and prevents a
/// same-UID caller from holding the IPC task open indefinitely by
/// passing `u64::MAX`.
const MAX_GRACE_SECS: u64 = 300;

fn check_grace_secs(secs: u64) -> Result<(), ErrorObject> {
  if secs > MAX_GRACE_SECS {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("grace_secs={secs} exceeds maximum {MAX_GRACE_SECS}; clamp client-side"),
    ));
  }
  Ok(())
}

async fn stop_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StopParams = parse_params(params)?;
  check_grace_secs(parsed.grace_secs)?;
  // Hand the launch to its owning backend and ask it to stop — the backend
  // decides *how* (SIGTERM a supervised child, or unload from its umbrella). The
  // caller resolves the owner (registry lookup by recorded backend id) but never
  // branches on process-vs-umbrella lifecycle.
  let backend = crate::daemon::launch_service::backend_for_launch(ctx, &parsed.launch_id).await;
  crate::backend::Backend::stop(&backend, ctx, &parsed.launch_id, parsed.grace_secs).await
}

/// The `(owning backend, umbrella model name)` behind a delegated launch id, or
/// `None` when `launch_id` isn't a delegated row. Delegated models have no
/// supervisor of their own, so the `L#` → (backend, name) binding is read off
/// the running snapshot stamped at launch; `logs_tail` reverse-maps through here
/// to find the umbrella whose log a delegated model shares. Names no backend —
/// the backend is resolved from the row's identity.
async fn delegated_target(
  ctx: &MethodContext,
  launch_id: &LaunchId,
) -> Option<(crate::backend::Backends, String)> {
  ctx
    .state
    .snapshot()
    .await
    .running
    .into_iter()
    .find(|r| r.launch_id.as_ref() == Some(launch_id))
    .and_then(|r| {
      r.delegated_backend_id()
        .map(|b| (crate::backend::backend_for_identity(&r.id), b.name.clone()))
    })
}

/// Flatten `ManagedState` to a JSON object whose `state` field is a
/// lowercase string label plus an optional `cause`. Used by
/// `stop_model` / `stop_all` responses and the status rows so every
/// surface reports model state in one shape.
pub(crate) fn flatten_state(state: &ManagedState) -> Value {
  match state.cause() {
    Some(cause) => json!({"state": state.label(), "cause": cause}),
    None => json!({"state": state.label()}),
  }
}

#[derive(Deserialize)]
struct StopExternalParams {
  pid: u32,
  /// Grace seconds between SIGTERM and SIGKILL. Mirrors
  /// [`StopParams::grace_secs`] for parity with managed stop.
  #[serde(default = "default_grace_secs")]
  grace_secs: u64,
}

/// Stop an unmanaged `llama-server` process the daemon previously
/// surfaced via the `external` snapshot. Sends SIGTERM, waits up
/// to `grace_secs`, then SIGKILL if the process is still alive.
/// The external snapshot is rebuilt next time `status` is fetched
/// (the supervisor doesn't drive sysinfo on a tick), so the row
/// will keep appearing until the next sweep refreshes it.
async fn stop_external_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StopExternalParams = parse_params(params)?;
  check_grace_secs(parsed.grace_secs)?;
  // Confirm the PID is one we surfaced as external and snapshot
  // its recorded start_time. We later re-verify the live
  // start_time matches before each signal to defend against PID
  // recycling: if the original process exits during the grace
  // window and the kernel hands the pid to an unrelated process,
  // its start_time will differ from our snapshot and we refuse to
  // signal it.
  let recorded_start_time = {
    let known = ctx
      .external
      .read()
      .await
      .iter()
      .find(|e| e.pid == parsed.pid)
      .map(|e| e.start_time_secs);
    match known {
      Some(s) => s,
      None => {
        return Err(ErrorObject::new(
          ErrorCode::InvalidParams,
          format!("pid {} is not a known external llama-server", parsed.pid),
        ))
      }
    }
  };
  // Bound the pid cast: a u32 > i32::MAX flips negative under
  // `as i32` and `libc::kill(neg, sig)` would signal a process
  // group. Kernel pid_max on every supported platform is well below
  // i32::MAX in practice, but the daemon shouldn't trust that.
  if parsed.pid > i32::MAX as u32 {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("pid {} exceeds i32::MAX; refusing to signal", parsed.pid),
    ));
  }

  // Helper: returns Some(true) if alive AND start_time matches, Some(false)
  // if alive but pid has been reused, None if dead. We sample via
  // `sysinfo` rather than `kill(pid, 0)` so we can compare start_time
  // — the cheap liveness check alone can't distinguish recycle.
  //
  // Defensive: if either the live or expected `start_time` is 0 we
  // can't *prove* identity (sysinfo can hand back 0 on some platforms /
  // for kernel processes, and adopted-but-already-dead entries are
  // seeded with `start_time_secs = 0` in `daemon::mod`). Treat that
  // as a mismatch — refusing to signal is the safe failure mode.
  //
  // Off-thread via `spawn_blocking`: sysinfo does synchronous /proc
  // I/O (Linux) or sysctl (macOS) per refresh. In the 100ms grace
  // loop that's ~50 calls per stop, and `stop_all` runs them in
  // parallel via `join_all` — left on the async worker, a fleet of
  // concurrent stops can saturate every reactor thread and stall
  // probe polling for a launching model.
  async fn live_and_same(pid: u32, expected_start: u64) -> Option<bool> {
    tokio::task::spawn_blocking(move || {
      use sysinfo::{Pid, ProcessRefreshKind, System};
      let refresh = ProcessRefreshKind::everything();
      let mut sys = System::new();
      sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        refresh,
      );
      sys.process(Pid::from_u32(pid)).map(|p| {
        let live = p.start_time();
        live != 0 && expected_start != 0 && live == expected_start
      })
    })
    .await
    .unwrap_or(None)
  }
  match live_and_same(parsed.pid, recorded_start_time).await {
    Some(true) => {}
    Some(false) => {
      ctx.external.write().await.retain(|e| e.pid != parsed.pid);
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!(
          "pid {} has been recycled; refusing to signal (start_time mismatch)",
          parsed.pid
        ),
      ));
    }
    None => {
      // Already gone — surface as success.
      ctx.external.write().await.retain(|e| e.pid != parsed.pid);
      return Ok(json!({
        "pid": parsed.pid,
        "killed_with_sigkill": false,
      }));
    }
  }
  // SIGTERM first — give the process time to exit cleanly. Goes
  // through [`ProcessControl`] so the Windows single-pid path stays
  // in one place rather than a second migration here.
  use crate::util::process_control::SignalTarget;
  let pc = crate::util::process_control::platform_default();
  pc.signal_graceful(SignalTarget::SinglePid(parsed.pid));
  let grace = Duration::from_secs(parsed.grace_secs);
  let mut elapsed = Duration::ZERO;
  let step = Duration::from_millis(100);
  while elapsed < grace {
    match live_and_same(parsed.pid, recorded_start_time).await {
      Some(true) => {}
      _ => break, // gone, or pid was recycled — either way stop signalling
    }
    tokio::time::sleep(step).await;
    elapsed += step;
  }
  // Final check; SIGKILL only if same process is still up.
  let mut sent_kill = false;
  if matches!(
    live_and_same(parsed.pid, recorded_start_time).await,
    Some(true)
  ) {
    pc.signal_kill(SignalTarget::SinglePid(parsed.pid));
    sent_kill = true;
  }
  ctx.external.write().await.retain(|e| e.pid != parsed.pid);
  Ok(json!({
    "pid": parsed.pid,
    "killed_with_sigkill": sent_kill,
  }))
}

#[derive(Default, Deserialize)]
struct StopAllParams {
  #[serde(default)]
  grace_secs: Option<u64>,
}

async fn stop_all_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  // `stop_all` is the only handler called with `None` params by the
  // TUI's old code path; treat absent / null as an empty options
  // object rather than rejecting at parse time.
  let parsed: StopAllParams = match params {
    Some(Value::Null) | None => StopAllParams::default(),
    other => parse_params(other)?,
  };
  let grace_secs = parsed.grace_secs.unwrap_or_else(default_grace_secs);
  check_grace_secs(grace_secs)?;
  let outcomes = stop_all_managed(ctx, Duration::from_secs(grace_secs)).await;
  let stopped: Vec<Value> = outcomes
    .iter()
    .map(|(launch_id, state)| json!({"launch_id": launch_id, "state": flatten_state(state)}))
    .collect();
  let count = stopped.len();
  Ok(json!({"stopped": stopped, "count": count}))
}

/// SIGTERM-then-SIGKILL every managed launch concurrently, drop them
/// from the registry, and prune `state.running`. Returns the
/// (launch_id, final_state) pairs for callers that need to surface
/// them on the wire.
///
/// Exposed so the daemon's shutdown path can kill its supervised
/// children before `run_foreground` returns. The supervisor spawns
/// `llama-server` with `setsid`, so without this hook a graceful
/// `daemon stop` / SIGINT / IPC `shutdown` leaves the children
/// running as init-owned orphans. R42's orphan adoption only intends
/// to rescue children from *crashes* (SIGKILL, segfault); it should
/// not turn deliberate shutdown into a leak.
///
/// The `join_all` keeps wall-clock equal to the slowest stop rather
/// than the sum — the original sequential loop blew the default IPC
/// client timeout for 2+ stuck launches.
/// The grace daemon shutdown gives each child, before any backend floor.
pub(crate) const SHUTDOWN_STOP_GRACE: Duration = Duration::from_secs(5);

pub(crate) async fn stop_all_managed(
  ctx: &MethodContext,
  grace: Duration,
) -> Vec<(LaunchId, ManagedState)> {
  use futures::future::join_all;
  let snap = ctx.supervisors.snapshot().await;
  let stops = snap.into_iter().map(|(launch_id, model)| async move {
    let final_state = model.stop(grace).await;
    let model_id = model.id().clone();
    let port = model.port();
    (launch_id, model_id, port, final_state)
  });
  let outcomes = join_all(stops).await;

  let mut stopped: Vec<(LaunchId, ManagedState)> = Vec::with_capacity(outcomes.len());
  let mut stopped_keys: Vec<(LaunchId, u16)> = Vec::with_capacity(outcomes.len());
  for (launch_id, _model_id, port, final_state) in outcomes {
    ctx.supervisors.remove(&launch_id).await;
    stopped_keys.push((launch_id.clone(), port));
    stopped.push((launch_id, final_state));
  }
  crate::daemon::launch_service::drop_running_snapshots(ctx, &stopped_keys).await;
  stopped
}

#[derive(Deserialize)]
struct LogsTailParams {
  launch_id: LaunchId,
  #[serde(default = "default_lines")]
  lines: usize,
}

fn default_lines() -> usize {
  200
}

async fn logs_tail_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: LogsTailParams = parse_params(params)?;
  // A delegated (managed-multiplexer) model has no process of its own — its log
  // *is* the shared umbrella's log, so tail that one (the owning backend's infra
  // launch).
  let lookup_id = match delegated_target(ctx, &parsed.launch_id).await {
    Some((backend, _)) => crate::backend::Backend::umbrella_launch_id(&backend)
      .unwrap_or_else(|| parsed.launch_id.clone()),
    None => parsed.launch_id.clone(),
  };
  let model = ctx.supervisors.get(&lookup_id).await.ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("unknown launch_id: {}", parsed.launch_id.as_str()),
    )
  })?;
  let tail = model.tail(parsed.lines).await;
  Ok(json!({
    "launch_id": parsed.launch_id,
    "lines": tail,
  }))
}

/// Sorted list of every method `dispatch_request` knows. Used by
/// the `capabilities` handler so clients can feature-detect. The
/// names here mirror the wire spec in `docs/architecture.md`; a new
/// method must be added in both places.
const PUBLIC_METHODS: &[&str] = &[
  "ping",
  "version",
  "capabilities",
  "shutdown",
  "list_models",
  "status",
  "start_model",
  "stop_model",
  "stop_all",
  "stop_external",
  "logs_tail",
  "presets_list",
  "presets_save",
  "presets_delete",
  "presets_show",
  "presets_all",
  "favorite_add",
  "favorite_remove",
  "favorite_list",
  "last_params_list",
];

fn supported_methods() -> Vec<&'static str> {
  let mut v = PUBLIC_METHODS.to_vec();
  v.sort();
  v
}

/// IPC `start_model` handler — a thin wrapper around
/// [`compose_and_spawn`](crate::daemon::launch_service::compose_and_spawn).
/// Keeps the JSON-RPC plumbing (parse params → call the launch service →
/// JSON-encode response) at the handler boundary so the proxy's
/// auto-start can call the service directly without round-tripping
/// through the dispatcher.
async fn start_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StartParams = parse_params(params)?;
  // IPC clients are user-initiated (TUI Launch, `llamastash start`,
  // bare JSON-RPC). The proxy's auto-start path bypasses this
  // handler and calls `compose_and_spawn` directly with
  // `LaunchOrigin::AutoStart`.
  let started =
    compose_and_spawn(ctx, parsed, crate::daemon::supervisor::LaunchOrigin::Manual).await?;
  let pid = started.model.pid().await;
  let mut resp = json!({
    "launch_id": started.launch_id,
    "model_id": started.model_id,
    "port": started.port,
    "pid": pid,
    "log_path": started.log_path,
  });
  // The accepted name, echoed rather than assumed: the client reports what the
  // daemon actually stamped (a stale pre-name daemon correctly reports unnamed
  // instead of the client printing the name it *asked* for). Omitted when unset
  // to keep the shape byte-stable for unnamed launches.
  if let Some(n) = &started.name {
    resp["launch_name"] = json!(n);
  }
  // Non-fatal advisories (dropped knobs, deepseek4 KV-blind note, ssd_streaming
  // bypass). Omitted when empty so the response stays byte-stable for launches
  // that raise none (every llama.cpp / Lemonade launch today).
  if !started.warnings.is_empty() {
    resp["warnings"] = json!(started.warnings);
  }
  // Layer provenance per resolved knob. Omitted when empty (a pure-fit launch
  // where every knob fell to the backend default resolves no real layer).
  if !started.layer_sources.is_empty() {
    resp["layer_sources"] = json!(started.layer_sources);
  }
  Ok(resp)
}

/// Header-derived launch inputs from one GGUF read: `(model id, architecture,
/// trained ctx window, routed-backend tag)`. The routed-backend tag is the
/// registry's verdict for this header (`None` = the default process backend).
pub(crate) type ResolvedModelInfo = (
  ModelId,
  Option<String>,
  Option<u32>,
  Vec<String>,
  // Embedded MTP (nextn) draft-head layer count, `Some(n>0)` ⇒ the model can
  // self-speculate with no separate drafter.
  Option<u32>,
);

/// The header-derived launch inputs, in the tuple shape the preset-key lookup
/// still wants. A thin adapter over [`crate::backend::resolve_identity_for_path`]
/// — the header read, the id, the arch and the supported-backend list all come
/// from there, so this shape can never drift from what a launch observes.
///
/// `async` because a caller cannot afford this inline: the header read is up to
/// ~16 MiB of synchronous file I/O and every caller is an IPC handler, so the
/// read runs on a blocking thread instead of stalling a tokio worker (same rule
/// the proxy's auto-start path follows).
pub(crate) async fn resolve_model_id_and_arch(
  path: &std::path::Path,
) -> Result<ResolvedModelInfo, ErrorObject> {
  let path = path.to_path_buf();
  let joined = tokio::task::spawn_blocking(move || {
    crate::backend::resolve_identity_for_path(&path, None)
      .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, e.to_string()))
  })
  .await;
  let r = joined.map_err(|join| {
    ErrorObject::new(
      ErrorCode::InternalError,
      format!("model identity resolution failed: {join}"),
    )
  })??;
  Ok((
    r.id,
    r.arch,
    r.native_ctx,
    r.supported_backends,
    r.mtp_embedded,
  ))
}

/// Project the daemon's catalog into the lean rows preset-key
/// classification reads (path + display label + arch).
pub(crate) async fn catalog_rows(ctx: &MethodContext) -> Vec<CatalogRow> {
  ctx
    .catalog
    .snapshot()
    .await
    .iter()
    .map(|m| {
      CatalogRow::for_resolution(
        m.path.display().to_string(),
        m.display_label.clone(),
        m.metadata.as_ref().and_then(|md| md.arch.clone()),
      )
    })
    .collect()
}

/// Resolve the per-model write key, the model's arch, and the projected
/// catalog rows for `model_path` — everything [`effective_presets`] needs
/// except a store snapshot. The key is the model's display name (basename
/// for a local GGUF) — what CLI/TUI saves write under; a model not in the
/// catalog falls back to its basename + GGUF-header arch. Split out so the
/// save path resolves this once and recomputes the effective set from a
/// single post-save store snapshot, rather than re-deriving the key.
async fn model_key_arch_rows(
  ctx: &MethodContext,
  model_path: &std::path::Path,
) -> (String, Option<String>, Vec<CatalogRow>) {
  let rows = catalog_rows(ctx).await;
  let path_str = model_path.display().to_string();
  // Always key by basename. When two discovered models share a basename
  // (the same GGUF cached in two roots), they intentionally share one preset
  // set — the read side (`effective_presets`) applies a basename key to every
  // model with that name.
  let (key, arch) = match rows.iter().find(|r| r.path == path_str) {
    Some(r) => (r.name(), r.arch.clone()),
    None => (
      crate::util::paths::model_file_label(model_path),
      resolve_model_id_and_arch(model_path)
        .await
        .ok()
        .and_then(|(_, a, _, _, _)| a),
    ),
  };
  (key, arch, rows)
}

/// Resolve the per-model write key + effective preset set for
/// `model_path` (a fresh store snapshot paired with [`model_key_arch_rows`]).
async fn model_key_and_effective(
  ctx: &MethodContext,
  model_path: &std::path::Path,
) -> (String, EffectivePresets) {
  let (key, arch, rows) = model_key_arch_rows(ctx, model_path).await;
  let store = ctx.presets.snapshot().await;
  let path_str = model_path.display().to_string();
  let eff = effective_presets(&key, &path_str, arch.as_deref(), &store, &rows);
  (key, eff)
}

/// A config-write failure (symlink/parent-mode/patch/IO) is a server-side
/// fault, not bad input — surfaces as a JSON-RPC internal error.
fn write_err(e: crate::config::writer::WriteError) -> ErrorObject {
  ErrorObject::new(
    ErrorCode::InternalError,
    format!("preset config write failed: {e}"),
  )
}

#[derive(Deserialize)]
struct PresetsListParams {
  model_path: PathBuf,
}

async fn presets_list_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsListParams = parse_params(params)?;
  let (key, eff) = model_key_and_effective(ctx, &parsed.model_path).await;
  let rows: Vec<Value> = eff
    .presets
    .iter()
    .map(|np| preset_row(np, is_default(&eff, &np.name)))
    .collect();
  Ok(json!({
    "model": key,
    "default": eff.default,
    "presets": rows,
  }))
}

#[derive(Deserialize)]
struct PresetsSaveParams {
  model_path: PathBuf,
  name: String,
  #[serde(default)]
  ctx: Option<u32>,
  #[serde(default)]
  reasoning: Option<bool>,
  #[serde(default)]
  mode: Option<LaunchModeWire>,
  #[serde(default)]
  knobs: crate::launch::knobs::KnobSet,
  #[serde(default)]
  extras: Vec<String>,
  /// Backend this preset pins. Launch *identity*, not a knob — it decides
  /// which backend's knobs apply at all, so it cannot be backend-declared.
  #[serde(default)]
  backend: Option<String>,
  /// Server (build/binary) this preset pins. Identity, like `backend`.
  #[serde(default)]
  server: Option<String>,
  /// Idle-TTL override in seconds for launches this preset starts (`0` = never
  /// unload). Residency policy, not a launch knob, so it never rides in `knobs`.
  /// Tri-state on purpose: absent = leave whatever the entry already pins,
  /// `null` = clear the pin, a number = set it. A caller that captures launch
  /// params (the TUI's `Ctrl+P`, `presets save --from-last`) sends nothing here,
  /// and must not silently delete a residency pin it never looked at.
  #[serde(default, deserialize_with = "clearable_u64::deserialize")]
  idle_ttl_secs: Option<Option<u64>>,
  /// Start this preset when the daemon boots. Same tri-state: absent = leave the
  /// entry's pin, `true` / `false` set or clear it.
  #[serde(default)]
  preload: Option<bool>,
}

/// Reads a present `idle_ttl_secs` while keeping "absent" distinguishable from
/// "sent as null". Plain `Option<Option<u64>>` will not do it: serde maps a JSON
/// `null` to the *outer* `None`, so an explicit clear would look exactly like a
/// caller that said nothing — which is the data loss this tri-state exists to
/// avoid.
mod clearable_u64 {
  use serde::de::{Deserialize, Deserializer};

  fn bad<D: serde::de::Error>() -> D {
    serde::de::Error::custom("idle_ttl_secs must be a non-negative integer or null")
  }

  pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<u64>>, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
      serde_json::Value::Null => Ok(Some(None)),
      serde_json::Value::Number(n) => match n.as_u64() {
        Some(secs) => Ok(Some(Some(secs))),
        None => Err(bad()),
      },
      _ => Err(bad()),
    }
  }
}

async fn presets_save_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsSaveParams = parse_params(params)?;
  if parsed.name.trim().is_empty() {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      "preset name must not be empty",
    ));
  }
  // Assemble the launch settings the preset stores, then fold them into a
  // config-layer body (ctx/reasoning move into the flat knobs; port drops).
  let mut lp = LaunchParams::new(
    parsed.model_path.clone(),
    parsed
      .mode
      .map(LaunchMode::from)
      .unwrap_or(LaunchMode::Chat),
  );
  lp.ctx = parsed.ctx;
  lp.reasoning = parsed.reasoning.unwrap_or(false);
  lp.knobs = parsed.knobs;
  // Identity: which backend and which of its builds this preset pins. Stored
  // verbatim so a saved preset can reproduce the run it was captured from.
  lp.backend = parsed
    .backend
    .as_deref()
    .map(crate::launch::params::BackendChoice::from_id)
    .unwrap_or_default();
  lp.server = parsed.server.clone();
  lp.extras = parsed.extras.into_iter().map(OsString::from).collect();
  let (key, arch, rows) = model_key_arch_rows(ctx, &parsed.model_path).await;
  // Residency policy sits beside the launch params rather than inside them: it
  // decides how long the launch *stays* up, not how it is launched. Because a
  // params capture knows nothing about it, an absent field inherits what the
  // entry already pins instead of dropping it; only an explicit `null` /
  // `false` clears.
  // Inherit from the entry the *name* actually resolves to, which may sit under
  // an arch or wildcard key the save is about to shadow with a per-model entry of
  // the same name; looking only under this model's own key would find nothing and
  // silently drop the pin being shadowed.
  let before = ctx.presets.snapshot().await;
  let path_str = parsed.model_path.display().to_string();
  let before_eff = effective_presets(&key, &path_str, arch.as_deref(), &before, &rows);
  let inherited = before_eff.named(&parsed.name);
  let source = before_eff.source_key(&parsed.name);
  let idle_ttl_secs = match parsed.idle_ttl_secs {
    Some(pin) => pin,
    None => inherited.and_then(|e| e.idle_ttl_secs),
  };
  // `preload` inherits only from a key that names this one model, the same test
  // boot applies: `preload: true` on an arch or family key is skipped at boot, so
  // copying it onto this model's own key would start it at boot from a save that
  // never asked, and a preloaded launch can never be unloaded again.
  let preload = parsed.preload.unwrap_or_else(|| {
    source.is_some_and(|source| {
      inherited.is_some_and(|e| e.preload)
        && matches!(crate::daemon::preload::preload_target(source, &rows),
                    Ok(path) if path == std::path::Path::new(&path_str))
    })
  });
  let body = crate::config::PresetBody {
    idle_ttl_secs,
    preload,
    ..preset_body_from_launch_params(&lp)
  };

  let saved_np = materialize_preset(&parsed.name, &body, parsed.model_path.clone());
  let prev = ctx
    .presets
    .save(&key, &parsed.name, body)
    .await
    .map_err(write_err)?;

  // Recompute `is_default` from a single post-save store snapshot, reusing
  // the key/arch/rows resolved above — the catalog can't change across the
  // save, so re-deriving the key (a second catalog snapshot) is wasted work.
  let store = ctx.presets.snapshot().await;
  let eff = effective_presets(&key, &path_str, arch.as_deref(), &store, &rows);
  let default = is_default(&eff, &parsed.name);
  let replaced = prev
    .map(|b| materialize_preset(&parsed.name, &b, parsed.model_path.clone()))
    .map(|np| preset_row(&np, default));
  Ok(json!({
    "model": key,
    "saved": preset_row(&saved_np, default),
    "replaced": replaced,
  }))
}

#[derive(Deserialize)]
struct PresetsDeleteParams {
  model_path: PathBuf,
  name: String,
}

async fn presets_delete_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsDeleteParams = parse_params(params)?;
  let (key, eff) = model_key_and_effective(ctx, &parsed.model_path).await;
  let default = is_default(&eff, &parsed.name);
  let removed = ctx
    .presets
    .delete(&key, &parsed.name)
    .await
    .map_err(write_err)?;
  let removed_row = removed
    .map(|b| materialize_preset(&parsed.name, &b, parsed.model_path.clone()))
    .map(|np| preset_row(&np, default));
  Ok(json!({
    "model": key,
    "removed": removed_row,
  }))
}

#[derive(Deserialize)]
struct PresetsShowParams {
  model_path: PathBuf,
  name: String,
}

async fn presets_show_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsShowParams = parse_params(params)?;
  let (key, eff) = model_key_and_effective(ctx, &parsed.model_path).await;
  let default = is_default(&eff, &parsed.name);
  let preset = eff.presets.get(&parsed.name);
  Ok(json!({
    "model": key,
    "name": parsed.name,
    "preset": preset.map(|np| preset_row(np, default)),
  }))
}

/// Raw config `presets:` map (every model/arch key → its block). The TUI
/// fetches this once per refresh and resolves each model's effective set
/// client-side (it already holds the catalog), so it can populate the
/// launch picker's preset cycle without a per-model round-trip.
async fn presets_all_handler(ctx: &MethodContext) -> Value {
  json!({ "presets": ctx.presets.snapshot().await })
}

fn is_default(eff: &EffectivePresets, name: &str) -> bool {
  eff.default.as_deref() == Some(name)
}

/// `(preset_count, default)` status hint for the model at `model_path`,
/// computed from pre-fetched catalog rows + a store snapshot so the
/// `status` row builder can hint every running model without re-snapshotting
/// per row. The arch is read from the model's own catalog row.
pub(crate) fn preset_hint(
  model_path: &str,
  rows: &[CatalogRow],
  store: &std::collections::BTreeMap<String, crate::config::ConfigPresetBlock>,
) -> (u32, Option<String>) {
  let row = rows.iter().find(|r| r.path == model_path);
  let name = row
    .map(|r| r.name())
    .unwrap_or_else(|| crate::util::paths::model_file_label(std::path::Path::new(model_path)));
  let arch = row.and_then(|r| r.arch.clone());
  let eff = effective_presets(&name, model_path, arch.as_deref(), store, rows);
  (eff.presets.len() as u32, eff.default)
}

fn preset_row(p: &NamedPreset, is_default: bool) -> Value {
  json!({
    "name": p.name,
    "params": p.params.to_wire(),
    // Presets live in config.yaml now; the provenance is constant but
    // surfaced so agents can distinguish a config preset from any future
    // source without re-deriving it.
    "source": "config",
    "is_default": is_default,
    // Residency policy: this preset's idle-TTL override (`0` = never unload,
    // null = the global `proxy.idle_ttl_secs`) and whether it preloads at boot.
    "idle_ttl_secs": p.idle_ttl_secs,
    "preload": p.preload,
  })
}

#[derive(Deserialize)]
struct FavoriteParams {
  model_path: PathBuf,
}

/// The favorites-store id for a model path. Shared by add + remove so both
/// build the same key.
///
/// A path with no file to hash — a registry entry, a snapshot directory — gets
/// the synthetic id its backend mints, path-identical to the catalog row so the
/// TUI's `★` (which matches favorites by `id.path`) resolves. This used to
/// special-case one backend by name, which both broke the no-leak rule and left
/// every later file-less shape reading a GGUF header that isn't there.
fn resolve_favorite_id(model_path: &std::path::Path) -> Result<ModelId, ErrorObject> {
  crate::backend::resolve_identity_for_path(model_path, None)
    .map(|r| r.id)
    .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, e.to_string()))
}

async fn favorite_add_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: FavoriteParams = parse_params(params)?;
  let id = resolve_favorite_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let added = ctx
    .state
    .mutate(|s| s.favorites.add(identity.clone()))
    .await;
  Ok(json!({
    "model_id": id,
    "added": added,
  }))
}

async fn favorite_remove_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: FavoriteParams = parse_params(params)?;
  let id = resolve_favorite_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let removed = ctx.state.mutate(|s| s.favorites.remove(&identity)).await;
  Ok(json!({
    "model_id": id,
    "removed": removed,
  }))
}

async fn favorite_list_handler(ctx: &MethodContext) -> Result<Value, ErrorObject> {
  let snapshot = ctx.state.snapshot().await;
  let entries: Vec<&FavoriteEntry> = snapshot.favorites.iter().collect();
  let body: Vec<Value> = entries.iter().map(|e| json!({"id": &e.id})).collect();
  Ok(json!({"favorites": body}))
}

/// Snapshot every persisted `last_params` entry. Used by the TUI to
/// pre-populate the launch picker with the most recent successful
/// launch params for the focused model (plan: "the picker is
/// pre-populated with last-params and named-preset values"). Keyed
/// by `model_path` so the TUI can look up without re-resolving
/// `ModelId`.
async fn last_params_list_handler(ctx: &MethodContext) -> Result<Value, ErrorObject> {
  let snapshot = ctx.state.snapshot().await;
  let rows: Vec<Value> = snapshot
    .last_params
    .iter()
    .map(|entry| {
      json!({
        "id": &entry.id,
        // A backend identity has no file; the launch path names its row.
        "model_path": entry
          .id
          .as_gguf()
          .map_or(&entry.params.model_path, |g| &g.path),
        "params": entry.params.to_wire(),
      })
    })
    .collect();
  Ok(json!({ "last_params": rows }))
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, ErrorObject> {
  let raw = params.unwrap_or(Value::Null);
  serde_json::from_value(raw)
    .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, format!("params parse error: {e}")))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::daemon::shutdown::ShutdownToken;
  use crate::discovery::ModelCatalog;

  fn ctx() -> MethodContext {
    MethodContext::new(ShutdownToken::new())
  }

  /// A header read for a model the catalog has no row for went onto a blocking
  /// thread, so cover that it still answers, and answers with the same tuple the
  /// sync resolver produced.
  #[tokio::test]
  async fn resolve_model_id_and_arch_reads_the_header_off_the_worker_thread() {
    let dir = crate::test_support::unique_temp_dir("ls-ipc", "header");
    let path = dir.join("model.gguf");
    std::fs::write(
      &path,
      crate::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .expect("write fixture gguf");

    let (id, arch, _, supported, _) = resolve_model_id_and_arch(&path).await.expect("header read");
    // The identity resolver canonicalizes on the way, which resolves the `/tmp`
    // symlink on macOS and drops the verbatim prefix Windows canonicalization
    // adds. Same helper, so the same answer on every platform.
    assert_eq!(
      id.path,
      crate::util::paths::canonicalize(&path).expect("canonical path")
    );
    assert_eq!(arch.as_deref(), Some("llama"));
    assert!(!supported.is_empty(), "routing tags come from the header");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test]
  async fn resolve_model_id_and_arch_maps_a_bad_path_to_invalid_params() {
    let err = resolve_model_id_and_arch(std::path::Path::new("/nope/definitely-missing.gguf"))
      .await
      .expect_err("a missing model has no identity");
    assert_eq!(
      err.code,
      ErrorCode::InvalidParams.as_i32(),
      "a bad path stays the caller's fault: {err:?}"
    );
  }

  /// A config-declared row keys its `last_params` by a backend identity, which
  /// has no GGUF path. `model_path` was null for it, so the TUI dropped the
  /// entry from its Recent section and from the launch picker's seed.
  #[tokio::test]
  async fn last_params_list_names_a_backend_identity_by_its_launch_path() {
    use crate::backend::identity::{BackendModelId, ModelIdentity};
    use crate::launch::mode::LaunchMode;
    use crate::launch::params::LaunchParams;
    let mut state = crate::daemon::state_store::DaemonState::default();
    state.upsert_last_params(
      ModelIdentity::Backend(BackendModelId {
        backend: "enginex".to_string(),
        name: "row".to_string(),
      }),
      LaunchParams::new(std::path::PathBuf::from("enginex://row"), LaunchMode::Chat),
      "enginex".to_string(),
    );
    let ctx = ctx().with_state(crate::daemon::context::PersistedState::new(state, None));
    let body = last_params_list_handler(&ctx).await.unwrap();
    assert_eq!(
      body["last_params"][0]["model_path"], "enginex://row",
      "{body}"
    );
  }

  #[tokio::test]
  async fn ping_returns_pong() {
    let req = Request::new(1, "ping", None);
    let resp = dispatch_request(&ctx(), req).await;
    assert_eq!(resp.result, Some(json!("pong")));
    assert!(resp.error.is_none());
  }

  #[tokio::test]
  async fn version_reports_package_metadata() {
    let resp = dispatch_request(&ctx(), Request::new(1, "version", None)).await;
    let body = resp.result.expect("version returns result");
    assert_eq!(body["name"], json!(env!("CARGO_PKG_NAME")));
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));
    assert!(body["pid"].is_number());
    assert!(body["uptime_seconds"].is_number());
    assert_eq!(body["connections"], json!(0));
  }

  #[tokio::test]
  async fn capabilities_reports_sorted_public_method_surface() {
    let resp = dispatch_request(&ctx(), Request::new(1, "capabilities", None)).await;
    let body = resp.result.expect("capabilities returns result");
    let methods = body["methods"].as_array().expect("methods array");
    let methods: Vec<&str> = methods
      .iter()
      .map(|v| v.as_str().expect("method names are strings"))
      .collect();

    let mut expected = PUBLIC_METHODS.to_vec();
    expected.sort();
    assert_eq!(methods, expected);
  }

  #[tokio::test]
  async fn shutdown_triggers_token() {
    let c = ctx();
    let token = c.shutdown.clone();
    let resp = dispatch_request(&c, Request::new(1, "shutdown", None)).await;
    assert!(resp.error.is_none());
    assert!(token.is_triggered(), "shutdown method must trip the token");
  }

  #[tokio::test]
  async fn unknown_method_returns_method_not_found() {
    let resp = dispatch_request(&ctx(), Request::new(1, "no-such", None)).await;
    let err = resp.error.expect("unknown method must error");
    assert_eq!(err.code, ErrorCode::MethodNotFound.as_i32());
    assert!(
      err.message.contains("no-such"),
      "error message should name the missing method, got: {}",
      err.message
    );
  }

  #[tokio::test]
  async fn list_models_returns_catalog_snapshot() {
    use std::path::PathBuf;

    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

    let catalog = ModelCatalog::new();
    catalog
      .upsert(DiscoveredModel {
        path: PathBuf::from("/m/seed.gguf"),
        parent: PathBuf::from("/m"),
        source: ModelSource::HuggingFace,
        metadata: Some(ModelMetadata {
          arch: Some("llama".to_string()),
          total_parameters: Some(7_000_000_000),
          parameter_label: Some("7B".to_string()),
          quant: Quant::Q4_K,
          quant_label: None,
          native_ctx: Some(8192),
          chat_template: None,
          tokenizer_kind: Some("llama".to_string()),
          reasoning_hint: false,
          mode_hint: ModeHint::Chat,
          weights_bytes: Some(4_000_000_000),
          lazy_tensor_bytes: Vec::new(),
          mtp: None,
        }),
        parse_error: None,
        split_siblings: Vec::new(),
        display_label: None,
        multimodal: None,
        supported_backends: Vec::new(),
        mtp_head: None,
      })
      .await;

    let c = MethodContext::with_catalog(ShutdownToken::new(), catalog);
    let resp = dispatch_request(&c, Request::new(1, "list_models", None)).await;
    assert!(resp.error.is_none());
    let body = resp.result.expect("list_models result body");
    let models = body
      .get("models")
      .and_then(Value::as_array)
      .expect("models array");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["path"], json!("/m/seed.gguf"));
    assert_eq!(models[0]["source"], json!("huggingface"));
    assert_eq!(models[0]["metadata"]["quant"], json!("Q4_K"));
  }

  #[tokio::test]
  async fn list_models_returns_empty_array_when_catalog_is_empty() {
    let resp = dispatch_request(&ctx(), Request::new(1, "list_models", None)).await;
    let body = resp.result.expect("result");
    assert_eq!(body["models"], json!([]));
  }

  #[tokio::test]
  async fn wrong_jsonrpc_version_returns_invalid_request() {
    let req = Request {
      jsonrpc: "1.0".into(),
      id: Some(json!(1)),
      method: "ping".into(),
      params: None,
    };
    let resp = dispatch_request(&ctx(), req).await;
    let err = resp.error.expect("wrong version must error");
    assert_eq!(err.code, ErrorCode::InvalidRequest.as_i32());
  }

  #[tokio::test]
  async fn start_model_without_launch_env_returns_internal_error() {
    let c = ctx();
    let req = Request::new(
      1,
      "start_model",
      Some(json!({"model_path": "/nonexistent.gguf"})),
    );
    let resp = dispatch_request(&c, req).await;
    let err = resp.error.expect("must error without launch env");
    assert_eq!(err.code, ErrorCode::InternalError.as_i32());
  }

  #[tokio::test]
  async fn favorite_add_with_unreadable_path_returns_invalid_params() {
    // No GGUF at this path → header-read fails → InvalidParams with
    // an actionable message naming the path.
    let c = ctx();
    let req = Request::new(
      1,
      "favorite_add",
      Some(json!({"model_path": "/no/such/path-9f3a.gguf"})),
    );
    let resp = dispatch_request(&c, req).await;
    let err = resp.error.expect("missing path must error");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("/no/such/path-9f3a.gguf"),
      "error message should name the missing path: {}",
      err.message
    );
  }

  #[tokio::test]
  async fn favorite_add_accepts_a_lemonade_registry_path() {
    // A fileless `lemonade://` path has no GGUF to hash — favoriting must still
    // work (via the synthetic id), keyed on the path the catalog row uses so
    // the TUI `★` resolves.
    let c = ctx();
    let resp = dispatch_request(
      &c,
      Request::new(
        1,
        "favorite_add",
        Some(json!({"model_path": "lemonade://qwen3.5-4b-FLM"})),
      ),
    )
    .await;
    let body = resp
      .result
      .expect("lemonade favorite must succeed, not error");
    assert_eq!(body["added"], json!(true));
    assert_eq!(body["model_id"]["path"], "lemonade://qwen3.5-4b-FLM");
    // favorite_list surfaces it with an `id.path` the TUI matches to the catalog.
    let list = dispatch_request(&c, Request::new(2, "favorite_list", None))
      .await
      .result
      .expect("favorite_list");
    assert_eq!(
      list["favorites"][0]["id"]["path"],
      "lemonade://qwen3.5-4b-FLM"
    );
  }

  #[tokio::test]
  async fn favorite_list_returns_empty_array_when_state_is_empty() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "favorite_list", None)).await;
    let body = resp.result.expect("favorite_list result body");
    assert_eq!(body["favorites"], json!([]));
  }

  #[tokio::test]
  async fn stop_external_refuses_pid_not_in_external_snapshot() {
    let c = ctx();
    let resp = dispatch_request(
      &c,
      Request::new(1, "stop_external", Some(json!({"pid": 999_999_999u32}))),
    )
    .await;
    let err = resp
      .error
      .expect("unknown external PID must reject — safety guard");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("999999999"),
      "error must name the rejected PID, got: {}",
      err.message
    );
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
    crate::test_support::running_row(&path.to_string_lossy())
      .identity(id)
      .pid(0)
      .port(port)
      .launch_id(launch_id)
      .params(LaunchParams::new(path, LaunchMode::Chat))
      .resolved_backend(&resolved_backend)
      .build()
  }

  #[tokio::test]
  async fn stop_model_of_delegated_row_clears_snapshot_even_without_umbrella() {
    // `stop_model` on a delegated row routes through `backend.stop`, whose
    // delegated branch unloads from the umbrella and drops the snapshot. The
    // umbrella is gone but the snapshot lingers (e.g. it crashed): the row must
    // still be clearable — the unload is best-effort, the bookkeeping removal is
    // the contract.
    let c = ctx();
    c.state
      .mutate(|s| {
        s.running
          .push(lemonade_running_snapshot("Qwen-X", 13305, "L1"))
      })
      .await;
    let resp = dispatch_request(
      &c,
      Request::new(1, "stop_model", Some(json!({"launch_id": "L1"}))),
    )
    .await;
    let body = resp.result.expect("delegated stop must succeed");
    assert_eq!(body["state"]["state"], json!("stopped"));
    let still_there = c
      .state
      .snapshot()
      .await
      .running
      .iter()
      .any(|r| r.delegated_backend_id().is_some());
    assert!(!still_there, "snapshot must be dropped");
    // Second stop: the row is unknown now — the snapshot is gone, so it
    // falls through to the supervisor path and errors like a bogus id.
    let second = dispatch_request(
      &c,
      Request::new(2, "stop_model", Some(json!({"launch_id": "L1"}))),
    )
    .await;
    let err = second.error.expect("double-stop must error");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(err.message.contains("L1"));
  }

  /// The residency fields are tri-state on the wire, because `Ctrl+P` re-saves a
  /// preset from a launch capture that knows nothing about them: absent inherits
  /// what the entry already pins, an explicit `null` / `false` clears it, a
  /// number or `true` pins it.
  #[tokio::test]
  async fn presets_save_inherits_residency_unless_told_otherwise() {
    // A discovered model, because a preload pin is only inheritable from a key
    // that names this model, and that is a catalog question.
    let c = arch_key_ctx(&["/m/a.gguf"]).await;
    let save = |params: Value, id: i64| {
      let c = &c;
      async move {
        dispatch_request(c, Request::new(id, "presets_save", Some(params))).await;
        let block = c.presets.snapshot().await;
        let entry = block
          .values()
          .filter_map(|b| b.entries.get("p"))
          .next()
          .cloned()
          .expect("saved entry");
        (entry.idle_ttl_secs, entry.preload)
      }
    };

    let pinned = save(
      json!({"model_path": "/m/a.gguf", "name": "p", "idle_ttl_secs": 60, "preload": true}),
      1,
    )
    .await;
    assert_eq!(pinned, (Some(60), true));

    let inherited = save(json!({"model_path": "/m/a.gguf", "name": "p"}), 2).await;
    assert_eq!(
      inherited,
      (Some(60), true),
      "a params-only re-save dropped the residency policy"
    );

    let cleared = save(
      json!({"model_path": "/m/a.gguf", "name": "p", "idle_ttl_secs": null, "preload": false}),
      3,
    )
    .await;
    assert_eq!(cleared, (None, false));
  }

  /// A re-save inherits the TTL pin from the entry the name resolves to, which
  /// may be an arch entry the save is about to shadow. It does *not* inherit a
  /// family key's `preload: true`: boot refuses to act on that pin, and copying
  /// it onto one model's own key would boot-load a model nobody asked to preload,
  /// permanently.
  #[tokio::test]
  async fn presets_save_inherits_a_ttl_from_the_entry_it_shadows() {
    let c = arch_key_ctx(&["/m/a.gguf", "/m/b.gguf"]).await;
    save_preset(
      &c,
      json!({"model_path": "/m/a.gguf", "name": "p", "ctx": 2048}),
    )
    .await;

    let saved = per_model_entry(&c).await;
    assert_eq!(
      saved.idle_ttl_secs,
      Some(60),
      "the arch entry's TTL pin was dropped by the shadowing save"
    );
    assert!(
      !saved.preload,
      "a family key's preload pin leaked onto a single-model key"
    );
  }

  /// The same arch key that scopes exactly one model is a per-model decision in
  /// all but spelling, so its preload pin does carry over.
  #[tokio::test]
  async fn presets_save_inherits_preload_when_the_key_names_one_model() {
    let c = arch_key_ctx(&["/m/a.gguf"]).await;
    save_preset(
      &c,
      json!({"model_path": "/m/a.gguf", "name": "p", "ctx": 2048}),
    )
    .await;
    assert!(
      per_model_entry(&c).await.preload,
      "a key naming only this model should pass its preload pin on"
    );
  }

  /// A catalog of qwen3 models plus an arch-keyed preset `p` pinning residency.
  async fn arch_key_ctx(paths: &[&str]) -> MethodContext {
    use crate::config::{ConfigPresetBlock, PresetBody};
    use crate::daemon::preset_store::ConfigPresetStore;
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    let catalog = ModelCatalog::new();
    for path in paths {
      catalog
        .upsert(DiscoveredModel {
          path: PathBuf::from(path),
          parent: PathBuf::from("/m"),
          source: ModelSource::UserPath,
          metadata: Some(ModelMetadata {
            arch: Some("qwen3".to_string()),
            total_parameters: Some(7_000_000_000),
            parameter_label: Some("7B".to_string()),
            quant: Quant::Q4_K,
            quant_label: None,
            native_ctx: Some(8192),
            chat_template: None,
            tokenizer_kind: Some("llama".to_string()),
            reasoning_hint: false,
            mode_hint: ModeHint::Chat,
            weights_bytes: Some(4_000_000_000),
            lazy_tensor_bytes: Vec::new(),
            mtp: None,
          }),
          parse_error: None,
          split_siblings: Vec::new(),
          display_label: None,
          multimodal: None,
          supported_backends: Vec::new(),
          mtp_head: None,
        })
        .await;
    }
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_presets(ConfigPresetStore::new(
      BTreeMap::from([(
        "qwen3".to_string(),
        ConfigPresetBlock {
          default: None,
          entries: BTreeMap::from([(
            "p".to_string(),
            PresetBody {
              idle_ttl_secs: Some(60),
              preload: true,
              ..Default::default()
            },
          )]),
        },
      )]),
      None,
    ))
  }

  async fn save_preset(c: &MethodContext, params: Value) {
    dispatch_request(c, Request::new(1, "presets_save", Some(params))).await;
  }

  /// The `p` entry written under the model's own key, not the arch one.
  async fn per_model_entry(c: &MethodContext) -> crate::config::PresetBody {
    c.presets
      .snapshot()
      .await
      .iter()
      .filter(|(key, _)| key.as_str() != "qwen3")
      .find_map(|(_, block)| block.entries.get("p"))
      .cloned()
      .expect("a per-model entry was written")
  }

  #[tokio::test]
  async fn presets_save_with_empty_name_rejects() {
    let c = ctx();
    let req = Request::new(
      1,
      "presets_save",
      Some(json!({"model_path": "/m/a.gguf", "name": ""})),
    );
    let resp = dispatch_request(&c, req).await;
    let err = resp.error.expect("empty name must error");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.to_lowercase().contains("preset name"),
      "got: {}",
      err.message
    );
  }

  #[test]
  fn preset_hint_reports_count_and_default() {
    use crate::config::{ConfigPresetBlock, PresetBody};
    use std::collections::BTreeMap;
    let rows = vec![CatalogRow::for_resolution(
      "/m/a.gguf".into(),
      None,
      Some("qwen2".into()),
    )];
    let mut entries = BTreeMap::new();
    for n in ["p1", "p2", "p3"] {
      entries.insert(n.to_string(), PresetBody::default());
    }
    let mut store = BTreeMap::new();
    store.insert(
      "a.gguf".to_string(),
      ConfigPresetBlock {
        default: Some("p2".into()),
        entries,
      },
    );
    let (count, default) = preset_hint("/m/a.gguf", &rows, &store);
    assert_eq!(count, 3);
    assert_eq!(default.as_deref(), Some("p2"));
    // A model with no presets reports zero / none.
    let (zero, none) = preset_hint("/m/other.gguf", &rows, &store);
    assert_eq!(zero, 0);
    assert!(none.is_none());
  }
}
