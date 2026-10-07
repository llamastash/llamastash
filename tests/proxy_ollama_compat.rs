//! Integration tests for the proxy's Tier 1 Ollama-compat surface:
//! `/api/tags`, `/api/version`, `/api/ps`, `/api/show`.
//!
//! These exist so Ollama-shape discovery libraries (the `ollama-python`
//! default code path, IDE plugins probing `GET /api/tags`,
//! `OLLAMA_HOST` env-based detection) recognise llamastash as
//! Ollama-compatible and fall through to the OpenAI compat endpoints
//! for inference. The Tier 2 inference surface (`/api/chat`,
//! `/api/generate`, `/api/embed`) is deferred to a future plan — see
//! TODO §Low priority.
//!
//! Coverage shape mirrors `tests/proxy_models.rs`: hand-built
//! [`ProxyState`] with a seeded [`ModelCatalog`], proxy listener on an
//! ephemeral port, plain-TCP HTTP/1.1 client. No daemon bring-up
//! because the four endpoints are pure projections of catalog +
//! supervisor snapshots.

#![cfg(feature = "test-fixtures")]

use std::{
  net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use llamastash::backend::lemonade::umbrella_launch_id;
use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::daemon::context::MethodContext;
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
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

// --- shared helpers ------------------------------------------------------
// Lifted from `proxy_models.rs` rather than shared via a `tests/common/`
// module to keep the integration tests self-contained — the per-file
// boilerplate is small enough that the duplication is cheaper than the
// indirection.

#[allow(dead_code)]
fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-oc", label)
}

#[allow(dead_code)]
fn pick_free_port() -> u16 {
  let l = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral");
  l.local_addr().expect("local_addr").port()
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

async fn http_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  parse_response(&buf)
}

async fn http_post(addr: SocketAddr, path: &str, body: &str) -> (u16, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!(
    "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
    len = body.len()
  );
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> (u16, Vec<u8>) {
  let needle = b"\r\n\r\n";
  let split = buf
    .windows(needle.len())
    .position(|w| w == needle)
    .expect("CRLFCRLF terminator");
  let head = std::str::from_utf8(&buf[..split]).expect("utf8 headers");
  let status: u16 = head
    .lines()
    .next()
    .expect("status line")
    .split_whitespace()
    .nth(1)
    .expect("status code")
    .parse()
    .expect("parse status");
  let body = buf[split + needle.len()..].to_vec();
  (status, body)
}

fn fake_metadata(arch: &str, mode: ModeHint) -> ModelMetadata {
  ModelMetadata {
    arch: Some(arch.to_string()),
    total_parameters: Some(7_000_000_000),
    parameter_label: Some("7B".to_string()),
    quant: Quant::Q4_K,
    quant_label: None,
    native_ctx: Some(8192),
    chat_template: Some("{{ messages }}".to_string()),
    tokenizer_kind: Some("llama".to_string()),
    reasoning_hint: false,
    mode_hint: mode,
    weights_bytes: Some(4_200_000_000),
    lazy_tensor_bytes: Vec::new(),
    mtp: None,
  }
}

fn make_model(
  path: &str,
  display_label: Option<&str>,
  arch: &str,
  mode: ModeHint,
) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
  DiscoveredModel {
    path: p,
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata(arch, mode)),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: display_label.map(str::to_string),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

fn make_parse_error_model(path: &str) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
  DiscoveredModel {
    path: p,
    parent,
    source: ModelSource::UserPath,
    metadata: None,
    parse_error: Some("synthetic parse failure".to_string()),
    split_siblings: Vec::new(),
    display_label: None,
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

async fn proxy_state_with_models(models: Vec<DiscoveredModel>) -> Arc<ProxyState> {
  proxy_state_with_models_compat(models, false).await
}

/// One build seam for every state in this file: the catalog, the optional
/// supervisor registry, Ollama drop-in mode, and the `proxy.aliases` table the
/// daemon hands `from_context_with_auth` in production. The wrappers below are
/// what the tests call.
async fn build_state(
  models: Vec<DiscoveredModel>,
  registry: Option<SupervisorRegistry>,
  ollama_compat: bool,
  aliases: &[(&str, &str)],
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx = MethodContext::with_catalog(ShutdownToken::new(), catalog);
  let ctx = match registry {
    Some(registry) => ctx.with_supervisors(registry),
    None => ctx,
  };
  let aliases = llamastash::config::ProxyAliases::from_pairs(
    aliases
      .iter()
      .map(|(name, target)| (name.to_string(), target.to_string())),
  );
  ProxyState::from_context_with_auth(
    &ctx,
    ollama_compat,
    true,
    None,
    DEFAULT_BODY_LIMIT_BYTES,
    &aliases,
  )
}

/// Like [`proxy_state_with_models`] with a `proxy.aliases` entry.
async fn proxy_state_with_alias(
  models: Vec<DiscoveredModel>,
  alias: (&str, &str),
) -> Arc<ProxyState> {
  proxy_state_with_aliases(models, vec![alias]).await
}

async fn proxy_state_with_aliases(
  models: Vec<DiscoveredModel>,
  aliases: Vec<(&str, &str)>,
) -> Arc<ProxyState> {
  build_state(models, None, false, &aliases).await
}

async fn proxy_state_with_models_compat(
  models: Vec<DiscoveredModel>,
  ollama_compat: bool,
) -> Arc<ProxyState> {
  build_state(models, None, ollama_compat, &[]).await
}

#[allow(dead_code)]
async fn http_head(addr: SocketAddr, path: &str) -> (u16, usize) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!("HEAD {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  let (status, body) = parse_response(&buf);
  (status, body.len())
}

// --- /api/version --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_version_returns_cargo_pkg_version() {
  let state = proxy_state_with_models(Vec::new()).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/version").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let version = v
    .get("version")
    .and_then(Value::as_str)
    .expect("version field");
  // Cargo.toml sets the crate version; this asserts the field is
  // present and non-empty without pinning a specific value (which
  // would churn on every release).
  assert!(
    !version.is_empty(),
    "version field must be non-empty: {version}"
  );
  // Exactly one field on the wire — discovery clients pin against
  // this shape.
  let obj = v.as_object().expect("object body");
  assert_eq!(obj.len(), 1, "wire shape: {v}");

  shutdown_listener(shutdown, handle).await;
}

// --- /api/tags -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_tags_returns_alphabetical_models_with_documented_fields() {
  let models = vec![
    make_model("/m/qwen3.gguf", Some("qwen3"), "qwen3", ModeHint::Chat),
    make_model("/m/llama.gguf", None, "llama", ModeHint::Chat),
    make_model("/m/gemma.gguf", Some("gemma:2b"), "gemma", ModeHint::Chat),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/tags").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let arr = v["models"].as_array().expect("models array");
  assert_eq!(arr.len(), 3);
  let names: Vec<&str> = arr.iter().map(|r| r["name"].as_str().unwrap()).collect();
  assert_eq!(
    names,
    ["gemma:2b", "llama", "qwen3"],
    "models sorted alphabetically"
  );
  // Per-row shape check: required Ollama fields present.
  let first = &arr[0];
  assert!(first["name"].is_string());
  assert!(first["model"].is_string());
  assert_eq!(
    first["name"], first["model"],
    "name and model agree on local-only rows"
  );
  assert!(first["modified_at"].is_string());
  assert!(first["size"].is_u64());
  let digest = first["digest"].as_str().expect("digest field");
  assert!(
    digest.starts_with("blake3:"),
    "digest uses blake3 prefix: {digest}"
  );
  let details = &first["details"];
  assert_eq!(details["format"], "gguf");
  assert_eq!(details["family"], "gemma");
  assert!(details["families"].is_array());
  assert_eq!(details["quantization_level"], "Q4_K");
  assert_eq!(details["parameter_size"], "7B");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_tags_empty_catalog_returns_empty_models_list() {
  let state = proxy_state_with_models(Vec::new()).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/tags").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let arr = v["models"].as_array().expect("models array");
  assert_eq!(arr.len(), 0, "empty catalog → empty list, not 404");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_tags_tolerates_parse_error_rows() {
  // A discovery row with `metadata: None` (header parse failed) must
  // still surface in the tags list — Ollama clients that pin against
  // /api/tags shouldn't see disappearing models on transient header
  // problems. The row carries empty `family` / `parameter_size` and
  // an `Unknown` quantization.
  let models = vec![
    make_model("/m/good.gguf", None, "llama", ModeHint::Chat),
    make_parse_error_model("/m/broken.gguf"),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/tags").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let arr = v["models"].as_array().expect("models array");
  assert_eq!(arr.len(), 2);
  let broken = arr
    .iter()
    .find(|r| r["name"] == "broken")
    .expect("broken row present");
  assert_eq!(broken["details"]["family"], "");
  assert_eq!(broken["details"]["quantization_level"], "Unknown");
  assert_eq!(broken["size"], 0);

  shutdown_listener(shutdown, handle).await;
}

// --- /api/ps -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_ps_with_no_running_supervisors_returns_empty_list() {
  // Catalog has models but no supervisor is Ready → /api/ps is empty.
  // Mirrors Ollama's behaviour: /api/tags lists files on disk,
  // /api/ps lists loaded models.
  let models = vec![make_model("/m/dormant.gguf", None, "llama", ModeHint::Chat)];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/ps").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let arr = v["models"].as_array().expect("models array");
  assert_eq!(arr.len(), 0);

  shutdown_listener(shutdown, handle).await;
}

// --- /api/show -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_returns_metadata_for_known_model() {
  let models = vec![make_model(
    "/m/qwen-coder.gguf",
    Some("qwen-coder:7b"),
    "qwen3",
    ModeHint::Chat,
  )];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"qwen-coder:7b"}"#).await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  // Documented top-level slots present.
  assert!(v["modelfile"].is_string(), "modelfile field present");
  assert!(v["parameters"].is_string(), "parameters field present");
  assert_eq!(v["template"], "{{ messages }}");
  assert_eq!(v["details"]["family"], "qwen3");
  assert_eq!(v["details"]["quantization_level"], "Q4_K");
  // model_info carries the typical Ollama-shape keys.
  let info = v["model_info"].as_object().expect("model_info object");
  assert_eq!(
    info.get("general.architecture"),
    Some(&Value::from("qwen3"))
  );
  assert_eq!(
    info.get("general.parameter_count"),
    Some(&Value::from(7_000_000_000_u64))
  );
  assert_eq!(
    info.get("general.parameter_label"),
    Some(&Value::from("7B"))
  );
  assert_eq!(info.get("general.context_length"), Some(&Value::from(8192)));
  // capabilities reflects Chat mode_hint.
  let caps = v["capabilities"].as_array().expect("capabilities array");
  let caps_str: Vec<&str> = caps.iter().filter_map(Value::as_str).collect();
  assert_eq!(caps_str, vec!["completion"]);

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_accepts_legacy_name_field() {
  // Older Ollama clients send `{"name": "model-name"}`; the proxy
  // must accept either.
  let models = vec![make_model("/m/legacy.gguf", None, "llama", ModeHint::Chat)];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"name":"legacy"}"#).await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["details"]["family"], "llama");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_resolves_a_proxy_alias() {
  // A tool hard-wired to `gpt-4o-mini` should get metadata for the model the
  // alias names, through the Ollama surface as well as the OpenAI one.
  let models = vec![make_model(
    "/m/qwen-coder.gguf",
    Some("qwen-coder:7b"),
    "qwen3",
    ModeHint::Chat,
  )];
  let state = proxy_state_with_alias(models, ("gpt-4o-mini", "qwen-coder:7b")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-4o-mini"}"#).await;
  assert_eq!(
    status, 200,
    "alias must resolve on /api/show: {status} {body:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["details"]["family"], "qwen3");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_beats_a_model_that_merely_contains_the_name() {
  // The alias is the operator's explicit mapping, so it outranks a partial
  // match: `gpt-4o-mini` must not be stolen by the `gpt-4o-mini-Q4_K_M.gguf`
  // that shows up on disk later.
  let models = vec![
    make_model(
      "/m/gpt-4o-mini-Q4_K_M.gguf",
      Some("gpt-4o-mini-Q4_K_M"),
      "llama",
      ModeHint::Chat,
    ),
    make_model(
      "/m/qwen-coder.gguf",
      Some("qwen-coder:7b"),
      "qwen3",
      ModeHint::Chat,
    ),
  ];
  let state = proxy_state_with_alias(models, ("gpt-4o-mini", "qwen-coder:7b")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-4o-mini"}"#).await;
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(
    v["details"]["family"], "qwen3",
    "the alias must answer, not the longer file name: {status} {body:?}"
  );

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_resolves_what_is_ambiguous_without_it() {
  // Two rows contain `gpt-oss`, so the bare name is ambiguous on its own. An
  // alias for it is the tie-break the operator wrote, and has to win over the
  // 400 the same string would otherwise get.
  let models = vec![
    make_model(
      "/m/gpt-oss-20b.gguf",
      Some("gpt-oss-20b"),
      "phi",
      ModeHint::Chat,
    ),
    make_model(
      "/m/gpt-oss-120b.gguf",
      Some("gpt-oss-120b"),
      "llama",
      ModeHint::Chat,
    ),
  ];
  let state = proxy_state_with_alias(models, ("gpt-oss", "gpt-oss-20b")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-oss"}"#).await;
  assert_eq!(
    status, 200,
    "the alias must settle what the catalog cannot: {status} {body:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["details"]["family"], "phi");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_target_has_to_name_a_model_on_its_own() {
  // `qwen` is a substring of this file's name and of a directory above it, but it
  // is not a model. Answering it would route to whichever row happened to contain
  // the string, and the client cannot send anything better.
  let models = vec![make_model(
    "/models/qwen/misc/obfuscated-name.gguf",
    None,
    "phi",
    ModeHint::Chat,
  )];
  let state = proxy_state_with_alias(models, ("gpt-4o-mini", "qwen")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-4o-mini"}"#).await;
  assert_eq!(
    status, 404,
    "a target that only matches as a substring is refused, not guessed: {status} {body:?}"
  );

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_may_name_a_path() {
  // The same rungs a client may send: a path is the unambiguous spelling, so it
  // belongs in the alias surface too.
  let models = vec![make_model(
    "/m/qwen-coder.gguf",
    Some("qwen-coder:7b"),
    "qwen3",
    ModeHint::Chat,
  )];
  let state = proxy_state_with_alias(models, ("gpt-4o-mini", "/m/qwen-coder.gguf")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-4o-mini"}"#).await;
  assert_eq!(status, 200, "a path target resolves: {status} {body:?}");
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["details"]["family"], "qwen3");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_pointing_at_another_alias_is_refused() {
  // `hardwired -> y` reaches no model of its own; following `y` would be a chain,
  // and a chain names no model.
  let models = vec![
    make_model(
      "/m/realmodel.gguf",
      Some("realmodel"),
      "llama",
      ModeHint::Chat,
    ),
    make_model(
      "/m/synthetic.gguf",
      Some("synthetic"),
      "phi",
      ModeHint::Chat,
    ),
  ];
  let state = proxy_state_with_aliases(models, vec![("hardwired", "y"), ("y", "realmodel")]).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, _) = http_post(addr, "/api/show", r#"{"model":"hardwired"}"#).await;
  assert_eq!(status, 404, "the chain is refused, not followed");
  let (status, body) = http_post(addr, "/api/show", r#"{"model":"y"}"#).await;
  assert_eq!(
    status, 200,
    "the entry that names a model still works: {body:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["details"]["family"], "llama");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_is_consulted_for_a_launch_address() {
  // `gpt-4o-mini@coder` is the aliased name plus a launch the client wants. The
  // alias has to be consulted for the model half, or the string falls through to
  // a partial match and reaches whichever longer file name contains it.
  let models = vec![
    make_model(
      "/m/gpt-4o-mini-Q4_K_M.gguf",
      Some("gpt-4o-mini-Q4_K_M"),
      "llama",
      ModeHint::Chat,
    ),
    make_model(
      "/m/qwen-coder.gguf",
      Some("qwen-coder:7b"),
      "qwen3",
      ModeHint::Chat,
    ),
  ];
  let state = proxy_state_with_alias(models, ("gpt-4o-mini", "qwen-coder:7b")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-4o-mini@coder"}"#).await;
  assert_eq!(
    status, 200,
    "the alias must answer its own address: {status} {body:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(
    v["details"]["family"], "qwen3",
    "the alias target, not the longer file name that contains the name: {body:?}"
  );

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_value_two_models_answer_to_names_its_candidates() {
  // Two rows answer to `qwen3-32b`. Naming one is the operator's call, and the
  // answer says so with the same candidate list a client gets when it sends an
  // ambiguous name itself.
  let models = vec![
    make_model("/m/a/qwen3-32b.gguf", None, "qwen3", ModeHint::Chat),
    make_model("/m/b/qwen3-32b.gguf", None, "qwen3", ModeHint::Chat),
  ];
  let state = proxy_state_with_alias(models, ("gpt-4o-mini", "qwen3-32b")).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-4o-mini"}"#).await;
  assert_eq!(
    status, 400,
    "an ambiguous value is ambiguity, not a missing model: {status} {body:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["error"]["type"], "ambiguous_model", "{body:?}");
  assert_eq!(
    v["error"]["matches"].as_array().map(|m| m.len()),
    Some(2),
    "both candidates are named: {body:?}"
  );

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_an_alias_value_that_is_a_real_model_wins_over_being_an_alias_name() {
  // `demo` is both an alias of its own and the id of a real model. An entry that
  // points at `demo` means the model, so this entry has to survive the table and
  // resolve.
  let models = vec![
    make_model("/m/demo.gguf", Some("demo"), "phi", ModeHint::Chat),
    make_model("/m/other.gguf", Some("other"), "llama", ModeHint::Chat),
  ];
  let state = proxy_state_with_aliases(models, vec![("fast", "demo"), ("demo", "other")]).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"fast"}"#).await;
  assert_eq!(
    status, 200,
    "a value naming a real model resolves: {status} {body:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["details"]["family"], "phi", "{body:?}");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_without_the_alias_still_calls_that_name_ambiguous() {
  // The same two rows with no alias: the alias is what changed behaviour, not
  // the matcher.
  let models = vec![
    make_model(
      "/m/gpt-oss-20b.gguf",
      Some("gpt-oss-20b"),
      "phi",
      ModeHint::Chat,
    ),
    make_model(
      "/m/gpt-oss-120b.gguf",
      Some("gpt-oss-120b"),
      "llama",
      ModeHint::Chat,
    ),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"gpt-oss"}"#).await;
  assert_eq!(
    status, 400,
    "an ambiguous name with no alias stays ambiguous: {status} {body:?}"
  );

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_missing_model_returns_404_model_not_found() {
  let state = proxy_state_with_models(Vec::new()).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"nonexistent"}"#).await;
  assert_eq!(status, 404);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["error"]["type"], "model_not_found");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_without_model_field_returns_400_model_required() {
  let state = proxy_state_with_models(Vec::new()).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{}"#).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["error"]["type"], "invalid_request");
  assert_eq!(v["error"]["code"], "model_required");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_ambiguous_reference_returns_400_with_matches() {
  // Two rows both contain "qwen" — substring lookup is ambiguous,
  // mirroring the OpenAI compat surface's ambiguous_model branch.
  let models = vec![
    make_model(
      "/m/qwen3-7b.gguf",
      Some("qwen3:7b"),
      "qwen3",
      ModeHint::Chat,
    ),
    make_model(
      "/m/qwen3-13b.gguf",
      Some("qwen3:13b"),
      "qwen3",
      ModeHint::Chat,
    ),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"qwen3"}"#).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["error"]["type"], "ambiguous_model");
  let matches = v["error"]["matches"].as_array().expect("matches array");
  assert_eq!(matches.len(), 2);

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_show_capabilities_reflect_mode_hint() {
  let models = vec![
    make_model(
      "/m/embed.gguf",
      Some("embed-mini"),
      "bert",
      ModeHint::Embedding,
    ),
    make_model(
      "/m/rerank.gguf",
      Some("rerank-bge"),
      "bert",
      ModeHint::Rerank,
    ),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"embed-mini"}"#).await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let caps: Vec<&str> = v["capabilities"]
    .as_array()
    .expect("array")
    .iter()
    .filter_map(Value::as_str)
    .collect();
  assert_eq!(caps, vec!["embedding"], "embedding mode → embedding cap");

  let (status, body) = http_post(addr, "/api/show", r#"{"model":"rerank-bge"}"#).await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let caps: Vec<&str> = v["capabilities"]
    .as_array()
    .expect("array")
    .iter()
    .filter_map(Value::as_str)
    .collect();
  assert_eq!(caps, vec!["rerank"], "rerank mode → rerank cap");

  shutdown_listener(shutdown, handle).await;
}

// --- /api/ps Ready supervisor + cross-endpoint digest -------------------
//
// The Ready branch of `/api/ps` needs a live supervisor in the
// registry. Pattern lifted from `tests/proxy_fallback.rs`: drop a
// minimal GGUF on disk, spawn `fake_llama_server` against it,
// register the resulting `ManagedModel` in a `SupervisorRegistry`,
// then wire that registry into the `MethodContext` the proxy reads.

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(30),
    timeout: Duration::from_secs(15),
  }
}

fn write_gguf(dir: &Path, name: &str, arch: &str) -> PathBuf {
  let path = dir.join(name);
  std::fs::write(&path, build_minimal_gguf(arch)).expect("write gguf");
  llamastash::util::paths::canonicalize(&path).expect("canonicalize")
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

async fn spawn_ready_model(
  catalog_path: &Path,
  log_dir: &Path,
  mode: LaunchMode,
  log_name: &str,
) -> ManagedModel {
  let port = pick_free_port();
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
    log_path: log_dir.join(log_name),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn");
  wait_for_ready(&model).await;
  model
}

async fn pre_launch(
  catalog_path: &Path,
  registry: &SupervisorRegistry,
  log_dir: &Path,
  mode: LaunchMode,
) -> ManagedModel {
  let model = spawn_ready_model(catalog_path, log_dir, mode, "ollama-ps.log").await;
  let launch_id = registry.next_id();
  registry.insert(launch_id, model.clone()).await;
  model
}

async fn proxy_state_with_models_and_registry(
  models: Vec<DiscoveredModel>,
  registry: SupervisorRegistry,
) -> Arc<ProxyState> {
  build_state(models, Some(registry), false, &[]).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_ps_returns_ready_supervisor_with_documented_fields() {
  let dir = unique_temp_dir("ps-ready");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let qwen3 = write_gguf(&dir, "qwen3-ready.gguf", "qwen3");

  let registry = SupervisorRegistry::new();
  let live = pre_launch(&qwen3, &registry, &log_dir, LaunchMode::Chat).await;

  // Catalog entry that points at the same canonical path the
  // supervisor's ModelId carries — that's how /api/ps joins
  // supervisor rows back to catalog metadata.
  let mut discovered = make_model(
    qwen3.to_str().unwrap(),
    Some("qwen3:7b"),
    "qwen3",
    ModeHint::Chat,
  );
  discovered.path = qwen3.clone();
  let state = proxy_state_with_models_and_registry(vec![discovered], registry).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/ps").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let arr = v["models"].as_array().expect("models array");
  assert_eq!(arr.len(), 1, "exactly one Ready supervisor: {v}");
  let row = &arr[0];

  // Required Ollama fields present.
  assert_eq!(row["name"], row["model"], "name and model agree");
  assert_eq!(row["name"], "qwen3:7b", "display_label wins");
  assert_eq!(row["size"], 4_200_000_000_u64, "weights_bytes projected");
  let digest = row["digest"].as_str().expect("digest field");
  assert!(
    digest.starts_with("blake3:"),
    "ps digest uses blake3 prefix: {digest}"
  );
  // Placeholder slots — pinned so a regression here surfaces loudly.
  assert_eq!(
    row["expires_at"], "9999-12-31T23:59:59Z",
    "no idle-TTL eviction → far-future placeholder"
  );
  assert_eq!(row["size_vram"], 0, "VRAM attribution TODO → 0");
  // Details block reuses the same projection as /api/tags.
  assert_eq!(row["details"]["family"], "qwen3");
  assert_eq!(row["details"]["quantization_level"], "Q4_K");
  assert_eq!(row["details"]["parameter_size"], "7B");

  let _ = live.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_ps_excludes_the_lemonade_umbrella_supervisor() {
  // Regression guard for the /api/ps drift fix. The Lemonade umbrella is
  // a multiplexer process, not a servable model, so /api/ps must skip the
  // supervisor parked under `umbrella_launch_id()`. The umbrella here is
  // deliberately Ready, so only the launch-id check — not the Ready gate —
  // can keep it out of the listing.
  let dir = unique_temp_dir("ps-umbrella");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let servable = write_gguf(&dir, "qwen3-servable.gguf", "qwen3");
  let umbrella = write_gguf(&dir, "umbrella.gguf", "qwen3");

  let registry = SupervisorRegistry::new();
  // One genuinely servable model under a normal launch id.
  let live = pre_launch(&servable, &registry, &log_dir, LaunchMode::Chat).await;
  // A second Ready supervisor parked under the umbrella id.
  let umbrella_model =
    spawn_ready_model(&umbrella, &log_dir, LaunchMode::Chat, "umbrella.log").await;
  registry
    .insert(umbrella_launch_id(), umbrella_model.clone())
    .await;

  let mut servable_row = make_model(
    servable.to_str().unwrap(),
    Some("qwen3:7b"),
    "qwen3",
    ModeHint::Chat,
  );
  servable_row.path = servable.clone();
  let state = proxy_state_with_models_and_registry(vec![servable_row], registry).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/api/ps").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let arr = v["models"].as_array().expect("models array");
  assert_eq!(
    arr.len(),
    1,
    "umbrella supervisor must be excluded; only the servable model remains: {v}"
  );
  assert_eq!(
    arr[0]["name"], "qwen3:7b",
    "the surviving row is the servable model, not the umbrella"
  );

  let _ = live.stop(Duration::from_secs(3)).await;
  let _ = umbrella_model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digest_is_stable_across_tags_and_ps_for_same_model() {
  // Cross-endpoint invariant: /api/tags and /api/ps emit the same
  // `digest` value for the same model. Clients that pin tags-to-ps
  // joins by digest depend on this — see `ollama_compat::digest_for_path`.
  let dir = unique_temp_dir("ps-digest");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let qwen3 = write_gguf(&dir, "qwen3-digest.gguf", "qwen3");

  let registry = SupervisorRegistry::new();
  let live = pre_launch(&qwen3, &registry, &log_dir, LaunchMode::Chat).await;

  let mut discovered = make_model(
    qwen3.to_str().unwrap(),
    Some("qwen3:7b"),
    "qwen3",
    ModeHint::Chat,
  );
  discovered.path = qwen3.clone();
  let state = proxy_state_with_models_and_registry(vec![discovered], registry).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (s_tags, b_tags) = http_get(addr, "/api/tags").await;
  assert_eq!(s_tags, 200);
  let v_tags: Value = serde_json::from_slice(&b_tags).expect("tags json");
  let tags_row = v_tags["models"]
    .as_array()
    .and_then(|a| a.iter().find(|r| r["name"] == "qwen3:7b"))
    .expect("tags row for qwen3:7b");

  let (s_ps, b_ps) = http_get(addr, "/api/ps").await;
  assert_eq!(s_ps, 200);
  let v_ps: Value = serde_json::from_slice(&b_ps).expect("ps json");
  let ps_row = v_ps["models"]
    .as_array()
    .and_then(|a| a.iter().find(|r| r["name"] == "qwen3:7b"))
    .expect("ps row for qwen3:7b");

  assert_eq!(
    tags_row["digest"], ps_row["digest"],
    "same model → same digest across /api/tags and /api/ps"
  );

  let _ = live.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// --- GET / and HEAD / ----------------------------------------------------
//
// The server-identity handshake the official `ollama` CLI (and other
// Ollama-Go clients) issue before any `/api/*` call. Pre-fix this
// returned 404 and the CLI bailed with "something went wrong"; the
// regression target is now `200 OK` + the mode-appropriate body.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_get_default_mode_identifies_as_llamastash() {
  let state = proxy_state_with_models_compat(Vec::new(), false).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/").await;
  assert_eq!(status, 200);
  // Trailing newline matches real Ollama's `"Ollama is running\n"`
  // wire form — agents that strcmp the body bytes see the same shape.
  assert_eq!(
    body, b"LlamaStash is running\n",
    "body should be the default-mode identity string"
  );

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_get_ollama_compat_mode_identifies_as_ollama() {
  let state = proxy_state_with_models_compat(Vec::new(), true).await;
  let (addr, shutdown, handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/").await;
  assert_eq!(status, 200);
  // Byte-exact match for `ollama` CLI compatibility — the Go client
  // version detection sometimes strcmp's the literal string.
  assert_eq!(body, b"Ollama is running\n");

  shutdown_listener(shutdown, handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_head_returns_200_with_no_body_in_either_mode() {
  // Real Ollama responds to HEAD / with 200 and an empty body (hyper
  // strips the body for HEAD automatically). The CLI's handshake
  // probe checks the status only — failing this is the bug that
  // motivated the whole root-route addition.
  for compat in [false, true] {
    let state = proxy_state_with_models_compat(Vec::new(), compat).await;
    let (addr, shutdown, handle) = spawn_listener_with_state(state).await;
    let (status, body_len) = http_head(addr, "/").await;
    assert_eq!(status, 200, "HEAD / must succeed in {compat:?} mode");
    assert_eq!(
      body_len, 0,
      "HEAD response must carry no body in {compat:?} mode"
    );
    shutdown_listener(shutdown, handle).await;
  }
}
