//! Managed-multiplexer integration: the generic supervisor spawns the
//! `lemond` umbrella from a `LemonadeBackend`-produced spec, reaches `/live`
//! readiness, and the typed client talks to the running umbrella — the
//! headline Phase 2 capability (Lemonade reachable through llamastash)
//! proven end-to-end against the `fake_lemond` fixture (no real `lemond`
//! or NPU needed).
//!
//! Per-model routing (a Lemonade model in the catalog + proxy forwarding to
//! the umbrella) is exercised in `lemonade_route_test.rs`.
#![cfg(feature = "test-fixtures")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use llamastash::backend::identity::{BackendModelId, ModelIdentity};
use llamastash::backend::lemonade::{
  ensure_umbrella, umbrella_launch_id, LemonadeBackend, LemonadeClient,
};
use llamastash::backend::{Backend, LaunchPlan};
use llamastash::config::{LemonadeConfig, PortRange};
use llamastash::daemon::context::{LaunchEnv, MethodContext};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::{LaunchId, SupervisorRegistry};
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::state_store::RunningSnapshot;
use llamastash::daemon::supervisor::{ManagedModel, ManagedState};
use llamastash::ipc::methods::dispatch_request;
use llamastash::ipc::protocol::Request;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use serde_json::json;

fn fake_lemond_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_lemond"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-lemon", label)
}

fn allocate_port() -> u16 {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  port
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(40),
    timeout: Duration::from_secs(5),
  }
}

/// `supervisor::spawn` returns at `Loading` and flips to `Ready` from its
/// background probe task; poll until the umbrella's `/live` probe succeeds.
async fn wait_ready(model: &ManagedModel) {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    match model.state().await {
      ManagedState::Ready => return,
      ManagedState::Error { cause } => panic!("umbrella errored: {cause}"),
      other => {
        assert!(
          Instant::now() < deadline,
          "umbrella not ready in time: {other:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
      }
    }
  }
}

/// Build the umbrella spec a `LemonadeBackend` would produce, pointed at the
/// `fake_lemond` binary on `port`.
fn umbrella_spec(port: u16) -> llamastash::backend::ProcessLaunchSpec {
  let params = LaunchParams::new(PathBuf::from("Qwen2.5-0.5B-Instruct"), LaunchMode::Chat);
  let plan =
    LemonadeBackend::new().prepare_launch(&params, port, fake_lemond_binary(), fast_probe());
  match plan {
    LaunchPlan::DelegateToManager(spec) => spec.umbrella,
    LaunchPlan::SpawnProcess(_) => panic!("lemonade must produce a DelegateToManager plan"),
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_spawns_lemond_umbrella_and_client_talks_to_it() {
  let logs = unique_temp("logs");
  std::fs::create_dir_all(&logs).unwrap();
  let registry = SupervisorRegistry::new();
  let port = allocate_port();

  // Ensure the umbrella: the generic supervisor spawns fake_lemond and
  // blocks until /live returns 200 (readiness from the LemonadeBackend spec).
  let model = ensure_umbrella(
    &registry,
    port,
    umbrella_spec(port),
    logs.join("lemond.log"),
  )
  .await
  .expect("umbrella should spawn");
  wait_ready(&model).await;
  assert_eq!(model.port(), port);

  // The typed client can now talk to the running umbrella.
  let client = LemonadeClient::new(port).expect("client");
  client.live().await.expect("/live reachable");
  let models = client.list_models().await.expect("models list");
  assert!(
    models.iter().any(|m| m == "Qwen2.5-0.5B-Instruct"),
    "fake lemond should list its models, got {models:?}"
  );
  client.load("Qwen2.5-0.5B-Instruct").await.expect("load ok");

  // Stop the umbrella so the spawned `fake_lemond` child doesn't outlive the
  // test. On Windows a leaked child holding inherited stdio handles makes
  // `cargo test` hang at exit; on Unix it's harmless but we clean up anyway.
  model.stop(Duration::from_secs(3)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_umbrella_is_idempotent() {
  let logs = unique_temp("logs2");
  std::fs::create_dir_all(&logs).unwrap();
  let registry = SupervisorRegistry::new();
  let port = allocate_port();

  let first = ensure_umbrella(&registry, port, umbrella_spec(port), logs.join("a.log"))
    .await
    .expect("first ensure");
  // A second ensure must reuse the registered umbrella, not spawn another.
  // Pass a different port to prove it is ignored when one already exists.
  let second = ensure_umbrella(
    &registry,
    allocate_port(),
    umbrella_spec(port),
    logs.join("b.log"),
  )
  .await
  .expect("second ensure");

  assert_eq!(
    first.port(),
    second.port(),
    "reused umbrella keeps its port"
  );
  let snapshot = registry.snapshot().await;
  let umbrellas = snapshot
    .iter()
    .filter(|(id, _)| *id == umbrella_launch_id())
    .count();
  assert_eq!(umbrellas, 1, "exactly one umbrella should be registered");

  // `first` and `second` are the same reused umbrella — stop it once so the
  // `fake_lemond` child doesn't leak (Windows `cargo test` exit hang).
  first.stop(Duration::from_secs(3)).await;
}

/// Regression for the blocking-preload bug: `start_model` used to await
/// the lemond load inside its IPC reply, so a cold load (up to lemond's
/// 120 s budget) outlived the CLI's 5 s reply timeout — the client hung
/// up, hyper cancelled the handler mid-await, and the launch silently
/// evaporated (no snapshot, no log line). The preload now runs as a
/// background task that records its outcome in the registry's
/// delegated-state map, so:
///   - the reply returns promptly even when the load is slow;
///   - the row surfaces `loading` → `ready` for a good load;
///   - a rejected load surfaces `error` + cause instead of a phantom
///     `ready` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_model_replies_promptly_and_records_preload_outcome() {
  let dir = unique_temp("preload");
  std::fs::create_dir_all(&dir).unwrap();
  let registry = SupervisorRegistry::new();
  let port = allocate_port();
  let env = LaunchEnv {
    // Never spawned: every launch in this test delegates to the umbrella.
    binary: PathBuf::from("/nonexistent/llama-server"),
    port_range: PortRange {
      start: 41000,
      end: 41999,
    },
    log_dir: dir.clone(),
    probe: fast_probe(),
    arch_defaults: Default::default(),
    servers: Default::default(),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::new(ShutdownToken::new())
    .with_supervisors(registry.clone())
    .with_launch_env(env)
    .with_backend(
      llamastash::backend::BackendConfig {
        lemonade: LemonadeConfig {
          enabled: Some(true),
          servers: vec![llamastash::backend::ServerConfig {
            binary: fake_lemond_binary(),
            name: None,
          }],
          port,
        },
        ..Default::default()
      },
      std::collections::BTreeMap::new(),
    );

  // Delegated rows now carry a plain `L#` launch id (shared with every other
  // backend), so locate them by their synthetic `lemonade://<name>` path — the
  // stable per-model handle — rather than a name-encoded id.
  let row_state = |body: &serde_json::Value, name: &str| -> Option<(String, Option<String>)> {
    body["models"].as_array().and_then(|models| {
      models
        .iter()
        .find(|m| m["id"]["path"] == format!("lemonade://{name}"))
        .map(|m| {
          (
            m["state"]["state"].as_str().unwrap_or_default().to_string(),
            m["state"]["cause"].as_str().map(str::to_string),
          )
        })
    })
  };
  let launch_id_of = |body: &serde_json::Value, name: &str| -> Option<String> {
    body["models"].as_array().and_then(|models| {
      models
        .iter()
        .find(|m| m["id"]["path"] == format!("lemonade://{name}"))
        .and_then(|m| m["launch_id"].as_str().map(str::to_string))
    })
  };
  let status = |ctx: &MethodContext, id: i64| {
    let ctx = ctx.clone();
    async move {
      dispatch_request(&ctx, Request::new(id, "status", None))
        .await
        .result
        .expect("status result")
    }
  };

  // Slow load (fake_lemond sleeps 1.5 s on a name containing `slow`): the
  // reply must come back well before the load completes.
  let started = Instant::now();
  let resp = dispatch_request(
    &ctx,
    Request::new(
      1,
      "start_model",
      Some(json!({"model_path": "lemonade://Qwen-slow"})),
    ),
  )
  .await;
  let elapsed = started.elapsed();
  let body = resp.result.expect("start_model result");
  // A delegated launch returns the *model's* own `L#` handle (drawn from the
  // same registry counter as any llama.cpp / ds4 launch), not the umbrella id.
  let slow_launch_id = body["launch_id"].as_str().expect("launch_id string");
  assert!(
    slow_launch_id.starts_with('L') && slow_launch_id != umbrella_launch_id().as_str(),
    "delegated start returns the model's L# handle, got {slow_launch_id}"
  );
  assert!(
    elapsed < Duration::from_millis(1200),
    "start_model must not block on the preload (took {elapsed:?})"
  );

  // The row exists immediately and reflects the in-flight preload, then
  // flips to ready once the (slow) load lands.
  let body = status(&ctx, 2).await;
  let (state, _) = row_state(&body, "Qwen-slow").expect("row emitted while preloading");
  assert!(
    state == "loading" || state == "ready",
    "in-flight preload surfaces as loading (or ready if it already landed), got {state}"
  );
  let deadline = Instant::now() + Duration::from_secs(15);
  loop {
    let body = status(&ctx, 3).await;
    let (state, cause) = row_state(&body, "Qwen-slow").expect("row stays emitted");
    if state == "ready" {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "slow preload never became ready (state {state}, cause {cause:?})"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  // Recents: a successful Lemonade preload records `last_params` (keyed on the
  // synthetic `lemonade://<name>` path), so the model surfaces in the TUI's
  // `↺ Recent` section like any other launch. Recorded on the Ready transition,
  // so poll briefly.
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let lp = dispatch_request(&ctx, Request::new(30, "last_params_list", None))
      .await
      .result
      .expect("last_params_list");
    let recorded = lp["last_params"]
      .as_array()
      .expect("last_params array")
      .iter()
      .any(|r| r["model_path"] == "lemonade://Qwen-slow");
    if recorded {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "a Ready Lemonade launch must record last_params: {lp}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  // Rejected load (name containing `fail` → 500): the row must surface
  // `error` with lemond's cause, not a phantom `ready`.
  let resp = dispatch_request(
    &ctx,
    Request::new(
      4,
      "start_model",
      Some(json!({"model_path": "lemonade://Broken-fail"})),
    ),
  )
  .await;
  resp.result.expect("start_model dispatches the preload");
  let deadline = Instant::now() + Duration::from_secs(15);
  let cause = loop {
    let body = status(&ctx, 5).await;
    let (state, cause) = row_state(&body, "Broken-fail").expect("failed row stays emitted");
    if state == "error" {
      break cause;
    }
    assert!(
      Instant::now() < deadline,
      "rejected preload never surfaced as error (state {state})"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  };
  assert!(
    cause.is_some_and(|c| !c.is_empty()),
    "error row must carry lemond's cause"
  );

  // The failed row is still clearable by its `L#` launch id.
  let body = status(&ctx, 6).await;
  let broken_id = launch_id_of(&body, "Broken-fail").expect("failed row carries a launch id");
  let resp = dispatch_request(
    &ctx,
    Request::new(7, "stop_model", Some(json!({"launch_id": broken_id}))),
  )
  .await;
  resp.result.expect("failed delegated row must be stoppable");

  if let Some(umbrella) = registry.get(&umbrella_launch_id()).await {
    umbrella.stop(Duration::from_secs(3)).await;
  }
}

/// Delegated-model visibility: a model made resident in the umbrella (its
/// RunningSnapshot persisted, as `start_delegated_lemonade` does) surfaces as
/// its own `status` row under the `lemonade:<name>` launch id, and
/// `stop_model` on that id unloads it from the umbrella + drops the row —
/// while the umbrella itself keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_projects_delegated_models_and_stop_unloads_them() {
  let logs = unique_temp("delegated");
  std::fs::create_dir_all(&logs).unwrap();
  let registry = SupervisorRegistry::new();
  let port = allocate_port();
  let umbrella = ensure_umbrella(
    &registry,
    port,
    umbrella_spec(port),
    logs.join("lemond.log"),
  )
  .await
  .expect("umbrella spawns");
  wait_ready(&umbrella).await;

  // Share the registry with a MethodContext and persist the snapshot the way
  // `start_delegated_lemonade` does after a successful preload.
  let ctx = MethodContext::new(ShutdownToken::new()).with_supervisors(registry);
  let name = "Qwen2.5-0.5B-Instruct";
  let client = LemonadeClient::new(port).expect("client");
  client.load(name).await.expect("preload");
  let identity = ModelIdentity::Backend(BackendModelId {
    backend: "lemonade".to_string(),
    name: name.to_string(),
  });
  ctx
    .state
    .mutate(|s| {
      s.running.push(RunningSnapshot {
        id: identity,
        pid: 0,
        port,
        started_at: 0,
        // The `L#` `start_delegated_lemonade` would have stamped from the
        // registry counter — the delegated row's sole home for its handle.
        launch_id: Some(LaunchId("L1".to_string())),
        name: None,
        params: LaunchParams::new(
          PathBuf::from(format!("lemonade://{name}")),
          LaunchMode::Chat,
        ),
        actuals: Default::default(),
        resolved_backend: "lemonade".to_string(),
      })
    })
    .await;

  // `status` projects the delegated row: catalog-matching path, the
  // umbrella's port, state mirrored from the (Ready) umbrella, and the
  // plain `L#` launch id.
  let resp = dispatch_request(&ctx, Request::new(1, "status", None)).await;
  let body = resp.result.expect("status result");
  let models = body["models"].as_array().expect("models array");
  let row = models
    .iter()
    .find(|m| m["id"]["path"] == format!("lemonade://{name}"))
    .expect("delegated lemonade row must be emitted");
  assert_eq!(row["launch_id"], "L1");
  assert_eq!(row["port"], json!(port));
  assert_eq!(row["state"]["state"], "ready");
  assert_eq!(row["mode"], "chat");
  // A delegated model has no process of its own — no pid (`-` in the CLI).
  assert!(
    row["pid"].is_null(),
    "delegated row must report no pid, got {:?}",
    row["pid"]
  );

  // Stop via the `L#` id: the umbrella unloads the model (fake_lemond clears
  // its resident slot) and the row disappears; the umbrella row stays.
  let resp = dispatch_request(
    &ctx,
    Request::new(2, "stop_model", Some(json!({"launch_id": "L1"}))),
  )
  .await;
  let stop_body = resp.result.expect("stop_model result");
  assert_eq!(stop_body["state"]["state"], "stopped");
  let health = client.health().await.expect("health");
  assert_eq!(
    health.model_loaded, None,
    "stop must unload the model from the umbrella"
  );
  let resp = dispatch_request(&ctx, Request::new(3, "status", None)).await;
  let body = resp.result.expect("status result");
  let models = body["models"].as_array().expect("models array");
  assert!(
    !models
      .iter()
      .any(|m| m["id"]["path"] == format!("lemonade://{name}")),
    "stopped delegated row must drop out of status"
  );
  let umbrella_row = models
    .iter()
    .find(|m| m["launch_id"] == umbrella_launch_id().as_str())
    .expect("umbrella row must survive a delegated stop");
  // The umbrella is the lemonade backend — its reported backend must be stable
  // `lemonade`, not inferred from an arbitrary snapshot sharing its port (which
  // flip-flopped to `llamacpp` when no delegated model was resident).
  assert_eq!(
    umbrella_row["backend"], "lemonade",
    "umbrella row backend must be deterministically lemonade"
  );
  // Only the umbrella carries a pid — it's the one real process.
  assert!(
    umbrella_row["pid"].is_number(),
    "umbrella row must carry the shared process pid, got {:?}",
    umbrella_row["pid"]
  );

  // A second stop of the same id is an InvalidParams error (unknown row).
  let resp = dispatch_request(
    &ctx,
    Request::new(4, "stop_model", Some(json!({"launch_id": "L1"}))),
  )
  .await;
  assert!(
    resp.error.is_some(),
    "double-stop of a delegated id must error"
  );

  umbrella.stop(Duration::from_secs(3)).await;
}
