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
//! - `--metrics` (mirrors real `llama-server`: with it, `GET /metrics` serves a
//!   small Prometheus document; without it the endpoint answers `501` with a
//!   `not_supported_error` body, which is how a server started without the flag
//!   behaves. Two servers, one with the flag and one without, give a fan-out test
//!   a working target and a refusing one.)
//! - `--metrics-status <N>` (test-only — the status `GET /metrics` answers with,
//!   standing in for an engine that answers success-but-not-`200`.)
//! - `--metrics-vllm` (test-only — append a vLLM-shaped section to that body: a
//!   counter declared under one name and sampled under `<name>_total`, and a
//!   histogram sampled as `_bucket`/`_sum`/`_count`. llama.cpp prints sample
//!   names that match its own `# TYPE` lines, so only a second shape catches a
//!   merge that keys a metric on the sample line instead of the declared family.)
//! - `--health-delay-ms <N>` (test-only — returns 503 until N ms
//!   after process start, then 200; lets tests exercise the
//!   Loading → Ready transition deterministically)
//! - `--trap-sigterm` (test-only — ignore SIGTERM so the supervisor's
//!   SIGKILL-after-5s path can be exercised)
//! - `--sigterm-exit-delay-ms <N>` (test-only — on SIGTERM, exit N ms later,
//!   like an engine that drains before it stops)
//! - `--print-env <VAR>` (test-only, repeatable — print `env VAR=<value>` to
//!   stdout at start, so a test can read the child's environment from its log)
//! - `--print-argv` (test-only — print `argv <args…>` to stdout at start)
//! - `--print-listing <path>` (test-only, one-shot — print that file to stdout
//!   and exit, standing in for an engine's `list --json`)
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
  /// Set with `--metrics`; decides whether `GET /metrics` serves text or 501.
  metrics: bool,
  /// `--metrics-delay-ms`: milliseconds to stall before answering `GET /metrics`,
  /// standing in for an engine that is wedged or still loading.
  metrics_delay_ms: u64,
  /// `--metrics-pad`: bytes of comment filler to append to the `/metrics` body.
  metrics_pad: usize,
  /// `--metrics-cut`: make the `/metrics` body exactly this many bytes, ending
  /// it with a sample line, so a reader with a smaller cap stops mid-sample.
  metrics_cut: Option<usize>,
  /// `--metrics-status`: the status `GET /metrics` answers with, for an engine
  /// whose scrape succeeds with something other than `200`.
  metrics_status: Option<u16>,
  /// `--metrics-vllm`: also answer `GET /metrics` with a vLLM-shaped section,
  /// where the sample names are the declared family name plus a suffix.
  metrics_vllm: bool,
  health_delay_ms: u64,
  trap_sigterm: bool,
  // Only read under cfg(unix): Windows has no SIGTERM to delay.
  #[cfg_attr(not(unix), allow(dead_code))]
  sigterm_exit_delay_ms: Option<u64>,
  print_env: Vec<String>,
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

/// Pad `body` to exactly `len` bytes with one comment line of the width that
/// fits, so the caller controls where the body ends without adding anything a
/// parser would read.
fn pad_to(body: &mut String, len: usize) {
  let fill = len.saturating_sub(body.len());
  if fill < 3 {
    return;
  }
  body.push('#');
  body.push_str(&".".repeat(fill - 2));
  body.push('\n');
}

fn parse_args() -> Args {
  let mut args = std::env::args().skip(1);
  let mut host = String::from("127.0.0.1");
  let mut port: u16 = 0;
  let mut model_path = String::from("/fixture/unknown.gguf");
  let mut ctx: Option<u32> = None;
  let mut fit_ctx: Option<u32> = None;
  let mut mode = Mode::Chat;
  let mut metrics = false;
  let mut metrics_delay_ms: u64 = 0;
  let mut metrics_pad: usize = 0;
  let mut metrics_cut: Option<usize> = None;
  let mut metrics_status: Option<u16> = None;
  let mut metrics_vllm = false;
  let mut health_delay_ms: u64 = 0;
  let mut trap_sigterm = false;
  let mut sigterm_exit_delay_ms = None;
  let mut print_env = Vec::new();
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
      "--metrics" => metrics = true,
      "--metrics-delay-ms" => {
        metrics_delay_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(0)
      }
      "--metrics-pad" => metrics_pad = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
      "--metrics-cut" => metrics_cut = args.next().and_then(|v| v.parse().ok()),
      "--metrics-status" => {
        metrics_status = args.next().and_then(|v| v.parse().ok());
      }
      "--metrics-vllm" => metrics_vllm = true,
      "--spec-type" => spec_mtp = args.next().as_deref() == Some("draft-mtp"),
      "--health-delay-ms" => {
        health_delay_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
      }
      "--trap-sigterm" => trap_sigterm = true,
      "--sigterm-exit-delay-ms" => {
        sigterm_exit_delay_ms = args.next().and_then(|v| v.parse().ok());
      }
      "--print-env" => print_env.extend(args.next()),
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
    metrics,
    metrics_delay_ms,
    metrics_pad,
    metrics_cut,
    metrics_status,
    metrics_vllm,
    health_delay_ms,
    trap_sigterm,
    sigterm_exit_delay_ms,
    print_env,
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
  // `--print-listing <path>` is a one-shot stand-in for an engine that owns a
  // model store: it prints a JSON listing and exits, so a `rows_from` entry can
  // be tested without an engine. A missing file exits non-zero, like the real
  // thing when its binary or store is absent.
  let argv: Vec<String> = std::env::args().collect();
  if let Some(i) = argv.iter().position(|a| a == "--print-listing") {
    let Some(path) = argv.get(i + 1) else {
      eprintln!("fake-llama-server: --print-listing needs a path");
      std::process::exit(2);
    };
    match std::fs::read_to_string(path) {
      Ok(body) => {
        print!("{body}");
        return;
      }
      Err(e) => {
        eprintln!("fake-llama-server: cannot read {path}: {e}");
        std::process::exit(1);
      }
    }
  }

  if std::env::args().skip(1).any(|a| a == "--list-devices") {
    print_fake_devices();
    return;
  }

  // `--help` is the daemon's capability probe (`caps::LoadModeDialect::probe`),
  // and like `--list-devices` it is one-shot: real llama-server prints and
  // exits. Without this the fixture would fall through and *serve*, so the
  // probe would block until its timeout on every boot — which is exactly what
  // it did, timing out the IPC calls in `cli_integration_test`.
  //
  // Advertising `--load-mode` puts the fixture on the current dialect, matching
  // the llama.cpp the project builds against. `FAKE_LLAMA_LEGACY_LOAD_FLAGS=1`
  // makes it answer as a pre-2026-09-09 build instead, for a test that needs
  // the other branch.
  if std::env::args().skip(1).any(|a| a == "--help") {
    if std::env::var("FAKE_LLAMA_LEGACY_LOAD_FLAGS").as_deref() == Ok("1") {
      println!("      --mmap, --no-mmap                whether to memory-map model");
      println!("      --mlock                          force system to keep model in RAM");
    } else {
      println!("-lm,   --load-mode MODE                 model loading mode (default: auto)");
    }
    return;
  }

  let args = parse_args();

  if args.trap_sigterm {
    // Re-register the SIGTERM handler as a no-op so the supervisor's
    // SIGKILL-after-5s grace path is exercised. SIGINT still exits
    // the test process cleanly. Windows has no SIGTERM — supervisor
    // tests that exercise force-kill use CTRL+BREAK + TerminateJobObject,
    // so the trap flag is a no-op on Windows.
    //
    // The handler is installed here, not inside the spawned task: the task
    // may not run before `/health` answers, and a SIGTERM landing in that
    // window would kill the process with the default action.
    #[cfg(unix)]
    {
      let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install sigterm handler");
      tokio::spawn(async move {
        while sig.recv().await.is_some() {
          eprintln!("fake-llama-server: ignoring SIGTERM (test mode)");
        }
      });
    }
  }

  #[cfg(unix)]
  if let Some(ms) = args.sigterm_exit_delay_ms {
    // Installed before the spawn for the same reason as the trap above.
    let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .expect("install sigterm handler");
    tokio::spawn(async move {
      if sig.recv().await.is_some() {
        eprintln!("fake-llama-server: SIGTERM, exiting in {ms} ms");
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        std::process::exit(0);
      }
    });
  }
  if std::env::args().any(|a| a == "--print-argv") {
    let rest: Vec<String> = std::env::args().skip(1).collect();
    println!("argv {}", rest.join(" "));
  }
  for var in &args.print_env {
    println!("env {var}={}", std::env::var(var).unwrap_or_default());
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
    ("GET", "/metrics") => {
      // Real `llama-server` only serves `/metrics` when started with `--metrics`;
      // without the flag the endpoint answers 501 and points at the flag.
      if args.metrics {
        if args.metrics_delay_ms > 0 {
          std::thread::sleep(std::time::Duration::from_millis(args.metrics_delay_ms));
        }
        // The names and shapes are llama.cpp's: `<unit>_total` counters plus
        // gauges, one label-free sample each, `# HELP` then `# TYPE`.
        let mut body = String::from(
          "# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed.\n\
# TYPE llamacpp:prompt_tokens_total counter\n\
llamacpp:prompt_tokens_total 21\n\
# HELP llamacpp:predicted_tokens_total Number of predicted tokens.\n\
# TYPE llamacpp:predicted_tokens_total counter\n\
llamacpp:predicted_tokens_total{model_name=\"fake\"} 7\n\
# HELP llamacpp:requests_processing Number of requests currently being processed.\n\
# TYPE llamacpp:requests_processing gauge\n\
llamacpp:requests_processing 0\n",
        );
        if args.metrics_vllm {
          // vLLM declares a family and prints its samples under derived names —
          // the one shape llama.cpp does not, and the one a merge that keys on
          // the sample line's name gets wrong.
          body.push_str(
            "# HELP vllm:prompt_tokens Number of prompt tokens processed.\n\
# TYPE vllm:prompt_tokens counter\n\
vllm:prompt_tokens_total{model_name=\"fake\"} 21\n\
# HELP vllm:time_to_first_token_seconds Histogram of time to first token.\n\
# TYPE vllm:time_to_first_token_seconds histogram\n\
vllm:time_to_first_token_seconds_bucket{model_name=\"fake\",le=\"0.1\"} 1\n\
vllm:time_to_first_token_seconds_sum{model_name=\"fake\"} 0.05\n\
vllm:time_to_first_token_seconds_count{model_name=\"fake\"} 1\n",
          );
        }
        if args.metrics_pad > 0 {
          // Filler is a comment line: it parses as nothing and only shows up in
          // the size, which is what a scraper's cap is about.
          while body.len() < args.metrics_pad {
            body.push_str("# pad\n");
          }
        }
        if let Some(cut) = args.metrics_cut {
          // The body ends mid-sample exactly at `cut`, so a reader capped below
          // it is left holding half a sample: padding to just before the cut,
          // then a sample line that runs past it.
          let tail = "llamacpp:tail_series_total 987654\n";
          pad_to(&mut body, cut.saturating_sub(tail.len() - 3));
          body.push_str(tail);
          body.truncate(cut);
        }
        write_response(
          &mut wr,
          args.metrics_status.unwrap_or(200),
          "text/plain; version=0.0.4",
          body.as_bytes(),
        )
        .await?;
      } else {
        write_response(
          &mut wr,
          501,
          "application/json",
          b"{\"error\":{\"code\":\"not_supported_error\",\"message\":\"the metrics endpoint is disabled; start the server with --metrics\",\"type\":\"not_supported_error\"}}",
        )
        .await?;
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
      // Request-log knobs, all body markers:
      //   __TEST_EMIT_TIMINGS__    → a chunked stream that ends with a chunk
      //                              carrying `timings`, shaped like the
      //                              one real llama-server b11390 sends.
      //   __TEST_EMIT_USAGE__      → the same shape, but the last chunk carries
      //                              `usage` and no `timings`, so the proxy has
      //                              token counts and no server-reported speed
      //                              and has to estimate it.
      //   __TEST_SLOW_STREAM__     → send the first frame, wait, then the rest,
      //                              so a test can hang up mid-stream.
      //   __TEST_TRUNCATE_STREAM__ → promise more bytes than are sent, then
      //                              close: an upstream that died mid-body.
      let want_timings = body_text.contains("__TEST_EMIT_TIMINGS__");
      let want_usage = body_text.contains("__TEST_EMIT_USAGE__");
      let want_slow = body_text.contains("__TEST_SLOW_STREAM__");
      let want_truncated = body_text.contains("__TEST_TRUNCATE_STREAM__");
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
      } else if want_usage {
        let stream = format!(
          "data: {{\"choices\":[{{\"finish_reason\":null,\"index\":0,\"delta\":{{\"content\":\"hi\"}}}}],\"model\":\"{m}\",\"object\":\"chat.completion.chunk\"}}\n\n\
           data: {{\"choices\":[{{\"finish_reason\":\"stop\",\"index\":0,\"delta\":{{}}}}],\"model\":\"{m}\",\"object\":\"chat.completion.chunk\",\"usage\":{{\"prompt_tokens\":41,\"completion_tokens\":3,\"total_tokens\":44}}}}\n\n\
           data: [DONE]\n\n",
          m = echoed_model
        );
        write_chunked_stream(&mut wr, &stream).await?;
      } else if want_timings {
        let stream = format!(
          "data: {{\"choices\":[{{\"finish_reason\":null,\"index\":0,\"delta\":{{\"content\":\"hi\"}}}}],\"model\":\"{m}\",\"object\":\"chat.completion.chunk\"}}\n\n\
           data: {{\"choices\":[{{\"finish_reason\":\"stop\",\"index\":0,\"delta\":{{}}}}],\"model\":\"{m}\",\"object\":\"chat.completion.chunk\",\"timings\":{{\"cache_n\":40,\"prompt_n\":1,\"prompt_ms\":25.704,\"prompt_per_second\":38.9,\"predicted_n\":3,\"predicted_ms\":51.759,\"predicted_per_second\":38.64}}}}\n\n\
           data: [DONE]\n\n",
          m = echoed_model
        );
        // Chunked, the way real llama-server streams: the body ends with
        // the zero-length chunk, not at a promised length.
        write_chunked_stream(&mut wr, &stream).await?;
      } else if want_slow || want_truncated {
        let first = format!(
          "data: {{\"model\":\"{m}\",\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n",
          m = echoed_model
        );
        let rest = "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let header = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          first.len() + rest.len()
        );
        wr.write_all(header.as_bytes()).await?;
        wr.write_all(first.as_bytes()).await?;
        wr.flush().await?;
        if want_slow {
          tokio::time::sleep(std::time::Duration::from_secs(3)).await;
          wr.write_all(rest.as_bytes()).await?;
        }
        // `want_truncated` returns here with `rest` unsent; the caller
        // closes the socket short of the promised length.
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
      // contract as the chat-completions arm above. `received_body` carries
      // the request bytes verbatim, so a test can also assert what a backend
      // body rewrite actually put on the wire.
      let body_text = std::str::from_utf8(&body).unwrap_or("");
      let echoed_model = extract_body_model(body_text).unwrap_or_else(|| args.model_path.clone());
      let body = serde_json::json!({
        "id": "msg_fake",
        "type": "message",
        "role": "assistant",
        "model": echoed_model,
        "received_body": body_text,
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
    // The image, audio and decision-model endpoints. The reply names the model it was told and
    // echoes the request bytes, so a test can check both where the request was
    // routed and what a rewrite put on the wire — a multipart `model` field
    // included.
    ("POST", "/v1/images/generations")
    | ("POST", "/v1/images/edits")
    | ("POST", "/v1/images/variations")
    | ("POST", "/v1/audio/speech")
    | ("POST", "/v1/audio/transcriptions")
    | ("POST", "/v1/audio/translations")
    | ("POST", "/v1/systemone") => {
      let content_type = headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");
      let body_text = String::from_utf8_lossy(&body).to_string();
      let echoed_model = if llamastash::util::multipart_body::is_multipart(content_type) {
        llamastash::util::multipart_body::field_value(&body, content_type, "model")
          .and_then(|raw| std::str::from_utf8(raw).ok())
          .map(str::to_string)
      } else {
        extract_body_model(&body_text)
      }
      .unwrap_or_else(|| args.model_path.clone());
      let body = serde_json::json!({
        "model": echoed_model,
        "text": "hi",
        "received_body": body_text,
      });
      write_response(
        &mut wr,
        200,
        "application/json",
        serde_json::to_vec(&body)?.as_slice(),
      )
      .await?;
    }
    // Engine paths a generic entry can declare through `routes`: the reply names
    // the target the engine was asked for, query included, so a test can tell a
    // forwarded request from one the proxy answered itself and check the query
    // survived.
    ("GET" | "POST", p2)
      if p2 == "/inference" || p2.starts_with("/internal") || p2.starts_with("/rw") =>
    {
      let content_type = headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");
      let body_text = String::from_utf8_lossy(&body).to_string();
      let echoed_model = if llamastash::util::multipart_body::is_multipart(content_type) {
        llamastash::util::multipart_body::field_value(&body, content_type, "model")
          .and_then(|raw| std::str::from_utf8(raw).ok())
          .map(str::to_string)
      } else {
        extract_body_model(&body_text)
      };
      let body = serde_json::json!({
        "target": path,
        "model": echoed_model,
        "received_body": body_text,
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

/// Cheap `body.model` extractor.
///
/// Returns `None` if the body isn't valid JSON or no `model` field is
/// present.
fn extract_body_model(body: &str) -> Option<String> {
  let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
  parsed.get("model")?.as_str().map(|s| s.to_string())
}

/// A server-sent body sent as HTTP chunked transfer encoding, ending with the
/// zero-length chunk the way a real streaming server ends.
async fn write_chunked_stream<W>(wr: &mut W, body: &str) -> std::io::Result<()>
where
  W: AsyncWriteExt + Unpin,
{
  let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
  wr.write_all(head.as_bytes()).await?;
  for event in body.split_inclusive("\n\n") {
    wr.write_all(format!("{:x}\r\n{event}\r\n", event.len()).as_bytes())
      .await?;
  }
  wr.write_all(b"0\r\n\r\n").await?;
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
