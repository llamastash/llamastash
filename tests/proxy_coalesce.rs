//! Single-flight coalescing for Unit 4's auto-start.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 4
//! Test scenarios — "two concurrent requests for the same dormant
//! model" and "three concurrent requests for three different
//! dormant models").
//!
//! Instrumentation: we observe single-flight by counting supervisor
//! registry entries after both requests complete. If coalescing
//! works, exactly one `compose_and_spawn` call ran — there is
//! exactly one supervisor for the path. Without coalescing, two
//! `compose_and_spawn` calls race on `reserve_port`; depending on
//! the port range, either one would error out (and the test fails
//! the 200 assertion) or both would succeed and the registry would
//! contain two entries on the same path. Either failure mode is
//! detected here.

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pc", label)
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

/// A range big enough to host two simultaneous launches. Picks two
/// distinct ephemeral ports and uses their min..=max as the range.
fn allocate_port_range(slots: usize) -> PortRange {
  let mut ports = Vec::with_capacity(slots);
  let mut listeners = Vec::with_capacity(slots);
  for _ in 0..slots {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    ports.push(l.local_addr().unwrap().port());
    listeners.push(l);
  }
  // Drop the listeners so the ports become available, then expand
  // the range to cover them — also fills any gap between the
  // chosen ports so the allocator can pick freely. Adding a small
  // headroom band ahead of `lo` covers the rare case where the
  // kernel hands a contiguous block.
  drop(listeners);
  let lo = *ports.iter().min().unwrap();
  let hi = *ports.iter().max().unwrap();
  PortRange { start: lo, end: hi }
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
    mtp_head: None,
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

async fn http_post(addr: SocketAddr, path: &str, body: &str) -> u16 {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!(
    "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  parse_status(&buf)
}

fn parse_status(buf: &[u8]) -> u16 {
  let needle = b"\r\n";
  let end = buf
    .windows(needle.len())
    .position(|w| w == needle)
    .expect("status line");
  let line = std::str::from_utf8(&buf[..end]).expect("utf8");
  line
    .split_whitespace()
    .nth(1)
    .expect("code")
    .parse()
    .expect("u16")
}

async fn stop_all(ctx: &MethodContext) {
  let snap = ctx.supervisors.snapshot().await;
  for (_, m) in snap {
    let _ = m.stop(Duration::from_secs(3)).await;
  }
}

// ---- Scenario 3: two concurrent requests for the same dormant model ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_requests_for_same_model_share_one_launch() {
  let dir = unique_temp("same");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  // Use a single-port range — if coalescing fails, the second
  // launch will error on port allocation and the test catches
  // either the failed 200 or the duplicate supervisor.
  let (state, ctx) = build_state(
    vec![discovered(&model_path, Some("qwen3"), "qwen3")],
    &log_dir,
    allocate_port_range(1),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"qwen3","messages":[]}"#.to_string();
  let body_a = body.clone();
  let body_b = body.clone();
  let (a, b) = tokio::join!(
    tokio::spawn(async move { http_post(addr, "/v1/chat/completions", &body_a).await }),
    tokio::spawn(async move { http_post(addr, "/v1/chat/completions", &body_b).await }),
  );
  let a = a.expect("join a");
  let b = b.expect("join b");
  assert_eq!(a, 200, "first request succeeds");
  assert_eq!(b, 200, "second request succeeds");

  let snap = ctx.supervisors.snapshot().await;
  assert_eq!(
    snap.len(),
    1,
    "single-flight: exactly one supervisor for the shared model id; got {}",
    snap.len()
  );

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 4: three concurrent requests for three different models -

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_concurrent_requests_for_distinct_models_each_launch() {
  let dir = unique_temp("distinct");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let a = write_gguf(&dir, "a.gguf", "llama");
  let b = write_gguf(&dir, "b.gguf", "qwen3");
  let c = write_gguf(&dir, "c.gguf", "bert");
  let (state, ctx) = build_state(
    vec![
      discovered(&a, Some("a"), "llama"),
      discovered(&b, Some("b"), "qwen3"),
      discovered(&c, Some("c"), "bert"),
    ],
    &log_dir,
    allocate_port_range(3),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let r_a = tokio::spawn(async move {
    http_post(
      addr,
      "/v1/chat/completions",
      r#"{"model":"a","messages":[]}"#,
    )
    .await
  });
  let r_b = tokio::spawn(async move {
    http_post(
      addr,
      "/v1/chat/completions",
      r#"{"model":"b","messages":[]}"#,
    )
    .await
  });
  let r_c = tokio::spawn(async move {
    http_post(
      addr,
      "/v1/chat/completions",
      r#"{"model":"c","messages":[]}"#,
    )
    .await
  });
  let (sa, sb, sc) = tokio::try_join!(r_a, r_b, r_c).expect("all complete");
  assert_eq!(sa, 200);
  assert_eq!(sb, 200);
  assert_eq!(sc, 200);

  let snap = ctx.supervisors.snapshot().await;
  assert_eq!(snap.len(), 3, "three distinct supervisors launched");

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
