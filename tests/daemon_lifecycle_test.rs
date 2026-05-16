//! Daemon-process lifecycle tests: lockfile contention, stale-lock
//! recovery, the cleanup invariant that shutdown removes both the
//! socket and the pidfile, the SIGINT mid-request drain budget, and
//! the state.json quarantine path.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use llamatui::daemon::{run_foreground, start_detached_with_exe, DaemonOptions, StartOutcome};
use llamatui::ipc::Client;
use tokio::time::timeout;

fn unique_temp_dir(label: &str) -> PathBuf {
  let suffix = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let dir = std::env::temp_dir().join(format!(
    "llamatui-lifecycle-{label}-{}-{suffix}",
    std::process::id()
  ));
  std::fs::create_dir_all(&dir).expect("temp dir creation");
  dir
}

fn opts_for(temp: &Path) -> DaemonOptions {
  // Daemon-lifecycle tests don't drive discovery or supervisor;
  // pin every path under the temp dir and accept defaults.
  DaemonOptions::rooted_at(temp.to_path_buf())
}

/// Poll until a connection to `path` succeeds — file existence isn't
/// enough because the test fixture can pre-seed a regular file at the
/// same path; the daemon will remove it and re-bind.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_start_reports_already_running() {
  let dir = unique_temp_dir("dup");
  let opts = opts_for(&dir);
  let opts_copy = opts.clone();

  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&opts_copy.socket_path).await;

  // Same state_dir — should observe the live pidfile and bail out.
  let outcome = run_foreground(opts_copy.clone())
    .await
    .expect("second start should return Ok");
  match outcome {
    StartOutcome::AlreadyRunning(pid) => assert_eq!(pid, std::process::id() as i32),
    StartOutcome::RanToCompletion => panic!("second start should not take the lock"),
  }

  // Shutdown the first daemon so the test cleans up.
  let mut client = Client::connect(&opts_copy.socket_path)
    .await
    .expect("connect to first daemon");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("first daemon must exit")
    .expect("join")
    .expect("daemon result");

  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_removes_socket_and_pidfile() {
  let dir = unique_temp_dir("cleanup");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let pidfile = dir.join("daemon.pid");
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  assert!(pidfile.exists(), "pidfile must exist while daemon runs");

  let mut client = Client::connect(&socket).await.expect("connect");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon must exit")
    .expect("join")
    .expect("daemon result");

  assert!(!socket.exists(), "socket file must be removed on shutdown");
  assert!(!pidfile.exists(), "pidfile must be removed on shutdown");

  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_socket_is_cleaned_before_bind() {
  let dir = unique_temp_dir("stale-sock");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();

  // Drop a non-socket file at the socket path to simulate a SIGKILL'd
  // previous run that never got to clean up. The daemon must remove it
  // before binding rather than failing.
  std::fs::write(&socket, b"this used to be a socket").expect("seed stale file");

  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  // Confirm we're talking to a real listener, not the stale file.
  let mut client = Client::connect(&socket).await.expect("connect");
  let _ = client
    .call("ping", None)
    .await
    .expect("ping after stale cleanup");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");

  std::fs::remove_dir_all(&dir).ok();
}

/// Regression test for the Unit 2 P2 follow-up: `start_detached` used to
/// re-exec the child as plain `llamatui daemon start`, which rebuilt
/// `DaemonOptions` from XDG defaults and silently ignored the caller's
/// `state_dir` / `socket_path`. With the hidden `--state-dir` /
/// `--socket-path` flags wired through, the child must bind the
/// caller-specified temp socket, not the production default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_detached_honours_caller_supplied_paths() {
  let dir = unique_temp_dir("detach-opts");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let pidfile = dir.join("daemon.pid");
  let exe = PathBuf::from(env!("CARGO_BIN_EXE_llamatui"));

  // `start_detached_with_exe` blocks on a sync poll loop, so push it off
  // the tokio reactor to keep the test runtime live.
  let opts_for_spawn = opts.clone();
  let outcome = tokio::task::spawn_blocking(move || start_detached_with_exe(opts_for_spawn, exe))
    .await
    .expect("join")
    .expect("start_detached should succeed");
  match outcome {
    StartOutcome::RanToCompletion => {}
    StartOutcome::AlreadyRunning(pid) => {
      panic!("temp paths should not collide with any running daemon (pid {pid})")
    }
  }

  // The child must be listening on *our* temp socket, not the XDG default.
  assert!(
    socket.exists(),
    "child must bind the caller-supplied socket at {}",
    socket.display()
  );
  assert!(
    pidfile.exists(),
    "child must drop its pidfile in the caller-supplied state dir at {}",
    pidfile.display()
  );

  let mut client = Client::connect(&socket)
    .await
    .expect("connect to detached child via temp socket");
  let _ = client
    .call("ping", None)
    .await
    .expect("ping detached child");
  let _ = client
    .call("shutdown", None)
    .await
    .expect("shutdown detached child");

  // Wait for the child to tear down its socket so the temp dir cleanup
  // doesn't race a still-running process.
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  while socket.exists() && std::time::Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert!(
    !socket.exists(),
    "detached child must remove its socket on shutdown"
  );

  std::fs::remove_dir_all(&dir).ok();
}

/// SIGINT mid-request drain: a request that's mid-flight when the
/// daemon is told to shut down must complete within the drain
/// timeout, not be dropped on the floor.
#[cfg(feature = "test-fixtures")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_drains_in_flight_request_within_budget() {
  let dir = unique_temp_dir("drain-completes");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  // Two clients on separate connections: one issues a slow call and
  // another taps the shutdown method while the first is in flight.
  let mut slow_client = Client::connect(&socket).await.expect("slow client connect");
  let mut shutdown_client = Client::connect(&socket).await.expect("shutdown client");

  let slow_call = tokio::spawn(async move {
    slow_client
      .call(
        "_test_sleep",
        Some(serde_json::json!({"ms": 800u64})),
      )
      .await
  });

  // Give the slow call a moment to write its request frame.
  tokio::time::sleep(Duration::from_millis(150)).await;
  shutdown_client
    .call("shutdown", None)
    .await
    .expect("shutdown call");

  // The slow call must still return (within drain budget + slop) and
  // the daemon must exit shortly after.
  let outcome = timeout(Duration::from_secs(5), slow_call)
    .await
    .expect("slow call did not return within drain window")
    .expect("join handle")
    .expect("slow call must succeed within drain budget");
  assert_eq!(outcome.get("slept_ms").and_then(|v| v.as_u64()), Some(800));

  timeout(Duration::from_secs(5), handle)
    .await
    .expect("daemon must exit")
    .expect("join")
    .expect("daemon result");

  std::fs::remove_dir_all(&dir).ok();
}

/// State.json corruption recovery: a malformed `state.json` is
/// quarantined as `state.json.broken-<ts>` and the daemon boots with
/// default state (zero favorites / presets / last_params / running).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_state_json_is_quarantined_on_boot() {
  let dir = unique_temp_dir("quarantine");
  let opts = opts_for(&dir);
  let socket = opts.socket_path.clone();

  // Seed a corrupt state.json before the daemon starts.
  std::fs::create_dir_all(&dir).expect("mk state dir");
  let state_path = dir.join("state.json");
  std::fs::write(&state_path, b"{ this is not valid json").expect("seed corrupt state.json");

  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  // Daemon must be up and report defaults via `status`.
  let mut client = Client::connect(&socket).await.expect("connect");
  let status = client.call("status", None).await.expect("status");
  let models = status
    .get("models")
    .and_then(|v| v.as_array())
    .expect("status.models is array");
  assert!(models.is_empty(), "default boot should have no models");

  let favs = client
    .call("favorite_list", None)
    .await
    .expect("favorite_list");
  let arr = favs
    .get("favorites")
    .and_then(|v| v.as_array())
    .expect("favorites array");
  assert!(arr.is_empty(), "default boot should have no favorites");

  // The broken file must have been renamed.
  let broken: Vec<_> = std::fs::read_dir(&dir)
    .expect("readdir")
    .filter_map(Result::ok)
    .filter(|e| {
      e.file_name()
        .to_string_lossy()
        .starts_with("state.json.broken-")
    })
    .collect();
  assert_eq!(broken.len(), 1, "state.json.broken-<ts> must exist");

  let _ = client.call("shutdown", None).await;
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon must exit")
    .expect("join")
    .expect("daemon result");

  std::fs::remove_dir_all(&dir).ok();
}
