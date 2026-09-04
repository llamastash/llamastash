//! Phase 2b Unit 3 — a Lemonade-backed model launches the umbrella and
//! routes inference through the proxy.
//!
//! End-to-end against the `fake_lemond` fixture (no real `lemond`/NPU):
//!   1. `ensure_umbrella` supervises `fake_lemond` and reaches `/live`.
//!   2. A Lemonade-tagged catalog row resolves like any other model.
//!   3. `POST /v1/chat/completions` for that model is forwarded to the
//!      umbrella's port with the `/api` prefix Lemonade serves OpenAI on,
//!      so the request lands on `fake_lemond`'s `/api/v1/chat/completions`.
//!   4. A second Lemonade model reuses the one umbrella.
//!   5. With no umbrella up, the request fails cleanly (503), never panics.
//!
//! Plan: docs/plans/2026-06-09-002-feat-lemonade-phase2b-plan.md (Unit 3).

#![cfg(feature = "test-fixtures")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use llamastash::backend::lemonade::{
  ensure_umbrella, umbrella_launch_id, LemonadeBackend, LemonadeClient,
};
use llamastash::backend::{Backend, LaunchPlan, ProcessLaunchSpec};
use llamastash::daemon::context::MethodContext;
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{ManagedModel, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::eviction;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

fn fake_lemond_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_lemond"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-lemroute", label)
}

fn allocate_port() -> u16 {
  let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  l.local_addr().unwrap().port()
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(40),
    timeout: Duration::from_secs(5),
  }
}

/// The umbrella spec a `LemonadeBackend` produces, pointed at `fake_lemond`.
fn umbrella_spec(port: u16) -> ProcessLaunchSpec {
  let params = LaunchParams::new(PathBuf::from("ignored"), LaunchMode::Chat);
  match LemonadeBackend::new().prepare_launch(&params, port, fake_lemond_binary(), fast_probe()) {
    LaunchPlan::DelegateToManager(spec) => spec.umbrella,
    LaunchPlan::SpawnProcess(_) => panic!("lemonade must produce a DelegateToManager plan"),
  }
}

/// A Lemonade-registry catalog row (no local file). Discovery (Unit 5)
/// produces these from `lemond /api/v1/models`; here we inject them directly.
fn lemonade_model(name: &str) -> DiscoveredModel {
  DiscoveredModel {
    path: PathBuf::from(format!("/lemonade/{name}")),
    parent: PathBuf::from("/lemonade"),
    source: ModelSource::Lemonade,
    metadata: None,
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: Some(name.to_string()),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

async fn wait_ready(model: &ManagedModel) {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    match model.state().await {
      ManagedState::Ready => return,
      ManagedState::Error { cause } => panic!("umbrella errored: {cause}"),
      other => {
        assert!(Instant::now() < deadline, "umbrella not ready: {other:?}");
        sleep(Duration::from_millis(25)).await;
      }
    }
  }
}

async fn proxy_state_with(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(supervisors);
  ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES)
}

async fn spawn_listener(
  state: Arc<ProxyState>,
) -> (SocketAddr, ShutdownToken, tokio::task::JoinHandle<()>) {
  let token = ShutdownToken::new();
  let status: StatusCell = new_status_cell();
  let token_for_task = token.clone();
  let status_for_task = Arc::clone(&status);
  let handle = tokio::spawn(async move {
    serve(state, loopback_addr(0), token_for_task, status_for_task)
      .await
      .expect("proxy serve returns Ok");
  });
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
      return (addr, token, handle);
    }
    assert!(Instant::now() < deadline, "listener never bound");
    sleep(Duration::from_millis(10)).await;
  }
}

async fn http_post(addr: SocketAddr, path: &str, body: &str) -> (u16, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!(
    "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  let needle = b"\r\n\r\n";
  let split = buf
    .windows(needle.len())
    .position(|w| w == needle)
    .expect("CRLFCRLF");
  let head = std::str::from_utf8(&buf[..split]).expect("utf8 head");
  let status: u16 = head
    .split_whitespace()
    .nth(1)
    .expect("status code")
    .parse()
    .expect("parse status");
  (status, buf[split + needle.len()..].to_vec())
}

async fn shutdown(token: ShutdownToken, handle: tokio::task::JoinHandle<()>) {
  token.trigger();
  let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lemonade_model_routes_through_proxy_to_umbrella() {
  let logs = unique_temp("route");
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

  let state = proxy_state_with(vec![lemonade_model("Qwen2.5-0.5B-Instruct")], registry).await;
  let (addr, token, handle) = spawn_listener(state).await;

  let (status, body) = http_post(
    addr,
    "/v1/chat/completions",
    r#"{"model":"Qwen2.5-0.5B-Instruct","messages":[{"role":"user","content":"hi"}]}"#,
  )
  .await;
  let body = String::from_utf8_lossy(&body);

  assert_eq!(
    status, 200,
    "lemonade chat must route to the umbrella; body={body}"
  );
  assert!(
    body.contains("hi from lemond") && body.contains("lemonade-chat-1"),
    "response must come from fake_lemond's /api/v1/chat/completions, got: {body}"
  );

  umbrella.stop(Duration::from_secs(3)).await;
  shutdown(token, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_lemonade_model_reuses_the_one_umbrella() {
  let logs = unique_temp("reuse");
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

  let state = proxy_state_with(
    vec![
      lemonade_model("Qwen2.5-0.5B-Instruct"),
      lemonade_model("Llama-3.1-8B"),
    ],
    registry,
  )
  .await;
  let (addr, token, handle) = spawn_listener(state).await;

  for model in ["Qwen2.5-0.5B-Instruct", "Llama-3.1-8B"] {
    let (status, body) = http_post(
      addr,
      "/v1/chat/completions",
      &format!(r#"{{"model":"{model}","messages":[]}}"#),
    )
    .await;
    let body = String::from_utf8_lossy(&body);
    assert_eq!(
      status, 200,
      "{model} should route to the shared umbrella; body={body}"
    );
    assert!(body.contains("hi from lemond"), "{model}: got {body}");
  }

  umbrella.stop(Duration::from_secs(3)).await;
  shutdown(token, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_lemonade_model_is_unloaded_but_umbrella_stays_up() {
  // Lifecycle-aware eviction: an idle Lemonade model is freed via
  // /api/v1/unload (not SIGTERM); the shared umbrella process stays Ready.
  let logs = unique_temp("evict");
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

  // Load a model; fake_lemond now reports it resident.
  let client = LemonadeClient::new(port).expect("client");
  client.load("Qwen2.5-0.5B-Instruct").await.expect("load");
  assert_eq!(
    client.health().await.unwrap().model_loaded.as_deref(),
    Some("Qwen2.5-0.5B-Instruct"),
    "model should be resident before the idle sweep"
  );

  // Build the proxy state around an explicit MethodContext so the test
  // can watch the persisted running snapshot the sweep mutates.
  let catalog = llamastash::discovery::ModelCatalog::new();
  catalog
    .upsert(lemonade_model("Qwen2.5-0.5B-Instruct"))
    .await;
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(registry.clone());
  let state =
    llamastash::proxy::state::ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES);
  // Persist the running snapshot + recorded state the way `start_model`
  // does, so the sweep's row cleanup has something real to clear.
  let identity = llamastash::backend::identity::ModelIdentity::Backend(
    llamastash::backend::identity::BackendModelId {
      backend: "lemonade".to_string(),
      name: "Qwen2.5-0.5B-Instruct".to_string(),
    },
  );
  ctx
    .state
    .mutate(|s| {
      s.running
        .push(llamastash::daemon::state_store::RunningSnapshot {
          id: identity,
          pid: 0,
          port,
          started_at: 0,
          // Real delegated rows always carry the `L#` stamped by the launch;
          // idle eviction reads it off the snapshot and hands it to `stop`, which
          // unloads the model from the umbrella. (A `None` here is treated as an
          // unreachable leftover everywhere, `status` included.)
          launch_id: Some(llamastash::daemon::registry::LaunchId(
            "evict-L1".to_string(),
          )),
          name: None,
          params: LaunchParams::new(
            PathBuf::from("lemonade://Qwen2.5-0.5B-Instruct"),
            LaunchMode::Chat,
          ),
          actuals: Default::default(),
          resolved_backend: "lemonade".to_string(),
        })
    })
    .await;
  registry
    .set_delegated_state("Qwen2.5-0.5B-Instruct", ManagedState::Ready)
    .await;
  // Stamp the umbrella's MRU, then sweep with a ~0 TTL so it counts idle.
  state.touch_mru(umbrella.id()).await;
  eviction::sweep_once(&state, Duration::from_nanos(1)).await;

  // The sweep dispatches the unload via tokio::spawn; poll the umbrella
  // until it reports no resident model.
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    if client.health().await.unwrap().model_loaded.is_none() {
      break;
    }
    assert!(Instant::now() < deadline, "idle model was never unloaded");
    sleep(Duration::from_millis(25)).await;
  }

  // An evicted model must also drop its running snapshot + recorded
  // state — same end state as a process eviction, where the supervisor
  // row is pruned — so `status` stops listing it as running.
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let gone = ctx.state.snapshot().await.running.is_empty();
    if gone {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "evicted model's running snapshot was never dropped"
    );
    sleep(Duration::from_millis(25)).await;
  }
  assert!(
    registry
      .delegated_state("Qwen2.5-0.5B-Instruct")
      .await
      .is_none(),
    "evicted model's recorded state must be forgotten"
  );

  // The umbrella process itself must still be registered + Ready.
  let still = registry
    .get(&umbrella_launch_id())
    .await
    .expect("umbrella still registered after model unload");
  assert!(
    matches!(still.state().await, ManagedState::Ready),
    "umbrella must stay up — only the model is unloaded"
  );

  // Clean up the umbrella child so `fake_lemond` doesn't leak past the test
  // (a leaked child hangs `cargo test` exit on Windows).
  umbrella.stop(Duration::from_secs(3)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lemonade_request_without_umbrella_fails_cleanly() {
  // Catalog has the Lemonade row but no umbrella is registered. The proxy
  // must surface a clean error (not a panic, not a GGUF-header-read 503).
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(vec![lemonade_model("Qwen2.5-0.5B-Instruct")], registry).await;
  let (addr, token, handle) = spawn_listener(state).await;

  let (status, body) = http_post(
    addr,
    "/v1/chat/completions",
    r#"{"model":"Qwen2.5-0.5B-Instruct","messages":[]}"#,
  )
  .await;
  let body = String::from_utf8_lossy(&body);
  assert_eq!(
    status, 503,
    "umbrella-down lemonade request must be a clean 503; got {status} body={body}"
  );
  assert!(
    body.contains("lemonade") || body.contains("umbrella") || body.contains("unavailable"),
    "error should name the unavailable backend, got: {body}"
  );

  shutdown(token, handle).await;
}
