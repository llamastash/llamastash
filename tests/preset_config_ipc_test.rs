//! End-to-end coverage that the config-backed preset CRUD round-trips
//! through a real daemon and survives a restart (config.yaml is the
//! source of truth), via Unit 4's `presets_*` IPC surface.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use llamastash::daemon::discovery_task::DiscoveryOptions;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::discovery::scanner::{ScanOptions, ScanRoot};
use llamastash::discovery::watcher::WatcherOptions;
use llamastash::discovery::ModelSource;
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::ipc::Client;
use serde_json::{json, Value};
use tokio::time::timeout;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-preset-ipc", label)
}

fn fast_watcher() -> WatcherOptions {
  WatcherOptions {
    debounce: Duration::from_millis(75),
    periodic_rescan: Duration::from_secs(30),
    channel_capacity: 16,
  }
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon did not become connectable: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

/// Spawn a daemon over `state` + `config_path`, discovering models under
/// `model_dir`. Returns the join handle so the caller can shut it down.
async fn spawn_daemon(
  state: &Path,
  config_path: &Path,
  model_dir: &Path,
) -> tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>> {
  let mut opts = DaemonOptions::rooted_at(state.to_path_buf());
  opts.config_path = Some(config_path.to_path_buf());
  // Mirror production `build_options`: seed the store from config.yaml so a
  // restart reloads hand-/app-written presets.
  opts.presets = llamastash::config::load_config_from_path(config_path)
    .config
    .presets;
  opts.proxy.enabled = false;
  opts.discovery = DiscoveryOptions {
    scan_roots: vec![ScanRoot {
      path: model_dir.to_path_buf(),
      source: ModelSource::UserPath,
    }],
    scan: ScanOptions::default(),
    watcher: fast_watcher(),
    lemonade_port: None,
    backend: Default::default(),
    backend_force: Default::default(),
  };
  let socket = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  handle
}

async fn shutdown(
  socket: &Path,
  handle: tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
) {
  let mut client = Client::connect(socket).await.expect("connect");
  client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
}

async fn wait_for_model(socket: &Path) -> String {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("model never discovered");
    }
    let mut client = Client::connect(socket).await.expect("connect");
    let body = client.call("list_models", None).await.expect("list_models");
    if let Some(path) = body["models"]
      .as_array()
      .and_then(|a| a.first())
      .and_then(|m| m["path"].as_str())
    {
      return path.to_string();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

async fn call(socket: &Path, method: &str, params: Value) -> Value {
  let mut client = Client::connect(socket).await.expect("connect");
  client
    .call(method, Some(params))
    .await
    .unwrap_or_else(|e| panic!("{method} failed: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preset_crud_round_trips_through_config_and_survives_restart() {
  let state = unique_temp("state");
  let cfg_dir = unique_temp("cfg");
  let config_path = cfg_dir.join("config.yaml");
  let model_dir = unique_temp("models");
  std::fs::write(model_dir.join("coder.gguf"), build_minimal_gguf("qwen2")).unwrap();

  // Boot 1: discover the model, save a preset.
  let handle = spawn_daemon(&state, &config_path, &model_dir).await;
  let socket = state.clone();
  let model_path = wait_for_model(&socket).await;

  let saved = call(
    &socket,
    "presets_save",
    json!({"model_path": model_path, "name": "long-ctx", "ctx": 65536}),
  )
  .await;
  assert_eq!(saved["saved"]["source"], json!("config"));
  assert_eq!(saved["saved"]["params"]["ctx"], json!(65536));
  assert_eq!(saved["replaced"], Value::Null, "fresh save has no previous");

  // It is visible in the list with config provenance.
  let listed = call(&socket, "presets_list", json!({"model_path": model_path})).await;
  let names: Vec<&str> = listed["presets"]
    .as_array()
    .unwrap()
    .iter()
    .filter_map(|p| p["name"].as_str())
    .collect();
  assert_eq!(names, vec!["long-ctx"]);
  assert_eq!(listed["presets"][0]["source"], json!("config"));

  // And it landed in config.yaml under the model basename.
  let cfg = llamastash::config::load_config_from_path(&config_path).config;
  assert!(cfg.presets.contains_key("coder.gguf"));

  shutdown(&socket, handle).await;

  // Boot 2: the preset is read back from config.yaml (no state.json).
  let handle = spawn_daemon(&state, &config_path, &model_dir).await;
  let model_path = wait_for_model(&socket).await;
  let after_restart = call(
    &socket,
    "presets_show",
    json!({"model_path": model_path, "name": "long-ctx"}),
  )
  .await;
  assert_eq!(
    after_restart["preset"]["params"]["ctx"],
    json!(65536),
    "preset survived restart"
  );

  // Delete removes it from config.yaml.
  let deleted = call(
    &socket,
    "presets_delete",
    json!({"model_path": model_path, "name": "long-ctx"}),
  )
  .await;
  assert_eq!(deleted["removed"]["name"], json!("long-ctx"));
  let empty = call(&socket, "presets_list", json!({"model_path": model_path})).await;
  assert!(empty["presets"].as_array().unwrap().is_empty());

  shutdown(&socket, handle).await;
  let cfg = llamastash::config::load_config_from_path(&config_path).config;
  assert!(
    !cfg.presets.contains_key("coder.gguf"),
    "model key pruned from config on last delete"
  );

  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&cfg_dir).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

/// `presets_save` carries the native (ds4) `backend_knobs` through to the
/// stored preset — the `Ctrl+P` save-from-running path relies on this so a
/// ds4 launch's `--power` / `--ssd-streaming` are save-able, not just
/// apply-able. Regression for the dropped `backend_knobs` in the save chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preset_save_carries_a_backends_own_knobs() {
  let state = unique_temp("bk-state");
  let cfg_dir = unique_temp("bk-cfg");
  let config_path = cfg_dir.join("config.yaml");
  let model_dir = unique_temp("bk-models");
  std::fs::write(model_dir.join("coder.gguf"), build_minimal_gguf("qwen2")).unwrap();

  let handle = spawn_daemon(&state, &config_path, &model_dir).await;
  let socket = state.clone();
  let model_path = wait_for_model(&socket).await;

  let saved = call(
    &socket,
    "presets_save",
    json!({
      "model_path": model_path,
      "name": "streamy",
      // A backend's own tunables ride the one knob map now, like any other.
      "knobs": { "ssd-streaming": true, "power": 60 },
    }),
  )
  .await;
  assert_eq!(
    saved["saved"]["params"]["knobs"]["ssd-streaming"],
    json!(true),
    "saved preset must carry the ssd-streaming knob: {saved}"
  );
  assert_eq!(saved["saved"]["params"]["knobs"]["power"], json!(60));

  // It round-trips through `presets_show` and lands in config.yaml.
  let shown = call(
    &socket,
    "presets_show",
    json!({"model_path": model_path, "name": "streamy"}),
  )
  .await;
  assert_eq!(
    shown["preset"]["params"]["knobs"]["ssd-streaming"],
    json!(true),
    "shown preset must carry the backend's own knobs: {shown}"
  );

  shutdown(&socket, handle).await;
  let cfg = llamastash::config::load_config_from_path(&config_path).config;
  let stored = cfg.presets.get("coder.gguf").expect("model key present");
  let body = stored.entries.get("streamy").expect("preset entry present");
  assert!(
    body.knobs.contains_by_name("ssd-streaming"),
    "a backend's own knobs must persist to config.yaml: {:?}",
    body.knobs
  );

  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&cfg_dir).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}
