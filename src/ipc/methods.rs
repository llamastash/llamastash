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
      ctx.shutdown.trigger();
      Response::ok(id, json!({"shutdown": "scheduled"}))
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
      use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
      let refresh = ProcessRefreshKind::everything();
      let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
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
  // Non-fatal advisories (dropped knobs, deepseek4 KV-blind note, ssd_streaming
  // bypass). Omitted when empty so the response stays byte-stable for launches
  // that raise none (every llama.cpp / Lemonade launch today).
  if !started.warnings.is_empty() {
    resp["warnings"] = json!(started.warnings);
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
pub(crate) fn resolve_model_id_and_arch(
  path: &std::path::Path,
) -> Result<ResolvedModelInfo, ErrorObject> {
  let r = crate::backend::resolve_identity_for_path(path, None)
    .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, e.to_string()))?;
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
      crate::util::paths::path_basename(model_path),
      resolve_model_id_and_arch(model_path)
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
  #[serde(default)]
  mtp: crate::launch::params::MtpEnable,
  #[serde(default)]
  mtp_draft_n: Option<u32>,
  /// Backend this preset pins. Launch *identity*, not a knob — it decides
  /// which backend's knobs apply at all, so it cannot be backend-declared.
  #[serde(default)]
  backend: Option<String>,
  /// Server (build/binary) this preset pins. Identity, like `backend`.
  #[serde(default)]
  server: Option<String>,
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
  // KD2 scoped MTP to "launch / TUI / preset"; the save path used to drop it,
  // so a preset could never pin speculation on or off.
  lp.mtp = parsed.mtp;
  lp.mtp_draft_n = parsed.mtp_draft_n;
  // Identity: which backend and which of its builds this preset pins. Stored
  // verbatim so a saved preset can reproduce the run it was captured from.
  lp.backend = parsed
    .backend
    .as_deref()
    .map(crate::launch::params::BackendChoice::from_id)
    .unwrap_or_default();
  lp.server = parsed.server.clone();
  lp.extras = parsed.extras.into_iter().map(OsString::from).collect();
  let body = preset_body_from_launch_params(&lp);

  let (key, arch, rows) = model_key_arch_rows(ctx, &parsed.model_path).await;
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
  let path_str = parsed.model_path.display().to_string();
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
    .unwrap_or_else(|| crate::util::paths::path_basename(std::path::Path::new(model_path)));
  let arch = row.and_then(|r| r.arch.clone());
  let eff = effective_presets(&name, model_path, arch.as_deref(), store, rows);
  (eff.presets.len() as u32, eff.default)
}

fn preset_row(p: &NamedPreset, is_default: bool) -> Value {
  json!({
    "name": p.name,
    "params": launch_params_row(&p.params),
    // Presets live in config.yaml now; the provenance is constant but
    // surfaced so agents can distinguish a config preset from any future
    // source without re-deriving it.
    "source": "config",
    "is_default": is_default,
  })
}

fn launch_params_row(p: &LaunchParams) -> Value {
  let mut row = json!({
    "model_path": p.model_path,
    "mode": p.mode.label(),
    "ctx": p.ctx,
    "port": p.port,
    "reasoning": p.reasoning,
    "knobs": &p.knobs,
    "extras": p.extras.iter().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>(),
    // Chosen server build (`null` when none) so the TUI picker's Server row
    // reopens on the last-used build.
    "server": p.server,
  });
  // Pinned backend, omitted at its `Auto` default so non-pinned rows stay
  // byte-stable. Same reason `mtp` below is additive: `start --preset` reads
  // this back to rebuild the preset's launch params, and leaving it out
  // silently dropped every `backend:` a preset declared.
  if let Some(id) = p.backend.explicit_id() {
    row["backend"] = Value::String(id.to_string());
  }
  // MTP intent, additive like the native knobs: omitted at its `Auto` default so
  // non-MTP rows stay byte-stable. The CLI reads this back off `presets_show` to
  // rebuild a preset's launch params, so leaving it out silently disarmed every
  // `mtp:` a preset declared.
  if !p.mtp.is_auto() {
    row["mtp"] = Value::String(p.mtp.label().to_string());
  }
  if let Some(n) = p.mtp_draft_n {
    row["mtp_draft_n"] = Value::from(n);
  }
  row
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
        "model_path": entry.id.as_gguf().map(|g| &g.path),
        "params": launch_params_row(&entry.params),
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

  /// A pinned backend has to survive onto the wire.
  ///
  /// It did not: the row carried `server` but never `backend`, so a preset
  /// declaring one could not be read back by anything, and `start --preset`
  /// launched on the default engine while reporting the preset's name.
  #[test]
  fn launch_params_row_carries_a_pinned_backend_and_omits_an_auto_one() {
    use crate::launch::mode::LaunchMode;
    use crate::launch::params::BackendChoice;
    use std::path::PathBuf;

    // Auto is the default, so the key stays absent and rows stay byte-stable.
    let auto = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    assert_eq!(auto.backend, BackendChoice::Auto);
    assert!(
      launch_params_row(&auto).get("backend").is_none(),
      "an auto backend must not widen the row"
    );

    // Pinned rides the row as the bare id, matching `server` beside it.
    let mut pinned = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    pinned.backend = BackendChoice::from_id(crate::backend::DEFAULT_BACKEND_ID);
    pinned.server = Some("build-two".into());
    let row = launch_params_row(&pinned);
    assert_eq!(
      row["backend"].as_str(),
      Some(crate::backend::DEFAULT_BACKEND_ID)
    );
    assert_eq!(row["server"].as_str(), Some("build-two"));
  }

  #[test]
  fn launch_params_row_omits_empty_backend_knobs_and_emits_when_set() {
    use crate::launch::mode::LaunchMode;
    use std::path::PathBuf;
    // Empty → the key is absent (byte-stable for llama.cpp / Lemonade).
    let lp = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    let row = launch_params_row(&lp);
    assert!(
      row["knobs"].as_object().is_some_and(|m| m.is_empty()),
      "an empty knob set renders as an empty map: {row}"
    );
    // Set → the key carries the map, with the same shape the TUI parses back.
    let mut lp2 = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    lp2.knobs.set_by_name("kv-disk-dir", "/tmp/kv");
    let row2 = launch_params_row(&lp2);
    assert_eq!(
      row2["knobs"]["kv-disk-dir"],
      serde_json::json!("/tmp/kv"),
      "a backend's own knob round-trips into the row like any other"
    );
  }

  #[test]
  fn launch_params_row_carries_server_pick_for_the_tui() {
    // Regression: the server pick is persisted in `last_params` but was
    // dropped from the `last_params_list` projection, so the TUI picker's
    // Server row never reopened on the last-used build.
    use crate::launch::mode::LaunchMode;
    use std::path::PathBuf;
    let lp = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    assert_eq!(
      launch_params_row(&lp)["server"],
      serde_json::Value::Null,
      "no pick → null"
    );
    let mut lp2 = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    lp2.server = Some("llamacpp-vulkan".into());
    assert_eq!(
      launch_params_row(&lp2)["server"],
      serde_json::json!("llamacpp-vulkan"),
      "the picked server build round-trips into the row for the TUI"
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
    crate::daemon::state_store::RunningSnapshot {
      id,
      pid: 0,
      port,
      started_at: 0,
      launch_id: Some(crate::daemon::registry::LaunchId(launch_id.to_string())),
      resolved_backend,
      params: LaunchParams::new(path, LaunchMode::Chat),
      actuals: Default::default(),
    }
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
