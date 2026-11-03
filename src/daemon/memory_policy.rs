//! Live admission policy, persisted before a changed reserve is published.

use std::path::PathBuf;
use std::sync::{Arc, RwLock as SyncRwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::launch::{admission, headroom::MemoryReserve};

#[derive(Clone)]
pub struct MemoryPolicy {
  inner: Arc<Inner>,
}

struct Inner {
  reserve: SyncRwLock<MemoryReserve>,
  config_path: Option<PathBuf>,
  update: std::sync::Mutex<()>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
  pub unified_memory_reserve_percent: MemoryReserve,
  pub default_reserve_percent: Option<u8>,
  pub unified: bool,
  pub available_bytes: Option<u64>,
  pub admissible_bytes: Option<u64>,
}

impl Default for MemoryPolicy {
  fn default() -> Self {
    Self::new(MemoryReserve::AUTO, None)
  }
}

impl MemoryPolicy {
  pub fn new(reserve: MemoryReserve, config_path: Option<PathBuf>) -> Self {
    Self {
      inner: Arc::new(Inner {
        reserve: SyncRwLock::new(reserve),
        config_path,
        update: std::sync::Mutex::new(()),
      }),
    }
  }

  pub fn reserve(&self) -> MemoryReserve {
    *self
      .inner
      .reserve
      .read()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
  }

  pub async fn set(&self, reserve: MemoryReserve) -> anyhow::Result<()> {
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
        serde_json::json!({"daemon": {"unified_memory_reserve_percent": reserve}}),
      )?;
      crate::config::writer::merge_and_write(path, additions)?;
      *inner
        .reserve
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = reserve;
      Ok::<_, anyhow::Error>(())
    })
    .await?
  }

  pub async fn budget(&self, metrics: Option<&Arc<RwLock<HostMetricsSnapshot>>>) -> MemoryBudget {
    let host = match metrics {
      Some(metrics) => Some(metrics.read().await),
      None => None,
    };
    MemoryBudget::from_snapshot(self.reserve(), host.as_deref())
  }
}

impl MemoryBudget {
  pub fn from_snapshot(reserve: MemoryReserve, host: Option<&HostMetricsSnapshot>) -> Self {
    let default_reserve_percent = host
      .filter(|h| admission::is_sampled(h))
      .and_then(|h| admission::pool_kind(h).default_reserve_percent());
    let available_bytes = host
      .filter(|h| default_reserve_percent.is_some() && h.ram_total_bytes > 0)
      .map(|h| admission::available_pool_bytes(h, false));
    let mut budget = Self {
      unified_memory_reserve_percent: reserve,
      default_reserve_percent,
      unified: default_reserve_percent.is_some(),
      available_bytes,
      admissible_bytes: None,
    };
    budget.admissible_bytes = budget.preview(reserve);
    budget
  }

  pub fn supports_editing(&self) -> bool {
    self.unified
      && self
        .default_reserve_percent
        .is_some_and(|percent| percent <= 99)
  }

  pub fn preview(&self, reserve: MemoryReserve) -> Option<u64> {
    self
      .available_bytes
      .zip(self.default_reserve_percent.filter(|p| *p <= 99))
      .map(|(available, default)| reserve.admissible_bytes(available, default))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn snapshot_preview_matches_admission_for_each_pool_and_missing_samples() {
    const GIB: u64 = 1024 * 1024 * 1024;
    for (backend, unified, default) in [
      (HostMetricsSnapshot::BACKEND_APPLE_METAL, false, Some(25)),
      (HostMetricsSnapshot::BACKEND_AMD, true, Some(0)),
      (HostMetricsSnapshot::BACKEND_NVIDIA, true, Some(0)),
      (HostMetricsSnapshot::BACKEND_UNKNOWN, true, Some(0)),
      (HostMetricsSnapshot::BACKEND_NVIDIA, false, None),
      (HostMetricsSnapshot::BACKEND_CPU_ONLY, false, None),
      (HostMetricsSnapshot::UNINITIALIZED_BACKEND, true, None),
    ] {
      for used in [24 * GIB, 64 * GIB, 65 * GIB] {
        let host = HostMetricsSnapshot {
          gpu_backend: backend.into(),
          unified,
          ram_total_bytes: 64 * GIB,
          ram_used_bytes: used,
          uma_shared_total_bytes: Some(32 * GIB),
          uma_shared_used_bytes: Some(12 * GIB),
          ..Default::default()
        };
        for reserve in [
          MemoryReserve::AUTO,
          MemoryReserve::percent(0).unwrap(),
          MemoryReserve::percent(10).unwrap(),
          MemoryReserve::percent(99).unwrap(),
        ] {
          let budget = MemoryBudget::from_snapshot(reserve, Some(&host));
          assert_eq!(budget.default_reserve_percent, default);
          assert_eq!(budget.supports_editing(), default.is_some());
          assert_eq!(budget.preview(reserve), budget.admissible_bytes);
          if default.is_some() {
            assert_eq!(
              budget.available_bytes,
              Some((64 * GIB).saturating_sub(used).min(20 * GIB))
            );
            assert_eq!(
              budget.admissible_bytes,
              Some(admission::effective_free_bytes(&host, reserve))
            );
          } else {
            assert_eq!(budget.available_bytes, None);
            assert_eq!(budget.admissible_bytes, None);
          }
        }
      }
    }
    for host in [None, Some(&HostMetricsSnapshot::default())] {
      let budget = MemoryBudget::from_snapshot(MemoryReserve::AUTO, host);
      assert!(!budget.supports_editing());
      assert_eq!(budget.admissible_bytes, None);
    }
  }

  #[tokio::test]
  async fn empty_root_config_save_survives_reload() {
    for original in [
      "{}\n",
      "# before\n{} # defaults\n# after\n",
      "---\n{}\n...\n",
    ] {
      let dir = crate::util::test_temp::unique_temp_dir("memory-empty-root");
      std::fs::create_dir_all(&dir).unwrap();
      let path = dir.join("config.yaml");
      std::fs::write(&path, original).unwrap();
      let policy = MemoryPolicy::new(MemoryReserve::AUTO, Some(path.clone()));
      policy
        .set(MemoryReserve::percent(10).unwrap())
        .await
        .unwrap();
      let text = std::fs::read_to_string(&path).unwrap();
      let config: crate::config::Config =
        yaml_serde::from_str(&text).expect("a saved default config must remain loadable");
      assert_eq!(
        config.daemon.unified_memory_reserve_percent,
        policy.reserve()
      );
      for comment in ["# before", "# defaults", "# after"] {
        if original.contains(comment) {
          assert!(text.contains(comment), "{text}");
        }
      }
      std::fs::remove_dir_all(dir).unwrap();
    }
  }

  #[tokio::test]
  async fn nonempty_flow_root_is_refused_without_changing_file_or_policy() {
    let dir = crate::util::test_temp::unique_temp_dir("memory-flow-root");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let original = "{theme: latte}\n";
    std::fs::write(&path, original).unwrap();
    let policy = MemoryPolicy::new(MemoryReserve::AUTO, Some(path.clone()));
    assert!(policy
      .set(MemoryReserve::percent(10).unwrap())
      .await
      .is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(policy.reserve(), MemoryReserve::AUTO);
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn reset_to_auto_survives_reload_and_concurrent_preset_edits() {
    let dir = crate::util::test_temp::unique_temp_dir("memory-policy-concurrent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
      &path,
      "# preserve me\ndaemon:\n  unified_memory_reserve_percent: 10\n",
    )
    .unwrap();
    let policy = MemoryPolicy::new(MemoryReserve::percent(10).unwrap(), Some(path.clone()));
    let preset_path = path.clone();
    let preset = tokio::task::spawn_blocking(move || {
      crate::config::presets_writer::upsert_preset(
        &preset_path,
        "model",
        "test",
        &serde_json::json!({"knobs": {"ctx-size": 1024}}),
      )
    });
    let (save, preset_result) = tokio::join!(policy.set(MemoryReserve::AUTO), preset);
    save.unwrap();
    preset_result.unwrap().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# preserve me"));
    let config: crate::config::Config = yaml_serde::from_str(&text).unwrap();
    assert_eq!(
      config.daemon.unified_memory_reserve_percent,
      MemoryReserve::AUTO
    );
    assert_eq!(policy.reserve(), MemoryReserve::AUTO);
    assert!(config.presets["model"].entries.contains_key("test"));
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn a_failed_write_does_not_change_live_admission() {
    let policy = MemoryPolicy::default();
    assert!(policy
      .set(MemoryReserve::percent(10).unwrap())
      .await
      .is_err());
    assert_eq!(policy.reserve(), MemoryReserve::AUTO);
    assert!(MemoryReserve::percent(100).is_err());
    assert_eq!(policy.reserve(), MemoryReserve::AUTO);
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
    let policy = MemoryPolicy::new(MemoryReserve::AUTO, Some(path.clone()));
    policy
      .set(MemoryReserve::percent(10).unwrap())
      .await
      .unwrap();
    assert_eq!(policy.reserve(), MemoryReserve::percent(10).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this") && text.contains("# keep too"));
    let config: crate::config::Config = yaml_serde::from_str(&text).unwrap();
    assert_eq!(
      config.daemon.unified_memory_reserve_percent,
      MemoryReserve::percent(10).unwrap()
    );
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
    let policy = MemoryPolicy::new(MemoryReserve::AUTO, Some(path.clone()));
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
      let _guard = crate::config::writer::CONFIG_MUTATION.lock().unwrap();
      locked_tx.send(()).unwrap();
      release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    locked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let saving = policy.clone();
    let caller = tokio::spawn(async move { saving.set(MemoryReserve::percent(10).unwrap()).await });
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
      while policy.reserve() != MemoryReserve::percent(10).unwrap() {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .unwrap();
    let config: crate::config::Config =
      yaml_serde::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
      config.daemon.unified_memory_reserve_percent,
      policy.reserve()
    );
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[tokio::test]
  async fn memory_reserve_write_error_preserves_live_policy_and_file() {
    let dir = crate::util::test_temp::unique_temp_dir("memory-reserve-write-error");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let original = "daemon: [invalid-container]\n";
    std::fs::write(&path, original).unwrap();
    let policy = MemoryPolicy::new(MemoryReserve::AUTO, Some(path.clone()));
    assert!(policy
      .set(MemoryReserve::percent(10).unwrap())
      .await
      .is_err());
    assert_eq!(policy.reserve(), MemoryReserve::AUTO);
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
      .with_memory_policy(MemoryPolicy::new(MemoryReserve::AUTO, Some(path.clone())));
    ctx.host_metrics = Some(Arc::new(RwLock::new(HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_APPLE_METAL.into(),
      unified: true,
      ram_total_bytes: 64 * 1024 * 1024 * 1024,
      ram_used_bytes: 24 * 1024 * 1024 * 1024,
      ..Default::default()
    })));
    for params in [
      json!({}),
      json!({"unified_memory_reserve_percent": 100}),
      json!({"unified_memory_reserve_percent": -1}),
      json!({"unified_memory_reserve_percent": 10.5}),
      json!({"unified_memory_reserve_percent": null}),
      json!({"unified_memory_reserve_percent": "10"}),
      json!({"unified_memory_reserve_percent": true}),
      json!({"unified_memory_reserve_percent": 10, "typo": true}),
    ] {
      let response =
        dispatch_request(&ctx, Request::new(1, "memory_reserve_set", Some(params))).await;
      assert_eq!(response.error.unwrap().code, -32602);
      assert_eq!(ctx.memory_policy.reserve(), MemoryReserve::AUTO);
    }
    let snapshot = ctx.host_metrics.as_ref().unwrap().read().await.clone();
    let demand = 34 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;
    let before =
      crate::launch::admission::effective_free_bytes(&snapshot, ctx.memory_policy.reserve());
    assert!(ctx.admission.try_admit(1, demand, before).is_err());
    let response = dispatch_request(
      &ctx,
      Request::new(
        2,
        "memory_reserve_set",
        Some(json!({"unified_memory_reserve_percent": 10})),
      ),
    )
    .await;
    assert!(response.error.is_none(), "{response:?}");
    let saved: MemoryBudget = serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(saved.supports_editing());
    assert_eq!(
      saved.unified_memory_reserve_percent,
      MemoryReserve::percent(10).unwrap()
    );
    let after =
      crate::launch::admission::effective_free_bytes(&snapshot, ctx.memory_policy.reserve());
    assert!(ctx.admission.try_admit(2, demand, after).is_ok());
    let status = dispatch_request(&ctx, Request::new(3, "status", None))
      .await
      .result
      .unwrap();
    assert_eq!(
      status["daemon"]["memory"]["unified_memory_reserve_percent"],
      10
    );
    assert_eq!(
      status["daemon"]["memory"]["admissible_bytes"],
      36 * 1024u64 * 1024 * 1024
    );
    let reloaded: crate::config::Config =
      yaml_serde::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
      reloaded.daemon.unified_memory_reserve_percent,
      MemoryReserve::percent(10).unwrap()
    );
    std::fs::remove_dir_all(dir).unwrap();
  }
}
