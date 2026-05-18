//! Spawn and shepherd a `llama-server` child for one user-requested
//! launch. Owns the state machine
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
//! 3. Hand the (pid, port) to `probe::poll_until_ready`; on 200,
//!    transition Loading → Ready. Timeout → Error.
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

use crate::daemon::probe::{self, ProbeOptions, ProbeOutcome};
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use crate::launch::params::{compose, LaunchParams};

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

/// Inputs to a launch. Owned by the caller (the IPC handler);
/// supervisor takes them and never hands them back.
#[derive(Debug, Clone)]
pub struct ManagedSpawn {
  pub id: ModelId,
  pub binary: PathBuf,
  pub params: LaunchParams,
  pub port: u16,
  pub mode: LaunchMode,
  pub log_path: PathBuf,
  pub probe: ProbeOptions,
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

  pub async fn pid(&self) -> Option<u32> {
    *self.inner.pid.read().await
  }

  pub async fn state(&self) -> ManagedState {
    self.inner.state.read().await.clone()
  }

  pub async fn ready_at(&self) -> Option<u64> {
    *self.inner.ready_at.read().await
  }

  /// Latest per-PID resource reading (CPU% + RSS). Mirrors the
  /// shape `resources::sample()` returns. `None` until the per-launch
  /// sampler has emitted its first non-priming reading.
  pub async fn latest_resource(&self) -> Option<super::resources::ResourceReading> {
    *self.inner.latest_resource.read().await
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
    signal_child_with_guard(self, libc::SIGTERM).await;
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
        signal_child_with_guard(self, libc::SIGKILL).await;
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
  let argv = compose(&input.params, input.port);
  let mut cmd = Command::new(&input.binary);
  cmd
    .args(&argv)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  // Strip llama-server's environment-variable overrides before spawn.
  // llama-server reads `LLAMA_ARG_*` for every CLI flag (e.g.
  // `LLAMA_ARG_HOST=0.0.0.0` overrides the `--host 127.0.0.1` we
  // pass in argv on some flag-parsing builds), so an inherited env
  // var would silently defeat the loopback-only contract that
  // `FORBIDDEN_ADVANCED_PREFIXES` enforces for argv. Strip the
  // specific bypass vectors rather than `env_clear()` so PATH /
  // HOME / library-search-path env vars the child legitimately
  // needs (CUDA, Metal, ROCm, BLAS) survive.
  for var in [
    "LLAMA_ARG_HOST",
    "LLAMA_ARG_PORT",
    "LLAMA_ARG_BIND",
    "LLAMA_ARG_LISTEN",
    "LLAMA_ARG_API_KEY",
    "LLAMA_ARG_SSL_KEY_FILE",
    "LLAMA_ARG_SSL_CERT_FILE",
  ] {
    cmd.env_remove(var);
  }
  #[cfg(unix)]
  {
    // SAFETY: `pre_exec` runs in the child between fork and exec.
    // `setsid` is on POSIX's async-signal-safe list — no
    // allocations, no locks, no tokio state touched.
    //
    // `pre_exec` here is the method on `tokio::process::Command`
    // (re-exposed from `std::os::unix::process::CommandExt`) — the
    // unused-import lint trips because rustc resolves the call
    // without needing the trait in scope.
    unsafe {
      cmd.pre_exec(|| {
        if libc::setsid() < 0 {
          return Err(std::io::Error::last_os_error());
        }
        Ok(())
      });
    }
  }
  let mut child = cmd.spawn().map_err(|e| SpawnError::Spawn(e.to_string()))?;
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

  // Transition to Loading and kick off the probe.
  model.transition(ManagedState::Loading).await;
  let probe_model = model.clone();
  let probe_opts = input.probe;
  spawn_supervised("probe", async move {
    let outcome = probe::poll_until_ready(probe_model.inner.port, probe_opts).await;
    match outcome {
      ProbeOutcome::Ready => {
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
        signal_child_with_guard(&probe_model, libc::SIGKILL).await;
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

/// Send `sig` to the supervised child, holding the child mutex
/// across the syscall so a concurrent reap can't recycle the pid
/// while we're delivering. If the child handle has already been
/// reaped (`Ok(Some(_))` from `try_wait`) or never spawned, this
/// is a no-op.
async fn signal_child_with_guard(model: &ManagedModel, sig: libc::c_int) {
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
  // SAFETY: `kill(2)` with a *negative* pid signals every process
  // in the corresponding process group. `pre_exec` ran `setsid()`
  // for the child, which made it both a session leader and a
  // process-group leader whose PGID equals its PID — so the
  // negated PID here is the PGID of llama-server and every
  // grandchild it spawned. We hold the child mutex across the
  // call so the kernel can't reap and recycle the PGID between
  // our `try_wait` check and the signal delivery.
  //
  // Audit §2.1 #3: signalling just `pid` left grandchildren
  // running after SIGTERM. The fix is a one-character negation
  // (the alternative is pulling in the `command-group` crate,
  // which boils down to the same syscall on Unix).
  unsafe {
    libc::kill(-(pid as i32), sig);
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
        inner.ring.lock().await.push(scratch.clone());
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

  fn push(&mut self, line: String) {
    if self.inner.len() == self.capacity {
      self.inner.pop_front();
    }
    self.inner.push_back(line);
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
    r.push("a".into());
    r.push("b".into());
    r.push("c".into());
    r.push("d".into());
    assert_eq!(r.tail(10), vec!["b", "c", "d"]);
  }

  #[test]
  fn ring_buffer_tail_respects_max() {
    let mut r = RingBuffer::with_capacity(5);
    for i in 0..5 {
      r.push(format!("{i}"));
    }
    let t = r.tail(2);
    assert_eq!(t, vec!["3", "4"]);
  }

  #[test]
  fn ring_buffer_tail_clamps_when_max_exceeds_len() {
    let mut r = RingBuffer::with_capacity(5);
    r.push("only".into());
    let t = r.tail(100);
    assert_eq!(t, vec!["only"]);
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
    let id = ModelId {
      path: PathBuf::from("/test/m.gguf"),
      header_blake3: [0u8; 32],
    };
    let params = LaunchParams::new(id.path.clone(), LaunchMode::Chat);
    let inner = Arc::new(ManagedInner {
      id,
      port: 41100,
      mode: LaunchMode::Chat,
      params,
      log_path: PathBuf::from("/tmp/llamadash-test.log"),
      ready_at: RwLock::new(None),
      state: RwLock::new(initial),
      pid: RwLock::new(None),
      ring: Mutex::new(RingBuffer::with_capacity(16)),
      child: Mutex::new(None),
      latest_resource: RwLock::new(None),
    });
    ManagedModel { inner }
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
