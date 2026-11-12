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
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::request_log::LogScope;
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use llamastash::test_support::{
  fake_metadata, header_value, http_get, http_post, shutdown_listener, spawn_listener,
};
use serde_json::Value;
use tokio::time::sleep;

// --- helpers -------------------------------------------------------------

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-ui", label)
}

fn allocate_port() -> u16 {
  // Batch-probe rather than binding one ephemeral and dropping it: the kernel
  // will not hand the same port to eight concurrent binds, so parallel test
  // binaries land on disjoint sets instead of racing for one number.
  llamastash::test_support::allocate_port_range(8).start
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

fn discovered(path: &str, display_label: Option<&str>) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p.parent().unwrap().to_path_buf();
  DiscoveredModel {
    path: p,
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata("llama")),
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
  spawn_fake_tagged(catalog_path, log_dir, "llamacpp", None).await
}

/// Like [`spawn_fake`] but tagged as another backend's launch, with an
/// explicit requested ctx. A backend that serves no web UI of its own is what
/// puts the proxy's `/ui` on the embedded-bundle path.
async fn spawn_fake_tagged(
  catalog_path: &str,
  log_dir: &Path,
  resolved_backend: &str,
  ctx: Option<u32>,
) -> llamastash::daemon::supervisor::ManagedModel {
  let port = allocate_port();
  let id = ModelId {
    path: PathBuf::from(catalog_path),
    header_blake3: [0u8; 32],
  };
  let mut params = LaunchParams::new(PathBuf::from(catalog_path), LaunchMode::Chat);
  params.ctx = ctx;
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
    resolved_backend: resolved_backend.to_string(),
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

async fn build_state(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
  api_key: Option<&str>,
) -> Arc<ProxyState> {
  build_state_with_cap(models, supervisors, api_key, DEFAULT_BODY_LIMIT_BYTES).await
}

/// Like [`build_state`] but with an explicit body cap — the
/// `proxy.max_body_size` seam the daemon passes through
/// `from_context_with_auth` in production.
async fn build_state_with_cap(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
  api_key: Option<&str>,
  max_body_size: usize,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(supervisors);
  ProxyState::from_context_with_auth(
    &ctx,
    false,
    true,
    api_key.map(str::to_string),
    max_body_size,
    true,
  )
}

/// Like [`build_state`] but also returns the [`MethodContext`] the state reads,
/// so a test can read the request log the proxy wrote into.
async fn build_state_and_ctx(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
) -> (Arc<ProxyState>, MethodContext) {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(supervisors);
  let state =
    ProxyState::from_context_with_auth(&ctx, false, true, None, DEFAULT_BODY_LIMIT_BYTES, true);
  (state, ctx)
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
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let (status, headers, _body) = http_get(addr, "/ui", &[]).await;
  assert_eq!(status, 302, "GET /ui must 302 to the trailing-slash form");
  assert_eq!(header_value(&headers, "location"), Some("/ui/"));

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
  let (addr, shutdown, handle) = spawn_listener(state).await;

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

  // The engine's own page is forwarded as the client sent it: no model name is
  // added here, so the fake answers with the `-m` path it fell back to, not
  // the launch's served name.
  let (_status, _h, body) = http_post(
    addr,
    "/ui/v1/chat/completions",
    r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    &[],
  )
  .await;
  let frame = String::from_utf8_lossy(&body);
  assert!(
    frame.contains(r#""model":"/fixture/solo.gguf""#),
    "the engine path forwards the body untouched: {frame}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_post_body_over_cap_returns_413() {
  let dir = unique_temp("uicap");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir).await;
  registry.insert(registry.next_id(), model.clone()).await;
  // A 1 KiB cap: the default (16 MiB) would let this body through, so
  // the refusal proves the configured cap is enforced on the `/ui`
  // forward path — the only `buffer_body` caller without a data-plane
  // twin, so it needs its own cap case.
  let state = build_state_with_cap(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
    None,
    1024,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  // A body just over the cap: `forward_ui` buffers it under the cap
  // before forwarding, so the 413 fires here, not upstream.
  let pad = "A".repeat(1024 + 16);
  let body = format!(r#"{{"model":"solo","pad":"{pad}"}}"#);
  let (status, _headers, response) = http_post(addr, "/ui/v1/chat/completions", &body, &[]).await;
  assert_eq!(status, 413);
  let v: Value = serde_json::from_slice(&response).expect("json body");
  assert_eq!(v["error"]["type"], "payload_too_large");
  let message = v["error"]["message"].as_str().expect("message");
  assert!(
    message.contains("1 KiB"),
    "413 message must name the configured cap, got: {message}"
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
  let (addr, shutdown, handle) = spawn_listener(state).await;

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
  assert_eq!(header_value(&headers, "location"), Some("/ui/"));
  let set_cookie = header_value(&headers, "set-cookie").expect("set-cookie present");
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
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let (status, headers, body) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 200, "zero running must be a page, not a 500");
  assert!(header_value(&headers, "content-type")
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
  let (addr, shutdown, handle) = spawn_listener(state).await;

  // No credential → 401 carrying a *Basic* challenge so a browser prompts.
  let (status, headers, _b) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 401);
  let challenge = header_value(&headers, "www-authenticate").unwrap_or("");
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

/// A chat sent from a web page is logged the way the same call to
/// `/v1/chat/completions` is, marked `ui` and filed under the canonical route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_ui_chat_is_logged_under_the_canonical_route() {
  let dir = unique_temp("log-engine");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let (state, ctx) = build_state_and_ctx(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let (status, _h, body) = http_post(
    addr,
    "/ui/v1/chat/completions",
    r#"{"model":"solo","messages":[{"role":"user","content":"hi"}]}"#,
    &[],
  )
  .await;
  assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

  let rows = ctx.requests.tail(LogScope::All, 10).rows;
  assert_eq!(rows.len(), 1, "one chat, one row: {rows:?}");
  let row = &rows[0];
  assert_eq!(
    row.route, "/v1/chat/completions",
    "the row carries the canonical route, not the `/ui` path"
  );
  assert!(row.ui, "the row is marked as UI traffic");
  assert_eq!(row.requested_model.as_deref(), Some("solo"));
  assert_eq!(row.model.as_deref(), Some("solo"));
  assert_eq!(row.model_path.as_deref(), Some("/fixture/solo.gguf"));
  assert_eq!(row.status, Some(200));
  assert!(row.launch_id.is_some(), "the row names the launch");
  assert!(row.note().contains("ui"), "note: {:?}", row.note());

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// Everything else the page asks for stays out of the log: its index and
/// assets, the engine's model list, `/props`, a stream lookup, and a refused
/// route. Both pages (the engine's own and the embedded one) are covered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_page_traffic_stays_out_of_the_log() {
  let dir = unique_temp("log-assets");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let (state, ctx) = build_state_and_ctx(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  for path in [
    "/ui/",
    "/ui/v1/models",
    "/ui/props",
    "/ui/bundle.js",
    "/ui/nope",
  ] {
    http_get(addr, path, &[]).await;
  }
  http_post(
    addr,
    "/ui/v1/streams/lookup",
    r#"{"conversation_ids":["x"]}"#,
    &[],
  )
  .await;

  let rows = ctx.requests.tail(LogScope::All, 10).rows;
  assert!(
    rows.is_empty(),
    "page traffic must log nothing: {:?}",
    rows.iter().map(|r| &r.route).collect::<Vec<_>>()
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// The embedded page sends no `model` in single-model mode. The row still names
/// the model that answered, and `requested_model` stays what the browser sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundled_ui_chat_is_logged_without_a_requested_model() {
  let dir = unique_temp("log-bundled");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake_tagged("/fixture/solo.gguf", &dir, "generic", Some(2048)).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let (state, ctx) = build_state_and_ctx(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let (status, _h, _body) = http_post(
    addr,
    "/ui/v1/chat/completions",
    r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    &[],
  )
  .await;
  assert_eq!(status, 200);
  // The embedded page's own surface, answered by the proxy.
  for path in ["/ui/", "/ui/props"] {
    http_get(addr, path, &[]).await;
  }
  http_post(
    addr,
    "/ui/v1/streams/lookup",
    r#"{"conversation_ids":["x"]}"#,
    &[],
  )
  .await;

  let rows = ctx.requests.tail(LogScope::All, 10).rows;
  assert_eq!(
    rows.len(),
    1,
    "only the generation call is logged: {:?}",
    rows.iter().map(|r| &r.route).collect::<Vec<_>>()
  );
  let row = &rows[0];
  assert_eq!(row.route, "/v1/chat/completions");
  assert!(row.ui);
  assert_eq!(
    row.requested_model, None,
    "the page sent no model, so nothing is invented for it"
  );
  assert_eq!(row.model.as_deref(), Some("solo"));

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// A launch whose engine serves no web UI of its own gets the embedded bundle
/// instead: the page and `/props` come from the proxy, the OpenAI surface
/// still reaches the engine, and the routes only the bundle's origin serves
/// answer the way an engine without them does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundled_launch_serves_the_embedded_ui() {
  let dir = unique_temp("bundled");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake_tagged("/fixture/solo.gguf", &dir, "generic", Some(2048)).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
    None,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let (status, headers, body) = http_get(addr, "/ui/", &[]).await;
  assert_eq!(status, 200);
  assert_eq!(
    header_value(&headers, "content-type"),
    Some("text/html; charset=utf-8")
  );
  assert_eq!(
    header_value(&headers, "cache-control"),
    Some("no-cache"),
    "the index revalidates so a bump reaches the browser"
  );
  let text = String::from_utf8_lossy(&body);
  assert!(
    text.contains("<!doctype html>") && !text.contains("fake-llama-webui"),
    "expected the embedded index, got: {text}"
  );

  let (status, headers, body) = http_get(addr, "/ui/props", &[]).await;
  assert_eq!(status, 200);
  assert_eq!(
    header_value(&headers, "content-type"),
    Some("application/json")
  );
  let props: Value = serde_json::from_slice(&body).expect("/props is JSON");
  assert_eq!(props["model_path"], "solo");
  assert_eq!(
    props["default_generation_settings"]["n_ctx"], 2048,
    "props reports the ctx the launch asked for"
  );
  assert!(props["default_generation_settings"]["params"].is_object());

  // Engine-only surface: slots answers 404, server tools report as disabled
  // with a 403 (what the page reads as "off"), and the resume lookup gets the
  // engine's "nothing running" answer, an empty list, which the page reads
  // without logging a problem.
  assert_eq!(http_get(addr, "/ui/slots", &[]).await.0, 404);
  assert_eq!(http_get(addr, "/ui/tools", &[]).await.0, 403);
  assert_eq!(
    http_get(addr, "/ui/v1/stream?conv_id=c&from=0", &[])
      .await
      .0,
    404,
    "stream resume stays off"
  );
  let (status, _h, body) = http_post(
    addr,
    "/ui/v1/streams/lookup",
    r#"{"conversation_ids":["x::y"]}"#,
    &[],
  )
  .await;
  assert_eq!(status, 200, "the resume lookup answers the page");
  assert_eq!(
    String::from_utf8_lossy(&body).trim(),
    "[]",
    "no stream is running through the proxy"
  );

  // A dot segment must not smuggle a path past the forwarded `/v1` prefix,
  // percent-encoded or not: the engine's own `/health` answers 200 at its root.
  assert_eq!(http_get(addr, "/ui/v1/../../nope", &[]).await.0, 404);
  assert_eq!(
    http_get(addr, "/ui/v1/%2e%2e/%2e%2e/health", &[]).await.0,
    404,
    "an encoded dot pair stays inside the bundle"
  );
  assert_eq!(
    http_get(addr, "/ui/v1/x/\\..\\..\\health", &[]).await.0,
    404,
    "backslash separators stay inside the bundle too"
  );

  // The OpenAI surface still reaches the engine.
  let (status, _h, body) = http_get(addr, "/ui/v1/models", &[]).await;
  assert_eq!(status, 200);
  assert!(String::from_utf8_lossy(&body).contains("/fixture/solo.gguf"));

  // The page sends no `model` in single-model mode and some engines refuse a
  // request without one, so the forward fills in the launch's served name. The
  // fake echoes the name it received back in the stream.
  let (status, _h, body) = http_post(
    addr,
    "/ui/v1/chat/completions",
    r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    &[],
  )
  .await;
  assert_eq!(status, 200);
  let frame = String::from_utf8_lossy(&body);
  let received = frame
    .lines()
    .find_map(|line| line.strip_prefix("data: "))
    .and_then(|data| serde_json::from_str::<Value>(data).ok())
    .and_then(|chunk| chunk["model"].as_str().map(str::to_string))
    .unwrap_or_default();
  assert_eq!(
    received, "solo",
    "the engine got the launch's served name, not the file path: {frame}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
