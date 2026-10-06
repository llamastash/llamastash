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
use llamastash::daemon::context::{LaunchEnv, MethodContext, PersistedState};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::state_store::DaemonState;
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
  build_state_with(
    registry,
    log_dir,
    PersistedState::new(DaemonState::default(), None),
  )
  .await
}

async fn build_state_with(
  registry: SupervisorRegistry,
  log_dir: &Path,
  persisted: PersistedState,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  let token = ShutdownToken::new();
  let env = LaunchEnv {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    log_dir: log_dir.to_path_buf(),
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: Default::default(),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::with_catalog(token, catalog)
    .with_supervisors(registry)
    .with_state(persisted)
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

/// An evicted launch must leave the registry and `state.running` the way an
/// explicit stop does. A leftover row keeps holding its launch name, so the next
/// `<model>@<name>` auto-start was refused with "already running as L2".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_drops_the_launch_and_its_running_row() {
  let dir = unique_temp("autostart-prune");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let (launch_id, _) = registry.snapshot().await.remove(0);
  let row = llamastash::test_support::running_row("/tmp/ls-pe.gguf")
    .launch_id(launch_id.as_str())
    .port(model.port())
    .name("coder")
    .build();
  let persisted = PersistedState::new(
    DaemonState {
      running: vec![row],
      ..Default::default()
    },
    None,
  );
  let state = build_state_with(registry.clone(), &log_dir, persisted.clone()).await;

  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(5)).await;
  eviction::sweep_once(&state, Duration::from_nanos(1)).await;

  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    let gone = registry.len().await == 0 && persisted.snapshot().await.running.is_empty();
    if gone {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "evicted launch still registered or persisted: registry={}, running={:?}",
      registry.len().await,
      persisted.snapshot().await.running,
    );
    sleep(Duration::from_millis(20)).await;
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

// ---------------------------------------------------------------------------
// Residency: per-preset idle TTL (U1) and unloading to make room (U2).
// Plan: docs/plans/2026-09-30-001-feat-model-residency-plan.md.
// ---------------------------------------------------------------------------

use llamastash::config::{ConfigPresetBlock, PresetBody};
use llamastash::daemon::preset_store::ConfigPresetStore;
use llamastash::launch::admission::Refusal;

/// Every fixture launch reads this path, and it doubles as the `presets:` key
/// the residency presets below are written under.
const MODEL_PATH: &str = "/tmp/ls-pe.gguf";

/// A preset store holding one entry for [`MODEL_PATH`] named `name`, carrying
/// the residency fields under test.
fn preset_store(name: &str, body: PresetBody) -> ConfigPresetStore {
  ConfigPresetStore::new(
    BTreeMap::from([(
      MODEL_PATH.to_string(),
      ConfigPresetBlock {
        default: None,
        entries: BTreeMap::from([(name.to_string(), body)]),
      },
    )]),
    None,
  )
}

async fn build_state_with_presets(
  registry: SupervisorRegistry,
  log_dir: &Path,
  persisted: PersistedState,
  presets: ConfigPresetStore,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  let token = ShutdownToken::new();
  let env = LaunchEnv {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    log_dir: log_dir.to_path_buf(),
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: Default::default(),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::with_catalog(token, catalog)
    .with_supervisors(registry)
    .with_state(persisted)
    .with_presets(presets)
    .with_launch_env(env);
  ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES)
}

/// Register `model`'s launch in `persisted` the way a real launch does, so the
/// sweep and make-room can see the preset it resolved and the demand the gate
/// priced it at.
async fn stamp_row(
  persisted: &PersistedState,
  registry: &SupervisorRegistry,
  model: &ManagedModel,
  preset: Option<&str>,
  demand: u64,
) {
  let snapshot = registry.snapshot().await;
  let launch_id = snapshot
    .iter()
    .find(|(_, m)| m.port() == model.port())
    .expect("launch is registered")
    .0
    .clone();
  let mut row = llamastash::test_support::running_row(MODEL_PATH)
    .launch_id(launch_id.as_str())
    .port(model.port())
    .projected_demand(demand);
  if let Some(p) = preset {
    row = row.preset(p);
  }
  let row = row.build();
  persisted.mutate(move |s| s.running.push(row)).await;
}

/// Poll until `model` leaves Ready (evicted / stopped).
async fn wait_until_not_ready(model: &ManagedModel, context: &str) {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if !matches!(model.state().await, ManagedState::Ready) {
      return;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "{context}: stayed Ready"
    );
    sleep(Duration::from_millis(20)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_skips_a_launch_whose_preset_pins_idle_ttl_zero() {
  let dir = unique_temp("preset-ttl-never");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &model, Some("forever"), 1).await;
  let state = build_state_with_presets(
    registry,
    &log_dir,
    persisted,
    preset_store(
      "forever",
      PresetBody {
        idle_ttl_secs: Some(0),
        ..Default::default()
      },
    ),
  )
  .await;

  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(5)).await;
  // A 1 ns global TTL would evict anything else.
  eviction::sweep_once(&state, Duration::from_nanos(1)).await;
  sleep(Duration::from_millis(100)).await;
  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "a preset pinning idle_ttl_secs: 0 was unloaded: {:?}",
    model.state().await,
  );
  let _ = model.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_evicts_on_a_preset_idle_ttl_shorter_than_the_global() {
  let dir = unique_temp("preset-ttl-short");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &model, Some("quick"), 1).await;
  let state = build_state_with_presets(
    registry,
    &log_dir,
    persisted,
    preset_store(
      "quick",
      PresetBody {
        idle_ttl_secs: Some(1),
        ..Default::default()
      },
    ),
  )
  .await;

  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(1_100)).await;
  // The 1 s preset TTL governs, not the hour-long global: an hour of global
  // TTL must not keep this launch resident for an hour.
  eviction::sweep_once(&state, Duration::from_secs(3600)).await;
  wait_until_not_ready(&model, "preset TTL of 1 s elapsed").await;
  std::fs::remove_dir_all(&dir).ok();
}

/// A refusal bigger than everything idle cannot be satisfied: make-room stops
/// nothing rather than unloading models for a launch that still will not fit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_room_stops_nothing_when_the_shortfall_exceeds_every_candidate() {
  let dir = unique_temp("make-room-all-or-nothing");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let model = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &model, None, 100).await;
  let state =
    build_state_with_presets(registry, &log_dir, persisted, ConfigPresetStore::empty()).await;
  state.touch_mru(model.id()).await;

  let fits = eviction::make_room(
    &state,
    &Refusal {
      demand_bytes: 1_000,
      effective_free_bytes: 100,
      reserved_bytes: 0,
    },
  )
  .await;
  assert!(!fits, "800 needed against 100 freeable must refuse");
  sleep(Duration::from_millis(100)).await;
  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "the all-or-nothing check unloaded a launch anyway"
  );
  let _ = model.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_room_unloads_the_least_recently_used_and_stops_there() {
  let dir = unique_temp("make-room-lru");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  // `stale` is never touched, so it is the least recently used; `fresh` is the
  // one a request just served.
  let stale = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let fresh = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &stale, None, 500).await;
  stamp_row(&persisted, &registry, &fresh, None, 500).await;
  let state =
    build_state_with_presets(registry, &log_dir, persisted, ConfigPresetStore::empty()).await;
  state.touch_mru(fresh.id()).await;

  let fits = eviction::make_room(
    &state,
    &Refusal {
      demand_bytes: 400,
      effective_free_bytes: 100,
      reserved_bytes: 0,
    },
  )
  .await;
  assert!(fits, "300 needed, 1000 idle freeable");
  wait_until_not_ready(&stale, "least recently used launch").await;
  assert!(
    matches!(fresh.state().await, ManagedState::Ready),
    "make-room unloaded more than the shortfall needed"
  );
  let _ = fresh.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// Manual launches (which is what a preload starts) and presets pinned to
/// `idle_ttl_secs: 0` are never candidates, so a refusal that only they could
/// cover is refused outright.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_room_never_picks_a_manual_or_never_unload_launch() {
  for (origin, preset) in [
    (LaunchOrigin::Manual, None),
    (LaunchOrigin::AutoStart, Some("forever")),
  ] {
    let dir = unique_temp("make-room-exempt");
    let log_dir = dir.join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let registry = SupervisorRegistry::new();
    let model = pre_launch(&log_dir, &registry, origin).await;
    let persisted = PersistedState::new(DaemonState::default(), None);
    stamp_row(&persisted, &registry, &model, preset, 10_000).await;
    let presets = match preset {
      Some(name) => preset_store(
        name,
        PresetBody {
          idle_ttl_secs: Some(0),
          ..Default::default()
        },
      ),
      None => ConfigPresetStore::empty(),
    };
    let state = build_state_with_presets(registry, &log_dir, persisted, presets).await;
    state.touch_mru(model.id()).await;

    let fits = eviction::make_room(
      &state,
      &Refusal {
        demand_bytes: 5_000,
        effective_free_bytes: 0,
        reserved_bytes: 0,
      },
    )
    .await;
    assert!(
      !fits,
      "{origin:?} preset={preset:?} was treated as a candidate"
    );
    sleep(Duration::from_millis(100)).await;
    assert!(
      matches!(model.state().await, ManagedState::Ready),
      "{origin:?} preset={preset:?} was unloaded"
    );
    let _ = model.stop(Duration::from_secs(2)).await;
    std::fs::remove_dir_all(&dir).ok();
  }
}

/// A candidate that takes a request between selection and the stop is skipped,
/// not unloaded under the request. The shortfall here needs one launch; the
/// least-recently-used one is the busy one, so make-room must move on to the
/// next candidate rather than giving up or evicting the active launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_room_skips_a_candidate_that_took_a_request() {
  let dir = unique_temp("make-room-busy");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let busy = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let idle = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &busy, None, 500).await;
  stamp_row(&persisted, &registry, &idle, None, 500).await;
  let state =
    build_state_with_presets(registry, &log_dir, persisted, ConfigPresetStore::empty()).await;
  // `busy` is the least recently used, so it is picked first — then it takes a
  // request before the stop lands.
  state.touch_mru(busy.id()).await;
  state.touch_mru(idle.id()).await;
  let _guard = busy.inflight_guard();
  assert_eq!(busy.inflight(), 1);

  let fits = eviction::make_room(
    &state,
    &Refusal {
      demand_bytes: 550,
      effective_free_bytes: 100,
      reserved_bytes: 0,
    },
  )
  .await;
  assert!(fits, "450 needed, the idle launch alone covers 500");
  assert!(
    matches!(busy.state().await, ManagedState::Ready),
    "a launch with a request in flight was unloaded"
  );
  wait_until_not_ready(&idle, "the idle candidate after the busy one").await;
  drop(_guard);
  assert_eq!(busy.inflight(), 0);
  let _ = busy.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ── slot save: the prompt cache kept across an eviction ──────────────────

/// A proxy state, and the context behind it, whose llama.cpp config has slot
/// saving on with a floor low enough for the fixture's word-sized tokens.
async fn build_slot_save_state(log_dir: &Path) -> (Arc<ProxyState>, MethodContext) {
  let mut backend = llamastash::backend::BackendConfig::default();
  backend.llamacpp.slot_save.enabled = true;
  backend.llamacpp.slot_save.min_tokens = 4;
  let env = LaunchEnv {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    log_dir: log_dir.to_path_buf(),
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: Default::default(),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::with_catalog(ShutdownToken::new(), ModelCatalog::new())
    .with_supervisors(SupervisorRegistry::new())
    .with_state(PersistedState::new(DaemonState::default(), None))
    .with_launch_env(env)
    .with_backend(backend, BTreeMap::new());
  let state = ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES);
  (state, ctx)
}

/// Launch `model_path` the way an auto-start does, with the launch knobs the
/// llama.cpp backend seeds from config. `child_env` reaches the fixture.
async fn launch_seeded(
  ctx: &MethodContext,
  log_dir: &Path,
  model_path: &Path,
  child_env: &[(&str, &str)],
) -> ManagedModel {
  use llamastash::backend::Backend;
  let port = allocate_port();
  let backend = LlamaCppBackend::new();
  let mut params = LaunchParams::new(model_path.to_path_buf(), LaunchMode::Chat);
  backend.seed_launch_knobs(ctx, &mut params);
  let mut plan = backend.process_spec(&params, port, fake_binary(), fast_probe());
  for (key, value) in child_env {
    plan.env.push((key.to_string(), value.into()));
  }
  let model = supervisor_spawn(ManagedSpawn {
    id: ModelId {
      path: model_path.to_path_buf(),
      header_blake3: [7u8; 32],
    },
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: log_dir.join(format!("slot-{port}.log")),
    plan,
    origin: LaunchOrigin::AutoStart,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_ready(&model).await;
  let launch_id = ctx.supervisors.next_id();
  ctx.supervisors.insert(launch_id, model.clone()).await;
  model
}

/// One completion straight to the fixture; returns `timings.cache_n`.
async fn complete(port: u16, prompt: &str) -> u64 {
  let body: serde_json::Value = reqwest::Client::new()
    .post(format!("http://127.0.0.1:{port}/completion"))
    .json(&serde_json::json!({ "prompt": prompt, "n_predict": 1, "cache_prompt": true }))
    .send()
    .await
    .expect("completion request")
    .json()
    .await
    .expect("completion json");
  body["timings"]["cache_n"].as_u64().expect("cache_n")
}

fn saved_slot_files(dir: &Path) -> Vec<String> {
  let mut names: Vec<String> = std::fs::read_dir(dir.join("slots"))
    .map(|entries| {
      entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
    })
    .unwrap_or_default();
  names.sort();
  names
}

async fn sweep_and_wait_until_unregistered(
  state: &Arc<ProxyState>,
  ctx: &MethodContext,
  model: &ManagedModel,
) {
  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(5)).await;
  eviction::sweep_once(state, Duration::from_nanos(1)).await;
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  while ctx.supervisors.len().await != 0 {
    assert!(
      std::time::Instant::now() < deadline,
      "launch still registered after the sweep: {:?}",
      model.state().await
    );
    sleep(Duration::from_millis(20)).await;
  }
}

const CONVERSATION: &str = "one two three four five six seven eight nine ten eleven twelve";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_evicted_launch_gets_its_slot_back_on_the_next_start() {
  let dir = unique_temp("slot-roundtrip");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let (state, ctx) = build_slot_save_state(&log_dir).await;
  let model_path = dir.join("roundtrip.gguf");

  let first = launch_seeded(&ctx, &log_dir, &model_path, &[]).await;
  // The reuse probe ran before Ready and cleaned up after itself.
  assert_eq!(saved_slot_files(&dir), Vec::<String>::new());
  assert_eq!(complete(first.port(), CONVERSATION).await, 0);

  sweep_and_wait_until_unregistered(&state, &ctx, &first).await;
  let saved = saved_slot_files(&dir);
  assert_eq!(saved.len(), 1, "one slot file expected, got {saved:?}");
  assert!(
    saved[0].starts_with("roundtrip.") && saved[0].ends_with(".slot0.bin"),
    "unexpected slot file name {saved:?}"
  );

  // Another port, same model and settings: the file is restored before Ready
  // and consumed, and the returning conversation is served from the slot.
  let second = launch_seeded(&ctx, &log_dir, &model_path, &[]).await;
  assert_eq!(saved_slot_files(&dir), Vec::<String>::new());
  let returning = format!("{CONVERSATION} thirteen");
  assert_eq!(complete(second.port(), &returning).await, 12);

  second.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_saved_when_the_engine_would_discard_a_restored_slot() {
  let dir = unique_temp("slot-noreuse");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let (state, ctx) = build_slot_save_state(&log_dir).await;
  let model_path = dir.join("noreuse.gguf");

  let model = launch_seeded(
    &ctx,
    &log_dir,
    &model_path,
    &[("FAKE_LLAMA_NO_PREFIX_REUSE", "1")],
  )
  .await;
  complete(model.port(), CONVERSATION).await;
  sweep_and_wait_until_unregistered(&state, &ctx, &model).await;

  assert_eq!(saved_slot_files(&dir), Vec::<String>::new());
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_below_the_token_floor_is_not_saved() {
  let dir = unique_temp("slot-small");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let (state, ctx) = build_slot_save_state(&log_dir).await;
  let model_path = dir.join("small.gguf");

  let model = launch_seeded(&ctx, &log_dir, &model_path, &[]).await;
  complete(model.port(), "one two three").await;
  sweep_and_wait_until_unregistered(&state, &ctx, &model).await;

  assert_eq!(saved_slot_files(&dir), Vec::<String>::new());
  std::fs::remove_dir_all(&dir).ok();
}

/// The save can take seconds. A request that lands while it runs must keep the
/// launch: the idle check that picked it is stale by the time the save returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_that_arrives_during_the_save_keeps_the_launch() {
  let dir = unique_temp("slot-busy");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let (state, ctx) = build_slot_save_state(&log_dir).await;
  let model_path = dir.join("busy.gguf");

  let model = launch_seeded(
    &ctx,
    &log_dir,
    &model_path,
    &[("FAKE_LLAMA_SLOT_SAVE_DELAY_MS", "800")],
  )
  .await;
  complete(model.port(), CONVERSATION).await;

  state.touch_mru(model.id()).await;
  sleep(Duration::from_millis(5)).await;
  eviction::sweep_once(&state, Duration::from_nanos(1)).await;
  sleep(Duration::from_millis(200)).await;
  let request = model.inflight_guard();
  sleep(Duration::from_millis(1500)).await;

  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "the launch was stopped under an in-flight request: {:?}",
    model.state().await
  );
  assert_eq!(ctx.supervisors.len().await, 1);
  drop(request);
  model.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}
