//! Integration tests for the `/ui` browser surface.
//!
//! Mirrors `tests/proxy_routing.rs`'s fixture pattern: spawn one or
//! more `fake_llama_server` children via the supervisor, register them
//! in a `SupervisorRegistry`, stand up the proxy listener, and drive it
//! with a hand-rolled HTTP client so we can assert status lines,
//! `Set-Cookie` / `WWW-Authenticate` headers, and forwarded bodies.
//!
//! Plan: docs/plans/2026-06-15-001-feat-proxy-ui-surface-plan.md.

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
use llamastash::daemon::registry::{LaunchId, SupervisorRegistry};
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{spawn as supervisor_spawn, ManagedSpawn, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::identity::ModelId;
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

// --- helpers -------------------------------------------------------------

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-ui", label)
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

fn fake_metadata() -> ModelMetadata {
  ModelMetadata {
    arch: Some("llama".to_string()),
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

fn discovered(path: &str, display_label: Option<&str>) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p.parent().unwrap().to_path_buf();
  DiscoveredModel {
    path: p,
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata()),
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

/// Spawn a fake_llama_server via the supervisor and return the Ready
/// `ManagedModel`. The `ModelId.path` matches `catalog_path` so the
/// proxy's catalog→supervisor join lines up.
async fn spawn_fake(
  catalog_path: &str,
  log_dir: &Path,
) -> llamastash::daemon::supervisor::ManagedModel {
  let port = allocate_port();
  let id = ModelId {
    path: PathBuf::from(catalog_path),
    header_blake3: [0u8; 32],
  };
  let params = LaunchParams::new(PathBuf::from(catalog_path), LaunchMode::Chat);
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = supervisor_spawn(ManagedSpawn {
    id,
    params,
    port,
    mode: LaunchMode::Chat,
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
  model
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

async fn build_state(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
  api_key: Option<&str>,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(supervisors);
  ProxyState::from_context_with_auth(&ctx, false, true, api_key.map(str::to_string))
}

/// Send a raw HTTP/1.1 GET and return `(status, headers, body)`. Does
/// not follow redirects, so 3xx + `Location` are observable.
async fn http_get(
  addr: SocketAddr,
  path: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
  for (k, v) in extra_headers {
    req.push_str(&format!("{k}: {v}\r\n"));
  }
  req.push_str("\r\n");
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
  headers
    .iter()
    .find(|(k, _)| k == name)
    .map(|(_, v)| v.as_str())
}

/// Standard base64 (padded) — the test crate doesn't pull the `base64`
/// dev-dependency, and browsers encode Basic credentials this way.
fn base64_encode(input: &[u8]) -> String {
  const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = String::new();
  for chunk in input.chunks(3) {
    let b0 = chunk[0] as u32;
    let b1 = *chunk.get(1).unwrap_or(&0) as u32;
    let b2 = *chunk.get(2).unwrap_or(&0) as u32;
    let n = (b0 << 16) | (b1 << 8) | b2;
    out.push(TBL[((n >> 18) & 63) as usize] as char);
    out.push(TBL[((n >> 12) & 63) as usize] as char);
    out.push(if chunk.len() > 1 {
      TBL[((n >> 6) & 63) as usize] as char
    } else {
      '='
    });
    out.push(if chunk.len() > 2 {
      TBL[(n & 63) as usize] as char
    } else {
      '='
    });
  }
  out
}

// --- tests ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_ui_redirects_to_slash() {
  let dir = unique_temp("redirect");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
    None,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, headers, _body) = http_get(addr, "/ui", &[]).await;
  assert_eq!(status, 302, "GET /ui must 302 to the trailing-slash form");
  assert_eq!(header(&headers, "location"), Some("/ui/"));

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_running_serves_ui_and_forwards_paths() {
  let dir = unique_temp("single");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
    None,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  // `/ui/` strips to `/` → the backend's web-UI index.
  let (status, _h, body) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 200);
  let text = String::from_utf8(body).expect("utf8 body");
  assert!(
    text.contains("fake-llama-webui"),
    "expected the backend web-UI body, got: {text}"
  );

  // A base-relative path strips to the backend root too: `/ui/v1/models`
  // → `/v1/models`, which the fake server answers with its model id.
  let (status, _h, body) = http_get(addr, "/ui/v1/models", &[]).await;
  assert_eq!(status, 200);
  let text = String::from_utf8(body).expect("utf8 body");
  assert!(
    text.contains("/fixture/solo.gguf"),
    "expected the forwarded /v1/models payload, got: {text}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_running_shows_chooser_then_cookie_pins() {
  let dir = unique_temp("chooser");
  let registry = SupervisorRegistry::new();
  let alpha = spawn_fake("/fixture/alpha.gguf", &dir).await;
  let beta = spawn_fake("/fixture/beta.gguf", &dir).await;
  // Insert with known launch ids: alpha=L1, beta=L2.
  registry.insert(registry.next_id(), alpha.clone()).await;
  let beta_id = registry.next_id();
  registry.insert(beta_id.clone(), beta.clone()).await;
  assert_eq!(beta_id, LaunchId::from_counter(2));
  let state = build_state(
    vec![
      discovered("/fixture/alpha.gguf", Some("alpha")),
      discovered("/fixture/beta.gguf", Some("beta")),
    ],
    registry,
    None,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  // Two running, no cookie → the chooser, linking each backend.
  let (status, _h, body) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 200);
  let text = String::from_utf8(body).expect("utf8 body");
  assert!(text.contains("Choose a model"), "expected chooser: {text}");
  assert!(text.contains("/ui/?target=L1") && text.contains("/ui/?target=L2"));
  assert!(text.contains("alpha") && text.contains("beta"));

  // Clicking a chooser link sets the pin cookie and 302s back to `/ui/`.
  let (status, headers, _b) = http_get(addr, "/ui/?target=L2", &[]).await;
  assert_eq!(status, 302);
  assert_eq!(header(&headers, "location"), Some("/ui/"));
  let set_cookie = header(&headers, "set-cookie").expect("set-cookie present");
  assert!(
    set_cookie.contains("ls_ui_target=L2") && set_cookie.contains("Path=/ui"),
    "unexpected Set-Cookie: {set_cookie}"
  );

  // The cookie pins asset/API requests to the chosen backend (beta).
  let (status, _h, body) = http_get(addr, "/ui/v1/models", &[("Cookie", "ls_ui_target=L2")]).await;
  assert_eq!(status, 200);
  let text = String::from_utf8(body).expect("utf8 body");
  assert!(
    text.contains("/fixture/beta.gguf") && !text.contains("/fixture/alpha.gguf"),
    "cookie must pin to beta, got: {text}"
  );

  // `/ui/switch` ignores the pin and re-shows the chooser so the user can
  // pick another model; the pinned one is marked current.
  let (status, _h, body) = http_get(addr, "/ui/switch", &[("Cookie", "ls_ui_target=L2")]).await;
  assert_eq!(
    status, 200,
    "/ui/switch must serve the chooser, not forward"
  );
  let text = String::from_utf8(body).expect("utf8 body");
  assert!(
    text.contains("Choose a model"),
    "expected switcher chooser: {text}"
  );
  assert!(text.contains("/ui/?target=L1") && text.contains("/ui/?target=L2"));
  assert!(
    text.matches(">current<").count() == 1,
    "exactly the pinned model is marked current: {text}"
  );

  let _ = alpha.stop(Duration::from_secs(3)).await;
  let _ = beta.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_running_serves_no_model_page() {
  let dir = unique_temp("empty");
  let registry = SupervisorRegistry::new();
  let state = build_state(Vec::new(), registry, None).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, headers, body) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 200, "zero running must be a page, not a 500");
  assert!(header(&headers, "content-type")
    .unwrap_or("")
    .contains("text/html"));
  let text = String::from_utf8(body).expect("utf8 body");
  assert!(text.contains("No model running"), "got: {text}");

  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_enforced_ui_challenges_basic_and_accepts_credentials() {
  let dir = unique_temp("auth");
  let key = "sk-llamastash-uitestkey";
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
    Some(key),
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  // No credential → 401 carrying a *Basic* challenge so a browser prompts.
  let (status, headers, _b) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 401);
  let challenge = header(&headers, "www-authenticate").unwrap_or("");
  assert!(
    challenge.starts_with("Basic"),
    "expected a Basic challenge, got: {challenge:?}"
  );

  // Exempt path stays open even with auth enforced.
  let (status, _h, _b) = http_get(addr, "/health", &[]).await;
  assert_eq!(status, 200, "/health must stay open under auth");

  // Basic base64(user:<key>) with the key as the password passes.
  let basic = format!("Basic {}", base64_encode(format!("x:{key}").as_bytes()));
  let (status, _h, body) = http_get(addr, "/ui/", &[("Authorization", &basic)]).await;
  assert_eq!(status, 200, "valid Basic credential must serve the UI");
  assert!(String::from_utf8(body)
    .unwrap()
    .contains("fake-llama-webui"));

  // Wrong password → 401.
  let bad = format!("Basic {}", base64_encode(b"x:wrong"));
  let (status, _h, _b) = http_get(addr, "/ui/", &[("Authorization", &bad)]).await;
  assert_eq!(status, 401, "wrong Basic password must be rejected");

  // Bearer still works on the UI surface (API path unchanged).
  let bearer = format!("Bearer {key}");
  let (status, _h, _b) = http_get(addr, "/ui/", &[("Authorization", &bearer)]).await;
  assert_eq!(status, 200, "Bearer with the key must still pass");

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
