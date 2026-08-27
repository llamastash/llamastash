//! End-to-end supervisor lifecycle tests against the `fake_llama_server`
//! fixture. Covers the Launching → Loading → Ready → Stopping →
//! Stopped state machine plus the per-launch log ring buffer.
//!
//! Built only when `--features test-fixtures` so the production
//! `cargo install` artefact never ships the fake binary.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use llamastash::backend::llama_cpp::{LlamaCppBackend, LLAMACPP_KNOB_FIT_CTX_FLOOR};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::supervisor::{spawn, FitGate, ManagedSpawn, ManagedState};
use llamastash::gguf::identity::ModelId;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-sl", label)
}

fn fake_id(tag: u8) -> ModelId {
  ModelId {
    path: PathBuf::from("/fixture/m.gguf"),
    header_blake3: [tag; 32],
  }
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(40),
    timeout: Duration::from_secs(5),
  }
}

fn allocate_port() -> u16 {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  port
}

async fn wait_for_state<P: Fn(&ManagedState) -> bool>(
  model: &llamastash::daemon::supervisor::ManagedModel,
  pred: P,
  budget: Duration,
) -> ManagedState {
  let deadline = std::time::Instant::now() + budget;
  loop {
    let s = model.state().await;
    if pred(&s) {
      return s;
    }
    if std::time::Instant::now() > deadline {
      panic!("supervisor never reached target state; current = {s:?}");
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launching_to_loading_to_ready_within_a_second() {
  let dir = unique_temp("happy");
  let port = allocate_port();
  let params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = spawn(ManagedSpawn {
    id: fake_id(1),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");

  let s = wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;
  assert!(
    matches!(s, ManagedState::Ready),
    "expected Ready, got {s:?}"
  );
  assert!(model.pid().await.is_some(), "PID populated by spawn");
  assert!(model.ready_at().await.is_some(), "ready_at stamped");

  let final_state = model.stop(Duration::from_secs(5)).await;
  assert_eq!(final_state, ManagedState::Stopped);
  std::fs::remove_dir_all(&dir).ok();
}

// Strict-fit ctx-clamp gate (R19). The fake server echoes `--fit-ctx`
// back as the resolved `/props` `n_ctx`, so launching with a floor and a
// larger trained window reproduces a memory-driven clamp deterministically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_fit_withholds_ready_when_ctx_clamped_to_floor() {
  let dir = unique_temp("strict-clamp");
  let port = allocate_port();
  let mut params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  // ctx delegated to --fit; the floor is emitted as --fit-ctx.
  let floor = 16_384;
  params.launch_config.insert(LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(), floor.to_string());
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = spawn(ManagedSpawn {
    id: fake_id(40),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    // Trained window 4x the floor → a floor-pinned resolution is
    // degradation, which strict mode must refuse.
    fit_gate: Some(FitGate {
      floor,
      native: floor * 4,
      strict: true,
    }),
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");

  let s = wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Error { .. }),
    Duration::from_secs(5),
  )
  .await;
  match s {
    ManagedState::Error { cause } => assert!(cause.contains("strict-fit"), "cause: {cause}"),
    other => panic!("expected Error, got {other:?}"),
  }
  // Ready was never declared.
  assert!(
    model.ready_at().await.is_none(),
    "strict refusal must not stamp ready_at"
  );
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_strict_flags_clamp_but_stays_ready() {
  let dir = unique_temp("soft-clamp");
  let port = allocate_port();
  let mut params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  let floor = 16_384;
  params.launch_config.insert(LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(), floor.to_string());
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = spawn(ManagedSpawn {
    id: fake_id(41),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: Some(FitGate {
      floor,
      native: floor * 4,
      strict: false,
    }),
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");

  wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;
  let actuals = model.actuals().await;
  assert_eq!(
    actuals.resolved_ctx,
    Some(floor),
    "fake /props echoes the floor"
  );
  assert!(actuals.ctx_clamped, "soft clamp flag must be set");
  let _ = model.stop(Duration::from_secs(5)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_at_model_max_is_not_flagged_as_clamped() {
  let dir = unique_temp("max-ctx");
  let port = allocate_port();
  let mut params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  let floor = 16_384;
  params.launch_config.insert(LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(), floor.to_string());
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  // Trained window == floor: settling at the floor is the model's own
  // ceiling, not memory pressure, so even strict mode must stay Ready.
  let model = spawn(ManagedSpawn {
    id: fake_id(42),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: Some(FitGate {
      floor,
      native: floor,
      strict: true,
    }),
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");

  wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;
  assert!(
    !model.actuals().await.ctx_clamped,
    "a floor that equals the trained window is not a clamp"
  );
  let _ = model.stop(Duration::from_secs(5)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_mode_records_correctly() {
  let dir = unique_temp("embed");
  let port = allocate_port();
  let mut params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Embedding);
  params.mode = LaunchMode::Embedding;
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = spawn(ManagedSpawn {
    id: fake_id(2),
    params,
    port,
    mode: LaunchMode::Embedding,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;
  assert_eq!(model.mode(), LaunchMode::Embedding);
  let _ = model.stop(Duration::from_secs(5)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_file_and_ring_buffer_capture_child_output() {
  let dir = unique_temp("logs");
  let log_path = dir.join("launch.log");
  let port = allocate_port();
  let params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = spawn(ManagedSpawn {
    id: fake_id(3),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: log_path.clone(),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;

  // The fake server prints `listening on …` as its first stdout line;
  // both surfaces (ring buffer + log file) must capture it.
  let mut tail_seen = false;
  for _ in 0..20 {
    if model
      .tail(50)
      .await
      .iter()
      .any(|l| l.contains("listening on"))
    {
      tail_seen = true;
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert!(tail_seen, "ring buffer must contain `listening on …`");
  let on_disk = wait_for_log_contents(&log_path, "listening on", Duration::from_secs(2)).await;
  assert!(on_disk, "log file must contain `listening on …`");

  let _ = model.stop(Duration::from_secs(5)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_timeout_triggers_error_state_and_releases_child() {
  // Health-delay > probe-timeout → probe times out before the fake
  // server starts answering 200. Supervisor must SIGKILL the child
  // and flip to Error{cause}.
  let dir = unique_temp("timeout");
  let port = allocate_port();
  let params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  let mut params = params;
  params.extras = vec![
    std::ffi::OsString::from("--health-delay-ms"),
    std::ffi::OsString::from("5000"),
  ];
  let plan = LlamaCppBackend::new().process_spec(
    &params,
    port,
    fake_binary(),
    ProbeOptions {
      interval: Duration::from_millis(50),
      timeout: Duration::from_millis(400),
    },
  );
  let model = spawn(ManagedSpawn {
    id: fake_id(4),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");

  let s = wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Error { .. }),
    Duration::from_secs(5),
  )
  .await;
  match s {
    ManagedState::Error { cause } => assert!(
      cause.contains("health probe timeout"),
      "cause should mention timeout, got `{cause}`"
    ),
    other => panic!("expected Error, got {other:?}"),
  }
  // Stop should be a no-op transition since the child has already
  // been killed by the timeout path.
  let _ = model.stop(Duration::from_millis(500)).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_trapping_child_gets_sigkilled_after_grace() {
  let dir = unique_temp("trap-sigterm");
  let port = allocate_port();
  let mut params = LaunchParams::new(PathBuf::from("/fixture/m.gguf"), LaunchMode::Chat);
  params.extras = vec![std::ffi::OsString::from("--trap-sigterm")];
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = spawn(ManagedSpawn {
    id: fake_id(5),
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("launch.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;
  // Grace short enough that the trapped SIGTERM doesn't kill the
  // child, forcing SIGKILL.
  let final_state = model.stop(Duration::from_millis(500)).await;
  assert_eq!(final_state, ManagedState::Stopped);
  std::fs::remove_dir_all(&dir).ok();
}

async fn wait_for_log_contents(path: &Path, needle: &str, budget: Duration) -> bool {
  let deadline = std::time::Instant::now() + budget;
  loop {
    if let Ok(contents) = std::fs::read_to_string(path) {
      if contents.contains(needle) {
        return true;
      }
    }
    if std::time::Instant::now() > deadline {
      return false;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}
