//! End-to-end smoke tests: spawn the daemon in-process on a temp socket
//! and drive ping / version / shutdown through the real IPC client.
//!
//! These exercise the full layer stack: UnixListener -> peercred ->
//! framing -> JSON-RPC parse -> dispatch -> framing -> client decode.
//! Unit tests pin individual pieces; this file proves they're wired up.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use llamadash::daemon::{run_foreground, DaemonOptions, StartOutcome};
use llamadash::ipc::Client;
use serde_json::json;
use tokio::time::timeout;

fn unique_temp_dir(label: &str) -> PathBuf {
  let suffix = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let dir = std::env::temp_dir().join(format!(
    "llamadash-ipc-{label}-{}-{suffix}",
    std::process::id()
  ));
  std::fs::create_dir_all(&dir).expect("temp dir creation");
  dir
}

fn opts_for(temp: &Path) -> DaemonOptions {
  DaemonOptions::rooted_at(temp.to_path_buf())
}

/// Spawn the daemon on a background task and wait until it becomes
/// connectable. Returns the JoinHandle so the test can await clean
/// shutdown. We poll connectability rather than file existence because
/// other tests pre-seed regular files at the socket path to exercise the
/// stale-cleanup path.
async fn spawn_daemon(
  opts: DaemonOptions,
) -> tokio::task::JoinHandle<anyhow::Result<StartOutcome>> {
  let socket_path = opts.socket_path.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!(
        "daemon did not become connectable within 3s: {}",
        socket_path.display()
      );
    }
    if Client::connect(&socket_path).await.is_ok() {
      return handle;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_roundtrips_through_real_socket() {
  let dir = unique_temp_dir("ping");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let handle = spawn_daemon(opts).await;

  let mut client = Client::connect(&socket)
    .await
    .expect("client should connect");
  let pong = client
    .call("ping", None)
    .await
    .expect("ping should succeed");
  assert_eq!(pong, json!("pong"));

  // Use shutdown to close the daemon cleanly.
  let _ = client.call("shutdown", None).await.expect("shutdown");

  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon must exit promptly after shutdown")
    .expect("join")
    .expect("daemon result");
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_reports_pid_uptime_and_connections() {
  let dir = unique_temp_dir("version");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let handle = spawn_daemon(opts).await;

  let mut client = Client::connect(&socket).await.expect("connect");
  let v = client.call("version", None).await.expect("version");
  assert_eq!(v["name"], json!("llamadash"));
  assert_eq!(v["pid"], json!(std::process::id()));
  assert!(v["uptime_seconds"].is_number());

  // `connections` should settle to 1 (this client) once the probe
  // connection that `spawn_daemon` used to wait for the socket has
  // been fully torn down on the daemon side. The decrement runs in
  // the per-connection task *after* it returns from
  // `serve_connection`, so on a busy scheduler the first `version`
  // call here can race with that decrement and observe 2. Poll a
  // few times until the count settles before asserting, so the
  // test stays meaningful (we still verify the count is exposed and
  // accurate) without flaking on scheduling.
  let mut connections = v["connections"].as_u64().expect("connections present");
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  while connections != 1 && std::time::Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(20)).await;
    let v = client.call("version", None).await.expect("version retry");
    connections = v["connections"].as_u64().expect("connections present");
  }
  assert_eq!(
    connections, 1,
    "connections must settle to 1 (this client) once the probe tears down"
  );

  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_method_returns_method_not_found() {
  let dir = unique_temp_dir("unknown");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let handle = spawn_daemon(opts).await;

  let mut client = Client::connect(&socket).await.expect("connect");
  let err = client
    .call("no-such-method", None)
    .await
    .expect_err("unknown method must error");
  let msg = err.to_string();
  assert!(
    msg.contains("-32601"),
    "expected method-not-found code: {msg}"
  );

  // Verify the connection survives the protocol-error response so the
  // client can keep using it.
  let pong = client.call("ping", None).await.expect("ping post-error");
  assert_eq!(pong, json!("pong"));

  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_file_is_mode_0600() {
  use std::os::unix::fs::PermissionsExt;

  let dir = unique_temp_dir("mode");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let handle = spawn_daemon(opts).await;

  let mode = std::fs::metadata(&socket)
    .expect("metadata")
    .permissions()
    .mode()
    & 0o777;
  assert_eq!(mode, 0o600, "socket must be 0600 to keep other UIDs out");

  let mut client = Client::connect(&socket).await.expect("connect");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  std::fs::remove_dir_all(&dir).ok();
}
