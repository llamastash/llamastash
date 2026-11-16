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
    |_| {},
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
    |_| {},
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
      |_| {},
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
    |_| {},
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

/// A launch the gate admitted and that then died is not a contradiction the
/// launch path can use: demand is a floor and free memory is `MemAvailable`, so a
/// resident mmap-backed engine reads as free. The death-triggered entry point is
/// therefore asked for the whole demand rather than a shortfall — that is its
/// only difference from a refusal, and this pair pins exactly that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_death_makes_room_when_the_gate_saw_plenty_of_room() {
  let dir = unique_temp("make-room-death");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let resident = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &resident, None, 500).await;
  let state =
    build_state_with_presets(registry, &log_dir, persisted, ConfigPresetStore::empty()).await;
  state.touch_mru(resident.id()).await;

  // 900 free against a 400 demand: a refusal shaped like this has no shortfall,
  // so the refusal-triggered path is right to leave everything loaded.
  let from_refusal = eviction::make_room(
    &state,
    &Refusal {
      demand_bytes: 400,
      effective_free_bytes: 900,
      reserved_bytes: 0,
    },
    |_| {},
  )
  .await;
  assert!(!from_refusal, "no shortfall, so nothing is unloaded");
  assert!(
    matches!(resident.state().await, ManagedState::Ready),
    "a refusal with no shortfall unloaded a launch"
  );

  // The same 400 bytes asked for after a launch the gate admitted died: the
  // 500-credit idle launch is unloaded for it.
  let from_death = eviction::make_room_after_launch_death(&state, 400, |_| {}).await;
  assert!(from_death, "400 demanded against 500 idle credit");
  wait_until_not_ready(&resident, "the launch credited for the dead one").await;
  std::fs::remove_dir_all(&dir).ok();
}

/// The death trigger keeps the all-or-nothing rule: a resident launch credited
/// under the dead launch's demand stays loaded rather than being unloaded for a
/// retry that still would not fit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_death_makes_no_room_when_nothing_covers_the_demand() {
  let dir = unique_temp("make-room-death-short");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let registry = SupervisorRegistry::new();
  let resident = pre_launch(&log_dir, &registry, LaunchOrigin::AutoStart).await;
  let persisted = PersistedState::new(DaemonState::default(), None);
  stamp_row(&persisted, &registry, &resident, None, 100).await;
  let state =
    build_state_with_presets(registry, &log_dir, persisted, ConfigPresetStore::empty()).await;
  state.touch_mru(resident.id()).await;

  let from_death = eviction::make_room_after_launch_death(&state, 400, |_| {}).await;
  assert!(!from_death, "100 freeable cannot cover a 400 demand");
  sleep(Duration::from_millis(100)).await;
  assert!(
    matches!(resident.state().await, ManagedState::Ready),
    "all-or-nothing unloaded a launch anyway"
  );
  let _ = resident.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}
