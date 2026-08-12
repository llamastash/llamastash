//! Spawn and shepherd one supervised child process for a user-requested
//! launch. Backend-agnostic: the binary, argv, env strip, and readiness
//! check all arrive in the [`ProcessLaunchSpec`] the backend produced
//! (see [`crate::backend`]); for llama.cpp that child is `llama-server`.
//! Owns the state machine
//! `Launching → Loading → Ready | Error{cause} → Stopping → Stopped`,
//! plus the stdout/stderr tee to a rotating log file and an
//! in-memory ring buffer (for the TUI Logs tab).
//!
//! Each `ManagedModel` is one supervisor instance — the daemon
//! holds a `BTreeMap<ModelId, ManagedModel>` keyed by canonical
//! model id (a single GGUF can be launched multiple times against
//! different ports; the daemon disambiguates by a `launch_id`
//! the supervisor generates).
//!
//! Process lifecycle:
//! 1. Spawn child with `Stdio::piped` stdout/stderr; apply
//!    `setsid` in `pre_exec` so the child survives daemon exit.
//! 2. Spawn one tokio task per stream that tees lines to the log
//!    file (rotating at 10 MiB, max 5 files per launch) and to a
//!    bounded ring buffer of the last 4096 lines.
//! 3. Hand the (pid, port) to `probe::poll_until_ready` with the
//!    backend's readiness endpoint; on the ready status, transition
//!    Loading → Ready. Timeout → Error.
//! 4. `stop()` sends SIGTERM, waits 5 s, sends SIGKILL if still
//!    alive. State transitions reflect each step.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};

/// Rotate logs at this byte size. Matches the module-level docstring.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Keep this many rotated segments (`<base>.1` … `<base>.N`).
const LOG_KEEP_SEGMENTS: usize = 5;

use crate::backend::{Backend, Backends, ProcessLaunchSpec, Readiness};
use crate::daemon::probe::{self, ProbeOutcome};
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

/// Snapshot the state-machine state of a managed model. Public so
/// the IPC `status` handler can serialise it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ManagedState {
  /// `tokio::process::Command::spawn` has been called; no PID yet.
  Launching,
  /// Child is running; `probe` has not yet seen a 200 response.
  Loading,
  /// `probe` saw 200 OK.
  Ready,
  /// Either spawn failed, the probe timed out, or the child exited
  /// before reaching Ready.
  Error { cause: String },
  /// `stop()` issued SIGTERM; SIGKILL still pending or process
  /// exit not yet observed.
  Stopping,
  /// Process has fully exited.
  Stopped,
}

impl ManagedState {
  /// Lowercase wire label (`"launching"`, `"ready"`, …). Stable —
  /// pinned parsers depend on these strings (P2-16).
  pub fn label(&self) -> &'static str {
    match self {
      ManagedState::Launching => "launching",
      ManagedState::Loading => "loading",
      ManagedState::Ready => "ready",
      ManagedState::Error { .. } => "error",
      ManagedState::Stopping => "stopping",
      ManagedState::Stopped => "stopped",
    }
  }

  /// Error cause string, if any. `Some` only for `Error{cause}`.
  pub fn cause(&self) -> Option<&str> {
    match self {
      ManagedState::Error { cause } => Some(cause.as_str()),
      _ => None,
    }
  }
}

/// Where a launch came from. Drives the proxy's idle-TTL sweeper:
/// auto-started supervisors are evictable when idle; manually-started
/// ones (TUI / CLI `start`) are treated as durable user intent and
/// stay resident regardless. Mirrors LM Studio's "manually loaded
/// models are exempt" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchOrigin {
  /// User explicitly launched via `llamastash start`, the TUI Launch
  /// action, or an IPC `start_model` call from any other client. Not
  /// evictable by the idle sweeper.
  Manual,
  /// Proxy `auto_start` (an inbound `/v1/...` request landed on an
  /// unloaded model). Evictable when idle for `proxy.idle_ttl_secs`.
  AutoStart,
}

impl LaunchOrigin {
  pub fn label(self) -> &'static str {
    match self {
      LaunchOrigin::Manual => "manual",
      LaunchOrigin::AutoStart => "auto_start",
    }
  }
}

/// RAII guard returned by [`ManagedModel::inflight_guard`]. Holds a
/// strong reference to the supervisor's `Arc<ManagedInner>` so the
/// `Drop` can decrement the counter even after the originating
/// `ManagedModel` handle has been dropped.
pub struct InflightGuard {
  inner: Arc<ManagedInner>,
}

impl Drop for InflightGuard {
  fn drop(&mut self) {
    self
      .inner
      .inflight
      .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
  }
}

/// Inputs to a launch. Owned by the caller (the IPC handler);
/// supervisor takes them and never hands them back.
#[derive(Debug, Clone)]
pub struct ManagedSpawn {
  pub id: ModelId,
  /// Resolved launch params, retained for introspection (the `params()`
  /// accessor, status projection). The argv actually spawned comes from
  /// `plan` — both are built from the same resolved params, so they
  /// agree by construction.
  pub params: LaunchParams,
  pub port: u16,
  pub mode: LaunchMode,
  pub log_path: PathBuf,
  /// The backend-produced process launch spec: binary, argv, env strip,
  /// readiness, and probe budget. The supervisor is backend-agnostic —
  /// it executes this spec without knowing which engine produced it.
  pub plan: ProcessLaunchSpec,
  /// How this launch entered the supervisor. Defaults to `Manual`
  /// (safe — never evicted) for callers that don't care.
  pub origin: LaunchOrigin,
  /// Strict-fit ctx-clamp readiness gate, populated by the caller
  /// only for fit-governed launches. `None` leaves the readiness path
  /// untouched (pinned ctx, missing trained-window metadata, Lemonade
  /// rows). See [`FitGate`].
  pub fit_gate: Option<FitGate>,
  /// The id of the backend this launch resolved to. The probe task uses it to
  /// call the right backend's `fetch_actuals` on Loading → Ready — so the
  /// supervisor reads post-launch actuals without naming an endpoint. An
  /// unknown / unmatched id falls back to the default backend.
  pub resolved_backend: String,
}

/// Resolved inputs for the strict-fit ctx-clamp readiness gate.
/// The caller builds this only for fit-governed launches (ctx delegated
/// to `--fit` and a known trained window); the supervisor's probe task
/// consumes it on the Loading → Ready transition.
#[derive(Debug, Clone, Copy)]
pub struct FitGate {
  /// The `--fit-ctx` floor llamastash passed.
  pub floor: u32,
  /// The model's trained context window.
  pub native: u32,
  /// Refuse (withhold Ready) vs. soft-notice on a detected clamp.
  pub strict: bool,
}

impl FitGate {
  /// A clamp is degradation only when `--fit` settled at (or below) the
  /// floor *and* the model could have gone higher — otherwise the floor
  /// is simply the model's own ceiling, not memory pressure.
  fn is_clamped(&self, resolved: u32) -> bool {
    resolved <= self.floor && self.native > self.floor
  }
}

/// One actively-managed launch. Cheap to clone via the `Arc` inside.
#[derive(Debug, Clone)]
pub struct ManagedModel {
  inner: Arc<ManagedInner>,
}

#[derive(Debug)]
struct ManagedInner {
  id: ModelId,
  port: u16,
  mode: LaunchMode,
  params: LaunchParams,
  log_path: PathBuf,
  /// Wall-clock seconds-since-epoch the model entered `Ready`.
  /// `None` until that transition.
  ready_at: RwLock<Option<u64>>,
  /// State machine head.
  state: RwLock<ManagedState>,
  /// PID, populated as soon as `spawn` returns. `None` only while
  /// the spawn call itself is still in flight.
  pid: RwLock<Option<u32>>,
  /// Bounded ring buffer for the TUI's Logs tab.
  ring: Mutex<RingBuffer>,
  /// Stays alive for the lifetime of the child; dropped on
  /// transition into `Stopped` or `Error`.
  child: Mutex<Option<Child>>,
  /// Latest per-PID resource reading (CPU% + RSS). `None` until the
  /// per-launch sampler has emitted at least one reading. Updated by
  /// the `resource_sampler` task spawned from [`spawn`].
  latest_resource: RwLock<Option<super::resources::ResourceReading>>,
  /// What `--fit` actually resolved, captured once by the readiness gate
  /// on the Loading → Ready transition (for fit-governed launches). The
  /// `last_params` recorder reads this to stamp the running snapshot so the
  /// backend's actuals are fetched at most once per launch. Empty until the
  /// gate runs (or for launches the gate skips).
  actuals: RwLock<super::actuals::Actuals>,
  /// Where the launch came from. Read by the idle-TTL sweeper so it
  /// only ever evicts `AutoStart` supervisors.
  origin: LaunchOrigin,
  /// Concurrent-request counter incremented when the proxy starts
  /// forwarding a request to this supervisor and decremented when
  /// the response body is dropped (success completion, abandoned
  /// connection, or upstream error). The idle-TTL sweeper skips
  /// supervisors with `inflight > 0` so a mid-stream generation
  /// can't get SIGTERM'd out from under the caller.
  inflight: std::sync::atomic::AtomicU64,
}

impl ManagedModel {
  pub fn id(&self) -> &ModelId {
    &self.inner.id
  }

  pub fn port(&self) -> u16 {
    self.inner.port
  }

  pub fn mode(&self) -> LaunchMode {
    self.inner.mode
  }

  pub fn params(&self) -> &LaunchParams {
    &self.inner.params
  }

  pub fn log_path(&self) -> &std::path::Path {
    &self.inner.log_path
  }

  pub fn origin(&self) -> LaunchOrigin {
    self.inner.origin
  }

  /// Snapshot the concurrent-request counter. The idle-TTL sweeper
  /// gates eviction on this — supervisors with `inflight > 0` are
  /// in the middle of serving a request and stay resident.
  pub fn inflight(&self) -> u64 {
    self
      .inner
      .inflight
      .load(std::sync::atomic::Ordering::SeqCst)
  }

  /// Increment `inflight` and return a guard. The guard's `Drop`
  /// decrements the counter — covers happy-path body completion,
  /// abandoned client connections, and upstream errors uniformly.
  /// Cloning the guard increments again; this is one-call-one-grant.
  pub fn inflight_guard(&self) -> InflightGuard {
    self
      .inner
      .inflight
      .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    InflightGuard {
      inner: Arc::clone(&self.inner),
    }
  }

  pub async fn pid(&self) -> Option<u32> {
    *self.inner.pid.read().await
  }

  pub async fn state(&self) -> ManagedState {
    self.inner.state.read().await.clone()
  }

  pub async fn ready_at(&self) -> Option<u64> {
    *self.inner.ready_at.read().await
  }

  /// Block until the model is `Ready`, or report why it won't be.
  ///
  /// [`spawn`] returns at `Loading` and flips to `Ready` only once the
  /// background probe sees the child's endpoint answer — so a caller that
  /// must talk to that endpoint has to wait for it, or it races the
  /// child's bind and hits connection-refused on a cold start (the
  /// Lemonade preload path). Polls the state until `Ready` (`Ok`), a
  /// terminal non-ready state (`Err` with the cause), or `timeout`
  /// elapses while still starting up (`Err`).
  pub async fn wait_until_ready(&self, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
      match self.state().await {
        ManagedState::Ready => return Ok(()),
        ManagedState::Error { cause } => return Err(cause),
        ManagedState::Stopping | ManagedState::Stopped => {
          return Err("stopped before becoming ready".to_string());
        }
        ManagedState::Launching | ManagedState::Loading => {
          if Instant::now() >= deadline {
            return Err(format!("not ready within {timeout:?}"));
          }
          tokio::time::sleep(Duration::from_millis(50)).await;
        }
      }
    }
  }

  /// Latest per-PID resource reading (CPU% + RSS). Mirrors the
  /// shape `resources::sample()` returns. `None` until the per-launch
  /// sampler has emitted its first non-priming reading.
  pub async fn latest_resource(&self) -> Option<super::resources::ResourceReading> {
    *self.inner.latest_resource.read().await
  }

  /// What `--fit` resolved, as captured by the readiness gate. Empty
  /// (`is_empty()`) for launches the gate skipped (pinned ctx / no
  /// trained-window metadata); the `last_params` recorder then fetches
  /// the actuals itself.
  pub async fn actuals(&self) -> super::actuals::Actuals {
    *self.inner.actuals.read().await
  }

  /// Snapshot of the most recent N lines the child wrote (stdout
  /// and stderr, interleaved in arrival order). Used by the
  /// `logs_tail` IPC method and the TUI Logs tab.
  pub async fn tail(&self, max: usize) -> Vec<String> {
    self.inner.ring.lock().await.tail(max)
  }

  /// Trigger graceful shutdown: SIGTERM, `grace` to honor it, then
  /// SIGKILL. Returns once the child has fully exited.
  ///
  /// Signal delivery is guarded against PID reuse: we re-check that
  /// the `Child` handle still reports a non-reaped pid under the
  /// child mutex before each `libc::kill`. Without this guard, a
  /// kernel that recycled the child's pid for an unrelated process
  /// during the grace window could see our SIGKILL.
  pub async fn stop(&self, grace: Duration) -> ManagedState {
    self.transition(ManagedState::Stopping).await;
    if self.inner.pid.read().await.is_none() {
      // Spawn never completed; nothing to signal.
      self.transition(ManagedState::Stopped).await;
      return self.state().await;
    }
    signal_child_with_guard(self, SignalFlavour::Graceful).await;
    let deadline = Instant::now() + grace;
    loop {
      if let Some(child) = self.inner.child.lock().await.as_mut() {
        if let Ok(Some(_status)) = child.try_wait() {
          break;
        }
      } else {
        break;
      }
      if Instant::now() >= deadline {
        signal_child_with_guard(self, SignalFlavour::Kill).await;
        // Wait for exit; SIGKILL is unignorable so this completes.
        if let Some(child) = self.inner.child.lock().await.as_mut() {
          let _ = child.wait().await;
        }
        break;
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    *self.inner.child.lock().await = None;
    self.transition(ManagedState::Stopped).await;
    self.state().await
  }

  /// Apply a state transition iff it is legal under the documented
  /// edges:
  ///
  /// * `Error` and `Stopped` are terminal — nothing transitions out
  ///   of them. (This preserves the probe's detailed `Error{cause}`
  ///   against a follow-up race from the exit-watcher, and stops a
  ///   long-running probe from clobbering `Stopped` after a
  ///   user-initiated stop.)
  /// * `Stopping` only accepts a transition to `Stopped` — once the
  ///   user initiates stop, neither a late probe-timeout nor a
  ///   simultaneous Ready signal should pre-empt their intent.
  ///
  /// Returns `true` if the transition fired, `false` if it was
  /// rejected. Callers may ignore the return value when the only
  /// goal is "make sure we're at least at this terminal state".
  pub(crate) async fn transition(&self, next: ManagedState) -> bool {
    let mut guard = self.inner.state.write().await;
    match (&*guard, &next) {
      // Terminal: don't overwrite.
      (ManagedState::Error { .. } | ManagedState::Stopped, _) => false,
      // Stop is in progress: only stop() may complete the journey.
      (ManagedState::Stopping, ManagedState::Stopped) => {
        *guard = next;
        true
      }
      (ManagedState::Stopping, _) => false,
      _ => {
        *guard = next;
        true
      }
    }
  }
}

/// Spawn the child, wire stdout/stderr to the log file + ring
/// buffer, kick off the probe, return the `ManagedModel`. The
/// supervisor task continues in the background; on Loading → Ready
/// it stamps the `ready_at` field and on a probe timeout flips to
/// `Error{cause}`.
pub async fn spawn(input: ManagedSpawn) -> Result<ManagedModel, SpawnError> {
  // The binary, argv, env-strip set, and readiness all come from the
  // backend's `prepare_launch` (see `crate::backend`). The supervisor is
  // backend-agnostic: it spawns `plan.binary` with `plan.argv`, removes
  // `plan.env_remove`, and probes `plan.readiness` — without knowing
  // which engine produced the spec. For llama.cpp `plan.argv` is exactly
  // `backend::llama_cpp::compose`'s output (pinned by parity tests).
  let mut cmd = Command::new(&input.plan.binary);
  cmd
    .args(&input.plan.argv)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  // Strip the backend-declared environment-variable bypass vectors
  // before spawn. For llama.cpp this is `LLAMA_ARG_*` (an inherited
  // `LLAMA_ARG_HOST=0.0.0.0` would silently defeat the loopback-only
  // argv contract) and `HF_*` (llamastash's own pull credentials, which
  // the child has no reason to see). The list + its full rationale live
  // with the backend at `crate::backend::llama_cpp::LLAMA_ENV_STRIP`.
  //
  // Strip the specific vectors rather than `env_clear()` so PATH / HOME /
  // library-search-path vars the child legitimately needs (CUDA, Metal,
  // ROCm, BLAS) survive.
  for var in &input.plan.env_remove {
    cmd.env_remove(var);
  }
  // Stamp an inheritance marker so a future daemon — possibly a
  // restart of this one, possibly an unrelated llamastash instance
  // on the same machine — can recognise this `llama-server` as
  // already-llamastash-launched on its boot sweep. The sweep reads
  // `/proc/<pid>/environ` (see `orphans::sweep`) and uses the
  // presence of this var to (a) treat the orphan's port as
  // unavailable in `collect_in_use_ports` so the allocator skips it,
  // and (b) surface a "launched by llamastash" hint on the external
  // row. Value is intentionally `"1"` rather than the state-dir or
  // daemon pid — a stable marker is all the Tier A port-tracking
  // path needs; richer attribution belongs in a future adoption
  // pass that re-incorporates the orphan as a managed supervisor.
  cmd.env("LLAMASTASH_LAUNCHED", "1");
  // Process-group setup + spawn go through [`ProcessControl`] so
  // a future Windows backend can swap in `CREATE_NEW_PROCESS_GROUP`
  // without touching this call site.
  let pc = crate::util::process_control::platform_default();
  let spawned = pc
    .spawn_supervised(cmd)
    .map_err(|e| SpawnError::Spawn(e.to_string()))?;
  let mut child = spawned.into_child();
  let pid = child.id();
  // Prepare the log file lazily — opening it ahead of the child
  // lets us bail out cleanly if the cache_dir/logs/ tree is
  // unwritable.
  let log_file = LogWriter::open(input.log_path.clone())
    .await
    .map_err(|e| SpawnError::Log(e.to_string()))?;
  let log_file = Arc::new(Mutex::new(log_file));

  let stdout = child.stdout.take().expect("piped stdout");
  let stderr = child.stderr.take().expect("piped stderr");

  let inner = Arc::new(ManagedInner {
    id: input.id.clone(),
    port: input.port,
    mode: input.mode,
    params: input.params.clone(),
    log_path: input.log_path.clone(),
    ready_at: RwLock::new(None),
    state: RwLock::new(ManagedState::Launching),
    pid: RwLock::new(pid),
    ring: Mutex::new(RingBuffer::with_capacity(4096)),
    child: Mutex::new(Some(child)),
    latest_resource: RwLock::new(None),
    actuals: RwLock::new(super::actuals::Actuals::default()),
    origin: input.origin,
    inflight: std::sync::atomic::AtomicU64::new(0),
  });
  let model = ManagedModel { inner };

  // Per-launch resource sampler (CPU% + RSS at 1 Hz). Mirrors the
  // host-metrics pattern: a tokio task pumps `sample_loop` readings
  // into a shared cell the IPC `status` handler reads. The task
  // exits when the child PID disappears (the sample_loop closes its
  // sender) or when the model lands in a terminal state.
  if let Some(pid) = pid {
    let sampler_model = model.clone();
    spawn_supervised("resource_sampler", async move {
      let mut rx = super::resources::sample_loop(pid, Duration::from_secs(1));
      while let Some(reading) = rx.recv().await {
        // The terminal-state check and the write into
        // `latest_resource` happen across two `.await` points. A
        // transition into Stopped/Error between them would let a
        // post-mortem reading land in the shared cell and leak out
        // via the next `status` poll. Hold the write lock first, then
        // re-check state under that guard so the write is gated by
        // the freshest known state.
        let mut slot = sampler_model.inner.latest_resource.write().await;
        match sampler_model.state().await {
          ManagedState::Stopped | ManagedState::Error { .. } => {
            // Clear any stale reading so the next status poll sees
            // a clean "no longer sampled" rather than a frozen
            // pre-stop snapshot.
            *slot = None;
            drop(slot);
            break;
          }
          _ => {
            *slot = Some(reading);
          }
        }
      }
    });
  }

  // Stream-pump tasks for stdout + stderr → ring buffer + log file.
  // Each task is wrapped in `spawn_supervised` so a panic surfaces as
  // a logged error instead of being silently swallowed by tokio. The
  // watchdog task is cheap (one extra `.await` on the JoinHandle).
  spawn_supervised(
    "pump_stdout",
    pump_stream(
      BufReader::new(stdout),
      Arc::clone(&model.inner),
      Arc::clone(&log_file),
      "stdout",
    ),
  );
  spawn_supervised(
    "pump_stderr",
    pump_stream(
      BufReader::new(stderr),
      Arc::clone(&model.inner),
      Arc::clone(&log_file),
      "stderr",
    ),
  );

  // Transition to Loading and kick off the probe. The endpoint + ready
  // status come from the backend's readiness declaration, not a
  // hardcoded `/health`.
  model.transition(ManagedState::Loading).await;
  let probe_model = model.clone();
  let probe_opts = input.plan.probe;
  let (ready_path, ready_status, expect_model_ids) = match &input.plan.readiness {
    Readiness::HttpPoll { path, ready_status } => (path.clone(), *ready_status, None),
    Readiness::HttpPollModelId {
      path,
      ready_status,
      expect_model_ids,
    } => (path.clone(), *ready_status, Some(expect_model_ids.clone())),
  };
  // Strict-fit ctx-clamp gate: the caller populates this only for
  // fit-governed launches; `None` leaves the readiness path unchanged.
  let fit_gate = input.fit_gate;
  // Resolve the launch's backend so the probe reads actuals from the right
  // engine (only when a fit gate is present) — no endpoint hard-coded here.
  let probe_backend = Backends::all()
    .into_iter()
    .find(|b| b.id() == input.resolved_backend)
    .unwrap_or_else(crate::backend::default_backend);
  spawn_supervised("probe", async move {
    let outcome = match &expect_model_ids {
      // ds4: 200 on `/v1/models` plus a body advertising a ds4 alias.
      Some(ids) => {
        probe::poll_until_ready_model_id(
          probe_model.inner.port,
          probe_opts,
          &ready_path,
          ready_status,
          ids,
        )
        .await
      }
      None => {
        probe::poll_until_ready(
          probe_model.inner.port,
          probe_opts,
          &ready_path,
          ready_status,
        )
        .await
      }
    };
    match outcome {
      ProbeOutcome::Ready => {
        // For fit-governed launches, read what `--fit` resolved before
        // declaring Ready: the gate needs it, and stashing it on the
        // model lets the `last_params` recorder reuse it instead of
        // fetching the actuals a second time. Best-effort — a failed fetch
        // yields empty actuals and the gate simply can't fire.
        // Unconditional, not just for fit-governed launches: `resolved_ctx` is
        // reported on every running row, and gating the fetch on the gate left
        // it permanently null for any backend that builds no gate. The default
        // trait impl does no I/O, so a backend that exposes no such endpoint
        // still pays nothing.
        let mut actuals = probe_backend
          .fetch_actuals(probe_model.inner.port, Duration::from_secs(5))
          .await;
        if let (Some(gate), Some(resolved)) = (fit_gate, actuals.resolved_ctx) {
          if gate.is_clamped(resolved) {
            actuals.ctx_clamped = true;
            if gate.strict {
              // Withhold Ready: refuse the launch outright so a strict
              // caller never routes traffic to a context-starved model.
              let cause = format!(
                "strict-fit: --fit clamped the context window to the floor ({} tokens) under \
                 memory pressure; the model's trained window is {} tokens. Free up memory or \
                 lower fit_ctx_floor to launch.",
                gate.floor, gate.native
              );
              probe_model.transition(ManagedState::Error { cause }).await;
              signal_child_with_guard(&probe_model, SignalFlavour::Kill).await;
              return;
            }
            // Soft notice (non-strict): keep the model Ready but flag the
            // clamp so the running surfaces can surface it.
            log::warn!(
              "supervisor: --fit clamped context to the floor ({} tokens, trained window {}) on \
               port {} under memory pressure",
              gate.floor,
              gate.native,
              probe_model.inner.port
            );
          }
        }
        *probe_model.inner.actuals.write().await = actuals;
        let secs = SystemTime::now()
          .duration_since(UNIX_EPOCH)
          .map(|d| d.as_secs())
          .unwrap_or_default();
        *probe_model.inner.ready_at.write().await = Some(secs);
        probe_model.transition(ManagedState::Ready).await;
      }
      ProbeOutcome::Timeout { last_status } => {
        let mut cause = String::from("health probe timeout");
        if let Some(s) = last_status {
          cause = format!("health probe timeout (last status {s})");
        }
        let tail = probe_model.tail(50).await;
        if !tail.is_empty() {
          cause.push_str("; last stderr lines:\n");
          cause.push_str(&tail.join("\n"));
        }
        // The transition is guarded — if the user already initiated
        // stop, this is a no-op and the SIGKILL below is the only
        // useful side-effect.
        probe_model.transition(ManagedState::Error { cause }).await;
        // Best-effort SIGKILL so we don't leave the unresponsive
        // child draining resources. Guarded against PID reuse by
        // taking the child mutex and re-verifying the handle is
        // still alive — see [`signal_child_with_guard`].
        signal_child_with_guard(&probe_model, SignalFlavour::Kill).await;
      }
    }
  });

  // Watch for child exit. Classification depends on the state the
  // child died in:
  //   Launching / Loading → `Error{cause}` with status + stderr tail
  //   Ready               → `Stopped` (orphan / external kill)
  //   Stopping            → `Stopped` (let stop() race us; idempotent)
  //   Error / Stopped     → no-op; probe / stop() already classified
  //
  // The classification reads the state under the same write lock it
  // ultimately writes through, so a concurrent probe transition can't
  // sneak in between read and write.
  let watcher_model = model.clone();
  spawn_supervised("exit_watcher", async move {
    loop {
      let mut guard = watcher_model.inner.child.lock().await;
      let watched = match guard.as_mut() {
        Some(c) => c,
        None => return,
      };
      let try_wait = watched.try_wait();
      let watched_pid = watched.id();
      drop(guard);
      match try_wait {
        Ok(Some(status)) => {
          // Snapshot tail before taking the write lock so we don't
          // hold both locks at once.
          let tail = watcher_model.tail(50).await;
          let mut state = watcher_model.inner.state.write().await;
          match &*state {
            ManagedState::Error { .. } | ManagedState::Stopped => {
              // Already classified; preserve the more-specific cause.
            }
            ManagedState::Ready | ManagedState::Stopping => {
              *state = ManagedState::Stopped;
            }
            ManagedState::Launching | ManagedState::Loading => {
              let mut cause = format!(
                "process exited before becoming ready (status: {:?})",
                status.code()
              );
              if !tail.is_empty() {
                cause.push_str("; last stderr lines:\n");
                cause.push_str(&tail.join("\n"));
              }
              *state = ManagedState::Error { cause };
            }
          }
          drop(state);
          // Release any backend-side bookkeeping for the pid (Windows
          // Job Object handle; no-op on Unix). Without this the
          // WindowsProcessControl map would retain one HANDLE per
          // naturally-exited launch for the daemon's lifetime —
          // meaningful under idle-TTL eviction churn.
          if let Some(exited_pid) = watched_pid {
            crate::util::process_control::platform_default().forget(exited_pid);
          }
          return;
        }
        Ok(None) => {}
        Err(_) => return,
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
  });

  Ok(model)
}

/// Spawn `fut` and forward any panic to the log instead of letting
/// it disappear when the `JoinHandle` is dropped. The watchdog task
/// only runs while the outer task is alive; it does not own a copy
/// of the work itself.
pub(crate) fn spawn_supervised<F>(name: &'static str, fut: F)
where
  F: std::future::Future<Output = ()> + Send + 'static,
{
  let handle = tokio::spawn(fut);
  tokio::spawn(async move {
    if let Err(e) = handle.await {
      if e.is_panic() {
        log::error!("supervisor: task {name} panicked: {e}");
      } else if e.is_cancelled() {
        log::debug!("supervisor: task {name} cancelled");
      }
    }
  });
}

/// Whether [`signal_child_with_guard`] should send the graceful or
/// kill signal. Crosses the trait boundary as the explicit signal
/// flavour so Windows backends don't need to translate a libc signum.
#[derive(Debug, Clone, Copy)]
enum SignalFlavour {
  Graceful,
  Kill,
}

/// Send `flavour` to the supervised child's process group, holding
/// the child mutex across the syscall so a concurrent reap can't
/// recycle the pid while we're delivering. If the child handle has
/// already been reaped (`Ok(Some(_))` from `try_wait`) or never
/// spawned, this is a no-op.
async fn signal_child_with_guard(model: &ManagedModel, flavour: SignalFlavour) {
  let mut guard = model.inner.child.lock().await;
  let Some(child) = guard.as_mut() else {
    return;
  };
  // Re-check liveness under the lock. If `try_wait` says
  // `Ok(Some(_))`, the kernel has reaped the zombie and the pid is
  // a candidate for recycling — don't signal.
  if !matches!(child.try_wait(), Ok(None)) {
    return;
  }
  let Some(pid) = child.id() else { return };
  // `setsid()` ran in `pre_exec` so the child is its own PGID
  // leader. Signalling `ProcessGroup(pid)` reaches every process
  // it forked — a SIGTERM to just the immediate child would leave
  // grandchildren running.
  //
  // We hold the child mutex across the trait call so the kernel
  // can't reap-then-recycle the PGID between our `try_wait` check
  // and the signal delivery.
  use crate::util::process_control::SignalTarget;
  let target = SignalTarget::ProcessGroup(pid);
  let pc = crate::util::process_control::platform_default();
  match flavour {
    SignalFlavour::Graceful => pc.signal_graceful(target),
    SignalFlavour::Kill => pc.signal_kill(target),
  }
}

/// Errors `spawn` can return synchronously.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
  /// `Command::spawn` failed (binary not executable, etc.).
  #[error("could not spawn llama-server: {0}")]
  Spawn(String),
  /// Log file could not be opened.
  #[error("could not open log file: {0}")]
  Log(String),
  /// A managed-umbrella port is already held by another process, so
  /// llamastash cannot bind it to supervise its own instance.
  #[error("127.0.0.1:{0} is already in use; cannot supervise a managed umbrella on it")]
  PortInUse(u16),
}

async fn pump_stream<R>(
  mut reader: BufReader<R>,
  inner: Arc<ManagedInner>,
  log_file: Arc<Mutex<LogWriter>>,
  source: &'static str,
) where
  R: tokio::io::AsyncRead + Unpin,
{
  // Reuse one buffer across iterations instead of paying for a fresh
  // `to_string` + `format!` per line. The prefix never changes for a
  // given stream so we can format it once and snip the per-line body
  // in place.
  let mut line = String::new();
  let prefix = format!("[{source}] ");
  let mut scratch = String::with_capacity(256);
  // Disk writes can wedge transiently (full filesystem, fs remounted
  // ro, quota exceeded). Previously we returned on the first failure,
  // silently stopping log capture for the lifetime of the child even
  // though the kernel pipe was still readable. Keep pumping the ring
  // buffer regardless so the TUI's Logs tab always reflects the
  // freshest output; log the disk error once per session so it shows
  // up in journal/stderr without spamming on every line.
  let mut disk_writes_disabled = false;
  loop {
    line.clear();
    match reader.read_line(&mut line).await {
      Ok(0) => return,
      Ok(_) => {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        scratch.clear();
        scratch.push_str(&prefix);
        scratch.push_str(trimmed);
        // Ring buffer stores fully-prefixed lines so logs_tail can
        // emit them verbatim. One owned clone here is the cost; the
        // alternative (sharing through Arc<str>) costs more allocs
        // overall under steady-state write rate.
        inner.ring.lock().await.push_copy(&scratch);
        if !disk_writes_disabled {
          let mut file = log_file.lock().await;
          if let Err(e) = file.write_line(scratch.as_bytes()).await {
            log::warn!(
              "supervisor: {source} log write failed: {e}; disk capture paused for this launch (ring buffer continues)"
            );
            disk_writes_disabled = true;
          }
        }
      }
      Err(e) => {
        log::warn!("supervisor: {source} stream read error: {e}");
        return;
      }
    }
  }
}

/// Rotating writer for one launch's log file. Wraps a `tokio::fs::File`
/// plus a running byte counter; when the counter crosses
/// [`LOG_ROTATE_BYTES`], the current file is renamed to `<base>.1`,
/// older segments shift up by one, and the [`LOG_KEEP_SEGMENTS`]th
/// segment is unlinked. Then a fresh file replaces the active path.
pub(crate) struct LogWriter {
  path: PathBuf,
  file: tokio::fs::File,
  written: u64,
}

impl LogWriter {
  pub(crate) async fn open(path: PathBuf) -> std::io::Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let std_file = std::fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&path)?;
    let written = std_file.metadata().map(|m| m.len()).unwrap_or(0);
    let file = tokio::fs::File::from_std(std_file);
    Ok(Self {
      path,
      file,
      written,
    })
  }

  async fn write_line(&mut self, body: &[u8]) -> std::io::Result<()> {
    self.file.write_all(body).await?;
    self.file.write_all(b"\n").await?;
    self.written += body.len() as u64 + 1;
    if self.written >= LOG_ROTATE_BYTES {
      // Flush before rotating so the renamed file has every line.
      let _ = self.file.flush().await;
      if let Err(e) = self.rotate().await {
        // Rotation failure shouldn't kill the writer; we just keep
        // appending to the existing oversize file and try again on
        // the next line.
        log::warn!(
          "supervisor: log rotate failed for {}: {e}",
          self.path.display()
        );
      }
    }
    Ok(())
  }

  async fn rotate(&mut self) -> std::io::Result<()> {
    // `rotate_segments` does up to `LOG_KEEP_SEGMENTS` blocking
    // `std::fs::rename` + one `remove_file` syscall. On the standard
    // tokio worker thread these stall every other task on the worker
    // until rotation finishes (negligible on ext4/xfs, 10s of ms on
    // ecryptfs / FUSE / slow NAS). Off-thread it via `spawn_blocking`
    // so concurrent probe polling / log pumps stay responsive.
    let rotate_path = self.path.clone();
    tokio::task::spawn_blocking(move || rotate_segments(&rotate_path, LOG_KEEP_SEGMENTS))
      .await
      .map_err(|e| std::io::Error::other(format!("rotate join: {e}")))??;
    let open_path = self.path.clone();
    let std_file = tokio::task::spawn_blocking(move || {
      std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&open_path)
    })
    .await
    .map_err(|e| std::io::Error::other(format!("rotate open join: {e}")))??;
    self.file = tokio::fs::File::from_std(std_file);
    self.written = 0;
    Ok(())
  }
}

/// Shift `<base>.<N-1>` → `<base>.<N>` for N..=2, rename the active
/// `<base>` → `<base>.1`, and unlink `<base>.<N+1>` (if any). Pure FS,
/// no I/O against the open file.
fn rotate_segments(base: &Path, keep: usize) -> std::io::Result<()> {
  let segment = |n: usize| -> PathBuf {
    let mut name = base
      .file_name()
      .map(|s| s.to_os_string())
      .unwrap_or_default();
    name.push(format!(".{n}"));
    base.with_file_name(name)
  };
  // Drop the oldest if we'd otherwise exceed `keep`.
  let oldest = segment(keep);
  if oldest.exists() {
    std::fs::remove_file(&oldest)?;
  }
  // Shift remaining segments up: .keep-1 → .keep, .keep-2 → .keep-1, …, .1 → .2.
  for n in (1..keep).rev() {
    let from = segment(n);
    if from.exists() {
      let to = segment(n + 1);
      std::fs::rename(&from, &to)?;
    }
  }
  // Rename the active file to .1.
  if base.exists() {
    std::fs::rename(base, segment(1))?;
  }
  Ok(())
}

/// Fixed-capacity ring buffer of stdout/stderr lines. Older lines
/// drop off as new ones arrive — 4096 lines is plenty for the TUI
/// Logs tab without bloating supervisor RAM.
#[derive(Debug)]
struct RingBuffer {
  inner: VecDeque<String>,
  capacity: usize,
}

impl RingBuffer {
  fn with_capacity(capacity: usize) -> Self {
    Self {
      inner: VecDeque::with_capacity(capacity),
      capacity,
    }
  }

  fn push_copy(&mut self, line: &str) {
    if self.capacity == 0 {
      return;
    }
    if self.inner.len() == self.capacity {
      if let Some(mut reused) = self.inner.pop_front() {
        reused.clear();
        reused.push_str(line);
        self.inner.push_back(reused);
        return;
      }
    }
    self.inner.push_back(line.to_owned());
  }

  fn tail(&self, max: usize) -> Vec<String> {
    let take = max.min(self.inner.len());
    self
      .inner
      .iter()
      .skip(self.inner.len() - take)
      .cloned()
      .collect()
  }
}

/// Construct a [`ManagedModel`] without spawning a child, for tests that
/// need registry contents rather than a live process. Lives outside
/// `mod tests` so sibling daemon modules can use it too.
#[cfg(test)]
pub(crate) mod test_support {
  use super::*;

  /// A model parked in `state`, on a fixed path and port.
  pub(crate) fn in_state(state: ManagedState) -> ManagedModel {
    let id = ModelId {
      path: PathBuf::from("/test/m.gguf"),
      header_blake3: [0u8; 32],
    };
    let params = LaunchParams::new(id.path.clone(), LaunchMode::Chat);
    ManagedModel {
      inner: Arc::new(ManagedInner {
        id,
        port: 41100,
        mode: LaunchMode::Chat,
        params,
        log_path: PathBuf::from("/tmp/llamastash-test.log"),
        ready_at: RwLock::new(None),
        state: RwLock::new(state),
        pid: RwLock::new(None),
        ring: Mutex::new(RingBuffer::with_capacity(16)),
        child: Mutex::new(None),
        latest_resource: RwLock::new(None),
        actuals: RwLock::new(crate::daemon::actuals::Actuals::default()),
        origin: LaunchOrigin::Manual,
        inflight: std::sync::atomic::AtomicU64::new(0),
      }),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn managed_state_label_and_cause_match_wire_shape() {
    // The label table backs the IPC `status` projection (P2-16).
    // `cause` is `Some` only for `Error{...}`.
    assert_eq!(ManagedState::Launching.label(), "launching");
    assert_eq!(ManagedState::Loading.label(), "loading");
    assert_eq!(ManagedState::Ready.label(), "ready");
    assert_eq!(ManagedState::Stopping.label(), "stopping");
    assert_eq!(ManagedState::Stopped.label(), "stopped");
    assert_eq!(
      ManagedState::Error {
        cause: "boom".into(),
      }
      .label(),
      "error"
    );

    assert!(ManagedState::Launching.cause().is_none());
    assert_eq!(
      ManagedState::Error {
        cause: "boom".into(),
      }
      .cause(),
      Some("boom")
    );
  }

  #[test]
  fn ring_buffer_drops_oldest_when_full() {
    let mut r = RingBuffer::with_capacity(3);
    r.push_copy("a");
    r.push_copy("b");
    r.push_copy("c");
    r.push_copy("d");
    assert_eq!(r.tail(10), vec!["b", "c", "d"]);
  }

  #[test]
  fn ring_buffer_tail_respects_max() {
    let mut r = RingBuffer::with_capacity(5);
    for i in 0..5 {
      r.push_copy(&format!("{i}"));
    }
    let t = r.tail(2);
    assert_eq!(t, vec!["3", "4"]);
  }

  #[test]
  fn ring_buffer_tail_clamps_when_max_exceeds_len() {
    let mut r = RingBuffer::with_capacity(5);
    r.push_copy("only");
    let t = r.tail(100);
    assert_eq!(t, vec!["only"]);
  }

  #[test]
  fn ring_buffer_zero_capacity_stays_empty() {
    let mut r = RingBuffer::with_capacity(0);
    r.push_copy("ignored");
    assert!(r.tail(10).is_empty());
  }

  #[test]
  fn managed_state_json_round_trip() {
    let v = ManagedState::Error {
      cause: "timeout".into(),
    };
    let s_err = serde_json::to_string(&v).unwrap();
    let back: ManagedState = serde_json::from_str(&s_err).unwrap();
    assert_eq!(back, v);
    let r = ManagedState::Ready;
    let s_ready = serde_json::to_string(&r).unwrap();
    assert_eq!(s_ready, "{\"state\":\"ready\"}");
  }

  fn test_model(initial: ManagedState) -> ManagedModel {
    test_support::in_state(initial)
  }

  #[test]
  fn fit_gate_clamp_detection() {
    let gate = FitGate {
      floor: 16_384,
      native: 131_072,
      strict: false,
    };
    // Pinned to the floor while the model could go higher → clamp.
    assert!(gate.is_clamped(16_384));
    // Defensive: a resolution somehow below the floor is still a clamp.
    assert!(gate.is_clamped(8_192));
    // Fit found headroom above the floor → not a clamp.
    assert!(!gate.is_clamped(65_536));
    // Floor at the trained ceiling → settling there is the model's own
    // limit, not memory pressure.
    let at_max = FitGate {
      floor: 16_384,
      native: 16_384,
      strict: true,
    };
    assert!(!at_max.is_clamped(16_384));
    // Trained window below the floor (fit clamps down to it) is the
    // model's limit too, never flagged.
    let small = FitGate {
      floor: 16_384,
      native: 8_192,
      strict: true,
    };
    assert!(!small.is_clamped(8_192));
  }

  #[tokio::test]
  async fn transition_rejects_moves_out_of_error() {
    let m = test_model(ManagedState::Error {
      cause: "probe timeout".into(),
    });
    assert!(!m.transition(ManagedState::Ready).await);
    assert!(!m.transition(ManagedState::Stopped).await);
    assert!(!m.transition(ManagedState::Stopping).await);
    // Original cause preserved.
    match m.state().await {
      ManagedState::Error { cause } => assert_eq!(cause, "probe timeout"),
      other => panic!("expected Error, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn transition_rejects_moves_out_of_stopped() {
    let m = test_model(ManagedState::Stopped);
    assert!(!m.transition(ManagedState::Ready).await);
    assert!(
      !m.transition(ManagedState::Error { cause: "x".into() })
        .await
    );
    assert!(matches!(m.state().await, ManagedState::Stopped));
  }

  #[tokio::test]
  async fn transition_rejects_stopping_to_ready_probe_race() {
    let m = test_model(ManagedState::Stopping);
    assert!(!m.transition(ManagedState::Ready).await);
    assert!(matches!(m.state().await, ManagedState::Stopping));
    // A late probe-timeout firing after user-stop must not pre-empt.
    assert!(
      !m.transition(ManagedState::Error {
        cause: "probe timeout".into()
      })
      .await
    );
    assert!(matches!(m.state().await, ManagedState::Stopping));
    // But Stopping → Stopped is still allowed (stop() completes).
    assert!(m.transition(ManagedState::Stopped).await);
    assert!(matches!(m.state().await, ManagedState::Stopped));
  }

  #[tokio::test]
  async fn legal_transitions_succeed() {
    let m = test_model(ManagedState::Launching);
    assert!(m.transition(ManagedState::Loading).await);
    assert!(m.transition(ManagedState::Ready).await);
    assert!(m.transition(ManagedState::Stopping).await);
    assert!(m.transition(ManagedState::Stopped).await);
  }

  #[tokio::test]
  async fn wait_until_ready_returns_immediately_when_ready() {
    let m = test_model(ManagedState::Ready);
    assert!(m.wait_until_ready(Duration::from_secs(1)).await.is_ok());
  }

  #[tokio::test]
  async fn wait_until_ready_propagates_error_cause() {
    let m = test_model(ManagedState::Error {
      cause: "probe timeout".into(),
    });
    let err = m
      .wait_until_ready(Duration::from_secs(1))
      .await
      .expect_err("error state must not report ready");
    assert_eq!(err, "probe timeout");
  }

  #[tokio::test]
  async fn wait_until_ready_errs_when_stopped_before_ready() {
    let m = test_model(ManagedState::Stopped);
    assert!(m.wait_until_ready(Duration::from_secs(1)).await.is_err());
  }

  #[tokio::test]
  async fn wait_until_ready_observes_a_late_ready_transition() {
    // The Lemonade preload's exact shape: handle is at Loading, a
    // separate task flips it to Ready shortly after — the waiter must
    // see it rather than racing ahead.
    let m = test_model(ManagedState::Loading);
    let probe = m.clone();
    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(60)).await;
      probe.transition(ManagedState::Ready).await;
    });
    assert!(m.wait_until_ready(Duration::from_secs(2)).await.is_ok());
  }

  #[tokio::test]
  async fn wait_until_ready_times_out_if_never_ready() {
    let m = test_model(ManagedState::Loading);
    let err = m
      .wait_until_ready(Duration::from_millis(80))
      .await
      .expect_err("a stuck-Loading model must time out");
    assert!(err.contains("not ready within"), "got {err}");
  }

  #[tokio::test]
  async fn second_transition_to_error_preserves_first_cause() {
    let m = test_model(ManagedState::Loading);
    assert!(
      m.transition(ManagedState::Error {
        cause: "probe timeout (last status 503)".into()
      })
      .await
    );
    // A follow-up Error from the exit-watcher must not overwrite.
    assert!(
      !m.transition(ManagedState::Error {
        cause: "process exited before becoming ready".into()
      })
      .await
    );
    match m.state().await {
      ManagedState::Error { cause } => {
        assert!(cause.contains("probe timeout"));
      }
      other => panic!("expected Error, got {other:?}"),
    }
  }
}
