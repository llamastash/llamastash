//! Host / Origin refusals on the proxy.
//!
//! A web page in the user's browser can reach the loopback proxy — via a
//! DNS-rebound name, or a cross-site form POST that carries the browser's
//! cached `/ui` Basic credentials. Neither may drive or read the engine.
//! The router refuses a foreign `Host` on a keyless loopback bind and an
//! off-origin `Origin` in every mode, before routing and before the request
//! log starts a row.
//!
//! Requests are written straight onto a raw socket: `test_support::http_post`
//! always sends the proxy's own `Host` and JSON content type, and these
//! tests need the values a hostile page would send.

#![cfg(feature = "test-fixtures")]

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::daemon::context::MethodContext;
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{spawn as supervisor_spawn, ManagedSpawn, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::identity::ModelId;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use llamastash::test_support::{fake_metadata, shutdown_listener, spawn_listener, write_gguf};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-prc", label)
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

/// A running chat supervisor on a real synthetic GGUF plus a catalog row
/// for it, so a *permitted* request really serves — the refusals must not
/// grow from some unrelated 4xx. Returns the context so a test can check
/// the request log and the running list stayed empty.
async fn setup() -> (
  SocketAddr,
  MethodContext,
  ShutdownToken,
  tokio::task::JoinHandle<()>,
) {
  let dir = unique_temp("checks");
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  let registry = SupervisorRegistry::new();
  let port = llamastash::test_support::allocate_port_range(1).start;
  let id = ModelId {
    path: model_path.clone(),
    header_blake3: [0u8; 32],
  };
  let params = LaunchParams::new(model_path.clone(), LaunchMode::Chat);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = supervisor_spawn(ManagedSpawn {
    id,
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: dir.join("fake.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn fake_llama_server");
  for _ in 0..250 {
    if matches!(model.state().await, ManagedState::Ready) {
      break;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  registry.insert(registry.next_id(), model.clone()).await;

  let catalog = ModelCatalog::new();
  catalog
    .upsert(DiscoveredModel {
      path: model_path.clone(),
      parent: model_path.parent().unwrap().to_path_buf(),
      source: ModelSource::UserPath,
      metadata: Some(fake_metadata("qwen3")),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: Some("qwen3".to_string()),
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    })
    .await;
  let ctx = MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(registry);
  let state =
    ProxyState::from_context_with_auth(&ctx, false, true, None, DEFAULT_BODY_LIMIT_BYTES, true);
  let (addr, shutdown, handle) = spawn_listener(state).await;
  (addr, ctx, shutdown, handle)
}

/// One raw HTTP/1.1 request with a caller-chosen `Host` and extra headers:
/// `(status, body)`. `host` replaces the normal loopback Host line.
async fn raw_request(
  addr: SocketAddr,
  method: &str,
  path: &str,
  host: &str,
  extra: &[(&str, &str)],
  body: Option<(&str, &str)>,
) -> (u16, Vec<u8>) {
  let mut stream = TcpStream::connect(addr).await.expect("connect");
  let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
  if let Some((content_type, _)) = body {
    request.push_str(&format!("Content-Type: {content_type}\r\n"));
  }
  for (k, v) in extra {
    request.push_str(&format!("{k}: {v}\r\n"));
  }
  if let Some((_, body)) = body {
    request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    stream.write_all(body.as_bytes()).await.expect("write body");
  } else {
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
  }
  let mut buf = Vec::new();
  tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut buf))
    .await
    .expect("read")
    .expect("body");
  let head = buf
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .unwrap_or(buf.len());
  let status: u16 = String::from_utf8_lossy(&buf[..head])
    .split_whitespace()
    .nth(1)
    .and_then(|code| code.parse().ok())
    .unwrap_or_else(|| {
      panic!(
        "no status line in {:?}",
        String::from_utf8_lossy(&buf[..head])
      )
    });
  (status, buf[head + 4..].to_vec())
}

/// The `http://<proxy>` origin of the running listener.
fn own_origin(addr: SocketAddr) -> String {
  format!("http://{addr}")
}

/// Assert a refused response: `403` with the named error `type`.
fn assert_forbidden(status: u16, body: &[u8], code: &str) {
  assert_eq!(
    status,
    403,
    "expected 403, body: {}",
    String::from_utf8_lossy(body)
  );
  let v: Value = serde_json::from_slice(body).expect("json error envelope");
  assert_eq!(v["error"]["type"], code, "{v}");
}

/// Nothing was routed: the proxy logged no request row and started no launch.
async fn assert_nothing_started(ctx: &MethodContext) {
  let rows = ctx.requests.snapshot().rows;
  assert!(
    rows.is_empty(),
    "a refused request must not log a row: {rows:?}"
  );
  let running = ctx.state.snapshot().await.running;
  assert!(
    running.is_empty(),
    "a refused request must start no launch: {running:?}"
  );
}

// ---- foreign Host ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dns_rebound_host_is_refused() {
  let (addr, ctx, shutdown, handle) = setup().await;
  let host = "rebound.example";
  let (status, body) = raw_request(addr, "GET", "/health", host, &[], None).await;
  assert_forbidden(status, &body, "forbidden_host");
  let text = String::from_utf8_lossy(&body);
  assert!(text.contains(host), "the message names the host: {text}");
  let (status, body) = raw_request(
    addr,
    "POST",
    "/v1/chat/completions",
    host,
    &[],
    Some(("application/json", r#"{"model":"qwen3","messages":[]}"#)),
  )
  .await;
  assert_forbidden(status, &body, "forbidden_host");
  assert_nothing_started(&ctx).await;
  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_host_spellings_pass() {
  let (addr, _ctx, shutdown, handle) = setup().await;
  let own = addr.to_string();
  for host in ["localhost", "LOCALHOST", "127.0.0.1"] {
    let (status, body) = raw_request(addr, "GET", "/health", host, &[], None).await;
    assert_eq!(
      status, 200,
      "Host: {host} must pass, got: {status} {body:?}"
    );
  }
  // The literal with the real port passes too.
  let (status, _) = raw_request(addr, "GET", "/health", &own, &[], None).await;
  assert_eq!(status, 200);
  shutdown_listener(shutdown, handle).await;
}

// ---- hostile Origin ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_site_form_post_is_refused_and_starts_nothing() {
  // The classic CSRF shape: a `text/plain` form POST, which a browser sends
  // cross-site without a preflight. It must never reach the engine.
  let (addr, ctx, shutdown, handle) = setup().await;
  let own = addr.to_string();
  let (status, body) = raw_request(
    addr,
    "POST",
    "/v1/chat/completions",
    &own,
    &[("Origin", "http://evil.example")],
    Some(("text/plain", "evil=1")),
  )
  .await;
  assert_forbidden(status, &body, "forbidden_origin");
  assert_nothing_started(&ctx).await;
  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_read_and_null_origin_are_refused() {
  let (addr, ctx, shutdown, handle) = setup().await;
  let own = addr.to_string();
  // A well-formed chat request from a hostile page: even the *read* is
  // refused, so no engine CORS header can leak a response through the proxy.
  let (status, body) = raw_request(
    addr,
    "POST",
    "/v1/chat/completions",
    &own,
    &[("Origin", "http://evil.example")],
    Some(("application/json", r#"{"model":"qwen3","messages":[]}"#)),
  )
  .await;
  assert_forbidden(status, &body, "forbidden_origin");
  // A sandboxed iframe sends `null`; a page can forge it, so it proves nothing.
  let (status, body) = raw_request(addr, "GET", "/", &own, &[("Origin", "null")], None).await;
  assert_forbidden(status, &body, "forbidden_origin");
  // The refusal covers the proxy's own surfaces too, not just `/v1/*`.
  let (status, body) = raw_request(
    addr,
    "GET",
    "/ui",
    &own,
    &[("Origin", "http://evil.example")],
    None,
  )
  .await;
  assert_forbidden(status, &body, "forbidden_origin");
  let (status, body) = raw_request(
    addr,
    "GET",
    "/health",
    &own,
    &[("Origin", "http://evil.example")],
    None,
  )
  .await;
  assert_forbidden(status, &body, "forbidden_origin");
  assert_nothing_started(&ctx).await;
  shutdown_listener(shutdown, handle).await;
}

// ---- what passes ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_origin_and_extension_origins_pass() {
  let (addr, ctx, shutdown, handle) = setup().await;
  let own = addr.to_string();
  let origin = own_origin(addr);
  let (status, body) = raw_request(
    addr,
    "POST",
    "/v1/chat/completions",
    &own,
    &[("Origin", &origin)],
    Some(("application/json", r#"{"model":"qwen3","messages":[]}"#)),
  )
  .await;
  assert_eq!(status, 200, "same-origin chat must serve: {body:?}");
  // A browser extension origin is an installed app, not a web page.
  let (status, body) = raw_request(
    addr,
    "POST",
    "/v1/chat/completions",
    &own,
    &[("Origin", "chrome-extension://abc")],
    Some(("application/json", r#"{"model":"qwen3","messages":[]}"#)),
  )
  .await;
  assert_eq!(status, 200, "extension origin must serve: {body:?}");
  // No Origin at all (curl, SDKs, health checks) is unaffected, and the
  // same-origin `/ui` redirect still works.
  let (status, _) = raw_request(addr, "GET", "/", &own, &[], None).await;
  assert_eq!(status, 200);
  let (status, _) = raw_request(addr, "GET", "/ui", &own, &[("Origin", &origin)], None).await;
  assert_eq!(status, 302, "same-origin /ui keeps its redirect");
  let _ = ctx;
  shutdown_listener(shutdown, handle).await;
}
