//! Minimal stand-in for `llama-server` used by Unit 5's integration
//! tests. Hand-rolls just enough HTTP/1.1 over `tokio::TcpListener`
//! to answer `GET /health`, `GET /v1/models`, `POST
//! /v1/chat/completions` (streamed SSE), `POST /v1/embeddings`,
//! `POST /v1/rerank`, and the Anthropic `POST /v1/messages` +
//! `POST /v1/messages/count_tokens`. CI doesn't have a real
//! `llama-server`, so every supervisor-lifecycle test runs against
//! this binary.
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
//! - `--list-devices` (one-shot, mirrors real `llama-server`: prints a
//!   fake adapter table and exits *without* serving). The number of
//!   adapters comes from the `FAKE_LLAMA_DEVICES` env var, default `0`
//!   so the fixture stays inert for every test that doesn't opt in.
//!   Set `FAKE_LLAMA_DEVICES=2` to emulate a multi-GPU host and light
//!   up the daemon's launch-device catalog + the TUI's Multi-GPU
//!   placement knobs (`device` / `tensor_split` / `main_gpu` /
//!   `split_mode`); `1` is single-GPU, `0` is CPU-only. Selectors are
//!   `Vulkan<N>` so they parse on every platform. The daemon's catalog
//!   probe (`list_devices::probe`) runs exactly `<binary>
//!   --list-devices`, so this is all it needs.
//!
//! Output: when serving, this binary prints its bound address to
//! stdout as the first line `listening on 127.0.0.1:<port>` so tests
//! can wait for that signal if they want.

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
  /// `--fit-ctx <floor>` — when no `-c` pins the window, `/props`
  /// reports this as the resolved `n_ctx`, emulating fit honoring the
  /// floor so the daemon's post-launch actuals fetch has something real
  /// to read.
  fit_ctx: Option<u32>,
  mode: Mode,
  health_delay_ms: u64,
  trap_sigterm: bool,
  /// Set when launched with `--spec-type draft-mtp` (MTP speculative decoding).
  /// Makes the fixture print a `draft acceptance = …` slot-timing line to
  /// stderr at startup so the daemon's log ring buffer carries it, exactly like
  /// a real llama-server after generation — the fixture the status
  /// draft-acceptance scan asserts against.
  spec_mtp: bool,
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

/// Emit a fake `--list-devices` table and return. The adapter count
/// comes from `FAKE_LLAMA_DEVICES` (default `0`): `0` emulates a
/// CPU-only host, `1` a single-GPU host, `2`+ a multi-GPU host. The
/// `Vulkan<N>` selectors parse on every platform and feed the daemon's
/// device catalog, which the TUI's Multi-GPU placement knobs gate on
/// (`Server::physical_device_count > 1` — one compute family and a
/// distinct name per adapter, so N devices here are N cards). Memory
/// numbers are arbitrary but descend per adapter so the rows are
/// visibly distinct.
fn print_fake_devices() {
  let n: u32 = std::env::var("FAKE_LLAMA_DEVICES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0);
  // Mirror real llama-server's header so a future parser tweak that
  // keys off it keeps working; `parse_list_devices` skips it today.
  println!("Available devices:");
  for i in 0..n {
    let total = 16384u32.saturating_sub(i * 4096).max(2048);
    let free = total.saturating_sub(1024);
    println!("  Vulkan{i}: Fake GPU {i} ({total} MiB, {free} MiB free)");
  }
  use std::io::Write;
  let _ = std::io::stdout().flush();
}

fn parse_args() -> Args {
  let mut args = std::env::args().skip(1);
  let mut host = String::from("127.0.0.1");
  let mut port: u16 = 0;
  let mut model_path = String::from("/fixture/unknown.gguf");
  let mut ctx: Option<u32> = None;
  let mut fit_ctx: Option<u32> = None;
  let mut mode = Mode::Chat;
  let mut health_delay_ms: u64 = 0;
  let mut trap_sigterm = false;
  let mut spec_mtp = false;
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--host" => host = args.next().expect("--host needs value"),
      "--port" => {
        port = args.next().expect("--port value").parse().expect("u16");
      }
      "-m" => model_path = args.next().expect("-m value"),
      "-c" => ctx = args.next().and_then(|v| v.parse().ok()),
      "--fit-ctx" => fit_ctx = args.next().and_then(|v| v.parse().ok()),
      "--embeddings" => mode = Mode::Embedding,
      "--reranking" => mode = Mode::Rerank,
      "--spec-type" => spec_mtp = args.next().as_deref() == Some("draft-mtp"),
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
    fit_ctx,
    mode,
    health_delay_ms,
    trap_sigterm,
    spec_mtp,
  }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
  // `--list-devices` is a one-shot, non-serving invocation: real
  // llama-server prints its adapter table and exits. The daemon's
  // launch-device catalog probe runs exactly `<binary> --list-devices`,
  // so honour it here and exit before binding any socket. Count comes
  // from `FAKE_LLAMA_DEVICES` (default 0 → inert / CPU-only).
  if std::env::args().skip(1).any(|a| a == "--list-devices") {
    print_fake_devices();
    return;
  }

  let args = parse_args();

  if args.trap_sigterm {
    // Re-register the SIGTERM handler as a no-op so the supervisor's
    // SIGKILL-after-5s grace path is exercised. SIGINT still exits
    // the test process cleanly. Windows has no SIGTERM — supervisor
    // tests that exercise force-kill use CTRL+BREAK + TerminateJobObject,
    // so the trap flag is a no-op on Windows.
    #[cfg(unix)]
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

  // MTP: emit a slot draft-acceptance line to stderr (captured by the daemon's
  // log ring buffer) so the status draft-acceptance scan has a real line to
  // find — same shape a real llama-server prints after generation.
  if args.spec_mtp {
    eprintln!(
      "slot print_timing: id  0 | task 0 | draft acceptance = 0.75000 (  120 accepted /   160 generated ), mean len =  2.50"
    );
    let _ = std::io::stderr().flush();
  }

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
    // Stock llama-server serves its web UI at `/`; mirror that with a
    // tiny recognisable page so the proxy's `/ui/` forwarding tests can
    // assert the body came from the backend (and not from a llamastash
    // chooser / no-model page).
    ("GET", "/") | ("GET", "/index.html") => {
      write_response(
        &mut wr,
        200,
        "text/html; charset=utf-8",
        b"<!doctype html><title>fake-llama-webui</title><body>fake webui</body>",
      )
      .await?;
    }
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
    ("GET", "/props") => {
      // Mirror llama-server's `/props`: the resolved context window the
      // daemon reads back as post-launch actuals (R6). Reports `-c` when
      // pinned, else the `--fit-ctx` floor, else a server-ish default.
      let resolved = args.ctx.or(args.fit_ctx).unwrap_or(4096);
      let body = serde_json::json!({
        "default_generation_settings": { "n_ctx": resolved },
        "total_slots": 1,
      });
      write_response(
        &mut wr,
        200,
        "application/json",
        body.to_string().as_bytes(),
      )
      .await?;
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
      // Mirror real llama-server's OpenAI-compat behavior: echo
      // `body.model` into every emitted frame's `model` field so
      // proxy/router code paths can rely on the pass-through
      // contract without per-chunk rewriting. See the proxy plan
      // (docs/plans/2026-05-21-001-feat-proxy-router-plan.md)
      // Risks row "rewrites body.model" for the falsifying outcome.
      let echoed_model = extract_body_model(body_text).unwrap_or_else(|| args.model_path.clone());
      if want_fail {
        write_response(
          &mut wr,
          400,
          "application/json",
          b"{\"error\":\"injected 400\"}",
        )
        .await?;
      } else if want_malformed {
        let stream = format!(
          "event: noise\ndata: {{not json at all\n\n\
           event: message\ndata: {{\"model\":\"{m}\",\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n\
           event: done\ndata: {{\"model\":\"{m}\",\"choices\":[{{\"finish_reason\":\"stop\"}}]}}\n\n",
          m = echoed_model
        );
        write_response(&mut wr, 200, "text/event-stream", stream.as_bytes()).await?;
      } else {
        let stream = format!(
          "event: message\ndata: {{\"model\":\"{m}\",\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n\
           event: done\ndata: {{\"model\":\"{m}\",\"choices\":[{{\"finish_reason\":\"stop\"}}]}}\n\n",
          m = echoed_model
        );
        write_response(&mut wr, 200, "text/event-stream", stream.as_bytes()).await?;
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
    ("POST", "/v1/messages") => {
      // Anthropic Messages API. Real llama-server (b6961+) answers this
      // natively. Echo `body.model` into the response so a proxy forward
      // test can assert model resolution + byte-pure pass-through, same
      // contract as the chat-completions arm above.
      let body_text = std::str::from_utf8(&body).unwrap_or("");
      let echoed_model = extract_body_model(body_text).unwrap_or_else(|| args.model_path.clone());
      let body = serde_json::json!({
        "id": "msg_fake",
        "type": "message",
        "role": "assistant",
        "model": echoed_model,
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
      });
      write_response(
        &mut wr,
        200,
        "application/json",
        serde_json::to_vec(&body)?.as_slice(),
      )
      .await?;
    }
    ("POST", "/v1/messages/count_tokens") => {
      let body = serde_json::json!({ "input_tokens": 2 });
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

/// Cheap `body.model` extractor.
///
/// Returns `None` if the body isn't valid JSON or no `model` field is
/// present.
fn extract_body_model(body: &str) -> Option<String> {
  let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
  parsed.get("model")?.as_str().map(|s| s.to_string())
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
