//! Method dispatch for the daemon's IPC layer.
//!
//! Unit 2 shipped `ping` / `version` / `shutdown` and `list_models`.
//! Unit 5 added the supervisor-touching methods: `status`,
//! `stop_model`, `stop_all`, `logs_tail`, `start_model`, plus the
//! state-store CRUD surfaces `presets_*` and `favorite_*`. Keeping
//! the registry as a `match` (rather than a `HashMap<&str, fn>`)
//! avoids dynamic-dispatch plumbing for what is, in practice, a
//! small fixed set of methods.

use std::{
  collections::BTreeMap,
  ffi::OsString,
  path::PathBuf,
  sync::{atomic::Ordering, Arc},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use super::protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
use crate::config::loader::PortRange;
use crate::daemon::orphans::ExternalProcess;
use crate::daemon::probe::ProbeOptions;
use crate::daemon::registry::{LaunchId, SupervisorRegistry};
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::{self, DaemonState, RunningSnapshot};
use crate::daemon::supervisor::{
  spawn as supervisor_spawn, ManagedModel, ManagedSpawn, ManagedState,
};
use crate::discovery::ModelCatalog;
use crate::gguf::header::{read_path as read_gguf_header, HeaderReadOptions};
use crate::gguf::identity::{compute as compute_model_id, ModelId};
use crate::gpu::GpuInfo;
use crate::launch::favorites::FavoriteEntry;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;
use crate::launch::presets::{NamedPreset, Presets};

/// Shared state that the daemon hands to each request handler. Cheap to
/// clone (`Arc` inside).
#[derive(Clone)]
pub struct MethodContext {
  /// Wall-clock instant the daemon began listening. `version` reports
  /// uptime relative to this.
  pub started_at: Instant,
  /// Triggered by the `shutdown` method or by SIGINT/SIGTERM.
  pub shutdown: ShutdownToken,
  /// Live connection count. Maintained by the accept loop; surfaced via
  /// `version` so `daemon status` can show it without a separate method.
  pub active_connections: Arc<std::sync::atomic::AtomicUsize>,
  /// Catalog of currently-discovered models. Populated by the daemon's
  /// discovery task; read by the `list_models` handler. Cheap to clone
  /// (`Arc<RwLock<…>>`).
  pub catalog: ModelCatalog,
  /// Active supervisor instances keyed by `LaunchId`. Populated by
  /// `start_model` and consumed by `status`, `stop_model`,
  /// `logs_tail`. Empty in tests that only exercise the discovery
  /// surface.
  pub supervisors: SupervisorRegistry,
  /// Snapshot of `gpu::probe()` taken at daemon start. `status`
  /// reports it alongside per-model resources so the UI can render
  /// a GPU panel.
  pub gpu: Arc<GpuInfo>,
  /// Persisted favorites / presets / last_params / running snapshots.
  /// `start_model`, `presets_*`, and `favorite_*` mutate it and
  /// flush to `state.json` after each change.
  pub state: PersistedState,
  /// Inputs the supervisor needs at launch time — binary path, port
  /// range, log directory, probe tuning. Optional because catalog-only
  /// IPC tests don't need to launch anything.
  pub launch: Option<LaunchEnv>,
  /// Snapshot of `llama-server` processes the daemon does *not*
  /// own. Populated by the orphan sweep at startup so `status`
  /// surfaces them read-only (plan: External rows). Wrapped in
  /// `RwLock` so a periodic re-sweep can refresh the slot without
  /// rebuilding the context.
  pub external: Arc<RwLock<Vec<ExternalProcess>>>,
  /// Hook for the accept-loop's peercred decision. Production uses
  /// [`crate::daemon::peercred::is_authorized_peer`]; tests can
  /// inject `|_| false` to drive the rejection branch.
  pub peer_authorizer: Arc<dyn Fn(crate::daemon::peercred::PeerCred) -> bool + Send + Sync>,
}

/// Wrapper around the in-memory `DaemonState` plus the directory
/// `state.json` lives in. Mutations go through the wrapped
/// `Mutex`; flushes are best-effort and just log on failure so a
/// transient I/O error doesn't take the daemon down.
#[derive(Clone)]
pub struct PersistedState {
  state: Arc<Mutex<DaemonState>>,
  /// `None` disables persistence; mutations stay in-memory. Tests
  /// that don't care about durability use this mode.
  state_dir: Option<PathBuf>,
}

impl PersistedState {
  pub fn new(state: DaemonState, state_dir: Option<PathBuf>) -> Self {
    Self {
      state: Arc::new(Mutex::new(state)),
      state_dir,
    }
  }

  pub fn ephemeral() -> Self {
    Self::new(DaemonState::default(), None)
  }

  /// Snapshot — cheap clone of the inner state.
  pub async fn snapshot(&self) -> DaemonState {
    self.state.lock().await.clone()
  }

  /// Mutate under the lock and flush. The closure receives the
  /// state mutably and returns a value the caller cares about.
  /// `flush_after` short-circuits the write when persistence is
  /// disabled.
  pub async fn mutate<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&mut DaemonState) -> R,
  {
    let mut guard = self.state.lock().await;
    let out = f(&mut guard);
    if let Some(dir) = self.state_dir.as_ref() {
      if let Err(e) = state_store::save(dir, &guard) {
        log::warn!("state-store: failed to persist after mutation: {e}");
      }
    }
    out
  }
}

/// Resources the supervisor needs to actually launch a child.
/// Centralised here so `start_model` doesn't have to hand-roll five
/// optional fields on `MethodContext`.
#[derive(Clone)]
pub struct LaunchEnv {
  pub binary: PathBuf,
  pub port_range: PortRange,
  pub log_dir: PathBuf,
  pub probe: ProbeOptions,
}

impl MethodContext {
  pub fn new(shutdown: ShutdownToken) -> Self {
    Self::with_catalog(shutdown, ModelCatalog::new())
  }

  /// Build a context with an externally-owned catalog. The daemon's
  /// `run_foreground` uses this to thread the same catalog into the
  /// discovery task and the dispatcher.
  pub fn with_catalog(shutdown: ShutdownToken, catalog: ModelCatalog) -> Self {
    Self {
      started_at: Instant::now(),
      shutdown,
      active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
      catalog,
      supervisors: SupervisorRegistry::new(),
      gpu: Arc::new(GpuInfo::CpuOnly),
      state: PersistedState::ephemeral(),
      launch: None,
      external: Arc::new(RwLock::new(Vec::new())),
      peer_authorizer: Arc::new(crate::daemon::peercred::is_authorized_peer),
    }
  }

  /// Builder helper: override the peercred decision. Test-only.
  pub fn with_peer_authorizer<F>(mut self, auth: F) -> Self
  where
    F: Fn(crate::daemon::peercred::PeerCred) -> bool + Send + Sync + 'static,
  {
    self.peer_authorizer = Arc::new(auth);
    self
  }

  /// Builder helper: seed the external (unmanaged `llama-server`)
  /// process snapshot. Production wiring populates it from the
  /// startup orphan sweep.
  pub fn with_external(self, external: Vec<ExternalProcess>) -> Self {
    Self {
      external: Arc::new(RwLock::new(external)),
      ..self
    }
  }

  /// Builder helper: attach a supervisor registry. Used by
  /// `run_foreground` so the dispatcher and the daemon share one
  /// supervisor map.
  pub fn with_supervisors(mut self, supervisors: SupervisorRegistry) -> Self {
    self.supervisors = supervisors;
    self
  }

  /// Builder helper: attach a probed GPU info snapshot.
  pub fn with_gpu(mut self, gpu: GpuInfo) -> Self {
    self.gpu = Arc::new(gpu);
    self
  }

  /// Builder helper: attach the persisted state-store handle.
  pub fn with_state(mut self, state: PersistedState) -> Self {
    self.state = state;
    self
  }

  /// Builder helper: attach the launch environment. `start_model`
  /// requires this to be set; without it the handler returns an
  /// `InvalidRequest`.
  pub fn with_launch_env(mut self, env: LaunchEnv) -> Self {
    self.launch = Some(env);
    self
  }
}

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
          "pid": std::process::id(),
          "uptime_seconds": uptime_secs,
          "connections": connections,
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
      let body = ctx.catalog.to_list_response().await;
      Response::ok(id, body)
    }
    "status" => Response::ok(id, status_response(ctx).await),
    "start_model" => match start_model_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "stop_model" => match stop_model_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "stop_all" => match stop_all_handler(ctx).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "stop_external" => match stop_external_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "logs_tail" => match logs_tail_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "presets_list" => match presets_list_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "presets_save" => match presets_save_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "presets_delete" => match presets_delete_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "presets_show" => match presets_show_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "favorite_add" => match favorite_add_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "favorite_remove" => match favorite_remove_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "favorite_list" => match favorite_list_handler(ctx).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "last_params_list" => match last_params_list_handler(ctx).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    other => Response::err(
      id,
      ErrorObject::new(
        ErrorCode::MethodNotFound,
        format!("unknown method: {other}"),
      ),
    ),
  }
}

/// Snapshot every active managed model plus the daemon's GPU info.
/// `status` is read-only; never triggers any state-machine transitions.
async fn status_response(ctx: &MethodContext) -> Value {
  let snap = ctx.supervisors.snapshot().await;
  let mut models: Vec<Value> = Vec::with_capacity(snap.len());
  for (launch_id, model) in snap {
    let state = model.state().await;
    let pid = model.pid().await;
    let ready_at = model.ready_at().await;
    models.push(json!({
      "launch_id": launch_id,
      "id": model.id(),
      "port": model.port(),
      "mode": model.mode().label(),
      "pid": pid,
      "ready_at": ready_at,
      "state": state,
    }));
  }
  // External — read-only rows for `llama-server` processes the
  // daemon doesn't own. Populated by the startup orphan sweep;
  // mirrors the plan's "External read-only" surface (plan: list-
  // pane glyph `⇪`). Stable shape: `{pid, cmdline, model_path}`.
  let external_snapshot = ctx.external.read().await.clone();
  let external: Vec<Value> = external_snapshot
    .iter()
    .map(|e| {
      json!({
        "pid": e.pid,
        "cmdline": e.cmdline,
        "model_path": e.model_path,
      })
    })
    .collect();
  json!({
    "models": models,
    "external": external,
    "gpu": ctx.gpu.as_ref(),
    "daemon": {
      "pid": std::process::id(),
      "uptime_seconds": ctx.started_at.elapsed().as_secs(),
      "active_connections": ctx.active_connections.load(Ordering::Relaxed),
    },
  })
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

async fn stop_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StopParams = parse_params(params)?;
  let model = ctx
    .supervisors
    .get(&parsed.launch_id)
    .await
    .ok_or_else(|| {
      ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("unknown launch_id: {}", parsed.launch_id.as_str()),
      )
    })?;
  let stopped_port = model.port();
  let final_state = model.stop(Duration::from_secs(parsed.grace_secs)).await;
  ctx.supervisors.remove(&parsed.launch_id).await;
  // Drop the running snapshot keyed by `(id, port)` so a second
  // launch of the same GGUF on a different port keeps its row.
  let stopped_id = model.id().clone();
  ctx
    .state
    .mutate(|s| s.running.retain(|r| !(r.id == stopped_id && r.port == stopped_port)))
    .await;
  Ok(json!({
    "launch_id": parsed.launch_id,
    "state": final_state,
  }))
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
  let pid_i = parsed.pid as i32;

  // Helper: returns Some(true) if alive AND start_time matches, Some(false)
  // if alive but pid has been reused, None if dead. We sample via
  // `sysinfo` rather than `kill(pid, 0)` so we can compare start_time
  // — the cheap liveness check alone can't distinguish recycle.
  fn live_and_same(pid: u32, expected_start: u64) -> Option<bool> {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
    let mut sys = System::new_with_specifics(
      RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_process(Pid::from_u32(pid));
    sys.process(Pid::from_u32(pid))
      .map(|p| p.start_time() == expected_start)
  }
  match live_and_same(parsed.pid, recorded_start_time) {
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
  // SIGTERM first — give the process time to exit cleanly.
  unsafe {
    libc::kill(pid_i, libc::SIGTERM);
  }
  let grace = Duration::from_secs(parsed.grace_secs);
  let mut elapsed = Duration::ZERO;
  let step = Duration::from_millis(100);
  while elapsed < grace {
    match live_and_same(parsed.pid, recorded_start_time) {
      Some(true) => {}
      _ => break, // gone, or pid was recycled — either way stop signalling
    }
    tokio::time::sleep(step).await;
    elapsed += step;
  }
  // Final check; SIGKILL only if same process is still up.
  let mut sent_kill = false;
  if matches!(
    live_and_same(parsed.pid, recorded_start_time),
    Some(true)
  ) {
    unsafe {
      libc::kill(pid_i, libc::SIGKILL);
    }
    sent_kill = true;
  }
  ctx.external.write().await.retain(|e| e.pid != parsed.pid);
  Ok(json!({
    "pid": parsed.pid,
    "killed_with_sigkill": sent_kill,
  }))
}

async fn stop_all_handler(ctx: &MethodContext) -> Result<Value, ErrorObject> {
  let snap = ctx.supervisors.snapshot().await;
  // Run the per-launch stops concurrently. Sequential iteration on
  // the original implementation serialised N × grace_secs, which
  // could blow the default IPC client timeout (5 s) for 2+ stuck
  // launches. `join_all` brings the wall-clock back to the slowest
  // stop, not the sum.
  use futures::future::join_all;
  let grace = Duration::from_secs(default_grace_secs());
  let stops = snap.into_iter().map(|(launch_id, model)| async move {
    let final_state = model.stop(grace).await;
    let model_id = model.id().clone();
    let port = model.port();
    (launch_id, model_id, port, final_state)
  });
  let outcomes = join_all(stops).await;

  let mut stopped: Vec<Value> = Vec::with_capacity(outcomes.len());
  let mut stopped_keys: Vec<(ModelId, u16)> = Vec::with_capacity(outcomes.len());
  for (launch_id, model_id, port, final_state) in outcomes {
    ctx.supervisors.remove(&launch_id).await;
    stopped_keys.push((model_id, port));
    stopped.push(json!({"launch_id": launch_id, "state": final_state}));
  }
  if !stopped_keys.is_empty() {
    ctx
      .state
      .mutate(|s| {
        s.running
          .retain(|r| !stopped_keys.iter().any(|(id, port)| *id == r.id && *port == r.port))
      })
      .await;
  }
  Ok(json!({"stopped": stopped}))
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
  let model = ctx
    .supervisors
    .get(&parsed.launch_id)
    .await
    .ok_or_else(|| {
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

#[derive(Deserialize)]
struct StartParams {
  /// Absolute path to the GGUF the user wants to launch. We compute
  /// the canonical `ModelId` by reading its header on the daemon
  /// side rather than trusting the caller — keeps the surface
  /// minimal for CLI/TUI clients.
  model_path: PathBuf,
  #[serde(default)]
  mode: Option<LaunchModeWire>,
  #[serde(default)]
  ctx: Option<u32>,
  #[serde(default)]
  port: Option<u16>,
  #[serde(default)]
  reasoning: Option<bool>,
  /// Free-form passthrough flags appended after the bundled set.
  #[serde(default)]
  advanced: Vec<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum LaunchModeWire {
  Chat,
  Embedding,
  Rerank,
}

impl From<LaunchModeWire> for LaunchMode {
  fn from(m: LaunchModeWire) -> Self {
    match m {
      LaunchModeWire::Chat => LaunchMode::Chat,
      LaunchModeWire::Embedding => LaunchMode::Embedding,
      LaunchModeWire::Rerank => LaunchMode::Rerank,
    }
  }
}

/// Upper bound on `ctx` (token-window) advertised on the IPC. The TUI
/// picker caps at 131_072 but CLI + direct JSON-RPC callers bypass
/// that; this stops a buggy or malicious request from sending
/// `--ctx u32::MAX` straight to llama-server.
const MAX_CTX_TOKENS: u32 = 1_048_576;

async fn start_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StartParams = parse_params(params)?;
  let env = ctx.launch.as_ref().ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InternalError,
      "daemon launch environment not configured (binary / port range / log dir missing)",
    )
  })?;

  // Resolve canonical ModelId from the GGUF header.
  let id = resolve_model_id(&parsed.model_path)?;

  // Mode resolution: explicit override > catalog hint > default to chat.
  // The CLI surface refuses to default silently when discovery says
  // "Unknown" (cli_args.rs::StartArgs::mode comment), but the daemon
  // is one layer down; a missing override here means the caller has
  // already accepted the default.
  let mode = parsed
    .mode
    .map(LaunchMode::from)
    .unwrap_or(LaunchMode::Chat);

  // Reject pinned port values that would corrupt our internal state
  // or require root: 0 means "OS pick" (llama-server would pick a
  // port we never track), <1024 needs root and is almost certainly
  // a typo / hostile.
  if let Some(p) = parsed.port {
    if p == 0 || p < 1024 {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("port {p} is not in the allowed range (>= 1024, not 0)"),
      ));
    }
  }
  // Validate ctx token-window bound.
  if let Some(c) = parsed.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }

  // Port allocation — race-safe. `reserve_port` is a CAS across
  // `collect_in_use_ports → allocate → reserve` so two concurrent
  // `start_model` calls cannot both walk away with the same port.
  // We must collect the live in-use list before taking the
  // reservation mutex, since `collect_in_use_ports` itself awaits
  // supervisor read locks.
  let live_in_use = collect_in_use_ports(ctx).await;
  let port = ctx
    .supervisors
    .reserve_port(parsed.port, &live_in_use, &env.port_range)
    .await
    .map_err(|e| {
      ErrorObject::new(ErrorCode::InternalError, format!("port allocation failed: {e}"))
    })?;

  // Compose LaunchParams.
  let mut launch_params = LaunchParams::new(parsed.model_path.clone(), mode);
  launch_params.ctx = parsed.ctx;
  launch_params.port = Some(port);
  launch_params.reasoning = parsed.reasoning.unwrap_or(false);
  launch_params.advanced = parsed.advanced.into_iter().map(OsString::from).collect();

  // Reject loopback-breaking / auth-bypass advanced flags before
  // spawn. `compose` strips defensively too, but failing fast here
  // gives callers a clear error instead of a silently-different argv.
  let banned = crate::launch::params::forbidden_in_advanced(&launch_params.advanced);
  if !banned.is_empty() {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!(
        "advanced flags refused (loopback / auth contract): {}",
        banned.join(", ")
      ),
    ));
  }

  // Per-launch log file under cache_dir/logs/<short-id>-<ts>.log.
  let log_path = build_log_path(&env.log_dir, &id);

  let spawn_result = supervisor_spawn(ManagedSpawn {
    id: id.clone(),
    binary: env.binary.clone(),
    params: launch_params.clone(),
    port,
    mode,
    log_path: log_path.clone(),
    probe: env.probe,
  })
  .await;
  let model = match spawn_result {
    Ok(m) => m,
    Err(e) => {
      // Free the reserved port so a retry can re-use it.
      ctx.supervisors.release_reserved_port(port).await;
      return Err(ErrorObject::new(
        ErrorCode::InternalError,
        format!("supervisor spawn: {e}"),
      ));
    }
  };

  let launch_id = ctx.supervisors.next_id();
  ctx
    .supervisors
    .insert(launch_id.clone(), model.clone())
    .await;
  // Live supervisor now owns the port; drop the in-flight reservation.
  ctx.supervisors.release_reserved_port(port).await;

  // Persist running snapshot. Retain by `(id, port)` so the same
  // GGUF launched twice against different ports persists both
  // snapshots — the orphan sweep can then re-adopt either one on
  // daemon restart instead of silently dropping the older.
  let pid = model.pid().await.unwrap_or(0) as i32;
  let started_at = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  ctx
    .state
    .mutate(|s| {
      s.running.retain(|r| !(r.id == id && r.port == port));
      s.running.push(RunningSnapshot {
        id: id.clone(),
        pid,
        port,
        started_at,
        params: launch_params.clone(),
      });
    })
    .await;

  // Background task: when the supervisor reaches Ready, stamp
  // last_params (per the plan — only updated on a *successful*
  // Loading → Ready transition). We poll because ManagedModel
  // doesn't expose a transition channel yet.
  spawn_last_params_recorder(ctx.state.clone(), model.clone(), id.clone(), launch_params);

  Ok(json!({
    "launch_id": launch_id,
    "model_id": id,
    "port": port,
    "pid": model.pid().await,
    "log_path": log_path,
  }))
}

fn spawn_last_params_recorder(
  state: PersistedState,
  model: ManagedModel,
  id: ModelId,
  params: LaunchParams,
) {
  tokio::spawn(async move {
    // The supervisor's probe runs with at most a 120s timeout in
    // production. Cap our wait at the same horizon so we don't
    // leak tasks for models that never come up.
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
      match model.state().await {
        ManagedState::Ready => {
          state
            .mutate(|s| s.upsert_last_params(id.clone(), params.clone()))
            .await;
          return;
        }
        ManagedState::Error { .. } | ManagedState::Stopped => return,
        _ => {}
      }
      if Instant::now() > deadline {
        return;
      }
      tokio::time::sleep(Duration::from_millis(200)).await;
    }
  });
}

async fn collect_in_use_ports(ctx: &MethodContext) -> Vec<u16> {
  ctx
    .supervisors
    .snapshot()
    .await
    .into_iter()
    .map(|(_, m)| m.port())
    .collect()
}

fn resolve_model_id(path: &std::path::Path) -> Result<ModelId, ErrorObject> {
  let header = read_gguf_header(path, HeaderReadOptions::default()).map_err(|e| {
    ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("could not read GGUF header at {}: {e}", path.display()),
    )
  })?;
  Ok(compute_model_id(path, &header.raw))
}

fn build_log_path(log_dir: &std::path::Path, id: &ModelId) -> PathBuf {
  let stem = id
    .path
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model");
  let ts = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  let short = id.short_fingerprint();
  log_dir.join(format!("{stem}-{short}-{ts}.log"))
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
  let id = resolve_model_id(&parsed.model_path)?;
  let snapshot = ctx.state.snapshot().await;
  let presets = snapshot.presets_map().get(&id).cloned().unwrap_or_default();
  Ok(json!({
    "model_id": id,
    "presets": presets.iter().map(preset_row).collect::<Vec<_>>(),
  }))
}

#[derive(Deserialize)]
struct PresetsSaveParams {
  model_path: PathBuf,
  name: String,
  #[serde(default)]
  ctx: Option<u32>,
  #[serde(default)]
  port: Option<u16>,
  #[serde(default)]
  reasoning: Option<bool>,
  #[serde(default)]
  mode: Option<LaunchModeWire>,
  #[serde(default)]
  advanced: Vec<String>,
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
  let id = resolve_model_id(&parsed.model_path)?;
  let mut params_value = LaunchParams::new(
    parsed.model_path.clone(),
    parsed
      .mode
      .map(LaunchMode::from)
      .unwrap_or(LaunchMode::Chat),
  );
  params_value.ctx = parsed.ctx;
  params_value.port = parsed.port;
  params_value.reasoning = parsed.reasoning.unwrap_or(false);
  params_value.advanced = parsed.advanced.into_iter().map(OsString::from).collect();
  let preset = NamedPreset {
    name: parsed.name.clone(),
    params: params_value.clone(),
  };

  let prev = ctx
    .state
    .mutate(|s| {
      let mut presets = s.presets_map().get(&id).cloned().unwrap_or_default();
      let prev = presets.upsert(preset.clone());
      s.upsert_presets(id.clone(), presets);
      prev
    })
    .await;

  Ok(json!({
    "model_id": id,
    "saved": preset_row(&preset),
    "replaced": prev.as_ref().map(preset_row),
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
  let id = resolve_model_id(&parsed.model_path)?;
  let removed = ctx
    .state
    .mutate(|s| {
      let mut presets = s.presets_map().get(&id).cloned().unwrap_or_default();
      let removed = presets.remove(&parsed.name);
      s.upsert_presets(id.clone(), presets);
      removed
    })
    .await;
  Ok(json!({
    "model_id": id,
    "removed": removed.as_ref().map(preset_row),
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
  let id = resolve_model_id(&parsed.model_path)?;
  let snapshot = ctx.state.snapshot().await;
  let preset = snapshot
    .presets_map()
    .get(&id)
    .and_then(|p| p.get(&parsed.name).cloned());
  Ok(json!({
    "model_id": id,
    "preset": preset.as_ref().map(preset_row),
  }))
}

fn preset_row(p: &NamedPreset) -> Value {
  json!({
    "name": p.name,
    "params": launch_params_row(&p.params),
  })
}

fn launch_params_row(p: &LaunchParams) -> Value {
  json!({
    "model_path": p.model_path,
    "mode": p.mode.label(),
    "ctx": p.ctx,
    "port": p.port,
    "reasoning": p.reasoning,
    "advanced": p.advanced.iter().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>(),
  })
}

#[derive(Deserialize)]
struct FavoriteParams {
  model_path: PathBuf,
}

async fn favorite_add_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: FavoriteParams = parse_params(params)?;
  let id = resolve_model_id(&parsed.model_path)?;
  let added = ctx.state.mutate(|s| s.favorites.add(id.clone())).await;
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
  let id = resolve_model_id(&parsed.model_path)?;
  let removed = ctx.state.mutate(|s| s.favorites.remove(&id)).await;
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
        "model_path": &entry.id.path,
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

// Compile-time guard so future commits don't drop the BTreeMap import
// silently (presets_map() returns a BTreeMap; the `unused_imports`
// lint would otherwise fire if all callers happened to ignore the map
// at once).
#[allow(dead_code)]
const _: fn() = || {
  let _ = std::mem::size_of::<BTreeMap<ModelId, Presets>>();
};

// Silence the unused-state-import warning when no test exercises it.
#[allow(dead_code)]
const _: fn() = || {
  let _ = std::mem::size_of::<ManagedState>();
};

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn ctx() -> MethodContext {
    MethodContext::new(ShutdownToken::new())
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
          native_ctx: Some(8192),
          chat_template: None,
          tokenizer_kind: Some("llama".to_string()),
          reasoning_hint: None,
          mode_hint: ModeHint::Chat,
          weights_bytes: Some(4_000_000_000),
        }),
        parse_error: None,
        split_siblings: Vec::new(),
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
}
