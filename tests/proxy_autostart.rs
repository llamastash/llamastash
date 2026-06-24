//! Integration tests for Unit 4's auto-start path.
//!
//! Spawn the proxy against a `MethodContext` wired with a real
//! `LaunchEnv` (the `fake_llama_server` test binary as the
//! `llama-server` stand-in) and a discovery row pointing at a
//! synthetic GGUF on disk. A request that names the dormant model
//! must drive the launch in-process and forward against the live
//! supervisor once Ready.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 4
//! Test scenarios — Happy path / Slow start).

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use llamastash::config::loader::PortRange;
use llamastash::daemon::context::{LaunchEnv, MethodContext};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pa", label)
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

fn allocate_port_range() -> PortRange {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  PortRange {
    start: port,
    end: port,
  }
}

/// Write a synthetic GGUF with the requested architecture under
/// `dir`, return its canonical absolute path.
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
    quant_label: None,
    native_ctx: Some(8192),
    chat_template: None,
    tokenizer_kind: Some("llama".to_string()),
    reasoning_hint: false,
    mode_hint: ModeHint::Chat,
    weights_bytes: Some(4_000_000_000),
  }
}

fn discovered(path: &Path, display_label: Option<&str>, arch: &str) -> DiscoveredModel {
  let parent = path.parent().expect("parent").to_path_buf();
  DiscoveredModel {
    path: path.to_path_buf(),
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata(arch)),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: display_label.map(str::to_string),
    multimodal: None,
    supported_backends: Vec::new(),
  }
}

async fn build_state(
  models: Vec<DiscoveredModel>,
  log_dir: &Path,
  port_range: PortRange,
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
    .with_supervisors(SupervisorRegistry::new())
    .with_launch_env(env);
  let state = ProxyState::from_context(&ctx, false, true);
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

/// Trigger shutdown and join the serve task with a generous budget.
/// Catches a hung serve loop instead of leaving a detached task that
/// would otherwise be silently torn down at runtime exit.
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

async fn stop_all(ctx: &MethodContext) {
  let snap = ctx.supervisors.snapshot().await;
  for (_, m) in snap {
    let _ = m.stop(Duration::from_secs(3)).await;
  }
}

// ---- Scenario 1: happy path — dormant model auto-starts ---------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dormant_model_auto_starts_and_forwards_without_fallback_headers() {
  let dir = unique_temp("happy");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  let (state, ctx) = build_state(
    vec![discovered(&model_path, Some("qwen3"), "qwen3")],
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = serde_json::json!({
    "model": "qwen3",
    "messages": [{"role":"user","content":"hi"}],
    "stream": true,
  })
  .to_string();
  let (status, headers, response) = http_post(addr, "/v1/chat/completions", &body).await;
  assert_eq!(status, 200, "happy path returns 200");
  for (k, _) in &headers {
    assert!(
      !k.starts_with("x-llamastash-"),
      "no fallback headers on happy path; got {k}"
    );
  }
  let text = String::from_utf8(response).expect("utf8");
  assert!(text.contains("\"model\":\"qwen3\""), "echo present: {text}");

  // After Ready, supervisor must be registered.
  let snap = ctx.supervisors.snapshot().await;
  assert_eq!(snap.len(), 1, "exactly one supervisor was launched");

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 2: slow start — request blocks for window then succeeds -

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_start_blocks_then_succeeds() {
  // The `--health-delay-ms` fixture knob makes /health return 503
  // until N ms after process start. We can't pass extra argv from
  // `compose_and_spawn` (it composes its own argv), so this test
  // instead validates that a typical autostart's wall-clock is
  // bounded — the supervisor probe interval is 30 ms in this test
  // setup, so an unloaded process clears probe in < 200 ms. We
  // verify the request blocks for *at least* the probe interval
  // (so we're observing the loading window) and then succeeds.
  let dir = unique_temp("slow");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  let (state, ctx) = build_state(
    vec![discovered(&model_path, Some("qwen3"), "qwen3")],
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let start = std::time::Instant::now();
  let body = r#"{"model":"qwen3","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
  let (status, _h, response) = http_post(addr, "/v1/chat/completions", body).await;
  let elapsed = start.elapsed();
  assert_eq!(status, 200);
  // The supervisor took at least one probe tick to mark Ready —
  // we don't claim a tight upper bound (CI flakes), just that
  // *some* loading window elapsed.
  assert!(
    elapsed >= Duration::from_millis(20),
    "auto-start should observe a loading window; elapsed = {elapsed:?}"
  );
  let text = String::from_utf8(response).expect("utf8");
  assert!(text.contains("\"model\":\"qwen3\""));

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 11: launch + Ready transition observed simultaneously ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_state_visible_to_registry_after_auto_start() {
  // The proxy's auto-start uses the same code path as IPC's
  // `start_model`. After Ready, the supervisor registry must
  // contain exactly one entry that the IPC `status` (if it ran)
  // would see — no shared-state desync.
  let dir = unique_temp("desync");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  let (state, ctx) = build_state(
    vec![discovered(&model_path, Some("qwen3"), "qwen3")],
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"qwen3","messages":[]}"#;
  let (status, _h, _b) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(status, 200);

  let snap = ctx.supervisors.snapshot().await;
  assert_eq!(snap.len(), 1);
  let (_lid, model) = &snap[0];
  use llamastash::daemon::supervisor::ManagedState;
  assert!(
    matches!(model.state().await, ManagedState::Ready),
    "supervisor must be Ready after auto-start"
  );

  // state.running snapshot should also reflect the launch
  // (`compose_and_spawn` writes it). The proxy state shares the
  // same `PersistedState` handle through the cloned MethodContext.
  let persisted = ctx.state.snapshot().await;
  assert_eq!(persisted.running.len(), 1);

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 12: in-flight fallback while original becomes Ready ------
//
// Not directly testable without async injection — the proxy reads
// the supervisor snapshot once at request time and commits to its
// target for the lifetime of the stream. We document the contract
// via the smaller "snapshot is taken at decision time" test below.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_request_for_same_model_skips_relaunch() {
  // After the first request succeeds, a second request for the
  // same model must observe the now-Ready supervisor on the hot
  // path and forward without triggering another launch.
  let dir = unique_temp("relaunch");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  let (state, ctx) = build_state(
    vec![discovered(&model_path, Some("qwen3"), "qwen3")],
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"qwen3","messages":[]}"#;
  let (s1, _h1, _b1) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(s1, 200);
  let after_first = ctx.supervisors.snapshot().await.len();
  let (s2, _h2, _b2) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(s2, 200);
  let after_second = ctx.supervisors.snapshot().await.len();
  assert_eq!(
    after_first, after_second,
    "second request must reuse the running supervisor; got {after_first} -> {after_second}"
  );

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 9 (partial): launch failure surfaces as 503 launch_failed -
//
// We can't easily inject a `Error{cause:"probe timeout"}` without
// stalling for the full probe deadline. The simpler form below
// drives the path by handing the auto-start a synthetic-GGUF row
// whose disk file *does* exist but whose canonical path won't
// resolve to a launchable binary signal — see
// `tests/proxy_fallback.rs` for the richer failure path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_start_failure_with_no_ready_models_returns_launch_failed() {
  let dir = unique_temp("fail");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  // Point the catalog at a path that doesn't exist on disk — the
  // canonical-id read inside `auto_start` fails and the helper
  // returns LaunchOutcome::Failed.
  let nonexistent = dir.join("never.gguf");
  let (state, ctx) = build_state(
    vec![discovered(&nonexistent, Some("never"), "qwen3")],
    &log_dir,
    allocate_port_range(),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"never","messages":[]}"#;
  let (status, _h, response) = http_post(addr, "/v1/chat/completions", body).await;
  assert_eq!(status, 503);
  let v: Value = serde_json::from_slice(&response).expect("json");
  assert_eq!(v["error"]["type"], "launch_failed");
  let running = v["error"]["running"].as_array().expect("running");
  assert!(running.is_empty(), "no Ready models to fall back to");

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
