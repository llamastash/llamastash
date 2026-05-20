//! Right-pane Unit 7 smoke coverage.
//!
//! Verifies that the [`crate::tui::oai_client`] streaming chat path
//! drains its SSE channel against the `fake_llama_server` fixture
//! and that `embed` / `rerank` one-shots round-trip too. Skipped
//! unless the `test-fixtures` feature is on so the fake binary ships
//! only in test builds (matches the `start_model_ipc_test` posture).

#![cfg(feature = "test-fixtures")]

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llamastash::config::loader::PortRange;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::ipc::Client;
use llamastash::tui::oai_client::{embed, rerank, spawn_chat_stream, ChatStreamMsg};
use serde_json::json;
use tokio::time::timeout;

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!(
    "llamastash-rp-{label}-{}-{nanos}",
    std::process::id()
  ));
  std::fs::create_dir_all(&p).expect("temp");
  p
}

async fn wait_for_socket(path: &std::path::Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon socket never appeared: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
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

async fn drive_to_ready_port() -> (u16, tokio::task::JoinHandle<()>, PathBuf) {
  let state = unique_temp("chat");
  let model_dir = unique_temp("chat-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = std::fs::canonicalize(&model_path).unwrap();
  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let daemon = tokio::spawn(async move {
    let _ = run_foreground(opts).await;
  });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");
  let start_body = client
    .call(
      "start_model",
      Some(json!({"model_path": &model_path_canon, "mode": "chat"})),
    )
    .await
    .expect("start_model");
  let launch_id = start_body["launch_id"]
    .as_str()
    .expect("launch_id")
    .to_string();
  let port = start_body["port"].as_u64().expect("port") as u16;
  // Wait briefly for Ready.
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    let body = client.call("status", None).await.expect("status");
    let models = body["models"].as_array().expect("models");
    if let Some(m) = models.iter().find(|m| m["launch_id"] == launch_id) {
      if m["state"]["state"] == json!("ready") {
        break;
      }
    }
    if std::time::Instant::now() > deadline {
      panic!("supervisor never reached Ready");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }
  (port, daemon, socket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_drains_deltas_and_signals_finished() {
  let (port, _daemon, _socket) = drive_to_ready_port().await;
  let mut rx = spawn_chat_stream(port, "m".into(), "hello?".into());

  let mut collected = String::new();
  let mut finished = false;
  let deadline = Duration::from_secs(3);
  while let Ok(Some(msg)) = timeout(deadline, rx.recv()).await {
    match msg {
      ChatStreamMsg::Delta(s) => collected.push_str(&s),
      ChatStreamMsg::Finished { .. } => {
        finished = true;
        break;
      }
      ChatStreamMsg::Error(e) => panic!("stream error: {e}"),
    }
  }
  assert!(finished, "stream must terminate with Finished");
  assert!(
    collected.contains("hi"),
    "fake server's `hi` delta must surface: got {collected:?}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embed_returns_dim_and_preview() {
  let (port, _daemon, _socket) = drive_to_ready_port().await;
  let result = embed(port, "m", "hello").await.expect("embed call");
  assert_eq!(result.dim, 3, "fake server returns a 3-d embedding");
  assert!(result.preview.len() <= 8);
  assert!(result.norm > 0.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_surfaces_http_4xx_as_error_message() {
  let (port, _daemon, _socket) = drive_to_ready_port().await;
  // The chat tab posts a JSON body containing the user's prompt; the
  // fake server treats the marker string as a request to return 400.
  let mut rx = spawn_chat_stream(
    port,
    "m".to_string(),
    "__TEST_INJECT_FAIL_400__".to_string(),
  );
  let msg = timeout(Duration::from_secs(2), rx.recv())
    .await
    .expect("must receive an error within 2s")
    .expect("stream must yield at least one message");
  match msg {
    ChatStreamMsg::Error(body) => {
      assert!(
        body.contains("400") || body.contains("injected"),
        "error must surface the 4xx body: got {body}"
      );
    }
    other => panic!("expected ChatStreamMsg::Error, got {other:?}"),
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_skips_malformed_sse_frame_and_emits_delta() {
  let (port, _daemon, _socket) = drive_to_ready_port().await;
  let mut rx = spawn_chat_stream(
    port,
    "m".to_string(),
    "__TEST_INJECT_MALFORMED_SSE__".to_string(),
  );
  let mut saw_delta = false;
  let mut saw_finished = false;
  while let Some(msg) = timeout(Duration::from_secs(2), rx.recv())
    .await
    .expect("timely")
  {
    match msg {
      ChatStreamMsg::Delta(d) => {
        if d == "hi" {
          saw_delta = true;
        }
      }
      ChatStreamMsg::Finished { .. } => {
        saw_finished = true;
        break;
      }
      ChatStreamMsg::Error(e) => panic!("malformed frame should be tolerated, got Error({e})"),
    }
  }
  assert!(saw_delta, "the good delta must arrive after the bad frame");
  assert!(saw_finished, "stream must terminate with Finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerank_returns_sorted_scores() {
  let (port, _daemon, _socket) = drive_to_ready_port().await;
  let ranked = rerank(port, "m", "query", &["doc one".into(), "doc two".into()])
    .await
    .expect("rerank call");
  assert!(!ranked.is_empty(), "fake server emits at least one result");
  // Scores must be monotone non-increasing — that's the contract
  // the rerank tab relies on when it picks the top candidate.
  for window in ranked.windows(2) {
    assert!(
      window[0].1 >= window[1].1,
      "rerank scores must be sorted descending: got {ranked:?}"
    );
  }
}
