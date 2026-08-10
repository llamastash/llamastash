//! Minimal stand-in for the real `vllm` launcher, used by the vLLM
//! integration tests. Hand-rolls just enough HTTP/1.1 over
//! `tokio::TcpListener` to answer what llamastash's readiness probe, orphan
//! sweep and proxy touch — and to reproduce the two vLLM-specific behaviours
//! that shaped the backend:
//!
//! - **slow engine init**: the real server profiles memory and builds the KV
//!   cache before it serves, 10-27 s on a 0.5B and longer on real models.
//!   `--load-delay-ms <n>` models that window. Two shapes are available so a
//!   test can cover either ordering: by default the listener stays unbound
//!   for `n` ms; with `--bind-early` it binds immediately but answers
//!   `/v1/models` with an **empty** list until the window closes. The second
//!   is the one a bare status check would fall for.
//! - **served-model-name**: `GET /v1/models` reports the `--served-model-name`
//!   value, never the model path. That is what readiness matches on, so
//!   `--served-model-name` lets a test stand up a *foreign* server (wrong id)
//!   on the reserved port and prove the probe rejects it.
//!
//! Argv mirrors the real invocation: `serve <model>`, `--served-model-name`,
//! `--host`, `--port`, `--max-model-len`, plus the native-knob flags, which
//! are accepted and ignored. A chat message containing `fail` returns 500,
//! matching `fake_llama_server`'s failure-injection marker.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main(flavor = "current_thread")]
async fn main() {
  let cfg = parse_args();
  let engine_ready = Arc::new(AtomicBool::new(cfg.load_delay_ms == 0));

  if cfg.bind_early {
    // Port is live immediately; the model list stays empty until the engine
    // "finishes". A status-only readiness probe passes here far too early.
    let flag = engine_ready.clone();
    let delay = cfg.load_delay_ms;
    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(delay)).await;
      flag.store(true, Ordering::SeqCst);
    });
  } else if cfg.load_delay_ms > 0 {
    tokio::time::sleep(Duration::from_millis(cfg.load_delay_ms)).await;
    engine_ready.store(true, Ordering::SeqCst);
  }

  let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))
    .await
    .expect("fake_vllm_server: bind");
  loop {
    let Ok((mut sock, _)) = listener.accept().await else {
      break;
    };
    let served = cfg.served_model_name.clone();
    let ready = engine_ready.clone();
    tokio::spawn(async move {
      let raw = read_request(&mut sock).await;
      route_and_reply(&mut sock, &raw, &served, ready.load(Ordering::SeqCst)).await;
    });
  }
}

struct Config {
  host: String,
  port: u16,
  served_model_name: String,
  load_delay_ms: u64,
  bind_early: bool,
}

fn parse_args() -> Config {
  let args: Vec<String> = env::args().collect();
  let mut host = "127.0.0.1".to_string();
  let mut port = 8000u16;
  // Mirrors the real default: absent `--served-model-name`, vLLM advertises
  // the model argument verbatim. llamastash always passes one precisely so
  // that a cache path never lands in `/v1/models`.
  let mut served_model_name = args.get(2).cloned().unwrap_or_default();
  let mut load_delay_ms = 0u64;
  let mut bind_early = false;
  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "--host" => {
        if let Some(v) = args.get(i + 1) {
          host = v.clone();
          i += 1;
        }
      }
      "--port" => {
        if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
          port = v;
          i += 1;
        }
      }
      "--served-model-name" => {
        if let Some(v) = args.get(i + 1) {
          served_model_name = v.clone();
          i += 1;
        }
      }
      "--load-delay-ms" => {
        if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
          load_delay_ms = v;
          i += 1;
        }
      }
      "--bind-early" => bind_early = true,
      // Every other flag the backend emits is accepted and ignored, the way a
      // real launcher tolerates its own tuning flags.
      _ => {}
    }
    i += 1;
  }
  Config {
    host,
    port,
    served_model_name,
    load_delay_ms,
    bind_early,
  }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
  let mut buf = vec![0u8; 16 * 1024];
  let mut raw = String::new();
  while let Ok(n) = sock.read(&mut buf).await {
    if n == 0 {
      break;
    }
    raw.push_str(&String::from_utf8_lossy(&buf[..n]));
    if let Some(head_end) = raw.find("\r\n\r\n") {
      let want = content_length(&raw[..head_end]).unwrap_or(0);
      if raw.len() >= head_end + 4 + want {
        break;
      }
    }
  }
  raw
}

fn content_length(head: &str) -> Option<usize> {
  head
    .lines()
    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
    .and_then(|l| l.split(':').nth(1))
    .and_then(|v| v.trim().parse().ok())
}

async fn route_and_reply(
  sock: &mut tokio::net::TcpStream,
  raw: &str,
  served: &str,
  engine_ready: bool,
) {
  let first = raw.lines().next().unwrap_or_default();
  let mut parts = first.split_whitespace();
  let method = parts.next().unwrap_or_default();
  let path = parts.next().unwrap_or_default();
  let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");

  let (status, payload) = match (method, path) {
    // vLLM answers /v1/models with an empty list until the engine is up, so
    // the model-id check is what actually gates readiness.
    ("GET", "/v1/models") if !engine_ready => (200, r#"{"object":"list","data":[]}"#.to_string()),
    ("GET", "/v1/models") => (
      200,
      format!(
        r#"{{"object":"list","data":[{{"id":"{served}","object":"model","created":0,"owned_by":"vllm","root":"{served}","max_model_len":2048}}]}}"#
      ),
    ),
    ("GET", "/health") if !engine_ready => (503, String::new()),
    ("GET", "/health") => (200, String::new()),
    ("GET", "/version") => (200, r#"{"version":"0.19.1-fake"}"#.to_string()),
    ("POST", "/v1/chat/completions") | ("POST", "/v1/completions") => {
      if !engine_ready {
        (503, r#"{"error":"engine still loading"}"#.to_string())
      } else if body.contains("fail") {
        (500, r#"{"error":"injected failure"}"#.to_string())
      } else {
        (
          200,
          format!(
            r#"{{"id":"cmpl-fake","object":"chat.completion","created":0,"model":"{served}","choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#
          ),
        )
      }
    }
    _ => (404, String::new()),
  };

  let reason = match status {
    200 => "OK",
    500 => "Internal Server Error",
    503 => "Service Unavailable",
    _ => "Not Found",
  };
  let resp = format!(
    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
    payload.len()
  );
  let _ = sock.write_all(resp.as_bytes()).await;
  let _ = sock.flush().await;
}
