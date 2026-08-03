//! Opt-in idle auto-shutdown (`daemon.idle_timeout_secs`).
//!
//! A background poller fires the shutdown token once the daemon has had
//! nothing to do for the configured span, so a forgotten `daemon stop`
//! doesn't keep a laptop awake. Disabled by default.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::registry::{LaunchId, SupervisorRegistry};
use super::shutdown::ShutdownToken;

/// Ceiling on how often idleness is sampled. Coarse on purpose: this
/// decides when to *stop* doing nothing, so polling harder costs more
/// than the precision is worth. A timeout shorter than this polls at the
/// timeout instead, so a small `idle_timeout_secs` isn't rounded up to
/// two full poll periods.
const MAX_POLL: Duration = Duration::from_secs(5);

/// Is the daemon doing nothing a user would miss?
///
/// Two signals: no servable model running, and no client attached. The
/// TUI and CLI hold control-plane connections while they're open, so a
/// user watching the dashboard keeps the daemon alive even with every
/// model stopped.
///
/// Infra launches — a managed multiplexer's shared umbrella, which the
/// daemon brings up itself at boot — don't count as work. They'd
/// otherwise pin the daemon awake forever on any host running one.
fn is_idle(launches: &[LaunchId], connections: &AtomicUsize) -> bool {
  connections.load(Ordering::Relaxed) == 0 && launches.iter().all(crate::backend::is_infra_launch)
}

/// Poll for idleness and trigger `token` once it has held for `timeout`.
///
/// The returned handle is not awaited by the daemon: the task exits on
/// its own after triggering, and is dropped with the runtime otherwise.
pub(super) fn spawn(
  supervisors: SupervisorRegistry,
  connections: Arc<AtomicUsize>,
  token: ShutdownToken,
  timeout: Duration,
) -> JoinHandle<()> {
  let poll = MAX_POLL.min(timeout);
  tokio::spawn(async move {
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
      tokio::select! {
        _ = token.wait_until_triggered() => return,
        _ = tokio::time::sleep(poll) => {}
      }
      if !is_idle(&supervisors.launch_ids().await, &connections) {
        idle_since = None;
        continue;
      }
      match idle_since {
        Some(since) if since.elapsed() >= timeout => {
          log::info!(
            "idle for {}s with no running models and no clients attached; shutting down",
            timeout.as_secs()
          );
          token.trigger();
          return;
        }
        Some(_) => {}
        None => idle_since = Some(tokio::time::Instant::now()),
      }
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Resolved the way [`crate::backend::umbrella_owner`] does, so the
  /// test names no backend.
  fn umbrella_id() -> LaunchId {
    crate::backend::Backends::all()
      .into_iter()
      .find_map(|b| crate::backend::Backend::umbrella_launch_id(&b))
      .expect("a managed-multiplexer backend exposing an umbrella launch id")
  }

  fn no_clients() -> AtomicUsize {
    AtomicUsize::new(0)
  }

  #[test]
  fn nothing_running_and_no_clients_is_idle() {
    assert!(is_idle(&[], &no_clients()));
  }

  #[test]
  fn an_attached_client_is_not_idle() {
    assert!(!is_idle(&[], &AtomicUsize::new(1)));
  }

  #[test]
  fn a_running_model_is_not_idle() {
    assert!(!is_idle(&[LaunchId("L1".into())], &no_clients()));
  }

  /// The shared umbrella is daemon-owned infra, not user work — a host
  /// running one must still be able to idle out. Regression: keying on
  /// a bare "registry is empty" check never idles on such a host.
  #[test]
  fn an_infra_umbrella_alone_is_still_idle() {
    let umbrella = umbrella_id();
    assert!(crate::backend::is_infra_launch(&umbrella));
    assert!(is_idle(&[umbrella], &no_clients()));
  }

  #[test]
  fn an_infra_umbrella_alongside_a_real_model_is_not_idle() {
    assert!(!is_idle(
      &[umbrella_id(), LaunchId("L1".into())],
      &no_clients()
    ));
  }

  /// `spawn` polls at `min(MAX_POLL, timeout)`, so a short timeout keeps
  /// these real-time tests quick without a paused-clock dependency.
  const TICK: Duration = Duration::from_millis(50);

  #[tokio::test]
  async fn triggers_shutdown_once_idle_outlasts_the_timeout() {
    let token = ShutdownToken::new();
    let handle = spawn(
      SupervisorRegistry::new(),
      Arc::new(AtomicUsize::new(0)),
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
    let token = ShutdownToken::new();
    let handle = spawn(
      SupervisorRegistry::new(),
      Arc::new(AtomicUsize::new(0)),
      token.clone(),
      Duration::from_secs(3600),
    );
    tokio::time::sleep(TICK * 4).await;
    assert!(!token.is_triggered());
    handle.abort();
  }

  /// A client attached the whole time must keep the daemon alive — the
  /// countdown restarts on every non-idle poll.
  #[tokio::test]
  async fn an_attached_client_keeps_the_daemon_alive() {
    let token = ShutdownToken::new();
    let handle = spawn(
      SupervisorRegistry::new(),
      Arc::new(AtomicUsize::new(1)),
      token.clone(),
      TICK,
    );
    tokio::time::sleep(TICK * 10).await;
    assert!(!token.is_triggered());
    handle.abort();
  }

  /// The poller must not outlive a shutdown it didn't cause.
  #[tokio::test]
  async fn exits_when_the_daemon_shuts_down_for_another_reason() {
    let token = ShutdownToken::new();
    let handle = spawn(
      SupervisorRegistry::new(),
      Arc::new(AtomicUsize::new(0)),
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
