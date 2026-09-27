//! Daemon-process lifecycle tests: lockfile contention, stale-lock
//! recovery, the cleanup invariant that shutdown removes both the
//! socket and the pidfile, the SIGINT mid-request drain budget, and
//! the state.json quarantine path.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use llamastash::daemon::{run_foreground, start_detached_with_exe, DaemonOptions, StartOutcome};
use llamastash::ipc::Client;
use tokio::time::timeout;

fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-lc", label)
}

fn opts_for(temp: &Path) -> DaemonOptions {
  // Daemon-lifecycle tests don't drive discovery or supervisor;
  // pin every path under the temp dir and accept defaults.
  DaemonOptions::rooted_at(temp.to_path_buf())
}

/// Poll until the daemon at `state_dir` responds to a `ping`. Connect
/// alone isn't enough — `Client::connect` succeeds on whatever
/// `runtime.json` is on disk (which could be a stale seed from a
/// previous run); only a successful round-trip proves the daemon is
/// actually behind the URL+token we just read.
async fn wait_for_socket(state_dir: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!(
        "daemon did not become connectable within 3s: {}",
        state_dir.display()
      );
    }
    if let Ok(mut client) = Client::connect(state_dir).await {
      if client.call("ping", None).await.is_ok() {
        return;
      }
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
  wait_for_socket(&opts_copy.state_dir).await;

  // Same state_dir — should observe the live pidfile and bail out.
  let outcome = run_foreground(opts_copy.clone())
    .await
    .expect("second start should return Ok");
  match outcome {
    StartOutcome::AlreadyRunning(pid) => assert_eq!(pid, std::process::id() as i32),
    StartOutcome::RanToCompletion => panic!("second start should not take the lock"),
  }

  // Shutdown the first daemon so the test cleans up.
  let mut client = Client::connect(&opts_copy.state_dir)
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
async fn shutdown_removes_runtime_file_and_pidfile() {
  let dir = unique_temp_dir("cleanup");
  let opts = opts_for(&dir);
  let attach = opts.state_dir.clone();
  let pidfile = dir.join("daemon.pid");
  let runtime = llamastash::daemon::runtime_file::path(&dir);
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&attach).await;

  assert!(pidfile.exists(), "pidfile must exist while daemon runs");
  assert!(
    runtime.exists(),
    "runtime.json must exist while daemon runs"
  );

  let mut client = Client::connect(&attach).await.expect("connect");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon must exit")
    .expect("join")
    .expect("daemon result");

  assert!(
    !runtime.exists(),
    "runtime.json must be removed on shutdown"
  );
  assert!(!pidfile.exists(), "pidfile must be removed on shutdown");

  std::fs::remove_dir_all(&dir).ok();
}

/// `daemon stop`, `daemon restart`, and the TUI restart key all wait here, so
/// it must return only once the lockfile is free for the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_and_wait_returns_once_the_lockfile_is_released() {
  use llamastash::daemon::restart::{shutdown_and_wait, ShutdownOutcome};
  let dir = unique_temp_dir("shutdown-wait");
  let opts = opts_for(&dir);
  let attach = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&attach).await;

  let outcome = shutdown_and_wait(&attach).await.expect("shutdown");
  assert_eq!(outcome, ShutdownOutcome::Gone);
  assert!(!dir.join("daemon.pid").exists(), "pidfile must be gone");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon must exit")
    .expect("join")
    .expect("daemon result");

  // Nothing left to stop: callers treat a connect error as "not running".
  assert!(matches!(
    shutdown_and_wait(&attach).await,
    Err(llamastash::ipc::ClientError::Connect(_))
  ));

  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_runtime_file_is_overwritten_on_start() {
  let dir = unique_temp_dir("stale-runtime");
  let opts = opts_for(&dir);
  let attach = opts.state_dir.clone();
  let runtime = llamastash::daemon::runtime_file::path(&dir);

  // Drop a stale `runtime.json` to simulate a SIGKILL'd previous
  // run that never got to clean up. The daemon must atomically
  // overwrite it on its next start rather than refusing.
  std::fs::create_dir_all(&dir).ok();
  std::fs::write(
    &runtime,
    br#"{"schema_version":1,"ipc_url":"http://127.0.0.1:1","ipc_token":"stale","started_at_unix":0,"daemon_pid":0}"#,
  )
  .expect("seed stale runtime.json");

  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&attach).await;

  // Confirm we're talking to the *new* daemon: the token differs
  // from the stale "stale" placeholder above, and the URL points
  // at a real port.
  let info = llamastash::daemon::runtime_file::load(&dir)
    .expect("load runtime.json")
    .expect("file present");
  assert_ne!(info.ipc_token, "stale", "runtime.json was not refreshed");
  assert!(
    info.ipc_url.starts_with("http://127.0.0.1:"),
    "unexpected ipc_url: {}",
    info.ipc_url
  );

  let mut client = Client::connect(&attach).await.expect("connect");
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

/// Regression: `start_detached` used to re-exec the child as plain
/// `llamastash daemon start`, which rebuilt `DaemonOptions` from XDG
/// defaults and silently ignored the caller's `state_dir`. With the
/// hidden `--state-dir` flag wired through, the child must bind under
/// the caller-supplied state directory, not the production default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_detached_honours_caller_supplied_paths() {
  let dir = unique_temp_dir("detach-opts");
  let mut opts = opts_for(&dir);
  // The detached child re-execs `daemon start`, whose backend fail-fast
  // precheck refuses to start without a resolvable `llama-server` — which
  // CI runners don't have. This test is about state-dir plumbing, not
  // backend readiness, so ride the designed escape hatch (and prove
  // `--force` propagates through the re-exec while we're at it).
  opts.force = true;
  let attach = opts.state_dir.clone();
  let pidfile = dir.join("daemon.pid");
  let runtime = llamastash::daemon::runtime_file::path(&dir);
  let exe = PathBuf::from(env!("CARGO_BIN_EXE_llamastash"));

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

  // The child must be running in *our* temp state dir, not the XDG
  // default. The pidfile + runtime.json are the visible markers.
  assert!(
    pidfile.exists(),
    "child must drop its pidfile in the caller-supplied state dir at {}",
    pidfile.display()
  );
  assert!(
    runtime.exists(),
    "child must drop runtime.json in the caller-supplied state dir at {}",
    runtime.display()
  );

  let mut client = Client::connect(&attach)
    .await
    .expect("connect to detached child via runtime.json");
  let _ = client
    .call("ping", None)
    .await
    .expect("ping detached child");
  // The detached child can tear down its listener before the shutdown
  // response is fully observed on this side — kernel-level socket close
  // races the in-process response flush. Treat the call as fire-and-
  // forget; the real post-condition is that `runtime.json` disappears,
  // which the poll loop below verifies. Mirrors the pattern used by
  // `corrupt_state_json_is_quarantined_on_boot`.
  let _ = client.call("shutdown", None).await;

  // Wait for the child to tear down its runtime.json so the temp dir
  // cleanup doesn't race a still-running process.
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  while runtime.exists() && std::time::Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert!(
    !runtime.exists(),
    "detached child must remove runtime.json on shutdown"
  );

  std::fs::remove_dir_all(&dir).ok();
}

/// SIGINT mid-request drain: a request that's mid-flight when the
/// daemon is told to shut down must complete within the drain
/// timeout, not be dropped on the floor.
#[cfg(feature = "test-fixtures")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_in_flight_request_within_budget() {
  let dir = unique_temp_dir("drain-completes");
  let opts = opts_for(&dir);
  let socket = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  // Two clients on separate connections: one issues a slow call and
  // another taps the shutdown method while the first is in flight.
  let mut slow_client = Client::connect(&socket).await.expect("slow client connect");
  let mut shutdown_client = Client::connect(&socket).await.expect("shutdown client");
  // Prove the slow client's connection is fully wired to a serve loop
  // before we hand it off — without this barrier the slow `_test_sleep`
  // frame might not yet have reached the daemon by the time we trigger
  // shutdown, and the in-flight assumption that drain has anything to
  // drain doesn't hold. A round-trip on `ping` proves the dispatcher
  // is reading frames on this socket.
  slow_client
    .call("ping", None)
    .await
    .expect("ping engages slow connection");

  let slow_call = tokio::spawn(async move {
    slow_client
      .call("_test_sleep", Some(serde_json::json!({"ms": 800u64})))
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
  let socket = opts.state_dir.clone();

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
