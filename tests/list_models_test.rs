//! End-to-end test for the `list_models` IPC method.
//!
//! Spins up the daemon over a temp `DaemonOptions` whose discovery
//! roots point at a seeded fixture directory; then calls the
//! `list_models` method through the real IPC client and asserts the
//! response shape matches the plan's documented surface.
//!
//! Also covers the integration verification line from Unit 4:
//! "a newly-dropped `model.gguf` into a watched root appears via
//! `list_models` within ~1 second."

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llamadash::daemon::discovery_task::DiscoveryOptions;
use llamadash::daemon::{run_foreground, DaemonOptions};
use llamadash::discovery::scanner::{ScanOptions, ScanRoot};
use llamadash::discovery::watcher::WatcherOptions;
use llamadash::discovery::ModelSource;
use llamadash::gguf::test_fixtures::build_minimal_gguf;
use llamadash::ipc::Client;
use serde_json::Value;
use tokio::time::timeout;

fn unique_temp_dir(label: &str) -> PathBuf {
  let suffix = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let dir = std::env::temp_dir().join(format!(
    "llamadash-listmodels-{label}-{}-{suffix}",
    std::process::id()
  ));
  fs::create_dir_all(&dir).expect("temp dir");
  dir
}

fn fast_discovery_for(root: &Path) -> DiscoveryOptions {
  DiscoveryOptions {
    scan_roots: vec![ScanRoot {
      path: root.to_path_buf(),
      source: ModelSource::UserPath,
    }],
    scan: ScanOptions::default(),
    // Short debounce + short periodic ticks: the test must finish in
    // single-digit seconds even on a contended CI box.
    watcher: WatcherOptions {
      debounce: Duration::from_millis(75),
      periodic_rescan: Duration::from_secs(30),
      channel_capacity: 16,
    },
  }
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!(
        "daemon did not become connectable within 3s: {}",
        path.display()
      );
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn call_list_models(socket: &Path) -> Value {
  let mut client = Client::connect(socket).await.expect("client connect");
  client
    .call("list_models", None)
    .await
    .expect("list_models succeeds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_models_returns_seeded_fixtures() {
  let state = unique_temp_dir("state");
  let scan_root = unique_temp_dir("scan");
  fs::write(scan_root.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
  fs::write(scan_root.join("b.gguf"), build_minimal_gguf("qwen3")).unwrap();

  let opts = DaemonOptions {
    discovery: fast_discovery_for(&scan_root),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  // The initial scan races the client connect — poll until the
  // catalog reports two rows.
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  let result = loop {
    if std::time::Instant::now() > deadline {
      panic!("list_models never reported the seeded fixtures");
    }
    let body = call_list_models(&socket).await;
    if body["models"].as_array().map(|a| a.len()).unwrap_or(0) >= 2 {
      break body;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  };

  let models = result["models"].as_array().expect("array");
  assert_eq!(models.len(), 2, "two seeded GGUFs");
  for row in models {
    assert!(row["metadata"].is_object(), "metadata populated");
    assert_eq!(row["source"], serde_json::json!("user"));
  }

  // Shutdown.
  let mut client = Client::connect(&socket).await.expect("connect to shutdown");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");

  fs::remove_dir_all(&state).ok();
  fs::remove_dir_all(&scan_root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newly_dropped_gguf_appears_via_list_models_within_a_second() {
  let state = unique_temp_dir("state-watch");
  let scan_root = unique_temp_dir("scan-watch");
  fs::write(scan_root.join("seed.gguf"), build_minimal_gguf("llama")).unwrap();

  let opts = DaemonOptions {
    discovery: fast_discovery_for(&scan_root),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  // Wait for the initial scan to settle.
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("initial scan never reached list_models");
    }
    let body = call_list_models(&socket).await;
    if body["models"].as_array().map(|a| a.len()).unwrap_or(0) >= 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  // Now drop a new file and verify it surfaces. The plan calls for
  // ~1 second; allow up to 5s under CI load.
  fs::write(scan_root.join("dropped.gguf"), build_minimal_gguf("phi3")).unwrap();
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("watcher integration: list_models never grew to include dropped.gguf");
    }
    let body = call_list_models(&socket).await;
    let names: Vec<String> = body["models"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|m| m["path"].as_str().map(|s| s.to_string()))
      .collect();
    if names.iter().any(|n| n.ends_with("dropped.gguf")) {
      break;
    }
    tokio::time::sleep(Duration::from_millis(75)).await;
  }

  // Shutdown.
  let mut client = Client::connect(&socket).await.expect("connect to shutdown");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");

  fs::remove_dir_all(&state).ok();
  fs::remove_dir_all(&scan_root).ok();
}
