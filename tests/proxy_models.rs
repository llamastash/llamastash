//! Integration tests for the proxy's `/v1/models` endpoint (Unit 2:
//! docs/plans/2026-05-21-001-feat-proxy-router-plan.md).
//!
//! Two layers of coverage:
//!
//! 1. Direct: hand-build a [`ProxyState`] with a seeded
//!    [`ModelCatalog`], spawn the proxy listener on an ephemeral
//!    port, and call `/v1/models` over the wire. This is fast and
//!    isolates routing/sorting/projection from the full daemon
//!    bring-up. Used for the happy paths, the empty-catalog case,
//!    the parse-error fallback, and the 200-row scale check.
//! 2. End-to-end: a single test spins up `run_foreground` with a
//!    fixture scan root containing two minimal GGUFs, then asserts
//!    `/v1/models` and `/health` reflect the discovered catalog.
//!    Mirrors the daemon-wiring smoke from `proxy_listener_test.rs`.
//!
//! Schema parity: every test that inspects the response body asserts
//! the documented OpenAI shape — `{"object":"list", "data":[...]}`
//! with each row carrying exactly `id` / `object` / `created` /
//! `owned_by`. The inline tests under `src/proxy/openai.rs` cover the
//! per-row serialization in isolation; these tests confirm the
//! handler emits the same shape end-to-end.

#![cfg(feature = "test-fixtures")]

use std::{
  net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use llamastash::config::loader::ProxyConfig;
use llamastash::daemon::context::MethodContext;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::ipc::Client;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

// --- shared helpers ------------------------------------------------------

fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pm", label)
}

fn pick_free_port() -> u16 {
  let l = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral");
  l.local_addr().expect("local_addr").port()
}

/// Spin up the proxy listener on an ephemeral port, backed by the
/// given `ProxyState`. Returns the bound address + shutdown token.
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

async fn http_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
  let mut sock = TcpStream::connect(addr).await.expect("connect");
  let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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
  // We always emit `Content-Length`, so the body is `buf` from
  // `split + 4` to EOF (we asked for Connection: close, so the
  // server closes after writing).
  let body = buf[split + needle.len()..].to_vec();
  (status, body)
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
    lazy_tensor_bytes: Vec::new(),
    mtp: None,
  }
}

fn make_model(path: &str, display_label: Option<&str>) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
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

/// Build a `ProxyState` whose catalog is pre-seeded with the given
/// models. Other slots (`supervisors`, `state`, `launch`) come from
/// a default-constructed `MethodContext` — Unit 2 only touches
/// `catalog`.
async fn proxy_state_with_models(models: Vec<DiscoveredModel>) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx = MethodContext::with_catalog(ShutdownToken::new(), catalog);
  ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES)
}

// --- direct-listener tests ----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_models_return_in_alphabetical_order() {
  // Seed three rows with intentionally-shuffled display labels so the
  // sort is observably non-trivial. `gemma:2b` (via display_label),
  // `llama-7b` (via path stem), `qwen3` (via display_label).
  let models = vec![
    make_model("/m/qwen3.gguf", Some("qwen3")),
    make_model("/m/llama.gguf", None), // file_stem → "llama"
    make_model("/m/gemma.gguf", Some("gemma:2b")),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/v1/models").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["object"], "list");
  let data = v["data"].as_array().expect("data array");
  assert_eq!(data.len(), 3, "three rows: {v}");

  // Sort order: ASCII lexicographic by `id`.
  let ids: Vec<&str> = data.iter().map(|r| r["id"].as_str().unwrap()).collect();
  assert_eq!(ids, vec!["gemma:2b", "llama", "qwen3"]);

  // Each row carries the documented four fields and only those.
  for row in data {
    let obj = row.as_object().expect("row object");
    assert_eq!(obj.len(), 4, "row has 4 fields: {row}");
    assert_eq!(obj.get("object"), Some(&serde_json::json!("model")));
    assert_eq!(obj.get("owned_by"), Some(&serde_json::json!("llamastash")));
    assert!(obj.get("created").is_some(), "created field present");
    assert!(obj.get("id").is_some(), "id field present");
  }

  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_models_sharing_a_basename_publish_two_reachable_ids() {
  // The same GGUF cached under two roots used to publish one id twice, and
  // a request for it 400'd `ambiguous_model` listing that id twice — the
  // model was unreachable from any OpenAI client, and the id `init` wrote
  // into tool configs always failed. Each row now publishes its
  // repo-qualified form; the row that shares nothing keeps its bare id.
  let models = vec![
    make_model("/hf/gemma-4-E2B-it-Q4_K_M.gguf", None),
    make_model("/lmstudio/gemma-4-E2B-it-Q4_K_M.gguf", None),
    make_model("/m/llama.gguf", None),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/v1/models").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let ids: Vec<&str> = v["data"]
    .as_array()
    .expect("data array")
    .iter()
    .map(|r| r["id"].as_str().unwrap())
    .collect();
  assert_eq!(
    ids,
    vec![
      "hf/gemma-4-E2B-it-Q4_K_M",
      "llama",
      "lmstudio/gemma-4-E2B-it-Q4_K_M",
    ],
    "colliding rows qualify, the unique row does not: {v}"
  );

  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_repo_cached_by_two_tools_publishes_source_qualified_ids() {
  // The ordinary duplicate, and the case the repo qualifier alone could not
  // separate: both roots derive `lmstudio-community/gemma-4-E2B-it-GGUF`, so
  // both rows used to fall through to raw absolute paths — machine-specific
  // ids that then went into tool configs.
  let mut hf = make_model(
    "/w/huggingface/hub/models--lmstudio-community--gemma-4-E2B-it-GGUF/snapshots/1/gemma-4-E2B-it-Q4_K_M.gguf",
    None,
  );
  hf.source = ModelSource::HuggingFace;
  let mut lms = make_model(
    "/w/lmstudio-models/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M.gguf",
    None,
  );
  lms.source = ModelSource::LmStudio;

  let state = proxy_state_with_models(vec![hf, lms]).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/v1/models").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let ids: Vec<&str> = v["data"]
    .as_array()
    .expect("data array")
    .iter()
    .map(|r| r["id"].as_str().unwrap())
    .collect();
  assert_eq!(
    ids,
    vec![
      "huggingface/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M",
      "lm-studio/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M",
    ],
    "the source separates them, so neither falls back to a path: {v}"
  );
  for id in &ids {
    assert!(
      !id.starts_with('/'),
      "`{id}` is an absolute path, not an id"
    );
  }

  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_catalog_returns_empty_data_not_error() {
  let state = proxy_state_with_models(Vec::new()).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;
  let (status, body) = http_get(addr, "/v1/models").await;
  assert_eq!(
    status, 200,
    "empty catalog is not a 404 / 500: status={status}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["object"], "list");
  let data = v["data"].as_array().expect("data array");
  assert!(data.is_empty(), "data is empty array, got: {v}");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parse_error_row_still_appears_with_file_stem_id() {
  // A model whose GGUF header parse failed: `metadata: None`,
  // `parse_error: Some(...)`. The CLI `llamastash list` still
  // surfaces it; the proxy must too — using `path.file_stem()` as
  // the `id`.
  let bad = DiscoveredModel {
    path: PathBuf::from("/m/broken.gguf"),
    parent: PathBuf::from("/m"),
    source: ModelSource::UserPath,
    metadata: None,
    parse_error: Some("BadMagic".to_string()),
    split_siblings: Vec::new(),
    display_label: None,
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  };
  let state = proxy_state_with_models(vec![bad]).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  let (status, body) = http_get(addr, "/v1/models").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  let data = v["data"].as_array().expect("data array");
  assert_eq!(data.len(), 1, "parse_error row still appears: {v}");
  assert_eq!(
    data[0]["id"], "broken",
    "id = path.file_stem() on parse_error"
  );
  assert_eq!(data[0]["object"], "model");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_hundred_models_stay_under_one_mib_and_sort_is_stable() {
  let mut models = Vec::with_capacity(200);
  for i in 0..200u32 {
    // Pad the index so the lexicographic sort matches the numeric
    // order — makes the assertion below readable.
    let label = format!("model-{i:03}");
    models.push(make_model(&format!("/m/{label}.gguf"), Some(&label)));
  }
  // Shuffle by inserting under a non-sorted key. The catalog's
  // BTreeMap stores by canonical path, so insertion order here
  // doesn't matter — but we still want the response sort to be the
  // *handler's* sort, not the BTreeMap's.
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;

  // Two back-to-back calls; the second must be byte-identical to the
  // first (stable sort, no time-dependent fields beyond `created`
  // which is hard-coded to 0).
  let (s1, b1) = http_get(addr, "/v1/models").await;
  let (s2, b2) = http_get(addr, "/v1/models").await;
  assert_eq!(s1, 200);
  assert_eq!(s2, 200);
  assert_eq!(b1, b2, "two calls return byte-identical bodies");

  // Under 1 MiB by a wide margin — each row is ~70 bytes.
  assert!(
    b1.len() < 1024 * 1024,
    "200-row response stays under 1 MiB; got {} bytes",
    b1.len()
  );

  let v: Value = serde_json::from_slice(&b1).expect("json body");
  let data = v["data"].as_array().expect("data array");
  assert_eq!(data.len(), 200);
  // Spot-check the alphabetical order at the boundaries.
  assert_eq!(data[0]["id"], "model-000");
  assert_eq!(data[199]["id"], "model-199");

  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_parity_with_documented_openai_shape() {
  // One row; verify every documented field has the documented type.
  // Recording a real OpenAI client fixture would be heavy; matching
  // the four-field shape inline is enough to lock the contract that
  // OpenAI's Python/Node SDKs deserialize against.
  let state = proxy_state_with_models(vec![make_model("/m/x.gguf", Some("x:1"))]).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;
  let (_status, body) = http_get(addr, "/v1/models").await;
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["object"].as_str(), Some("list"));
  let row = &v["data"][0];
  assert!(row["id"].is_string(), "id is string");
  assert_eq!(row["object"].as_str(), Some("model"));
  assert!(row["created"].is_u64(), "created is number");
  assert!(row["owned_by"].is_string(), "owned_by is string");
  // Reject any drift: exactly four documented fields.
  assert_eq!(
    row.as_object().unwrap().keys().collect::<Vec<_>>().len(),
    4,
    "no extra fields snuck in: {row}"
  );
  shutdown_listener(shutdown, listener_handle).await;
}

// --- /health counts ------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_zero_when_catalog_and_supervisors_are_empty() {
  // Default MethodContext has an empty catalog and zero supervisors;
  // /health must report 0/0 rather than the wire-shape stand-in.
  let state = proxy_state_with_models(Vec::new()).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;
  let (status, body) = http_get(addr, "/health").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["status"], "ok");
  assert_eq!(v["models_loaded"], 0, "no supervisors → 0");
  assert_eq!(v["models_discovered"], 0, "empty catalog → 0");
  shutdown_listener(shutdown, listener_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_models_discovered_matches_catalog_length() {
  // Three discovered models. No supervisors are launched here (Unit
  // 2 only needs to confirm the count is sourced from the catalog),
  // so `models_loaded` stays 0 — exercised end-to-end in Unit 3+.
  let models = vec![
    make_model("/m/a.gguf", None),
    make_model("/m/b.gguf", None),
    make_model("/m/c.gguf", None),
  ];
  let state = proxy_state_with_models(models).await;
  let (addr, shutdown, listener_handle) = spawn_listener_with_state(state).await;
  let (status, body) = http_get(addr, "/health").await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json body");
  assert_eq!(v["models_discovered"], 3);
  assert_eq!(v["models_loaded"], 0, "no ready supervisors");
  shutdown_listener(shutdown, listener_handle).await;
}

// --- daemon-wiring smoke -------------------------------------------------

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon not connectable within 3s: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    sleep(Duration::from_millis(20)).await;
  }
}

async fn wait_for_proxy(addr: SocketAddr) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("proxy not connectable within 3s: {addr}");
    }
    if TcpStream::connect(addr).await.is_ok() {
      return;
    }
    sleep(Duration::from_millis(20)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_proxy_models_matches_discovery_catalog() {
  // Spin up the daemon with a scan root containing two minimal
  // GGUFs. Once discovery finishes, `/v1/models` must return both
  // rows. This is the parity check that the maintainer's manual
  // verification line (`curl /v1/models | jq '.data | length' ==
  // llamastash list --json | jq '.models | length'`) is intended
  // to cover.
  use llamastash::daemon::discovery_task::DiscoveryOptions;
  use llamastash::discovery::scanner::{ScanOptions, ScanRoot};
  use llamastash::discovery::watcher::WatcherOptions;

  let state_dir = unique_temp_dir("e2e-state");
  let scan_root = unique_temp_dir("e2e-scan");
  std::fs::write(scan_root.join("alpha.gguf"), build_minimal_gguf("llama")).unwrap();
  std::fs::write(scan_root.join("bravo.gguf"), build_minimal_gguf("qwen3")).unwrap();

  let port = pick_free_port();
  let opts = DaemonOptions {
    discovery: DiscoveryOptions {
      scan_roots: vec![ScanRoot {
        path: scan_root.clone(),
        source: ModelSource::UserPath,
      }],
      scan: ScanOptions::default(),
      watcher: WatcherOptions {
        debounce: Duration::from_millis(75),
        periodic_rescan: Duration::from_secs(30),
        channel_capacity: 16,
      },
      lemonade_port: None,
      backend: Default::default(),
      backend_force: Default::default(),
    },
    proxy: ProxyConfig {
      enabled: true,
      port: Some(port),
      ..ProxyConfig::default()
    },
    ..DaemonOptions::rooted_at(state_dir.clone())
  };
  let socket = opts.state_dir.clone();
  let proxy_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  wait_for_socket(&socket).await;
  wait_for_proxy(proxy_addr).await;

  // Poll until both models surface — initial discovery races the
  // accept loop.
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  let body = loop {
    if std::time::Instant::now() > deadline {
      panic!("/v1/models never reported the two seeded GGUFs");
    }
    let (status, body) = http_get(proxy_addr, "/v1/models").await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).expect("json body");
    if v["data"].as_array().map(|a| a.len()).unwrap_or(0) >= 2 {
      break v;
    }
    sleep(Duration::from_millis(75)).await;
  };

  let data = body["data"].as_array().expect("array");
  assert_eq!(data.len(), 2);
  let ids: Vec<&str> = data.iter().map(|r| r["id"].as_str().unwrap()).collect();
  // file_stem() → "alpha" and "bravo"; alphabetical order is
  // already a → b.
  assert_eq!(ids, vec!["alpha", "bravo"]);

  // /health counts now reflect real values (R158 / R159):
  let (hs, hb) = http_get(proxy_addr, "/health").await;
  assert_eq!(hs, 200);
  let hv: Value = serde_json::from_slice(&hb).expect("json");
  assert_eq!(hv["models_discovered"], 2);
  assert_eq!(
    hv["models_loaded"], 0,
    "no supervisors launched in this test"
  );

  // Shutdown.
  let mut client = Client::connect(&socket).await.expect("connect daemon");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  let _ = timeout(Duration::from_secs(3), handle).await;
  std::fs::remove_dir_all(&state_dir).ok();
  std::fs::remove_dir_all(&scan_root).ok();
}
