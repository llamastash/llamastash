//! Boot-preload integration tests.
//!
//! Drives [`preload::run`] against a `MethodContext` wired like a real daemon:
//! a catalog with synthetic GGUF rows, a `LaunchEnv` whose binary is
//! `fake_llama_server`, and a preset store. Asserts what the operator asked for
//! actually comes up, in order, and that anything they did not ask for (an
//! unknown reference) is skipped rather than fatal.
//!
//! Plan: docs/plans/2026-09-30-001-feat-model-residency-plan.md unit U3.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use llamastash::config::{ConfigPresetBlock, PresetBody};
use llamastash::daemon::context::{LaunchEnv, MethodContext, PersistedState};
use llamastash::daemon::preload;
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::state_store::DaemonState;
use llamastash::daemon::supervisor::{LaunchOrigin, ManagedModel, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::proxy::eviction;
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use tokio::time::sleep;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-preload", label)
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

fn write_gguf(dir: &Path, name: &str) -> PathBuf {
  let path = dir.join(name);
  std::fs::write(&path, build_minimal_gguf("llama")).expect("write gguf");
  llamastash::util::paths::canonicalize(&path).expect("canonicalize")
}

fn discovered(path: &Path) -> DiscoveredModel {
  DiscoveredModel {
    path: path.to_path_buf(),
    parent: path.parent().expect("parent").to_path_buf(),
    source: ModelSource::UserPath,
    metadata: Some(ModelMetadata {
      arch: Some("llama".to_string()),
      total_parameters: Some(1_000_000),
      parameter_label: Some("1M".to_string()),
      quant: Quant::Q4_K,
      quant_label: None,
      native_ctx: Some(8192),
      chat_template: None,
      tokenizer_kind: Some("llama".to_string()),
      reasoning_hint: false,
      mode_hint: ModeHint::Chat,
      weights_bytes: Some(1_000_000),
      lazy_tensor_bytes: Vec::new(),
      mtp: None,
    }),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: None,
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

/// Presets keyed by the model's canonical path — the key shape a per-model
/// `presets:` entry must have to resolve for that model.
fn preset_block(
  entries: Vec<(PathBuf, String, PresetBody)>,
) -> BTreeMap<String, ConfigPresetBlock> {
  let mut by_key: BTreeMap<String, BTreeMap<String, PresetBody>> = BTreeMap::new();
  for (path, name, body) in entries {
    by_key
      .entry(path.display().to_string())
      .or_default()
      .insert(name, body);
  }
  by_key
    .into_iter()
    .map(|(key, entries)| {
      (
        key,
        ConfigPresetBlock {
          default: None,
          entries,
        },
      )
    })
    .collect()
}

/// A daemon context with `models` discovered, presets seeded, and the fake
/// server binary wired.
async fn build_ctx(
  models: Vec<DiscoveredModel>,
  presets: BTreeMap<String, ConfigPresetBlock>,
) -> MethodContext {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let dir = unique_temp("ctx");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let env = LaunchEnv {
    binary: Some(fake_binary()),
    port_range: llamastash::test_support::allocate_port_range(32),
    log_dir,
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: Default::default(),
    default_launch_mode: Default::default(),
  };
  MethodContext::with_catalog(ShutdownToken::new(), catalog)
    .with_supervisors(SupervisorRegistry::new())
    .with_state(PersistedState::new(DaemonState::default(), None))
    .with_presets(llamastash::daemon::preset_store::ConfigPresetStore::new(
      presets, None,
    ))
    .with_launch_env(env)
}

/// The launch ids + live states of everything the daemon supervises.
async fn launch_states(ctx: &MethodContext) -> Vec<(String, ManagedState)> {
  let mut out = Vec::new();
  for (id, model) in ctx.supervisors.snapshot().await {
    out.push((id.as_str().to_string(), model.state().await));
  }
  out
}

async fn wait_for_supervisors(ctx: &MethodContext, want: usize) -> Vec<(String, ManagedModel)> {
  let deadline = std::time::Instant::now() + Duration::from_secs(20);
  loop {
    let states = launch_states(ctx).await;
    if states.len() == want && states.iter().all(|(_, s)| matches!(s, ManagedState::Ready)) {
      return ctx
        .supervisors
        .snapshot()
        .await
        .into_iter()
        .map(|(id, m)| (id.as_str().to_string(), m))
        .collect();
    }
    assert!(
      std::time::Instant::now() < deadline,
      "expected {want} ready launch(es), got {states:?}",
    );
    sleep(Duration::from_millis(30)).await;
  }
}

/// The live states of `models`, for an assertion that cannot await in a closure.
async fn states_of(models: &[(String, ManagedModel)]) -> Vec<(String, ManagedState)> {
  let mut out = Vec::new();
  for (id, model) in models {
    out.push((id.clone(), model.state().await));
  }
  out
}

async fn run_preload(ctx: &MethodContext, entries: Vec<String>) {
  preload::run(ctx.clone(), entries, ShutdownToken::new()).await;
}

/// `daemon.preload:` takes a plain reference and a `<model>@<preset>` address,
/// and each launch is manual so the idle sweep leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preload_starts_listed_models_in_order() {
  let dir = unique_temp("listed");
  let a = write_gguf(&dir, "alpha.gguf");
  let b = write_gguf(&dir, "beta.gguf");
  let presets = preset_block(vec![(b.clone(), "warm".to_string(), PresetBody::default())]);
  let ctx = build_ctx(vec![discovered(&a), discovered(&b)], presets).await;

  // Preload launches one model at a time, in list order, and does not move on
  // until the current one is Ready. Watch the running rows from another task:
  // the order the two paths first appear in is the order they were launched.
  let watcher = {
    let ctx = ctx.clone();
    let want = [a.clone(), b.clone()];
    tokio::spawn(async move {
      let mut seen: Vec<PathBuf> = Vec::new();
      for _ in 0..1500 {
        for row in ctx.state.snapshot().await.running {
          let path = row.params.model_path.clone();
          if want.contains(&path) && !seen.contains(&path) {
            seen.push(path);
          }
        }
        if seen.len() == want.len() {
          break;
        }
        sleep(Duration::from_millis(2)).await;
      }
      seen
    })
  };
  run_preload(&ctx, vec!["alpha".to_string(), "beta@warm".to_string()]).await;
  let order = watcher.await.expect("watcher task");
  assert_eq!(
    order,
    vec![a.clone(), b.clone()],
    "preload launched out of list order (or in parallel)"
  );

  let ready = wait_for_supervisors(&ctx, 2).await;
  assert!(
    ready
      .iter()
      .all(|(_, m)| m.origin() == LaunchOrigin::Manual),
    "a preloaded launch is not manual: {:?}",
    ready.iter().map(|(id, _)| id).collect::<Vec<_>>()
  );
  let running = ctx.state.snapshot().await.running;
  assert_eq!(running.len(), 2, "both launches stamped a running row");
  let named = running
    .iter()
    .filter_map(|r| r.name.clone())
    .collect::<Vec<_>>();
  assert!(
    named.contains(&"warm".to_string()),
    "preset address lost its name: {named:?}"
  );
  let preset_stamp = running
    .iter()
    .filter_map(|r| r.preset.clone())
    .collect::<Vec<_>>();
  assert_eq!(preset_stamp, vec!["warm".to_string()]);

  // Manual origin means the sweep is not allowed to touch it, even at a 1 ns TTL.
  let state: Arc<ProxyState> =
    ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES);
  for (_, model) in &ready {
    state.touch_mru(model.id()).await;
  }
  sleep(Duration::from_millis(5)).await;
  eviction::sweep_once(&state, Duration::from_nanos(1)).await;
  sleep(Duration::from_millis(100)).await;
  let after_sweep = states_of(&ready).await;
  assert!(
    after_sweep
      .iter()
      .all(|(_, s)| matches!(s, ManagedState::Ready)),
    "the idle sweep unloaded a preloaded model: {after_sweep:?}"
  );
  for (_, model) in &ready {
    let _ = model.stop(Duration::from_secs(2)).await;
  }
  std::fs::remove_dir_all(&dir).ok();
}

/// A preset that pins `preload: true` starts on its own, with nothing listed in
/// `daemon.preload:`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preload_starts_a_preset_that_pins_preload_true() {
  let dir = unique_temp("preset-flag");
  let model = write_gguf(&dir, "gamma.gguf");
  let presets = BTreeMap::from([(
    model.display().to_string(),
    ConfigPresetBlock {
      default: None,
      entries: BTreeMap::from([(
        "always".to_string(),
        PresetBody {
          preload: true,
          idle_ttl_secs: Some(0),
          ..Default::default()
        },
      )]),
    },
  )]);
  let ctx = build_ctx(vec![discovered(&model)], presets).await;

  run_preload(&ctx, Vec::new()).await;

  let ready = wait_for_supervisors(&ctx, 1).await;
  let running = ctx.state.snapshot().await.running;
  assert_eq!(running.len(), 1);
  assert_eq!(running[0].preset.as_deref(), Some("always"));
  assert_eq!(running[0].name.as_deref(), Some("always"));
  let (_, model) = &ready[0];
  let _ = model.stop(Duration::from_secs(2)).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// An entry that names nothing is logged and skipped, and says nothing about the
/// entries around it — a typo in `daemon.preload` must never take the daemon
/// down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preload_skips_unresolvable_entries() {
  let dir = unique_temp("unresolvable");
  let a = write_gguf(&dir, "delta.gguf");
  let ctx = build_ctx(vec![discovered(&a)], BTreeMap::new()).await;

  run_preload(
    &ctx,
    vec![
      "no-such-model".to_string(),
      "delta@no-such-preset".to_string(),
      "/no/such/launch.yaml".to_string(),
    ],
  )
  .await;

  assert!(
    ctx.supervisors.snapshot().await.is_empty(),
    "an unresolvable entry launched something anyway"
  );
  std::fs::remove_dir_all(&dir).ok();
}
