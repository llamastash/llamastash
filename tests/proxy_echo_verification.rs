//! Echo-verification of `llama-server`'s OpenAI-compat behavior.
//!
//! Pass-through forwarding (no body rewriting on fallback) depends on
//! the upstream echoing `request.body.model` into the response. This
//! test exercises the same property against the
//! `tests/fixtures/fake_llama_server.rs` test binary — which mirrors
//! real llama-server behavior — so the contract is locked in CI.
//!
//! If a future bump to a real llama-server reveals the response does
//! NOT echo `request.body.model`, fallback would have to JSON-parse /
//! per-chunk SSE rewrite instead of byte-piping.

#![cfg(feature = "test-fixtures")]

use std::{net::SocketAddr, time::Duration};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_llama_server_echoes_request_body_model() {
  let bin = env!("CARGO_BIN_EXE_fake_llama_server");
  let mut child = Command::new(bin)
    .args(["--host", "127.0.0.1", "--port", "0", "-m", "/sentinel.gguf"])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .expect("spawn fake_llama_server");
  let stdout = child.stdout.take().expect("stdout");
  let mut lines = BufReader::new(stdout).lines();
  let first = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
    .await
    .expect("fixture announces within 5s")
    .expect("read line")
    .expect("non-empty line");
  let bound: SocketAddr = first
    .strip_prefix("listening on ")
    .expect("first stdout line starts with `listening on `")
    .parse()
    .expect("parse fixture addr");

  let body = serde_json::json!({
    "model": "sentinel-x42",
    "messages": [{"role": "user", "content": "hi"}],
    "stream": true,
  })
  .to_string();
  let mut sock = TcpStream::connect(bound).await.expect("connect fixture");
  let req = format!(
    "POST /v1/chat/completions HTTP/1.1\r\nHost: {bound}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
    body.len(),
    body
  );
  sock.write_all(req.as_bytes()).await.expect("write");
  let mut response = Vec::new();
  sock.read_to_end(&mut response).await.expect("read");
  let _ = child.kill().await;

  let response_str = String::from_utf8_lossy(&response).to_string();
  // The fixture emits two `data:` frames per request — message +
  // done — both carrying the echo.
  let mut saw_echo = false;
  for line in response_str.lines() {
    let Some(payload) = line.strip_prefix("data: ") else {
      continue;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(payload) else {
      continue;
    };
    if parsed.get("model").and_then(|v| v.as_str()) == Some("sentinel-x42") {
      saw_echo = true;
      break;
    }
  }
  assert!(
    saw_echo,
    "expected response.model == \"sentinel-x42\" on at least one SSE frame; full response: {response_str}"
  );
}
