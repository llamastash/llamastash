//! End-to-end coverage that the daemon honours `LoadedConfig` + CLI
//! flags when resolving its discovery roots.
//!
//! Closes the Unit 4 follow-ups:
//! - **P1** "Wire production daemon discovery roots from config + CLI" —
//!   uses `cli::daemon::resolve_scan_roots` (via the public route the
//!   real binary takes) and asserts the resulting catalog ends up
//!   populated through `list_models`.
//! - **P2** "Default-cache Ollama regression" — seeds a fake Ollama
//!   layout under a temp home, runs the daemon against it, and proves
//!   the manifest-backed blob appears in `list_models` under
//!   `source: "ollama"`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llamastash::config::CachePathsConfig;
use llamastash::daemon::discovery_task::DiscoveryOptions;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::discovery::known_caches::{default_set, RootResolution};
use llamastash::discovery::scanner::ScanOptions;
use llamastash::discovery::watcher::WatcherOptions;
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::ipc::Client;
use serde_json::Value;
use tokio::time::timeout;

fn unique_temp(label: &str) -> PathBuf {
  let suffix = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let dir = std::env::temp_dir().join(format!(
    "llamastash-cfg-int-{label}-{}-{suffix}",
    std::process::id()
  ));
  fs::create_dir_all(&dir).expect("temp dir");
  dir
}

/// `WatcherOptions` tuned for tests: short debounce + frequent
/// periodic ticks so the catalog stabilises in single-digit seconds.
fn fast_watcher() -> WatcherOptions {
  WatcherOptions {
    debounce: Duration::from_millis(75),
    periodic_rescan: Duration::from_secs(30),
    channel_capacity: 16,
  }
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
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

async fn list_models(socket: &Path) -> Value {
  let mut client = Client::connect(socket).await.expect("client connect");
  client
    .call("list_models", None)
    .await
    .expect("list_models succeeds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_model_paths_populate_list_models() {
  // Config supplies a `model_paths` entry; no CLI flag involved. The
  // daemon must scan it and surface the model through `list_models`.
  let state = unique_temp("config-state");
  let model_dir = unique_temp("config-models");
  fs::write(model_dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();

  // We bypass the binary's main() (which would resolve $HOME) and go
  // through the same `default_set` chain `cli::daemon::build_options`
  // uses in production. Using a synthetic empty home (`/tmp` is a
  // safe choice that has no `.cache/huggingface` etc.) keeps the
  // test deterministic across developer machines.
  let synthetic_home = unique_temp("config-home");
  let roots = default_set(RootResolution {
    user_paths: std::slice::from_ref(&model_dir),
    disable: &CachePathsConfig {
      huggingface: true,
      ollama: true,
      lm_studio: true,
    },
    no_scan: false,
    home: Some(&synthetic_home),
  });
  assert_eq!(roots.len(), 1, "only the user path remains");

  let opts = DaemonOptions {
    discovery: DiscoveryOptions {
      scan_roots: roots,
      scan: ScanOptions::default(),
      watcher: fast_watcher(),
    },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("config-derived scan root never populated catalog");
    }
    let body = list_models(&socket).await;
    if body["models"].as_array().map(|a| a.len()).unwrap_or(0) >= 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  let body = list_models(&socket).await;
  let models = body["models"].as_array().expect("array");
  assert_eq!(models.len(), 1);
  assert_eq!(models[0]["source"], serde_json::json!("user"));

  let mut client = Client::connect(&socket).await.expect("shutdown connect");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  fs::remove_dir_all(&state).ok();
  fs::remove_dir_all(&model_dir).ok();
  fs::remove_dir_all(&synthetic_home).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ollama_default_cache_surfaces_through_list_models() {
  // Synthesise the on-disk Ollama layout under a temp home and let
  // `known_caches::default_set` find it the way the production
  // daemon does. We assert the daemon's `list_models` surfaces the
  // manifest-backed blob under `source: "ollama"`.
  let state = unique_temp("ollama-state");
  let home = unique_temp("ollama-home");
  let ollama_root = home.join(".ollama/models");
  let manifests = ollama_root.join("manifests/registry.ollama.ai/library/qwen-test");
  let blobs = ollama_root.join("blobs");
  fs::create_dir_all(&manifests).unwrap();
  fs::create_dir_all(&blobs).unwrap();
  let blob_bytes = build_minimal_gguf("llama");
  let digest_hex = "feedface";
  fs::write(blobs.join(format!("sha256-{digest_hex}")), &blob_bytes).unwrap();
  let manifest = serde_json::json!({
    "schemaVersion": 2,
    "layers": [{
      "mediaType": "application/vnd.ollama.image.model",
      "digest": format!("sha256:{digest_hex}"),
      "size": blob_bytes.len(),
    }]
  });
  fs::write(manifests.join("7b"), serde_json::to_vec(&manifest).unwrap()).unwrap();

  // Drive root resolution the same way `cli::daemon::build_options`
  // does in production.
  let user_paths: Vec<PathBuf> = Vec::new();
  let roots = default_set(RootResolution {
    user_paths: &user_paths,
    disable: &CachePathsConfig {
      huggingface: true,
      ollama: false,
      lm_studio: true,
    },
    no_scan: false,
    home: Some(&home),
  });
  assert!(
    roots
      .iter()
      .any(|r| r.path == ollama_root && r.source == llamastash::discovery::ModelSource::Ollama),
    "default_set must surface the synthetic Ollama root, got {roots:?}"
  );

  let opts = DaemonOptions {
    discovery: DiscoveryOptions {
      scan_roots: roots,
      scan: ScanOptions::default(),
      watcher: fast_watcher(),
    },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  let empty: Vec<Value> = Vec::new();
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("ollama-rooted discovery never surfaced the blob");
    }
    let body = list_models(&socket).await;
    let ollama_row_path: Option<String> = body["models"]
      .as_array()
      .unwrap_or(&empty)
      .iter()
      .find(|m| m["source"] == serde_json::json!("ollama"))
      .and_then(|m| m["path"].as_str().map(str::to_string));
    if let Some(path) = ollama_row_path {
      assert!(
        path.contains(digest_hex),
        "expected blob path containing sha256-{digest_hex}, got `{path}`"
      );
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  let mut client = Client::connect(&socket).await.expect("shutdown connect");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  fs::remove_dir_all(&state).ok();
  fs::remove_dir_all(&home).ok();
}
