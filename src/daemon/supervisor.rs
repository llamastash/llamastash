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
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};

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

  /// Snapshot of the most recent N lines the child wrote (stdout
  /// and stderr, interleaved in arrival order). Used by the
  /// `logs_tail` IPC method and the TUI Logs tab.
  pub async fn tail(&self, max: usize) -> Vec<String> {
    self.inner.ring.lock().await.tail(max)
  }

  /// Trigger graceful shutdown: SIGTERM, 5 s grace, then SIGKILL.
  /// Returns once the child has fully exited.
  pub async fn stop(&self, grace: Duration) -> ManagedState {
    self.transition(ManagedState::Stopping).await;
    let pid = match *self.inner.pid.read().await {
      Some(p) => p as i32,
      None => {
        // Spawn never completed; nothing to signal.
        self.transition(ManagedState::Stopped).await;
        return self.state().await;
      }
    };
    // SIGTERM first. Best-effort: a "no such process" return value
    // just means the child already exited.
    unsafe {
      libc::kill(pid, libc::SIGTERM);
    }
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
        unsafe {
          libc::kill(pid, libc::SIGKILL);
        }
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
  ensure_parent(&input.log_path).map_err(|e| SpawnError::Log(e.to_string()))?;
  let log_file = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(&input.log_path)
    .map_err(|e| SpawnError::Log(e.to_string()))?;
  let log_file = Arc::new(Mutex::new(tokio::fs::File::from_std(log_file)));

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
  });
  let model = ManagedModel { inner };

  // Stream-pump tasks for stdout + stderr → ring buffer + log file.
  let pump_stdout = pump_stream(
    BufReader::new(stdout),
    Arc::clone(&model.inner),
    Arc::clone(&log_file),
    "stdout",
  );
  let pump_stderr = pump_stream(
    BufReader::new(stderr),
    Arc::clone(&model.inner),
    Arc::clone(&log_file),
    "stderr",
  );
  tokio::spawn(pump_stdout);
  tokio::spawn(pump_stderr);

  // Transition to Loading and kick off the probe.
  model.transition(ManagedState::Loading).await;
  let probe_model = model.clone();
  let probe_opts = input.probe;
  tokio::spawn(async move {
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
        probe_model.transition(ManagedState::Error { cause }).await;
        // Best-effort SIGKILL so we don't leave the unresponsive
        // child draining resources.
        if let Some(child_pid) = probe_model.pid().await {
          unsafe {
            libc::kill(child_pid as i32, libc::SIGKILL);
          }
        }
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
  tokio::spawn(async move {
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

/// Errors `spawn` can return synchronously.
#[derive(Debug)]
pub enum SpawnError {
  /// `Command::spawn` failed (binary not executable, etc.).
  Spawn(String),
  /// Log file could not be opened.
  Log(String),
}

impl std::fmt::Display for SpawnError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Spawn(s) => write!(f, "could not spawn llama-server: {s}"),
      Self::Log(s) => write!(f, "could not open log file: {s}"),
    }
  }
}

impl std::error::Error for SpawnError {}

fn ensure_parent(path: &std::path::Path) -> std::io::Result<()> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  Ok(())
}

async fn pump_stream<R>(
  mut reader: BufReader<R>,
  inner: Arc<ManagedInner>,
  log_file: Arc<Mutex<tokio::fs::File>>,
  source: &'static str,
) where
  R: tokio::io::AsyncRead + Unpin,
{
  let mut line = String::new();
  loop {
    line.clear();
    match reader.read_line(&mut line).await {
      Ok(0) => return,
      Ok(_) => {
        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
        let stamped = format!("[{source}] {trimmed}");
        inner.ring.lock().await.push(stamped.clone());
        let mut file = log_file.lock().await;
        let _ = file.write_all(stamped.as_bytes()).await;
        let _ = file.write_all(b"\n").await;
      }
      Err(e) => {
        log::warn!("supervisor: {source} stream read error: {e}");
        return;
      }
    }
  }
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
      log_path: PathBuf::from("/tmp/llamatui-test.log"),
      ready_at: RwLock::new(None),
      state: RwLock::new(initial),
      pid: RwLock::new(None),
      ring: Mutex::new(RingBuffer::with_capacity(16)),
      child: Mutex::new(None),
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
      !m.transition(ManagedState::Error {
        cause: "x".into()
      })
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
