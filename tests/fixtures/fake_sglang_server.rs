//! Minimal stand-in for the real `sglang` launcher, used by the SGLang
//! integration tests. Hand-rolls just enough HTTP/1.1 over
//! `tokio::TcpListener` to answer what llamastash's readiness probe, orphan
//! sweep, actuals fetch and proxy touch, and reproduces the behaviours that
//! shaped the backend:
//!
//! - **slow engine init**: weight load, CUDA graph capture and warmup keep
//!   the real server unready for a long window. `--load-delay-ms <n>` models
//!   it; with `--bind-early` the listener binds immediately but answers
//!   `/v1/models` with an **empty** list until the window closes, which a
//!   bare status check would fall for.
//! - **served-model-name**: `GET /v1/models` reports the `--served-model-name`
//!   value (one string on SGLang), never the model path.
//! - **`/get_server_info`**: echoes `context_length`, which is where the
//!   backend reads the resolved window from, and `max_total_tokens` so a test
//!   can see the guard's cap land.
//!
//! Argv mirrors the real invocation: `serve --model-path <dir>`,
//! `--served-model-name`, `--host`, `--port`, plus the native-knob flags,
//! which are accepted and ignored. The argv it was spawned with is written to
//! `<model-path>/fake_sglang_argv` (one token per line) so a test can assert
//! on what the daemon actually composed. A chat message containing `fail`
//! returns 500, matching `fake_llama_server`'s failure-injection marker.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Tuning flags the backend may legitimately emit — the native-knob table plus
/// the shared context flag. Kept in step with `src/backend/sglang/knobs.rs`; a
/// knob added there and not here fails loudly, which is the point.
const KNOWN_TUNING_FLAGS: &[&str] = &[
  "--context-length",
  "--mem-fraction-static",
  "--max-total-tokens",
  "--enable-unified-memory",
  "--max-running-requests",
  "--quantization",
  "--trust-remote-code",
  "--tool-call-parser",
  "--reasoning-parser",
  // Documented extras a test may pass through the `-- <extras>` tail.
  "--max-prefill-tokens",
  "--chunked-prefill-size",
  "--random-seed",
];

/// The subset above that takes no value.
const BOOL_TUNING_FLAGS: &[&str] = &["--enable-unified-memory", "--trust-remote-code"];

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main(flavor = "current_thread")]
async fn main() {
  let cfg = parse_args();
  if let Some(dir) = &cfg.model_path {
    let argv: Vec<String> = env::args().skip(1).collect();
    let _ = std::fs::write(
      std::path::Path::new(dir).join("fake_sglang_argv"),
      argv.join("\n"),
    );
  }
  let engine_ready = Arc::new(AtomicBool::new(cfg.load_delay_ms == 0));

  if cfg.bind_early {
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
    .expect("fake_sglang_server: bind");
  let cfg = Arc::new(cfg);
  loop {
    let Ok((mut sock, _)) = listener.accept().await else {
      break;
    };
    let cfg = cfg.clone();
    let ready = engine_ready.clone();
    tokio::spawn(async move {
      let raw = read_request(&mut sock).await;
      route_and_reply(&mut sock, &raw, &cfg, ready.load(Ordering::SeqCst)).await;
    });
  }
}

struct Config {
  host: String,
  port: u16,
  model_path: Option<String>,
  served_model_name: String,
  context_length: u64,
  max_total_tokens: Option<u64>,
  load_delay_ms: u64,
  bind_early: bool,
}

fn parse_args() -> Config {
  let args: Vec<String> = env::args().collect();
  let mut host = "127.0.0.1".to_string();
  let mut port = 30000u16;
  let mut model_path = None;
  let mut served_model_name = String::new();
  let mut context_length = 4096u64;
  let mut max_total_tokens = None;
  let mut load_delay_ms = 0u64;
  let mut bind_early = false;
  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "serve" => {}
      "--model-path" => {
        if let Some(v) = args.get(i + 1) {
          model_path = Some(v.clone());
          // Mirrors the real default: absent `--served-model-name`, SGLang
          // advertises the model argument verbatim.
          if served_model_name.is_empty() {
            served_model_name = v.clone();
          }
          i += 1;
        }
      }
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
      "--context-length" => {
        if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
          context_length = v;
          i += 1;
        }
      }
      "--max-total-tokens" => {
        if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
          max_total_tokens = Some(v);
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
      // A real launcher refuses an argv it cannot parse, and so must this: an
      // accept-everything fixture cannot detect a misspelled flag, a missing
      // value, or a dropped tail. Anything not in the tuning set is a
      // construction error and exits non-zero.
      other if other.starts_with('-') => {
        if !KNOWN_TUNING_FLAGS.contains(&other.split('=').next().unwrap_or(other)) {
          eprintln!("fake_sglang_server: unrecognised flag {other:?}");
          std::process::exit(2);
        }
        if !other.contains('=')
          && !BOOL_TUNING_FLAGS.contains(&other)
          && args.get(i + 1).is_some_and(|v| !v.starts_with('-'))
        {
          i += 1;
        }
      }
      other => {
        eprintln!("fake_sglang_server: unexpected positional {other:?}");
        std::process::exit(2);
      }
    }
    i += 1;
  }
  if model_path.is_none() {
    eprintln!("fake_sglang_server: --model-path is required");
    std::process::exit(2);
  }
  Config {
    host,
    port,
    model_path,
    served_model_name,
    context_length,
    max_total_tokens,
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
  cfg: &Config,
  engine_ready: bool,
) {
  let first = raw.lines().next().unwrap_or_default();
  let mut parts = first.split_whitespace();
  let method = parts.next().unwrap_or_default();
  let path = parts.next().unwrap_or_default();
  let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
  let served = &cfg.served_model_name;

  let (status, payload) = match (method, path) {
    ("GET", "/v1/models") if !engine_ready => (200, r#"{"object":"list","data":[]}"#.to_string()),
    ("GET", "/v1/models") => (
      200,
      format!(
        r#"{{"object":"list","data":[{{"id":"{served}","object":"model","created":0,"owned_by":"sglang","root":"{served}","max_model_len":{}}}]}}"#,
        cfg.context_length
      ),
    ),
    ("GET", "/health") if !engine_ready => (503, String::new()),
    ("GET", "/health") => (200, String::new()),
    ("GET", "/get_server_info") => (
      200,
      format!(
        r#"{{"served_model_name":"{served}","context_length":{},"max_total_tokens":{},"max_total_num_tokens":{},"version":"0.5.18-fake"}}"#,
        cfg.context_length,
        cfg
          .max_total_tokens
          .map_or("null".to_string(), |n| n.to_string()),
        cfg.max_total_tokens.unwrap_or(1_000_000),
      ),
    ),
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
