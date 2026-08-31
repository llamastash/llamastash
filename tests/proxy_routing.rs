//! Integration tests for Unit 3 (name resolution + HTTP forwarding).
//!
//! The fixture pattern:
//!   1. Spawn `fake_llama_server` (the `test-fixtures` binary) via
//!      `supervisor::spawn`. The supervisor probes `/health`, the
//!      fake answers 200, and we wait until `ManagedState::Ready`.
//!   2. Register the resulting `ManagedModel` in a fresh
//!      `SupervisorRegistry`. The catalog gets a matching
//!      `DiscoveredModel` so `resolve_model` can find the row.
//!   3. Build a `ProxyState` pointing at that catalog + registry
//!      and spawn the proxy listener on an ephemeral port.
//!   4. Drive the proxy with a hand-rolled HTTP client (avoiding a
//!      reqwest dep in the test crate) so we can assert byte
//!      exactness of the forwarded body.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 3).

#![cfg(feature = "test-fixtures")]

use std::{
  net::SocketAddr,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::daemon::context::MethodContext;
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{spawn as supervisor_spawn, ManagedSpawn, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::identity::ModelId;
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

// --- helpers -------------------------------------------------------------

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pr", label)
}

fn allocate_port() -> u16 {
  let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  l.local_addr().unwrap().port()
}

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(30),
    timeout: Duration::from_secs(5),
  }
}

fn fake_metadata(arch: &str) -> ModelMetadata {
  ModelMetadata {
    arch: Some(arch.to_string()),
    total_parameters: Some(7_000_000_000),
    parameter_label: Some("7B".to_string()),
    quant: Quant::Q4_K,
    quant_label: None,
    native_ctx: Some(8192),
    chat_template: None,
    tokenizer_kind: Some("llama".to_string()),
    reasoning_hint: false,
    mode_hint: ModeHint::Chat,
    weights_bytes: Some(4_000_000_000),
    mtp: None,
  }
}

fn discovered(path: &str, display_label: Option<&str>, arch: &str) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p.parent().unwrap().to_path_buf();
  DiscoveredModel {
    path: p,
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata(arch)),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: display_label.map(str::to_string),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
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
    sleep(Duration::from_millis(20)).await;
  }
}

/// Spawn a fake_llama_server via the supervisor and return the
/// resulting `ManagedModel` (already Ready). Caller is responsible
/// for stopping it at the end of the test.
async fn spawn_fake_supervisor(
  catalog_path: &str,
  log_dir: &Path,
  mode: LaunchMode,
) -> (llamastash::daemon::supervisor::ManagedModel, u16, ModelId) {
  let port = allocate_port();
  // The `ModelId.path` must match the catalog row's path so the
  // `same_path` check inside `route::decide` lines them up.
  let id = ModelId {
    path: PathBuf::from(catalog_path),
    header_blake3: [0u8; 32],
  };
  let params = LaunchParams::new(PathBuf::from(catalog_path), mode);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = supervisor_spawn(ManagedSpawn {
    id: id.clone(),
    params,
    port,
    mode,
    log_path: log_dir.join("fake.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn fake_llama_server");
  wait_for_state(
    &model,
    |s| matches!(s, ManagedState::Ready),
    Duration::from_secs(5),
  )
  .await;
  (model, port, id)
}

async fn spawn_listener_with_state(
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

/// Build a ProxyState whose catalog has the supplied discovered
/// models and whose supervisor registry is the provided one (default
/// body cap).
async fn proxy_state_with(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
) -> Arc<ProxyState> {
  proxy_state_with_cap(models, supervisors, DEFAULT_BODY_LIMIT_BYTES).await
}

/// Like [`proxy_state_with`] but with an explicit body cap — the
/// `proxy.max_body_size` seam the daemon passes through
/// `from_context_with_auth` in production.
async fn proxy_state_with_cap(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
  max_body_size: usize,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(supervisors);
  ProxyState::from_context(&ctx, false, true, max_body_size)
}

/// Send an HTTP POST and read the response head + body. Returns
/// `(status, headers, body_bytes)`. Closes the connection after.
async fn http_post(
  addr: SocketAddr,
  path: &str,
  body: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let mut req = format!(
    "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
    body.len()
  );
  for (k, v) in extra_headers {
    req.push_str(&format!("{k}: {v}\r\n"));
  }
  req.push_str("\r\n");
  req.push_str(body);
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
    .expect("CRLFCRLF terminator");
  let head = std::str::from_utf8(&buf[..split]).expect("utf8 headers");
  let mut lines = head.split("\r\n");
  let status_line = lines.next().expect("status line");
  let status: u16 = status_line
    .split_whitespace()
    .nth(1)
    .expect("status code")
    .parse()
    .expect("parse status");
  let mut headers = Vec::new();
  for l in lines {
    if let Some((k, v)) = l.split_once(':') {
      headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
  }
  let body = buf[split + needle.len()..].to_vec();
  (status, headers, body)
}

// --- happy paths --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completion_streams_back_byte_identical() {
  let dir = unique_temp("chat");
  let catalog_path = "/fixture/qwen3.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Chat).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;

  // Catalog row whose path matches the supervisor's ModelId path
  // (the proxy uses path equality to map `resolve_model`'s row to
  // the running supervisor).
  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("qwen3"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = serde_json::json!({
    "model": "qwen3",
    "messages": [{"role": "user", "content": "hi"}],
    "stream": true,
  })
  .to_string();

  let (status, headers, response_body) = http_post(addr, "/v1/chat/completions", &body, &[]).await;
  assert_eq!(status, 200);

  // No fallback headers on the happy path.
  for (k, _) in &headers {
    assert!(
      !k.starts_with("x-llamastash-"),
      "no fallback headers on the happy path; saw {k}"
    );
  }

  // The body contains the echoed model name verbatim. We don't
  // assert byte-exact equality against a direct curl here (would
  // require routing reqwest into the test crate); instead we check
  // the SSE frames contain the expected echo. Byte-exactness is
  // guaranteed by `forward.rs` piping `bytes_stream()` straight
  // through; the echo assertion confirms no rewrite happened.
  let text = String::from_utf8(response_body).expect("utf8 response body");
  assert!(
    text.contains("\"model\":\"qwen3\""),
    "echoed model present in: {text}"
  );
  assert!(
    text.contains("\"delta\":{\"content\":\"hi\"}"),
    "expected message frame in: {text}"
  );
  // R-15: the fake fixture emits two SSE data frames (the message
  // and the finish_reason:stop terminator). Count them so a future
  // regression that silently drops the second frame surfaces here.
  let data_frames = text.matches("\ndata:").count() + usize::from(text.starts_with("data:"));
  assert!(
    data_frames >= 2,
    "expected >=2 SSE data frames; saw {data_frames}: {text}"
  );
  assert!(
    text.contains("\"finish_reason\":\"stop\""),
    "expected finish_reason stop frame in: {text}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embeddings_endpoint_round_trips_json() {
  let dir = unique_temp("embed");
  let catalog_path = "/fixture/nomic-embed.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Embedding).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;

  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("nomic-embed"), "bert")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = serde_json::json!({
    "model": "nomic-embed",
    "input": "hello",
  })
  .to_string();

  let (status, _headers, response_body) = http_post(addr, "/v1/embeddings", &body, &[]).await;
  assert_eq!(status, 200);
  // The fake fixture returns a fixed shape; we just assert it
  // parses + carries one embedding row.
  let parsed: Value = serde_json::from_slice(&response_body).expect("json body");
  assert_eq!(parsed["object"], "list");
  assert!(parsed["data"][0]["embedding"].is_array());

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerank_endpoint_forwards() {
  let dir = unique_temp("rerank");
  let catalog_path = "/fixture/bge-rerank.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Rerank).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;

  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("bge-rerank"), "bert")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = serde_json::json!({
    "model": "bge-rerank",
    "query": "x",
    "documents": ["a", "b"],
  })
  .to_string();

  let (status, _headers, response_body) = http_post(addr, "/v1/rerank", &body, &[]).await;
  assert_eq!(status, 200);
  let parsed: Value = serde_json::from_slice(&response_body).expect("json body");
  assert!(parsed["results"].is_array());

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_messages_endpoint_forwards() {
  // The Anthropic `/v1/messages` surface rides the same byte-pure
  // forward path as `/v1/chat/completions`: `body.model` resolves to a
  // running supervisor and the response streams back untouched.
  let dir = unique_temp("messages");
  let catalog_path = "/fixture/qwen-chat.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Chat).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;

  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("qwen-chat"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = serde_json::json!({
    "model": "qwen-chat",
    "max_tokens": 16,
    "messages": [{"role": "user", "content": "hi"}],
  })
  .to_string();

  let (status, _headers, response_body) = http_post(addr, "/v1/messages", &body, &[]).await;
  assert_eq!(status, 200);
  let parsed: Value = serde_json::from_slice(&response_body).expect("json body");
  assert_eq!(parsed["type"], "message");
  // Pass-through contract: the upstream echoes the resolved model back.
  assert_eq!(parsed["model"], "qwen-chat");

  // `/v1/messages/count_tokens` rides the same path.
  let (ct_status, _h, ct_body) = http_post(addr, "/v1/messages/count_tokens", &body, &[]).await;
  assert_eq!(ct_status, 200);
  let ct: Value = serde_json::from_slice(&ct_body).expect("json body");
  assert!(ct["input_tokens"].is_number());

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// --- error paths --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_model_name_returns_404_model_not_found() {
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(
    vec![discovered("/m/qwen3.gguf", Some("qwen3"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = r#"{"model":"definitely-absent","messages":[]}"#;
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", body, &[]).await;
  assert_eq!(status, 404);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["type"], "model_not_found");
  // Empty matches list still serializes to the wire field for the
  // not-found case (we want clients to be able to branch on
  // presence-of-field).
  assert!(
    v["error"]["matches"]
      .as_array()
      .map(|a| a.is_empty())
      .unwrap_or(true),
    "matches absent or empty: {v}"
  );
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_substring_returns_400_with_candidates() {
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(
    vec![
      discovered("/m/qwen-coder-7b.gguf", None, "qwen3"),
      discovered("/m/qwen-coder-13b.gguf", None, "qwen3"),
    ],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = r#"{"model":"qwen-coder","messages":[]}"#;
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", body, &[]).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["type"], "ambiguous_model");
  let names: Vec<&str> = v["error"]["matches"]
    .as_array()
    .expect("matches array")
    .iter()
    .filter_map(|x| x.as_str())
    .collect();
  assert!(names.contains(&"qwen-coder-7b.gguf"), "got: {names:?}");
  assert!(names.contains(&"qwen-coder-13b.gguf"), "got: {names:?}");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_model_field_returns_400_model_required() {
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(Vec::new(), registry).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", "{}", &[]).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["type"], "invalid_request");
  assert_eq!(v["error"]["code"], "model_required");
  assert_eq!(v["error"]["param"], "model");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_model_string_returns_400_model_required() {
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(Vec::new(), registry).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, _headers, response) =
    http_post(addr, "/v1/chat/completions", r#"{"model":""}"#, &[]).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["code"], "model_required");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_match_without_running_supervisor_returns_launch_failed() {
  // Catalog row exists but no supervisor is Ready *and* the path
  // points at a non-existent file → Unit 4's auto-start fails at
  // the GGUF header read; no Ready fallback exists either → 503
  // `launch_failed` with `running: []`.
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(
    vec![discovered("/m/qwen3.gguf", Some("qwen3"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = r#"{"model":"qwen3","messages":[]}"#;
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", body, &[]).await;
  assert_eq!(status, 503);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["type"], "launch_failed");
  let running = v["error"]["running"]
    .as_array()
    .expect("running array present");
  assert!(running.is_empty(), "no Ready models to fall back to");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_exceeding_cap_returns_413() {
  let registry = SupervisorRegistry::new();
  // A 1 KiB cap: the default (16 MiB) would let this body through,
  // so the refusal proves the configured cap — not the default —
  // is in force.
  let state = proxy_state_with_cap(Vec::new(), registry, 1024).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  // A body just over the cap: the `Limited` adapter trips before
  // serde even runs.
  let pad = "A".repeat(1024 + 16);
  let body = format!(r#"{{"model":"x","pad":"{pad}"}}"#);
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", &body, &[]).await;
  assert_eq!(status, 413);
  // Lock the error envelope shape: a regression that responds with
  // a plain hyper 413 or the wrong `type` discriminator must fail
  // this test, not just slip through unnoticed because the status
  // code happens to match.
  let v: Value = serde_json::from_slice(&response).expect("json body");
  assert_eq!(v["error"]["type"], "payload_too_large");
  // The message names the configured cap (human unit + exact byte
  // count), not the default.
  let message = v["error"]["message"].as_str().expect("message");
  assert!(
    message.contains("1 KiB"),
    "413 message must name the configured cap, got: {message}"
  );
  assert!(
    message.contains("1024 bytes"),
    "413 message must carry the exact byte count, got: {message}"
  );
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_cap_disables_the_body_check() {
  let registry = SupervisorRegistry::new();
  // A cap of 0 disables the check: a body larger than the default
  // (16 MiB) must be buffered in full, not 413'd at the default and
  // not rejected on the first byte. The model is in the catalog but
  // not running, so routing answers 503 _after_ the full buffer — a
  // 413 anywhere would mean the check is still in force.
  let state = proxy_state_with_cap(
    vec![discovered("/m/qwen3.gguf", Some("qwen3"), "qwen3")],
    registry,
    0,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let pad = "A".repeat(17 * 1024 * 1024 + 64);
  let body = format!(r#"{{"model":"qwen3","pad":"{pad}"}}"#);
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", &body, &[]).await;
  assert_eq!(
    status, 503,
    "a 17 MiB body under a 0 cap must buffer fully and reach routing, got: {status}"
  );
  let v: Value = serde_json::from_slice(&response).expect("json body");
  assert_eq!(v["error"]["type"], "launch_failed");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_between_old_and_new_default_passes_cap() {
  // 3 MiB: the pre-#65 hard cap was 2 MiB (413), the new default is
  // 16 MiB. No model is running, so the routing answer is 503
  // launch_failed — proof the body survived the cap and reached
  // routing, not the 413 path.
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(
    vec![discovered("/m/qwen3.gguf", Some("qwen3"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let pad = "A".repeat(3 * 1024 * 1024);
  let body = format!(r#"{{"model":"qwen3","pad":"{pad}"}}"#);
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", &body, &[]).await;
  assert_ne!(status, 413, "3 MiB body must pass the 16 MiB default cap");
  assert_eq!(status, 503);
  let v: Value = serde_json::from_slice(&response).expect("json body");
  assert_eq!(v["error"]["type"], "launch_failed");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_body_returns_400() {
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(Vec::new(), registry).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, _h, response) = http_post(addr, "/v1/chat/completions", "{not json", &[]).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["type"], "invalid_request");
  shutdown_listener(shutdown, listener_handle).await;
}

// --- hop-by-hop header handling -----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hop_by_hop_headers_dont_break_forwarding() {
  // A client sending `Connection: close` (plus the upstream sending
  // `Connection: close` back) should still produce a clean 200 with
  // the upstream's body intact, no hop-by-hop framing leakage.
  let dir = unique_temp("hop");
  let catalog_path = "/fixture/hop.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Chat).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;

  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("hop"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = r#"{"model":"hop","messages":[],"stream":true}"#;
  // The inbound request already carries `Connection: close` (added
  // by `http_post`). For this test we additionally inject a
  // hop-by-hop `Keep-Alive` so the strip path is exercised end-to-end.
  let (status, headers, response_body) = http_post(
    addr,
    "/v1/chat/completions",
    body,
    &[("Keep-Alive", "timeout=5"), ("TE", "trailers")],
  )
  .await;
  assert_eq!(status, 200);
  // The proxy must strip hop-by-hop headers it sourced from the
  // upstream's response (e.g. `Keep-Alive`, `Transfer-Encoding`)
  // before passing them to the client. We can't assert on
  // `Connection` here: hyper's HTTP/1.1 protocol layer manages that
  // header itself based on the inbound client's framing
  // (`Connection: close` is reflected back), independent of
  // whether the proxy emitted it. Asserting the *upstream-sourced*
  // hop-by-hop headers are absent is enough to lock the contract.
  let lowered: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
  assert!(
    !lowered.contains(&"keep-alive"),
    "hop-by-hop `keep-alive` must be stripped from response; got: {lowered:?}"
  );
  assert!(
    !lowered.contains(&"transfer-encoding"),
    "hop-by-hop `transfer-encoding` must be stripped from response; got: {lowered:?}"
  );
  // Body forwarded correctly (the echo lands here).
  let text = String::from_utf8(response_body).expect("utf8");
  assert!(text.contains("\"model\":\"hop\""), "body forwarded: {text}");

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// --- upstream 500 pass-through ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_400_passes_through_as_400() {
  // The fake fixture's failure-injection knob: query string
  // `?fail=400` OR a magic marker `__TEST_INJECT_FAIL_400__` in the
  // body. We use the body marker — the query-string form would
  // require a fragment in `path`, which the proxy strips via its
  // path_and_query handling but the fake doesn't notice.
  let dir = unique_temp("upstream500");
  let catalog_path = "/fixture/fail.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Chat).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;

  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("fail"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let body = r#"{"model":"fail","messages":[],"__TEST_INJECT_FAIL_400__":true}"#;
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", body, &[]).await;
  assert_eq!(status, 400);
  // Upstream's error body is forwarded byte-for-byte.
  let text = String::from_utf8(response).expect("utf8");
  assert!(text.contains("injected 400"), "upstream body in: {text}");

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_unreachable_after_stop_returns_structured_error() {
  // R-14: lock the error envelope shape when the supervisor we
  // picked dies before forwarding completes. Two valid responses:
  //   - 502 upstream_unreachable (if the proxy still has a stale
  //     Ready snapshot and reqwest fails to connect), OR
  //   - 503 launch_failed (if the supervisor is gone by the time
  //     decide() snapshots and auto-start fails to restart it).
  // Either way: structured OpenAI envelope, no hang, no naked 500.
  let dir = unique_temp("unreachable");
  let catalog_path = "/fixture/unreachable.gguf";
  let registry = SupervisorRegistry::new();
  let (model, _port, _id) = spawn_fake_supervisor(catalog_path, &dir, LaunchMode::Chat).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;
  let state = proxy_state_with(
    vec![discovered(catalog_path, Some("unreachable"), "qwen3")],
    registry,
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  // Stop the supervisor, wait for the state machine to settle.
  let _ = model.stop(Duration::from_secs(3)).await;
  tokio::time::sleep(Duration::from_millis(50)).await;

  let body = r#"{"model":"unreachable","messages":[]}"#;
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", body, &[]).await;
  assert!(
    status == 502 || status == 503,
    "expected 502 upstream_unreachable or 503 launch_failed; got {status}"
  );
  let v: Value = serde_json::from_slice(&response).expect("json body");
  let kind = v["error"]["type"].as_str().unwrap_or("");
  assert!(
    matches!(kind, "upstream_unreachable" | "launch_failed"),
    "expected upstream_unreachable or launch_failed; got {kind:?}"
  );

  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_request_closes_within_header_read_timeout() {
  // R-03 regression test: assert the inbound `header_read_timeout`
  // wired into hyper::server::conn::http1::Builder actually fires.
  // A client that opens a TCP socket, writes a partial request
  // line, then idles must have its connection closed by the proxy
  // within HEADER_READ_TIMEOUT (30s production; 35s budget here so
  // CI noise doesn't flake). If a future tweak drops the timeout
  // from the builder chain, this test hangs past the budget.
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpStream;

  let dir = unique_temp("partial");
  let registry = SupervisorRegistry::new();
  let state = proxy_state_with(Vec::new(), registry).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let mut sock = TcpStream::connect(addr).await.expect("connect");
  // Partial request line, no newline, no Host header. Server waits
  // on the rest of the headers; the timeout decides when to give up.
  sock.write_all(b"GET /he").await.expect("write partial");
  sock.flush().await.ok();

  let mut buf = vec![0u8; 64];
  let read = tokio::time::timeout(Duration::from_secs(35), sock.read(&mut buf)).await;
  let n = read
    .expect("proxy failed to close partial-request connection within HEADER_READ_TIMEOUT budget")
    .expect("read");
  // `read` of 0 = clean EOF (peer closed). Hyper closes the socket
  // once the timeout fires; we don't expect any response bytes.
  assert_eq!(n, 0, "expected EOF on timeout; got {n} bytes: {buf:?}");

  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
