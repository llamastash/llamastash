//! End-to-end smoke tests for the HTTP control-plane listener.
//!
//! Spawns a real daemon, waits for `runtime.json` to appear, reads
//! the bearer token + URL, and drives `POST /rpc` / `GET /health`
//! through reqwest. Exercises the full layer stack: TcpListener →
//! hyper service → bearer middleware → JSON-RPC parse → dispatch →
//! HTTP response.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use llamastash::daemon::{run_foreground, runtime_file, DaemonOptions};
use llamastash::ipc::Client;
use llamastash::test_support::DAEMON_STARTUP_TIMEOUT;
use serde_json::{json, Value};
use tokio::time::timeout;

fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-cp", label)
}

fn opts_for(temp: &Path) -> DaemonOptions {
  // `rooted_at` sets control_plane_port = 0 so the kernel picks an
  // ephemeral port — every test can run in parallel without
  // colliding on 48134.
  DaemonOptions::rooted_at(temp.to_path_buf())
}

/// Poll the state directory until `runtime.json` lands; then return
/// the parsed `RuntimeInfo`.
async fn wait_for_runtime_info(state_dir: &Path) -> runtime_file::RuntimeInfo {
  let deadline = std::time::Instant::now() + DAEMON_STARTUP_TIMEOUT;
  loop {
    if std::time::Instant::now() > deadline {
      panic!(
        "runtime.json did not appear within {DAEMON_STARTUP_TIMEOUT:?} in {}",
        state_dir.display()
      );
    }
    match runtime_file::load(state_dir) {
      Ok(Some(info)) => return info,
      _ => tokio::time::sleep(Duration::from_millis(20)).await,
    }
  }
}

async fn shutdown_via_socket(socket: &Path) {
  // Shut down over the Unix socket so teardown does not couple to the
  // HTTP layer under test.
  let mut client = Client::connect(socket).await.expect("attach for shutdown");
  let _ = client.call("shutdown", None).await.expect("shutdown rpc");
}

async fn join_daemon(
  handle: tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
) {
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon must exit within 3s")
    .expect("daemon join")
    .expect("daemon result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_ping_roundtrips_with_bearer_token() {
  let dir = unique_temp_dir("ping");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let info = wait_for_runtime_info(&state_dir).await;
  let client = reqwest::Client::new();
  let resp = client
    .post(format!("{}/rpc", info.ipc_url))
    .bearer_auth(&info.ipc_token)
    .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}))
    .send()
    .await
    .expect("send ping");
  assert_eq!(resp.status(), 200);
  let body: Value = resp.json().await.expect("ping body json");
  assert_eq!(body["result"], json!("pong"));
  assert_eq!(body["id"], json!(1));

  shutdown_via_socket(&socket).await;
  join_daemon(handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_without_bearer_returns_401() {
  let dir = unique_temp_dir("noauth");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let info = wait_for_runtime_info(&state_dir).await;
  let client = reqwest::Client::new();
  let resp = client
    .post(format!("{}/rpc", info.ipc_url))
    .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}))
    .send()
    .await
    .expect("send no-auth ping");
  assert_eq!(resp.status(), 401);

  shutdown_via_socket(&socket).await;
  join_daemon(handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_with_wrong_bearer_returns_401() {
  let dir = unique_temp_dir("wrongauth");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let info = wait_for_runtime_info(&state_dir).await;
  let client = reqwest::Client::new();
  let resp = client
    .post(format!("{}/rpc", info.ipc_url))
    .bearer_auth("definitely-not-the-real-token")
    .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping","params":null}))
    .send()
    .await
    .expect("send wrong-auth ping");
  assert_eq!(resp.status(), 401);

  shutdown_via_socket(&socket).await;
  join_daemon(handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_succeeds_without_bearer() {
  let dir = unique_temp_dir("health");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let info = wait_for_runtime_info(&state_dir).await;
  let client = reqwest::Client::new();
  let resp = client
    .get(format!("{}/health", info.ipc_url))
    .send()
    .await
    .expect("send health");
  assert_eq!(resp.status(), 200);
  let body: Value = resp.json().await.expect("health body json");
  assert_eq!(body["status"], json!("ok"));

  shutdown_via_socket(&socket).await;
  join_daemon(handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_dispatches_existing_methods_unchanged() {
  // Sanity check that the existing dispatch table (not just `ping`)
  // routes correctly through the HTTP layer. `list_models` is a
  // good proxy: it touches the catalog and returns a real result
  // shape, but does not require a `LaunchEnv`.
  let dir = unique_temp_dir("dispatch");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let info = wait_for_runtime_info(&state_dir).await;
  let client = reqwest::Client::new();
  let resp = client
    .post(format!("{}/rpc", info.ipc_url))
    .bearer_auth(&info.ipc_token)
    .json(&json!({"jsonrpc":"2.0","id":7,"method":"list_models","params":null}))
    .send()
    .await
    .expect("send list_models");
  assert_eq!(resp.status(), 200);
  let body: Value = resp.json().await.expect("list_models body");
  assert!(
    body.get("result").is_some(),
    "list_models must return a result envelope: {body}"
  );
  assert_eq!(body["id"], json!(7));

  shutdown_via_socket(&socket).await;
  join_daemon(handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_json_is_removed_on_shutdown() {
  let dir = unique_temp_dir("cleanup");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let _info = wait_for_runtime_info(&state_dir).await;
  let runtime_path = runtime_file::path(&state_dir);
  assert!(
    runtime_path.exists(),
    "runtime.json must exist while daemon runs"
  );

  shutdown_via_socket(&socket).await;
  join_daemon(handle).await;

  assert!(
    !runtime_path.exists(),
    "runtime.json must be removed on shutdown"
  );
  std::fs::remove_dir_all(&dir).ok();
}
