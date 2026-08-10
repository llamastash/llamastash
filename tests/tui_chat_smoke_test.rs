//! Right-pane Unit 7 smoke coverage.
//!
//! Verifies that the [`crate::tui::oai_client`] streaming chat path
//! drains its SSE channel against the `fake_llama_server` fixture
//! and that `embed` / `rerank` one-shots round-trip too. Skipped
//! unless the `test-fixtures` feature is on so the fake binary ships
//! only in test builds (matches the `start_model_ipc_test` posture).

#![cfg(feature = "test-fixtures")]

use std::path::PathBuf;
use std::time::Duration;

use llamastash::config::loader::PortRange;
use llamastash::daemon::probe::{poll_until_ready, ProbeOptions, ProbeOutcome};
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::ipc::Client;
use llamastash::tui::events::Event;
use llamastash::tui::oai_client::{embed, rerank, spawn_chat_stream, ChatStreamMsg};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Spawn the chat stream against a fresh `mpsc::Sender<Event>` and
/// return the receiver side. The integration tests below pre-date the
/// kdash-style unified event channel and used to get back a
/// `Receiver<ChatStreamMsg>` directly; this helper bridges them onto
/// the new `Event::ChatStream(...)` envelope without rewriting every
/// assertion.
fn spawn_chat_stream_for_test(port: u16, model: &str, prompt: &str) -> mpsc::Receiver<Event> {
  let (tx, rx) = mpsc::channel::<Event>(64);
  spawn_chat_stream(port, model.to_string(), prompt.to_string(), tx);
  rx
}

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-rp", label)
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
  llamastash::test_support::allocate_port_range(8)
}

/// Holds the daemon task + socket and, on drop, cleanly shuts the daemon down
/// so its `setsid`-detached `fake_llama_server` child doesn't outlive the test
/// as an init-owned orphan — the leak these smoke tests historically caused
/// (see the `allocate_port_range` note). Uses the shared sync-shutdown helper,
/// mirroring cli_integration_test's `DaemonHandle`.
struct ChatDaemon {
  socket: PathBuf,
  join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ChatDaemon {
  fn drop(&mut self) {
    if let Some(join) = self.join.take() {
      let _ = llamastash::test_support::sync_shutdown_daemon(&self.socket);
      // Let `run_foreground` finish `stop_all_managed` + remove runtime.json,
      // bounded to match the 5 s per-launch SIGTERM grace.
      let runtime = llamastash::daemon::runtime_file::path(&self.socket);
      let deadline = std::time::Instant::now() + Duration::from_secs(5);
      while runtime.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
      }
      join.abort();
    }
  }
}

async fn drive_to_ready_port() -> (u16, ChatDaemon) {
  let state = unique_temp("chat");
  let model_dir = unique_temp("chat-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();
  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
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
  match poll_until_ready(
    port,
    ProbeOptions {
      interval: Duration::from_millis(100),
      // 30 s headroom: the macOS GitHub runner periodically takes 15+
      // seconds to launch the fake binary under parallel test load,
      // and a short cap surfaces as flake without exercising anything
      // about the supervisor itself.
      timeout: Duration::from_secs(30),
    },
    "/health",
    200,
  )
  .await
  {
    ProbeOutcome::Ready => {}
    ProbeOutcome::Timeout { last_status } => {
      let mut detail = format!("supervisor never reached Ready on port {port}");
      if let Ok(mut status_client) = Client::connect(&socket).await {
        if let Ok(body) = status_client.call("status", None).await {
          if let Some(m) = body["models"]
            .as_array()
            .and_then(|models| models.iter().find(|m| m["launch_id"] == launch_id))
          {
            let state = m["state"]["state"].as_str().unwrap_or("unknown");
            let cause = m["state"]["cause"].as_str().unwrap_or("");
            if cause.is_empty() {
              detail.push_str(&format!(" (status={state}, last_health={last_status:?})"));
            } else {
              detail.push_str(&format!(
                " (status={state}, cause={cause}, last_health={last_status:?})"
              ));
            }
          }
        }
      }
      panic!("{detail}");
    }
  }
  (
    port,
    ChatDaemon {
      socket,
      join: Some(daemon),
    },
  )
}

// All five tests in this file are `#[cfg_attr(windows, ignore)]`.
// Each one drives a full daemon + supervisor + fake_llama_server +
// reqwest HTTP client per test. cargo test runs them in parallel,
// and on windows-latest the cumulative process/port/socket churn
// from four to five tests racing on a small runner consistently
// causes them to slip past cargo test's 60s "running too long"
// reporter and into hangs.
//
// They are TUI HTTP-client smoke tests — the supervisor surface they
// exercise is otherwise well-covered by supervisor_ipc_test,
// start_model_ipc_test, and supervisor_lifecycle_test (all of which
// run green on Windows after the Phase B fix). See TODO.md R2 for the
// follow-up to either serialize them via a process-wide mutex or
// restructure the binary so the chat HTTP client can be exercised
// without spinning up a full daemon stack per test.
#[cfg_attr(
  windows,
  ignore = "windows: parallel HTTP test resource contention — R2 follow-up"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_drains_deltas_and_signals_finished() {
  let (port, _daemon) = drive_to_ready_port().await;
  let mut rx = spawn_chat_stream_for_test(port, "m", "hello?");

  let mut collected = String::new();
  let mut finished = false;
  let deadline = Duration::from_secs(3);
  while let Ok(Some(Event::ChatStream(msg))) = timeout(deadline, rx.recv()).await {
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

#[cfg_attr(
  windows,
  ignore = "windows: parallel HTTP test resource contention — R2 follow-up"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embed_returns_dim_and_preview() {
  let (port, _daemon) = drive_to_ready_port().await;
  let result = embed(port, "m", "hello").await.expect("embed call");
  assert_eq!(result.dim, 3, "fake server returns a 3-d embedding");
  assert!(result.preview.len() <= 8);
  assert!(result.norm > 0.0);
}

#[cfg_attr(
  windows,
  ignore = "windows: parallel HTTP test resource contention — R2 follow-up"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_surfaces_http_4xx_as_error_message() {
  let (port, _daemon) = drive_to_ready_port().await;
  // The chat tab posts a JSON body containing the user's prompt; the
  // fake server treats the marker string as a request to return 400.
  let mut rx = spawn_chat_stream_for_test(port, "m", "__TEST_INJECT_FAIL_400__");
  let evt = timeout(Duration::from_secs(2), rx.recv())
    .await
    .expect("must receive an error within 2s")
    .expect("stream must yield at least one message");
  let Event::ChatStream(msg) = evt else {
    panic!("expected ChatStream event, got {evt:?}")
  };
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

#[cfg_attr(
  windows,
  ignore = "windows: parallel HTTP test resource contention — R2 follow-up"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_skips_malformed_sse_frame_and_emits_delta() {
  let (port, _daemon) = drive_to_ready_port().await;
  let mut rx = spawn_chat_stream_for_test(port, "m", "__TEST_INJECT_MALFORMED_SSE__");
  let mut saw_delta = false;
  let mut saw_finished = false;
  while let Some(Event::ChatStream(msg)) = timeout(Duration::from_secs(2), rx.recv())
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

#[cfg_attr(
  windows,
  ignore = "windows: parallel HTTP test resource contention — R2 follow-up"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerank_returns_sorted_scores() {
  let (port, _daemon) = drive_to_ready_port().await;
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
