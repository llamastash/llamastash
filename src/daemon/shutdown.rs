//! Cooperative shutdown signalling.
//!
//! One `ShutdownToken` is shared by the accept loop, every per-connection
//! task, the signal-handler task, and the IPC `shutdown` method. Anyone
//! can trigger it; everyone learns about it on the same `Notify`. Triggers
//! are sticky: a late awaiter that hits `wait_until_triggered` after the
//! token has already been tripped returns immediately rather than blocking
//! forever.

use std::sync::{
  atomic::{AtomicBool, Ordering},
  Arc,
};

use tokio::sync::Notify;

/// Cheap-to-clone shutdown handle. Internally an `Arc` over the shared
/// state.
#[derive(Clone)]
pub struct ShutdownToken {
  inner: Arc<Inner>,
}

struct Inner {
  triggered: AtomicBool,
  notify: Notify,
}

impl ShutdownToken {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(Inner {
        triggered: AtomicBool::new(false),
        notify: Notify::new(),
      }),
    }
  }

  /// Mark the daemon as shutting down. Wakes every awaiter on the
  /// `Notify`. Idempotent — repeat calls are a no-op.
  pub fn trigger(&self) {
    self.inner.triggered.store(true, Ordering::SeqCst);
    self.inner.notify.notify_waiters();
  }

  pub fn is_triggered(&self) -> bool {
    self.inner.triggered.load(Ordering::SeqCst)
  }

  /// Wait until the token is triggered. Returns immediately if it has
  /// already been tripped (sticky behaviour — late awaiters don't block).
  pub async fn wait_until_triggered(&self) {
    loop {
      if self.is_triggered() {
        return;
      }
      let notified = self.inner.notify.notified();
      if self.is_triggered() {
        return;
      }
      notified.await;
      if self.is_triggered() {
        return;
      }
    }
  }
}

impl Default for ShutdownToken {
  fn default() -> Self {
    Self::new()
  }
}

/// Install SIGINT + SIGTERM handlers that trip `token` when either signal
/// arrives. Runs on a dedicated tokio task. Exits on first signal.
///
/// If handler installation fails, this triggers `token` itself so the
/// daemon does not silently degrade to a SIGINT-immune state — the
/// operator gets a clean refusal instead of a daemon they can't stop
/// without `kill -9`.
#[cfg(unix)]
pub fn install_signal_handlers(token: ShutdownToken) -> tokio::task::JoinHandle<()> {
  use tokio::signal::unix::{signal, SignalKind};

  tokio::spawn(async move {
    let mut sigint = match signal(SignalKind::interrupt()) {
      Ok(s) => s,
      Err(e) => {
        log::error!(
          "failed to install SIGINT handler: {e}; triggering shutdown so the daemon is not signal-immune"
        );
        token.trigger();
        return;
      }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
      Ok(s) => s,
      Err(e) => {
        log::error!(
          "failed to install SIGTERM handler: {e}; triggering shutdown so the daemon is not signal-immune"
        );
        token.trigger();
        return;
      }
    };
    tokio::select! {
      _ = sigint.recv() => log::info!("SIGINT received; initiating shutdown"),
      _ = sigterm.recv() => log::info!("SIGTERM received; initiating shutdown"),
    }
    token.trigger();
  })
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use tokio::time::timeout;

  use super::*;

  #[tokio::test]
  async fn new_token_is_not_triggered() {
    let t = ShutdownToken::new();
    assert!(!t.is_triggered());
  }

  #[tokio::test]
  async fn trigger_marks_token_and_wakes_waiters() {
    let t = ShutdownToken::new();
    let waiter = t.clone();
    let task = tokio::spawn(async move { waiter.wait_until_triggered().await });

    tokio::time::sleep(Duration::from_millis(10)).await;
    t.trigger();

    // Should complete promptly — long timeout is just to fail loud if the
    // notify path regresses.
    timeout(Duration::from_secs(2), task)
      .await
      .expect("waiter must wake within 2s")
      .expect("join handle");
    assert!(t.is_triggered());
  }

  #[tokio::test]
  async fn wait_returns_immediately_when_already_triggered() {
    let t = ShutdownToken::new();
    t.trigger();
    timeout(Duration::from_millis(100), t.wait_until_triggered())
      .await
      .expect("already-triggered token must not block the awaiter");
  }

  #[tokio::test]
  async fn trigger_is_idempotent() {
    let t = ShutdownToken::new();
    t.trigger();
    t.trigger();
    t.trigger();
    assert!(t.is_triggered());
  }

  #[tokio::test]
  async fn many_waiters_all_wake_on_single_trigger() {
    let t = ShutdownToken::new();
    let mut joins = Vec::new();
    for _ in 0..16 {
      let w = t.clone();
      joins.push(tokio::spawn(async move { w.wait_until_triggered().await }));
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    t.trigger();
    for j in joins {
      timeout(Duration::from_secs(2), j)
        .await
        .expect("each waiter must wake")
        .expect("join handle");
    }
  }
}
