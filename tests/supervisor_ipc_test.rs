//! End-to-end coverage that the daemon's `status`, `stop_model`,
//! and `logs_tail` IPC methods drive the supervisor surface. The
//! test spawns a `ManagedModel` directly (the start_model IPC path
//! lands in Unit 8 once the CLI handler is wired) and asserts every
//! handler returns the documented shape.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::supervisor::{spawn, ManagedSpawn, ManagedState};
use llamastash::daemon::DaemonOptions;
use llamastash::gguf::identity::ModelId;
use llamastash::ipc::Client;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use serde_json::json;
use tokio::time::timeout;

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!(
    "llamastash-supipc-{label}-{}-{nanos}",
    std::process::id()
  ));
  std::fs::create_dir_all(&p).expect("temp");
  p
}

fn allocate_port() -> u16 {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  port
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon socket never appeared: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_lists_active_supervised_model() {
  let state = unique_temp("state");
  let logs = state.join("logs");
  std::fs::create_dir_all(&logs).unwrap();
  let registry = SupervisorRegistry::new();

  // Spin up a ManagedModel and stash it under a known LaunchId.
  let port = allocate_port();
  let model = spawn(ManagedSpawn {
    id: ModelId {
      path: PathBuf::from("/fixture/m.gguf"),
      header_blake3: [7u8; 32],
    },
    binary: fake_binary(),
    params: LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat),
    port,
    mode: LaunchMode::Chat,
    log_path: logs.join("launch.log"),
    probe: ProbeOptions {
      interval: Duration::from_millis(40),
      timeout: Duration::from_secs(5),
    },
  })
  .await
  .expect("spawn supervisor");
  let launch_id = registry.next_id();
  registry.insert(launch_id.clone(), model.clone()).await;

  // Start the daemon with that registry attached.
  let opts = DaemonOptions::rooted_at(state.clone());
  let socket = opts.socket_path.clone();
  let registry_for_daemon = registry.clone();
  let daemon =
    tokio::spawn(async move { run_foreground_with_supervisors(opts, registry_for_daemon).await });
  wait_for_socket(&socket).await;

  // Wait for Ready so status has something deterministic to assert.
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if matches!(model.state().await, ManagedState::Ready) {
      break;
    }
    if std::time::Instant::now() > deadline {
      panic!("supervisor never reached Ready");
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }

  // Call `status`.
  let mut client = Client::connect(&socket).await.expect("connect");
  // Poll until the host-metrics sampler has produced a real reading
  // (`ram_total_bytes > 0`). The sampler's first tick lands one
  // interval after spawn (~1s); without this poll the assertions
  // below would only verify the wire shape, not that the sampler
  // actually runs.
  let body = {
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
      let body = client.call("status", None).await.expect("status");
      let ram_total = body["host"]["ram_total_bytes"].as_u64().unwrap_or(0);
      if ram_total > 0 {
        break body;
      }
      if std::time::Instant::now() > deadline {
        panic!("host-metrics sampler never primed: {body:#?}");
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
  };
  let models = body["models"].as_array().expect("models array");
  assert_eq!(models.len(), 1);
  assert_eq!(models[0]["launch_id"], json!(launch_id.as_str()));
  assert_eq!(models[0]["port"], json!(port));
  assert_eq!(models[0]["mode"], json!("chat"));
  assert_eq!(models[0]["state"]["state"], json!("ready"));
  assert!(body["gpu"]["backend"].as_str().is_some());
  assert!(
    body["host"].is_object(),
    "status response must include a `host` field: {body:#?}"
  );
  assert!(
    body["host"]["gpu_backend"].as_str().is_some(),
    "host snapshot must carry a backend label: {:#?}",
    body["host"]
  );
  // After the poll above, the snapshot is no longer the
  // `unsampled` sentinel — verify the transition explicitly so a
  // regression that leaves the sentinel in place becomes a test
  // failure.
  assert_ne!(
    body["host"]["gpu_backend"].as_str().unwrap(),
    "unsampled",
    "sampler should have transitioned past the sentinel"
  );
  assert!(
    body["host"]["ram_total_bytes"].as_u64().unwrap_or(0) > 0,
    "primed snapshot must carry a non-zero RAM total"
  );

  // `logs_tail` returns the ring buffer contents.
  let logs_body = client
    .call(
      "logs_tail",
      Some(json!({"launch_id": launch_id.as_str(), "lines": 200})),
    )
    .await
    .expect("logs_tail");
  let lines = logs_body["lines"].as_array().expect("lines array");
  assert!(
    lines.iter().any(|l| l
      .as_str()
      .map(|s| s.contains("listening on"))
      .unwrap_or(false)),
    "expected `listening on …` in {lines:?}"
  );

  // `stop_model` brings the supervisor down cleanly.
  let stop_body = client
    .call(
      "stop_model",
      Some(json!({"launch_id": launch_id.as_str(), "grace_secs": 5})),
    )
    .await
    .expect("stop_model");
  assert_eq!(stop_body["state"]["state"], json!("stopped"));

  // After stop, status reports an empty model list.
  let body = client.call("status", None).await.expect("post-stop status");
  assert!(body["models"].as_array().unwrap().is_empty());

  let _ = client.call("shutdown", None).await;
  timeout(Duration::from_secs(3), daemon)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  std::fs::remove_dir_all(&state).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_model_returns_error_for_unknown_launch_id() {
  let state = unique_temp("unknown");
  let registry = SupervisorRegistry::new();
  let opts = DaemonOptions::rooted_at(state.clone());
  let socket = opts.socket_path.clone();
  let registry_for_daemon = registry.clone();
  let daemon =
    tokio::spawn(async move { run_foreground_with_supervisors(opts, registry_for_daemon).await });
  wait_for_socket(&socket).await;

  let mut client = Client::connect(&socket).await.expect("connect");
  let err = client
    .call("stop_model", Some(json!({"launch_id": "L9999"})))
    .await
    .expect_err("must reject unknown launch_id");
  let msg = format!("{err}");
  assert!(msg.contains("unknown launch_id"), "got `{msg}`");

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
}

/// Spin up the daemon with a caller-supplied supervisor registry.
/// The production daemon would do this inside `run_foreground`, but
/// for tests we want a registry we can pre-populate (Unit 8 wires
/// `start_model` IPC).
async fn run_foreground_with_supervisors(
  mut opts: DaemonOptions,
  supervisors: SupervisorRegistry,
) -> anyhow::Result<llamastash::daemon::StartOutcome> {
  // The test's run_foreground call needs to use the same registry
  // we constructed up-top. We currently lack a public seam, so the
  // test exposes one via a small wrapper that mirrors the
  // run_foreground steps but injects the supervisors+gpu on
  // `MethodContext`.
  use llamastash::daemon::{lockfile::acquire, lockfile::AcquireOutcome};
  use llamastash::ipc::methods::MethodContext;
  use std::fs;

  // 1. PID lockfile.
  let lock = match acquire(&opts.state_dir)? {
    AcquireOutcome::Acquired(l) => l,
    AcquireOutcome::AlreadyRunning { pid, .. } => {
      return Ok(llamastash::daemon::StartOutcome::AlreadyRunning(pid));
    }
  };
  if opts.socket_path.exists() {
    fs::remove_file(&opts.socket_path)?;
  }
  if let Some(parent) = opts.socket_path.parent() {
    fs::create_dir_all(parent)?;
  }
  let listener = tokio::net::UnixListener::bind(&opts.socket_path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&opts.socket_path, fs::Permissions::from_mode(0o600))?;
  }
  let token = llamastash::daemon::shutdown::ShutdownToken::new();
  let _signal = llamastash::daemon::shutdown::install_signal_handlers(token.clone());
  let catalog = llamastash::discovery::ModelCatalog::new();
  let _discovery =
    llamastash::daemon::discovery_task::spawn(catalog.clone(), opts.discovery.clone());
  let sampler = llamastash::daemon::host_metrics::spawn(token.clone(), Duration::from_secs(1));
  let ctx = MethodContext::with_catalog(token, catalog)
    .with_supervisors(supervisors)
    .with_gpu(llamastash::gpu::GpuInfo::CpuOnly)
    .with_sampler(sampler);
  // Suppress unused-mut warning when opts isn't mutated further.
  let _ = &mut opts;
  let result = llamastash::daemon::server::serve(listener, ctx).await;
  let _ = fs::remove_file(&opts.socket_path);
  drop(lock);
  result.map(|()| llamastash::daemon::StartOutcome::RanToCompletion)
}
