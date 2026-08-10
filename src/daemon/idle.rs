//! Opt-in idle auto-shutdown (`daemon.idle_timeout_secs`).
//!
//! A background poller fires the shutdown token once the daemon has had
//! nothing to do for the configured span, so a forgotten `daemon stop`
//! doesn't keep a laptop awake. Disabled by default.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use super::registry::SupervisorRegistry;
use super::shutdown::ShutdownToken;
use super::supervisor::ManagedState;

/// Ceiling on how often idleness is sampled. Coarse on purpose: this
/// decides when to *stop* doing nothing, so polling harder costs more
/// than the precision is worth. A timeout below this polls at the
/// timeout instead.
const MAX_POLL: Duration = Duration::from_secs(5);

/// Last-touched clock shared by every surface a user reaches the daemon
/// through.
///
/// Polling a connection *gauge* only sees clients attached at the
/// sampling instant, which misses the common shapes entirely: one-shot
/// CLI calls open and close between two polls, and proxy requests never
/// touch the control plane at all. Each surface stamps this instead, so
/// "idle" means *nothing has happened since* rather than *nothing is
/// happening right now*.
///
/// Cheap to clone (one `Arc`); stores millis from a per-process base so
/// a read is a relaxed atomic load.
#[derive(Clone, Debug)]
pub struct Activity {
  base: Instant,
  last_millis: Arc<AtomicU64>,
}

impl Activity {
  pub fn new() -> Self {
    Self {
      base: Instant::now(),
      last_millis: Arc::new(AtomicU64::new(0)),
    }
  }

  /// Record that something reached the daemon. Called on every
  /// control-plane and proxy connection, at open and at close, so both
  /// long-lived and one-shot clients register.
  pub fn mark(&self) {
    let now = self.base.elapsed().as_millis() as u64;
    self.last_millis.fetch_max(now, Ordering::Relaxed);
  }

  /// How long since the last [`Self::mark`].
  pub fn idle_for(&self) -> Duration {
    let last = self.last_millis.load(Ordering::Relaxed);
    self
      .base
      .elapsed()
      .saturating_sub(Duration::from_millis(last))
  }
}

impl Default for Activity {
  fn default() -> Self {
    Self::new()
  }
}

/// Is the daemon holding work a user would miss right now?
///
/// A launch counts only while it is *live*. A child that crashed or was
/// stopped leaves a terminal-state entry in the registry, and treating
/// that as work would pin the daemon awake forever — the opposite of
/// what a walk-away timeout is for.
///
/// Infra launches — a managed multiplexer's shared umbrella, which the
/// daemon brings up itself at boot — never count: they'd otherwise pin
/// every host running one.
async fn is_busy(supervisors: &SupervisorRegistry, connections: &AtomicUsize) -> bool {
  if connections.load(Ordering::Relaxed) > 0 {
    return true;
  }
  for (id, model) in supervisors.snapshot().await {
    if crate::backend::is_infra_launch(&id) {
      continue;
    }
    if !matches!(
      model.state().await,
      ManagedState::Stopped | ManagedState::Error { .. }
    ) {
      return true;
    }
  }
  false
}

/// Poll for idleness and trigger `token` once nothing has touched the
/// daemon for `timeout`.
///
/// The returned handle is not awaited by the daemon: the task exits on
/// its own after triggering, and is dropped with the runtime otherwise.
pub(super) fn spawn(
  supervisors: SupervisorRegistry,
  connections: Arc<AtomicUsize>,
  activity: Activity,
  token: ShutdownToken,
  timeout: Duration,
) -> JoinHandle<()> {
  let poll = MAX_POLL.min(timeout);
  tokio::spawn(async move {
    loop {
      tokio::select! {
        _ = token.wait_until_triggered() => return,
        _ = tokio::time::sleep(poll) => {}
      }
      // Ongoing work keeps the clock stamped, so the countdown always
      // runs from the last moment the daemon had something to do —
      // never from the poll that first noticed.
      if is_busy(&supervisors, &connections).await {
        activity.mark();
        continue;
      }
      if activity.idle_for() >= timeout {
        log::info!(
          "idle for {}s with no running models and no client or proxy traffic; shutting down",
          timeout.as_secs()
        );
        token.trigger();
        return;
      }
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::daemon::registry::LaunchId;
  use crate::daemon::supervisor::test_support;

  /// Resolved the way [`crate::backend::umbrella_owner`] does, so the
  /// test names no backend.
  fn umbrella_id() -> LaunchId {
    crate::backend::Backends::all()
      .into_iter()
      .find_map(|b| crate::backend::Backend::umbrella_launch_id(&b))
      .expect("a managed-multiplexer backend exposing an umbrella launch id")
  }

  fn idle_inputs() -> (SupervisorRegistry, Arc<AtomicUsize>) {
    (SupervisorRegistry::new(), Arc::new(AtomicUsize::new(0)))
  }

  #[tokio::test]
  async fn nothing_running_and_no_clients_is_not_busy() {
    let (supervisors, connections) = idle_inputs();
    assert!(!is_busy(&supervisors, &connections).await);
  }

  #[tokio::test]
  async fn an_attached_client_is_busy() {
    let (supervisors, connections) = idle_inputs();
    connections.store(1, Ordering::Relaxed);
    assert!(is_busy(&supervisors, &connections).await);
  }

  /// The shared umbrella is daemon-owned infra, not user work — a host
  /// running one must still idle out. Regression: a bare "registry is
  /// empty" check never idles on such a host.
  #[tokio::test]
  async fn an_infra_umbrella_alone_is_not_busy() {
    let (supervisors, connections) = idle_inputs();
    let umbrella = umbrella_id();
    assert!(crate::backend::is_infra_launch(&umbrella));
    supervisors
      .insert(umbrella, test_support::in_state(ManagedState::Ready))
      .await;
    assert!(!is_busy(&supervisors, &connections).await);
  }

  #[tokio::test]
  async fn a_live_model_is_busy() {
    let (supervisors, connections) = idle_inputs();
    supervisors
      .insert(
        LaunchId("L1".into()),
        test_support::in_state(ManagedState::Ready),
      )
      .await;
    assert!(is_busy(&supervisors, &connections).await);
  }

  /// Regression: a child that crashed or was stopped leaves a
  /// terminal-state row in the registry. Counting it as work pinned the
  /// daemon awake forever — exactly the walk-away case the timeout is
  /// for.
  #[tokio::test]
  async fn a_terminal_state_launch_is_not_busy() {
    for state in [
      ManagedState::Stopped,
      ManagedState::Error {
        cause: "child exited".into(),
      },
    ] {
      let (supervisors, connections) = idle_inputs();
      supervisors
        .insert(LaunchId("L1".into()), test_support::in_state(state.clone()))
        .await;
      assert!(
        !is_busy(&supervisors, &connections).await,
        "a {state:?} launch must not count as work"
      );
    }
  }

  #[tokio::test]
  async fn a_stopping_launch_is_still_busy() {
    let (supervisors, connections) = idle_inputs();
    supervisors
      .insert(
        LaunchId("L1".into()),
        test_support::in_state(ManagedState::Stopping),
      )
      .await;
    assert!(is_busy(&supervisors, &connections).await);
  }

  #[test]
  fn a_mark_resets_the_idle_clock() {
    let activity = Activity::new();
    std::thread::sleep(Duration::from_millis(30));
    assert!(activity.idle_for() >= Duration::from_millis(25));
    activity.mark();
    assert!(activity.idle_for() < Duration::from_millis(25));
  }

  /// `spawn` polls at `min(MAX_POLL, timeout)`, so a short timeout keeps
  /// these real-time tests quick without a paused-clock dependency.
  const TICK: Duration = Duration::from_millis(50);

  #[tokio::test]
  async fn triggers_shutdown_once_idle_outlasts_the_timeout() {
    let (supervisors, connections) = idle_inputs();
    let token = ShutdownToken::new();
    let handle = spawn(
      supervisors,
      connections,
      Activity::new(),
      token.clone(),
      TICK,
    );
    tokio::time::timeout(Duration::from_secs(10), handle)
      .await
      .expect("idle monitor should trigger well inside 10s")
      .expect("monitor task");
    assert!(token.is_triggered());
  }

  #[tokio::test]
  async fn does_not_trigger_before_the_timeout_elapses() {
    let (supervisors, connections) = idle_inputs();
    let token = ShutdownToken::new();
    let handle = spawn(
      supervisors,
      connections,
      Activity::new(),
      token.clone(),
      Duration::from_secs(3600),
    );
    tokio::time::sleep(TICK * 4).await;
    assert!(!token.is_triggered());
    handle.abort();
  }

  #[tokio::test]
  async fn an_attached_client_keeps_the_daemon_alive() {
    let (supervisors, connections) = idle_inputs();
    connections.store(1, Ordering::Relaxed);
    let token = ShutdownToken::new();
    let handle = spawn(
      supervisors,
      connections,
      Activity::new(),
      token.clone(),
      TICK,
    );
    tokio::time::sleep(TICK * 10).await;
    assert!(!token.is_triggered());
    handle.abort();
  }

  /// A live model keeps the daemon alive even with nothing attached.
  #[tokio::test]
  async fn a_running_model_keeps_the_daemon_alive() {
    let (supervisors, connections) = idle_inputs();
    supervisors
      .insert(
        LaunchId("L1".into()),
        test_support::in_state(ManagedState::Ready),
      )
      .await;
    let token = ShutdownToken::new();
    let handle = spawn(
      supervisors,
      connections,
      Activity::new(),
      token.clone(),
      TICK,
    );
    tokio::time::sleep(TICK * 10).await;
    assert!(!token.is_triggered());
    handle.abort();
  }

  /// Regression: sampling a connection *gauge* missed one-shot CLI calls
  /// and all proxy traffic, so a daemon under continuous use shut down.
  /// Traffic between polls now keeps it alive.
  #[tokio::test]
  async fn traffic_between_polls_keeps_the_daemon_alive() {
    let (supervisors, connections) = idle_inputs();
    let activity = Activity::new();
    let token = ShutdownToken::new();
    let handle = spawn(
      supervisors,
      connections,
      activity.clone(),
      token.clone(),
      TICK * 3,
    );
    // Never attached at a poll instant — only stamped in between.
    for _ in 0..12 {
      tokio::time::sleep(TICK).await;
      activity.mark();
    }
    assert!(!token.is_triggered());
    handle.abort();
  }

  /// The poller must not outlive a shutdown it didn't cause.
  #[tokio::test]
  async fn exits_when_the_daemon_shuts_down_for_another_reason() {
    let (supervisors, connections) = idle_inputs();
    let token = ShutdownToken::new();
    let handle = spawn(
      supervisors,
      connections,
      Activity::new(),
      token.clone(),
      Duration::from_secs(3600),
    );
    token.trigger();
    tokio::time::timeout(Duration::from_secs(10), handle)
      .await
      .expect("monitor should observe the shutdown token promptly")
      .expect("monitor task");
  }
}
