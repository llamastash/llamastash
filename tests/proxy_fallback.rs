//! Family-MRU fallback for Unit 4's auto-start.
//!
//! Pattern: pre-launch a real supervisor for one model (the eventual
//! fallback target), wire the catalog with a *second* entry whose
//! disk path is missing (so `auto_start` fails at the header read),
//! then request the missing model. The proxy should fall back to
//! the Ready supervisor and stamp the `x-llamastash-served-by` +
//! `x-llamastash-fallback-reason: launch_failed` headers.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 4
//! Test scenarios — error paths + family preference).

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::config::loader::PortRange;
use llamastash::daemon::context::{LaunchEnv, MethodContext};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{
  spawn as supervisor_spawn, ManagedModel, ManagedSpawn, ManagedState,
};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::identity::ModelId;
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pf", label)
}

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(30),
    timeout: Duration::from_secs(15),
  }
}

fn allocate_port() -> u16 {
  let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  l.local_addr().unwrap().port()
}

fn allocate_port_range() -> PortRange {
  let port = allocate_port();
  PortRange {
    start: port,
    end: port,
  }
}

fn write_gguf(dir: &Path, name: &str, arch: &str) -> PathBuf {
  let path = dir.join(name);
  std::fs::write(&path, build_minimal_gguf(arch)).expect("write gguf");
  llamastash::util::paths::canonicalize(&path).expect("canonicalize")
}

fn fake_metadata(arch: &str) -> ModelMetadata {
  ModelMetadata {
    arch: Some(arch.to_string()),
    total_parameters: Some(7_000_000_000),
    parameter_label: Some("7B".to_string()),
    quant: Quant::Q4_K,
    native_ctx: Some(8192),
    chat_template: None,
    tokenizer_kind: Some("llama".to_string()),
    reasoning_hint: false,
    mode_hint: ModeHint::Chat,
    weights_bytes: Some(4_000_000_000),
    mtp: None,
  }
}

fn discovered(path: &Path, display_label: Option<&str>, arch: Option<&str>) -> DiscoveredModel {
  let parent = path.parent().expect("parent").to_path_buf();
  DiscoveredModel {
    path: path.to_path_buf(),
    parent,
    source: ModelSource::UserPath,
    metadata: arch.map(fake_metadata),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: display_label.map(str::to_string),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

async fn wait_for_ready(model: &ManagedModel) {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if matches!(model.state().await, ManagedState::Ready) {
      return;
    }
    if std::time::Instant::now() > deadline {
      panic!("supervisor never reached Ready");
    }
    sleep(Duration::from_millis(20)).await;
  }
}

/// Pre-launch a supervised fake_llama_server for `catalog_path` and
/// register it in the supervisor registry. Returns the live model
/// handle so the test can stop it at the end.
async fn pre_launch(
  catalog_path: &Path,
  registry: &SupervisorRegistry,
  log_dir: &Path,
  mode: LaunchMode,
) -> ManagedModel {
  let port = allocate_port();
  let id = ModelId {
    path: catalog_path.to_path_buf(),
    header_blake3: [0u8; 32],
  };
  let params = LaunchParams::new(catalog_path.to_path_buf(), mode);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = supervisor_spawn(ManagedSpawn {
    id,
    params,
    port,
    mode,
    log_path: log_dir.join("fallback.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_ready(&model).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;
  model
}

async fn build_state(
  models: Vec<DiscoveredModel>,
  registry: SupervisorRegistry,
  log_dir: &Path,
  port_range: PortRange,
) -> (Arc<ProxyState>, MethodContext) {
  build_state_with_fallback(models, registry, log_dir, port_range, true).await
}

async fn build_state_with_fallback(
  models: Vec<DiscoveredModel>,
  registry: SupervisorRegistry,
  log_dir: &Path,
  port_range: PortRange,
  fallback_enabled: bool,
) -> (Arc<ProxyState>, MethodContext) {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let token = ShutdownToken::new();
  let env = LaunchEnv {
    binary: fake_binary(),
    port_range,
    log_dir: log_dir.to_path_buf(),
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::with_catalog(token, catalog)
    .with_supervisors(registry)
    .with_launch_env(env);
  let state = ProxyState::from_context(&ctx, false, fallback_enabled);
  (state, ctx)
}

async fn spawn_listener(
  state: Arc<ProxyState>,
) -> (SocketAddr, ShutdownToken, tokio::task::JoinHandle<()>) {
  let token = ShutdownToken::new();
  let status: StatusCell = new_status_cell();
  let bind_addr = loopback_addr(0);
  let token_for_task = token.clone();
  let status_for_task = Arc::clone(&status);
  let handle = tokio::spawn(async move {
    serve(state, bind_addr, token_for_task, status_for_task)
      .await
      .expect("proxy serve returns Ok");
  });
  let bound = wait_for_listening(&status, Duration::from_secs(2))
    .await
    .expect("listener reaches Listening");
  (bound, token, handle)
}

/// Trigger shutdown and join the serve task; catches a hung serve loop.
async fn shutdown_listener(shutdown: ShutdownToken, handle: tokio::task::JoinHandle<()>) {
  shutdown.trigger();
  tokio::time::timeout(Duration::from_secs(5), handle)
    .await
    .expect("proxy serve loop must exit after shutdown.trigger()")
    .expect("proxy serve task must not panic");
}

async fn wait_for_listening(status: &StatusCell, budget: Duration) -> Option<SocketAddr> {
  let deadline = std::time::Instant::now() + budget;
  while std::time::Instant::now() < deadline {
    if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
      return Some(addr);
    }
    sleep(Duration::from_millis(10)).await;
  }
  None
}

async fn http_post(
  addr: SocketAddr,
  path: &str,
  body: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!(
    "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let needle = b"\r\n\r\n";
  let split = buf
    .windows(needle.len())
    .position(|w| w == needle)
    .expect("CRLFCRLF");
  let head = std::str::from_utf8(&buf[..split]).expect("utf8");
  let mut lines = head.split("\r\n");
  let status_line = lines.next().expect("status");
  let status: u16 = status_line
    .split_whitespace()
    .nth(1)
    .expect("code")
    .parse()
    .expect("u16");
  let mut headers = Vec::new();
  for l in lines {
    if let Some((k, v)) = l.split_once(':') {
      headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
  }
  let body = buf[split + needle.len()..].to_vec();
  (status, headers, body)
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
  headers
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

async fn stop_all(ctx: &MethodContext, extras: &[ManagedModel]) {
  let snap = ctx.supervisors.snapshot().await;
  for (_, m) in snap {
    let _ = m.stop(Duration::from_secs(3)).await;
  }
  for m in extras {
    let _ = m.stop(Duration::from_secs(3)).await;
  }
}

// ---- Scenario 6: launch fails + matching-arch Ready model exists -------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_to_matching_arch_running_model_emits_headers() {
  let dir = unique_temp("match");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  // Pre-launch a Ready model with arch=qwen3. Its catalog path is
  // a real synthetic GGUF so the proxy can resolve the discovered
  // row to the supervisor via the same-path check.
  let ready_path = write_gguf(&dir, "ready-qwen3.gguf", "qwen3");
  let registry = SupervisorRegistry::new();
  let live = pre_launch(&ready_path, &registry, &log_dir, LaunchMode::Chat).await;

  // Catalog also has a row for a missing file with arch=qwen3 —
  // requesting this name drives auto_start to fail at the header
  // read, then the family-MRU fallback fires.
  let missing = dir.join("missing-qwen3.gguf");
  let (state, ctx) = build_state(
    vec![
      discovered(&ready_path, Some("ready-qwen3"), Some("qwen3")),
      discovered(&missing, Some("missing-qwen3"), Some("qwen3")),
    ],
    registry,
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"missing-qwen3","messages":[]}"#;
  let (status, headers, response) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(status, 200, "fallback succeeds");
  let served = header_value(&headers, "x-llamastash-served-by").expect("served-by header");
  assert_eq!(served, "ready-qwen3");
  let reason =
    header_value(&headers, "x-llamastash-fallback-reason").expect("fallback-reason header");
  assert_eq!(reason, "launch_failed");

  // Body is forwarded upstream from the Ready model (its echo
  // mirrors the *fallback*'s request payload — i.e., body.model =
  // "missing-qwen3" because we never rewrite the body). This
  // confirms the byte-pipe contract held on the fallback path.
  let text = String::from_utf8(response).expect("utf8");
  assert!(
    text.contains("\"model\":\"missing-qwen3\""),
    "echo present (body forwarded byte-for-byte): {text}"
  );

  stop_all(&ctx, &[live]).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 7: launch fails + only different-arch Ready exists ------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_to_different_arch_model_when_no_family_match() {
  // Family preference is a *soft* preference (not a requirement)
  // per R155. With no family match available, any-MRU still
  // delivers a fallback.
  let dir = unique_temp("crossarch");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let ready_path = write_gguf(&dir, "ready-llama.gguf", "llama");
  let registry = SupervisorRegistry::new();
  let live = pre_launch(&ready_path, &registry, &log_dir, LaunchMode::Chat).await;

  let missing = dir.join("missing-qwen3.gguf");
  let (state, ctx) = build_state(
    vec![
      discovered(&ready_path, Some("ready-llama"), Some("llama")),
      discovered(&missing, Some("missing-qwen3"), Some("qwen3")),
    ],
    registry,
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"missing-qwen3","messages":[]}"#;
  let (status, headers, _response) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(status, 200);
  let served = header_value(&headers, "x-llamastash-served-by").expect("served-by");
  assert_eq!(served, "ready-llama", "cross-arch any-MRU fallback fires");
  let reason = header_value(&headers, "x-llamastash-fallback-reason").expect("fallback-reason");
  // Cross-arch fallback (qwen3 → llama) is observable via the
  // `family_mismatch` reason so clients can distinguish in-family
  // substitution (graceful) from cross-arch fallback (different
  // response shape — embeddings/rerank in particular).
  assert_eq!(reason, "family_mismatch");

  stop_all(&ctx, &[live]).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 8: requested model has no arch metadata -----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_arch_requested_falls_back_to_any_mru() {
  // R155 unknown-arch fallthrough: when the requested model row
  // has no `general.architecture`, the family-prefer step is
  // skipped and we pick on MRU alone.
  let dir = unique_temp("unkarch");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let ready_path = write_gguf(&dir, "ready-bert.gguf", "bert");
  let registry = SupervisorRegistry::new();
  let live = pre_launch(&ready_path, &registry, &log_dir, LaunchMode::Chat).await;

  // Synthetic GGUF row (no metadata block at all → arch=None).
  let missing = dir.join("missing-noarch.gguf");
  let (state, ctx) = build_state(
    vec![
      discovered(&ready_path, Some("ready-bert"), Some("bert")),
      // arch=None: discovered() with `None` arch skips the metadata.
      discovered(&missing, Some("missing-noarch"), None),
    ],
    registry,
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"missing-noarch","messages":[]}"#;
  let (status, headers, _response) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(status, 200);
  let served = header_value(&headers, "x-llamastash-served-by").expect("served-by");
  assert_eq!(served, "ready-bert");
  let reason = header_value(&headers, "x-llamastash-fallback-reason").expect("reason");
  // Requested arch was None → picked arch is `bert` (known). The
  // proxy treats unknown-arch fallthrough as `family_mismatch` too,
  // since the client can't trust the response shape either way.
  assert_eq!(reason, "family_mismatch");

  stop_all(&ctx, &[live]).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 10: per-request retry — no caching of the failure ------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_request_after_launch_failure_falls_back_independently() {
  // R155: per-request retry — no caching of the failure. After a
  // failed auto-start, a subsequent request must independently
  // engage the fallback path (and ideally retry the auto-start;
  // the proxy's current behavior is "try again every time", which
  // this test exercises).
  let dir = unique_temp("retry");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let ready_path = write_gguf(&dir, "ready-qwen3.gguf", "qwen3");
  let registry = SupervisorRegistry::new();
  let live = pre_launch(&ready_path, &registry, &log_dir, LaunchMode::Chat).await;

  let missing = dir.join("missing-qwen3.gguf");
  let (state, ctx) = build_state(
    vec![
      discovered(&ready_path, Some("ready-qwen3"), Some("qwen3")),
      discovered(&missing, Some("missing-qwen3"), Some("qwen3")),
    ],
    registry,
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"missing-qwen3","messages":[]}"#;
  let (s1, h1, _b1) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(s1, 200);
  assert_eq!(
    header_value(&h1, "x-llamastash-served-by"),
    Some("ready-qwen3")
  );
  let (s2, h2, _b2) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(s2, 200, "second request must also succeed via fallback");
  assert_eq!(
    header_value(&h2, "x-llamastash-served-by"),
    Some("ready-qwen3")
  );

  stop_all(&ctx, &[live]).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 11: fallback disabled — 503 instead of cross-model serve

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_disabled_returns_503_instead_of_picking_other_model() {
  // When the operator sets `ProxyConfig::fallback_enabled = false`
  // (via config, `--no-proxy-fallback`, or the env var), a failed
  // auto-start must surface as a 503 `launch_failed` envelope —
  // never as a silent cross-model serve. This is the contract the
  // disable flag exists to enforce: an embedding client must not
  // receive a chat-shape payload from a fallback Ready supervisor.
  let dir = unique_temp("nofallback");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  // Same shape as Scenario 6: a Ready qwen3 model that would
  // otherwise be the fallback target, plus a catalog row for a
  // missing file whose auto-start will fail.
  let ready_path = write_gguf(&dir, "ready-qwen3.gguf", "qwen3");
  let registry = SupervisorRegistry::new();
  let live = pre_launch(&ready_path, &registry, &log_dir, LaunchMode::Chat).await;

  let missing = dir.join("missing-qwen3.gguf");
  let (state, ctx) = build_state_with_fallback(
    vec![
      discovered(&ready_path, Some("ready-qwen3"), Some("qwen3")),
      discovered(&missing, Some("missing-qwen3"), Some("qwen3")),
    ],
    registry,
    &log_dir,
    allocate_port_range(),
    false,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"missing-qwen3","messages":[]}"#;
  let (status, headers, _response) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(
    status, 503,
    "fallback disabled — launch_failed must return 503"
  );
  // No fallback headers are stamped on the 503 path.
  assert_eq!(
    header_value(&headers, "x-llamastash-served-by"),
    None,
    "served-by header must not leak on 503"
  );
  assert_eq!(
    header_value(&headers, "x-llamastash-fallback-reason"),
    None,
    "fallback-reason header must not leak on 503"
  );

  stop_all(&ctx, &[live]).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
