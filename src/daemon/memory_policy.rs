//! Live admission policy, persisted before a changed reserve is published.

use std::path::PathBuf;
use std::sync::{
  atomic::{AtomicU8, Ordering},
  Arc,
};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::launch::headroom::DEFAULT_APPLE_RESERVE_PERCENT;

#[derive(Clone)]
pub struct MemoryPolicy {
  inner: Arc<Inner>,
}

struct Inner {
  percent: AtomicU8,
  config_path: Option<PathBuf>,
  update: std::sync::Mutex<()>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryBudget {
  pub apple_memory_reserve_percent: u8,
  pub apple_unified: bool,
  pub available_bytes: Option<u64>,
  pub admissible_bytes: Option<u64>,
}

impl Default for MemoryPolicy {
  fn default() -> Self {
    Self::new(DEFAULT_APPLE_RESERVE_PERCENT, None)
  }
}

impl MemoryPolicy {
  pub fn new(percent: u8, config_path: Option<PathBuf>) -> Self {
    Self {
      inner: Arc::new(Inner {
        percent: AtomicU8::new(percent),
        config_path,
        update: std::sync::Mutex::new(()),
      }),
    }
  }

  pub fn percent(&self) -> u8 {
    self.inner.percent.load(Ordering::Relaxed)
  }

  pub async fn set(&self, percent: u8) -> anyhow::Result<()> {
    anyhow::ensure!(
      percent <= 99,
      "apple_memory_reserve_percent must be between 0 and 99"
    );
    let inner = Arc::clone(&self.inner);
    // A disconnected caller must not leave the persisted and live policies apart.
    tokio::task::spawn_blocking(move || {
      let _update = inner
        .update
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
      let path = inner
        .config_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no config path is available; reserve was not changed"))?;
      let additions = yaml_serde::to_value(
        serde_json::json!({"daemon": {"apple_memory_reserve_percent": percent}}),
      )?;
      crate::config::writer::merge_and_write(path, additions)?;
      inner.percent.store(percent, Ordering::Relaxed);
      Ok::<_, anyhow::Error>(())
    })
    .await?
  }

  pub async fn budget(&self, metrics: Option<&Arc<RwLock<HostMetricsSnapshot>>>) -> MemoryBudget {
    let percent = self.percent();
    let mut budget = MemoryBudget {
      apple_memory_reserve_percent: percent,
      apple_unified: false,
      available_bytes: None,
      admissible_bytes: None,
    };
    if let Some(metrics) = metrics {
      let host = metrics.read().await;
      budget.apple_unified = host.gpu_backend == HostMetricsSnapshot::BACKEND_APPLE_METAL;
      if budget.apple_unified && host.ram_total_bytes > 0 {
        budget.available_bytes = Some(host.ram_total_bytes.saturating_sub(host.ram_used_bytes));
        budget.admissible_bytes = Some(crate::launch::admission::effective_free_bytes(
          &host, percent,
        ));
      }
    }
    budget
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn a_failed_write_does_not_change_live_admission() {
    let policy = MemoryPolicy::default();
    assert!(policy.set(10).await.is_err());
    assert_eq!(policy.percent(), 25);
    assert!(policy.set(100).await.is_err());
    assert_eq!(policy.percent(), 25);
  }

  #[tokio::test]
  async fn save_preserves_comments_and_survives_reload() {
    let dir = crate::util::test_temp::unique_temp_dir("memory-policy");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
      &path,
      "# keep this\ndaemon:\n  probe_timeout_secs: 300 # keep too\n",
    )
    .unwrap();
    let policy = MemoryPolicy::new(25, Some(path.clone()));
    policy.set(10).await.unwrap();
    assert_eq!(policy.percent(), 10);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this") && text.contains("# keep too"));
    let config: crate::config::Config = yaml_serde::from_str(&text).unwrap();
    assert_eq!(config.daemon.apple_memory_reserve_percent, 10);
    assert_eq!(config.daemon.probe_timeout_secs, 300);
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn memory_reserve_save_finishes_if_the_caller_disconnects() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = crate::util::test_temp::unique_temp_dir("memory-reserve-cancel");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let policy = MemoryPolicy::new(25, Some(path.clone()));
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
      let _guard = crate::config::writer::CONFIG_MUTATION.lock().unwrap();
      locked_tx.send(()).unwrap();
      release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    locked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let saving = policy.clone();
    let caller = tokio::spawn(async move { saving.set(10).await });
    tokio::time::timeout(Duration::from_secs(3), async {
      while policy.inner.update.try_lock().is_ok() {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .unwrap();
    caller.abort();
    let _ = caller.await;
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
      while policy.percent() != 10 {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .unwrap();
    let config: crate::config::Config =
      yaml_serde::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(config.daemon.apple_memory_reserve_percent, policy.percent());
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn memory_reserve_write_error_preserves_live_policy_and_file() {
    let dir = crate::util::test_temp::unique_temp_dir("memory-reserve-write-error");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let original = "daemon: [invalid-container]\n";
    std::fs::write(&path, original).unwrap();
    let policy = MemoryPolicy::new(25, Some(path.clone()));
    assert!(policy.set(10).await.is_err());
    assert_eq!(policy.percent(), 25);
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn memory_reserve_ipc_updates_the_live_budget_and_rejects_bad_values() {
    use crate::daemon::{context::MethodContext, shutdown::ShutdownToken};
    use crate::ipc::{methods::dispatch_request, protocol::Request};
    use serde_json::json;

    let dir = crate::util::test_temp::unique_temp_dir("memory-reserve-ipc");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, "theme: latte # keep\n").unwrap();
    let mut ctx = MethodContext::new(ShutdownToken::new())
      .with_memory_policy(MemoryPolicy::new(25, Some(path.clone())));
    ctx.host_metrics = Some(Arc::new(RwLock::new(HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_APPLE_METAL.into(),
      unified: true,
      ram_total_bytes: 64 * 1024 * 1024 * 1024,
      ram_used_bytes: 24 * 1024 * 1024 * 1024,
      ..Default::default()
    })));
    for params in [
      json!({}),
      json!({"apple_memory_reserve_percent": 100}),
      json!({"apple_memory_reserve_percent": -1}),
      json!({"apple_memory_reserve_percent": 10.5}),
      json!({"apple_memory_reserve_percent": 10, "typo": true}),
    ] {
      let response =
        dispatch_request(&ctx, Request::new(1, "memory_reserve_set", Some(params))).await;
      assert_eq!(response.error.unwrap().code, -32602);
      assert_eq!(ctx.memory_policy.percent(), 25);
    }
    let snapshot = ctx.host_metrics.as_ref().unwrap().read().await.clone();
    let demand = 34 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;
    let before =
      crate::launch::admission::effective_free_bytes(&snapshot, ctx.memory_policy.percent());
    assert!(ctx.admission.try_admit(1, demand, before).is_err());
    let response = dispatch_request(
      &ctx,
      Request::new(
        2,
        "memory_reserve_set",
        Some(json!({"apple_memory_reserve_percent": 10})),
      ),
    )
    .await;
    assert!(response.error.is_none(), "{response:?}");
    let after =
      crate::launch::admission::effective_free_bytes(&snapshot, ctx.memory_policy.percent());
    assert!(ctx.admission.try_admit(2, demand, after).is_ok());
    let status = dispatch_request(&ctx, Request::new(3, "status", None))
      .await
      .result
      .unwrap();
    assert_eq!(
      status["daemon"]["memory"]["apple_memory_reserve_percent"],
      10
    );
    assert_eq!(
      status["daemon"]["memory"]["admissible_bytes"],
      36 * 1024u64 * 1024 * 1024
    );
    let reloaded: crate::config::Config =
      yaml_serde::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(reloaded.daemon.apple_memory_reserve_percent, 10);
    std::fs::remove_dir_all(dir).unwrap();
  }
}
