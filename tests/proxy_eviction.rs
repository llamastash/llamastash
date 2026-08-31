//! Idle-TTL eviction integration tests.
//!
//! Spins up a real `fake_llama_server` supervisor, drives it through
//! one `eviction::sweep_once` pass, and asserts the supervisor lands
//! in `Stopping` / `Stopped` when it's an idle auto-start row and
//! stays `Ready` when it's manually-launched or has in-flight
//! requests. Mirrors `tests/proxy_fallback.rs`'s shape so the fixture
//! setup is familiar.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::config::loader::PortRange;
use llamastash::daemon::context::{LaunchEnv, MethodContext};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{
  spawn as supervisor_spawn, LaunchOrigin, ManagedModel, ManagedSpawn, ManagedState,
};
use llamastash::discovery::ModelCatalog;
use llamastash::gguf::identity::ModelId;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::eviction;
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use tokio::time::sleep;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pe", label)
}

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(30),
    timeout: Duration::from_secs(15),
  }
}

fn allocate_port() -> u16 {
  let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  l.local_addr().unwrap().port()
}

fn allocate_port_range() -> PortRange {
  llamastash::test_support::allocate_port_range(8)
}

async fn wait_for_ready(model: &ManagedModel) {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if matches!(model.state().await, ManagedState::Ready) {
      return;
    }
    if std::time::Instant::now() > deadline {
      panic!("supervisor never reached Ready");
    }
    sleep(Duration::from_millis(20)).await;
  }
}

/// Launch one fake_llama_server supervisor and register it.
async fn pre_launch(
  log_dir: &Path,
  registry: &SupervisorRegistry,
  origin: LaunchOrigin,
) -> ManagedModel {
  let port = allocate_port();
  let id = ModelId {
    path: PathBuf::from(format!("/tmp/ls-pe-{port}.gguf")),
    header_blake3: [0u8; 32],
  };
  let params = LaunchParams::new(PathBuf::from("/tmp/ls-pe.gguf"), LaunchMode::Chat);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = supervisor_spawn(ManagedSpawn {
    id,
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: log_dir.join("evict.log"),
    plan,
    origin,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_ready(&model).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;
  model
}

async fn build_state(registry: SupervisorRegistry, log_dir: &Path) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  let token = ShutdownToken::new();
  let env = LaunchEnv {
    binary: fake_binary(),
    port_range: allocate_port_range(),
    log_dir: log_dir.to_path_buf(),
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::with_catalog(token, catalog)
    .with_supervisors(registry)
    .with_launch_env(env);
  ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_evicts_idle_auto_start_supervisor() {
  let dir = unique_temp("autostart-idle");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let state = build_state(registry, &log_dir).await;

  // Stamp the MRU so the supervisor has an `Instant`, then let a tick
  // elapse so even a 1-ns TTL counts as "stale".
  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(5)).await;

  eviction::sweep_once(&state, Duration::from_nanos(1)).await;

  // `stop` is non-blocking; poll briefly until the watcher flips the
  // state to Stopping/Stopped.
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    match model.state().await {
      ManagedState::Stopping | ManagedState::Stopped => break,
      _ if std::time::Instant::now() > deadline => {
        panic!(
          "auto_start supervisor stayed in {:?} after eviction sweep",
          model.state().await,
        );
      }
      _ => sleep(Duration::from_millis(20)).await,
    }
  }
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_skips_manual_launched_supervisor() {
  let dir = unique_temp("manual-skip");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::Manual).await;
  let state = build_state(registry, &log_dir).await;

  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(5)).await;

  eviction::sweep_once(&state, Duration::from_nanos(1)).await;

  // Manual-origin supervisors must stay Ready regardless of TTL.
  sleep(Duration::from_millis(50)).await;
  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "manual launch was evicted: state={:?}",
    model.state().await,
  );
  let _ = model.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_skips_auto_start_with_inflight_request() {
  let dir = unique_temp("inflight-skip");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let state = build_state(registry, &log_dir).await;

  state.touch_mru(model.id()).await;
  // Take a guard — `inflight()` is now 1.
  let _guard = model.inflight_guard();
  assert_eq!(model.inflight(), 1);
  sleep(Duration::from_millis(5)).await;

  eviction::sweep_once(&state, Duration::from_nanos(1)).await;

  // Refcount-gated: supervisor must still be Ready even with a stale
  // MRU because a forward is in progress.
  sleep(Duration::from_millis(50)).await;
  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "in-flight auto_start was evicted: state={:?}",
    model.state().await,
  );

  // Now drop the guard; a follow-up sweep must evict.
  drop(_guard);
  assert_eq!(model.inflight(), 0);
  eviction::sweep_once(&state, Duration::from_nanos(1)).await;
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    match model.state().await {
      ManagedState::Stopping | ManagedState::Stopped => break,
      _ if std::time::Instant::now() > deadline => {
        panic!(
          "auto_start with inflight=0 should have evicted; state={:?}",
          model.state().await,
        );
      }
      _ => sleep(Duration::from_millis(20)).await,
    }
  }
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_skips_auto_start_within_ttl() {
  let dir = unique_temp("within-ttl");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let state = build_state(registry, &log_dir).await;

  state.touch_mru(model.id()).await;
  // 10 second TTL — the freshly-touched stamp is well within window.
  eviction::sweep_once(&state, Duration::from_secs(10)).await;

  sleep(Duration::from_millis(50)).await;
  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "auto_start within TTL was evicted: state={:?}",
    model.state().await,
  );
  let _ = model.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}
