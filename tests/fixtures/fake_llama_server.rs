//! Minimal stand-in for `llama-server` used by Unit 5's integration
//! tests. Hand-rolls just enough HTTP/1.1 over `tokio::TcpListener`
//! to answer `GET /health`, `GET /v1/models`, `POST
//! /v1/chat/completions` (streamed SSE), `POST /v1/embeddings`, and
//! `POST /v1/rerank`. CI doesn't have a real `llama-server`, so
//! every supervisor-lifecycle test runs against this binary.
//!
//! Flags accepted (matching real `llama-server` enough for the
//! supervisor's argv to be portable):
//! - `--host <ADDR>` (default `127.0.0.1`)
//! - `--port <N>`
//! - `-m <PATH>` (recorded; the fixture echoes it back from
//!   `/v1/models` so tests can assert the right model is being run)
//! - `-c <N>` (recorded; no behaviour change)
//! - `--embeddings`, `--reranking` (records the mode)
//! - `--health-delay-ms <N>` (test-only — returns 503 until N ms
//!   after process start, then 200; lets tests exercise the
//!   Loading → Ready transition deterministically)
//! - `--trap-sigterm` (test-only — ignore SIGTERM so the supervisor's
//!   SIGKILL-after-5s path can be exercised)
//!
//! Output: this binary prints its bound address to stdout as the
//! first line `listening on 127.0.0.1:<port>` so tests can wait
//! for that signal if they want.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
struct Args {
  host: String,
  port: u16,
  model_path: String,
  ctx: Option<u32>,
  mode: Mode,
  health_delay_ms: u64,
  trap_sigterm: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Chat,
  Embedding,
  Rerank,
}

impl Mode {
  fn label(&self) -> &'static str {
    match self {
      Mode::Chat => "chat",
      Mode::Embedding => "embedding",
      Mode::Rerank => "rerank",
    }
  }
}

fn parse_args() -> Args {
  let mut args = std::env::args().skip(1);
  let mut host = String::from("127.0.0.1");
  let mut port: u16 = 0;
  let mut model_path = String::from("/fixture/unknown.gguf");
  let mut ctx: Option<u32> = None;
  let mut mode = Mode::Chat;
  let mut health_delay_ms: u64 = 0;
  let mut trap_sigterm = false;
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--host" => host = args.next().expect("--host needs value"),
      "--port" => {
        port = args.next().expect("--port value").parse().expect("u16");
      }
      "-m" => model_path = args.next().expect("-m value"),
      "-c" => ctx = args.next().and_then(|v| v.parse().ok()),
      "--embeddings" => mode = Mode::Embedding,
      "--reranking" => mode = Mode::Rerank,
      "--health-delay-ms" => {
        health_delay_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
      }
      "--trap-sigterm" => trap_sigterm = true,
      // Silently ignore unknown flags so the supervisor can pass
      // through reasoning bundles + advanced overrides without
      // teaching the fixture every llama-server flag.
      _ => {}
    }
  }
  Args {
    host,
    port,
    model_path,
    ctx,
    mode,
    health_delay_ms,
    trap_sigterm,
  }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
  let args = parse_args();

  if args.trap_sigterm {
    // Re-register the SIGTERM handler as a no-op so the supervisor's
    // SIGKILL-after-5s grace path is exercised. SIGINT still exits
    // the test process cleanly.
    tokio::spawn(async {
      let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install sigterm handler");
      while sig.recv().await.is_some() {
        eprintln!("fake-llama-server: ignoring SIGTERM (test mode)");
      }
    });
  }

  let bind_addr = format!("{}:{}", args.host, args.port);
  let listener = TcpListener::bind(&bind_addr).await.expect("bind");
  let bound = listener.local_addr().expect("local_addr");
  // First line on stdout — tests can poll for this.
  println!("listening on {bound}");
  // flush so the parent (which captures stdout via piped()) sees it
  // immediately rather than buffering.
  use std::io::Write;
  let _ = std::io::stdout().flush();

  let started_at = Arc::new(Instant::now());
  let args = Arc::new(args);
  loop {
    let (sock, _) = match listener.accept().await {
      Ok(v) => v,
      Err(e) => {
        eprintln!("accept error: {e}");
        return;
      }
    };
    let args = Arc::clone(&args);
    let started_at = Arc::clone(&started_at);
    tokio::spawn(async move {
      if let Err(e) = handle(sock, args, started_at).await {
        eprintln!("connection error: {e}");
      }
    });
  }
}

async fn handle(
  mut sock: TcpStream,
  args: Arc<Args>,
  started_at: Arc<Instant>,
) -> std::io::Result<()> {
  let (rd, mut wr) = sock.split();
  let mut br = BufReader::new(rd);
  let mut request_line = String::new();
  let read = br.read_line(&mut request_line).await?;
  if read == 0 {
    return Ok(());
  }
  let mut parts = request_line.split_whitespace();
  let method = parts.next().unwrap_or("").to_string();
  let path = parts.next().unwrap_or("").to_string();

  // Drain headers.
  let mut content_length: usize = 0;
  let mut headers: HashMap<String, String> = HashMap::new();
  loop {
    let mut line = String::new();
    if br.read_line(&mut line).await? == 0 {
      break;
    }
    if line == "\r\n" || line == "\n" {
      break;
    }
    if let Some((k, v)) = line.split_once(':') {
      let key = k.trim().to_ascii_lowercase();
      let value = v.trim().to_string();
      if key == "content-length" {
        content_length = value.parse().unwrap_or(0);
      }
      headers.insert(key, value);
    }
  }

  let mut body = vec![0u8; content_length];
  if content_length > 0 {
    br.read_exact(&mut body).await?;
  }

  match (method.as_str(), path.as_str()) {
    ("GET", "/health") => {
      let elapsed = started_at.elapsed().as_millis() as u64;
      if elapsed < args.health_delay_ms {
        write_response(
          &mut wr,
          503,
          "application/json",
          b"{\"status\":\"loading\"}",
        )
        .await?;
      } else {
        write_response(&mut wr, 200, "application/json", b"{\"status\":\"ok\"}").await?;
      }
    }
    ("GET", "/v1/models") => {
      let body = serde_json::json!({
        "object": "list",
        "data": [{
          "id": args.model_path,
          "object": "model",
          "owned_by": "fake-llama-server",
          "mode": args.mode.label(),
          "ctx": args.ctx,
        }],
      });
      write_response(
        &mut wr,
        200,
        "application/json",
        serde_json::to_vec(&body)?.as_slice(),
      )
      .await?;
    }
    ("POST", "/v1/chat/completions") => {
      // Test failure-injection knobs via query string:
      //   ?fail=400   → return a 400 with a JSON error body
      //   ?malformed-sse=1 → emit a bogus `data:` frame before the
      //                     valid one so the client must skip it
      //                     and still surface the good delta.
      // Both cases let the tui_chat_smoke_test exercise the error
      // paths in `oai_client::spawn_chat_stream`.
      let query = path
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
      // We accept either query-string flags (when the test client
      // can rewrite the URL) or magic marker strings in the
      // request body. The latter is convenient when the test
      // drives `spawn_chat_stream` which builds the URL itself.
      let body_text = std::str::from_utf8(&body).unwrap_or("");
      let want_fail = query.contains("fail=400") || body_text.contains("__TEST_INJECT_FAIL_400__");
      let want_malformed =
        query.contains("malformed-sse=1") || body_text.contains("__TEST_INJECT_MALFORMED_SSE__");
      if want_fail {
        write_response(
          &mut wr,
          400,
          "application/json",
          b"{\"error\":\"injected 400\"}",
        )
        .await?;
      } else if want_malformed {
        let stream = b"event: noise\ndata: {not json at all\n\n\
                       event: message\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                       event: done\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n";
        write_response(&mut wr, 200, "text/event-stream", stream).await?;
      } else {
        let stream = b"event: message\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                       event: done\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n";
        write_response(&mut wr, 200, "text/event-stream", stream).await?;
      }
    }
    ("POST", "/v1/embeddings") => {
      let body = serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "embedding": [0.1, 0.2, 0.3], "index": 0}],
      });
      write_response(
        &mut wr,
        200,
        "application/json",
        serde_json::to_vec(&body)?.as_slice(),
      )
      .await?;
    }
    ("POST", "/v1/rerank") => {
      let body = serde_json::json!({
        "results": [{"index": 0, "relevance_score": 0.42}],
      });
      write_response(
        &mut wr,
        200,
        "application/json",
        serde_json::to_vec(&body)?.as_slice(),
      )
      .await?;
    }
    _ => {
      write_response(
        &mut wr,
        404,
        "application/json",
        b"{\"error\":\"not found\"}",
      )
      .await?;
    }
  }
  let _ = wr.shutdown().await;
  Ok(())
}

async fn write_response<W>(wr: &mut W, status: u16, ctype: &str, body: &[u8]) -> std::io::Result<()>
where
  W: AsyncWriteExt + Unpin,
{
  let reason = match status {
    200 => "OK",
    404 => "Not Found",
    503 => "Service Unavailable",
    _ => "Status",
  };
  let header = format!(
    "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  );
  wr.write_all(header.as_bytes()).await?;
  wr.write_all(body).await?;
  Ok(())
}
