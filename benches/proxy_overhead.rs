//! Latency + throughput benchmark for the OpenAI-compat proxy.
//!
//! Three benchmark groups (`routing_decision`, `first_token`,
//! `throughput`), each with a direct and a proxied arm; the delta
//! between the arms is the proxy overhead.
//!
//! There is no instrumentation hook in the proxy: a `bench-hooks`
//! feature would force a new conditional on the hot path. Instead the
//! bench measures client-side round-trips, and treats "client-side
//! proxy roundtrip minus client-side direct roundtrip" as a
//! close-enough proxy of routing overhead at our load level.
//!
//! Two consecutive bench runs on the same machine should land within
//! ~10% of each other on p50 (criterion's `--quick` mode gives
//! noisier numbers; the maintainer's reference run uses the default
//! warm-up + sample budget). Outliers are reported by criterion's
//! HTML output.
//! Runbook: docs/runbooks/proxy-latency-bench.md.

#![cfg(feature = "test-fixtures")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::daemon::context::MethodContext;
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{spawn as supervisor_spawn, ManagedSpawn, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::identity::ModelId;
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::server::{loopback_addr, new_status_cell, serve, ProxyStatus, StatusCell};
use llamastash::proxy::state::ProxyState;
use tokio::runtime::Runtime;

// --- shared harness ------------------------------------------------------

/// Catalog + registry agree on this; the resolver hits an exact-name
/// match so there's no fuzzy-search noise in the numbers.
const BENCH_MODEL_NAME: &str = "bench-model";

/// Small + non-streaming so the routing-decision arm doesn't pay SSE
/// framing costs.
const BENCH_BODY_NONSTREAM: &str =
  r#"{"model":"bench-model","messages":[{"role":"user","content":"hi"}]}"#;
const BENCH_BODY_STREAM: &str =
  r#"{"model":"bench-model","messages":[{"role":"user","content":"hi"}],"stream":true}"#;

/// State the bench harness owns for the life of one criterion run.
struct Harness {
  proxy_addr: SocketAddr,
  upstream_addr: SocketAddr,
  /// Triggered on drop to tear down the proxy listener cleanly.
  shutdown: ShutdownToken,
  /// Keeps the supervisor alive for the bench lifetime; on drop we
  /// stop the fake child.
  supervisor: llamastash::daemon::supervisor::ManagedModel,
  /// Tokio runtime owning the proxy task. Bench inner loops run on
  /// a separate blocking client (std `TcpStream`) so they don't fight
  /// the runtime for scheduling.
  rt: Runtime,
  /// Temp dir holding the supervisor's log file. Cleared on drop.
  workdir: PathBuf,
}

impl Drop for Harness {
  fn drop(&mut self) {
    // Signal the proxy task to drain.
    self.shutdown.trigger();
    // We block on the runtime for the stop call so drop is
    // synchronous from the bench's perspective.
    let sup = self.supervisor.clone();
    let _ = self
      .rt
      .block_on(async move { sup.stop(Duration::from_secs(3)).await });
    let _ = std::fs::remove_dir_all(&self.workdir);
  }
}

/// Construct the harness. Spawns the fake_llama_server through the
/// real supervisor (matching how `tests/proxy_routing.rs` does it),
/// registers it with a `ProxyState`, and stands up the proxy
/// listener on an ephemeral port.
fn build_harness() -> Harness {
  let rt = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(2)
    .enable_all()
    .build()
    .expect("tokio runtime");

  let workdir = unique_temp("bench");
  let catalog_path = workdir
    .join("bench-model.gguf")
    .to_string_lossy()
    .into_owned();

  // Spawn the fake server + register + spin up the proxy on one
  // runtime task so we can `block_on` once and get all three handles
  // back to the bench harness.
  let (proxy_addr, upstream_addr, shutdown, supervisor) = rt.block_on(async {
    // The supervisor probes /health and transitions to Ready before
    // returning.
    let upstream_port = pick_free_port();
    let model_id = ModelId {
      path: PathBuf::from(&catalog_path),
      header_blake3: [0u8; 32],
    };
    let params = LaunchParams::new(PathBuf::from(&catalog_path), LaunchMode::Chat);
    let plan = LlamaCppBackend::new().process_spec(
      &params,
      upstream_port,
      PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server")),
      ProbeOptions {
        interval: Duration::from_millis(20),
        timeout: Duration::from_secs(5),
      },
    );
    let supervisor = supervisor_spawn(ManagedSpawn {
      id: model_id.clone(),
      params,
      port: upstream_port,
      mode: LaunchMode::Chat,
      log_path: workdir.join("fake.log"),
      plan,
      origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
      fit_gate: None,
      resolved_backend: "llamacpp".to_string(),
    })
    .await
    .expect("spawn fake_llama_server");
    wait_for_ready(&supervisor).await;

    let catalog = ModelCatalog::new();
    catalog
      .upsert(discovered(&catalog_path, BENCH_MODEL_NAME, "qwen3"))
      .await;
    let registry = SupervisorRegistry::new();
    let launch_id = registry.next_id();
    registry.insert(launch_id, supervisor.clone()).await;
    let ctx = MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(registry);
    let state = ProxyState::from_context(
      &ctx,
      false,
      true,
      llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES,
    );

    let shutdown = ShutdownToken::new();
    let status: StatusCell = new_status_cell();
    let bind_addr = loopback_addr(0);
    let shutdown_for_task = shutdown.clone();
    let status_for_task = Arc::clone(&status);
    tokio::spawn(async move {
      serve(state, bind_addr, shutdown_for_task, status_for_task)
        .await
        .expect("proxy serve returns Ok");
    });
    let proxy_addr = wait_for_listening(&status, Duration::from_secs(2))
      .await
      .expect("proxy must reach Listening");

    let upstream_addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, upstream_port));
    (proxy_addr, upstream_addr, shutdown, supervisor)
  });

  Harness {
    proxy_addr,
    upstream_addr,
    shutdown,
    supervisor,
    rt,
    workdir,
  }
}

async fn wait_for_ready(model: &llamastash::daemon::supervisor::ManagedModel) {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    if matches!(model.state().await, ManagedState::Ready) {
      return;
    }
    if Instant::now() > deadline {
      panic!("fake_llama_server never reached Ready");
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn wait_for_listening(status: &StatusCell, budget: Duration) -> Option<SocketAddr> {
  let deadline = Instant::now() + budget;
  while Instant::now() < deadline {
    if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
      return Some(addr);
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  None
}

fn unique_temp(label: &str) -> PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!(
    "llamastash-bench-{label}-{}-{nanos}",
    std::process::id()
  ));
  std::fs::create_dir_all(&p).expect("temp");
  p
}

fn pick_free_port() -> u16 {
  std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
    .expect("bind ephemeral")
    .local_addr()
    .expect("local_addr")
    .port()
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

fn discovered(path: &str, display_label: &str, arch: &str) -> DiscoveredModel {
  let p = PathBuf::from(path);
  let parent = p
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap_or_else(|| PathBuf::from("/"));
  DiscoveredModel {
    path: p,
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata(arch)),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: Some(display_label.to_string()),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

// --- HTTP client primitives used by the bench inner loops ---------------

/// Send `POST <path>` with `body`, read the entire response.
/// Returns elapsed wall-clock time + response byte count.
fn post_and_drain(addr: SocketAddr, path: &str, body: &str) -> (Duration, usize) {
  let mut sock = TcpStream::connect(addr).expect("connect");
  sock
    .set_nodelay(true)
    .expect("nodelay (loopback should always honor this)");
  let req = build_request(addr, path, body);
  let start = Instant::now();
  sock.write_all(req.as_bytes()).expect("write");
  let mut buf = Vec::with_capacity(2048);
  let mut tmp = [0u8; 4096];
  loop {
    match sock.read(&mut tmp) {
      Ok(0) => break,
      Ok(n) => buf.extend_from_slice(&tmp[..n]),
      Err(e) => panic!("read: {e}"),
    }
  }
  let elapsed = start.elapsed();
  (elapsed, buf.len())
}

/// Variant of [`post_and_drain`] that returns once the first byte of
/// the response has been read. Used for the first-token axis.
fn post_first_byte(addr: SocketAddr, path: &str, body: &str) -> Duration {
  let mut sock = TcpStream::connect(addr).expect("connect");
  sock.set_nodelay(true).expect("nodelay");
  let req = build_request(addr, path, body);
  let start = Instant::now();
  sock.write_all(req.as_bytes()).expect("write");
  let mut tmp = [0u8; 1];
  // `read_exact` of one byte blocks until the server flushes the
  // first header byte. On loopback this is essentially the moment
  // hyper writes `H` of `HTTP/1.1`. Both arms see the same shape so
  // the constant `H`-then-rest framing cancels in the direct/proxied
  // delta.
  sock.read_exact(&mut tmp).expect("first byte");
  let first = start.elapsed();
  // Drain the rest so the connection closes cleanly (the fake server
  // emits Connection: close anyway).
  let mut sink = [0u8; 4096];
  while matches!(sock.read(&mut sink), Ok(n) if n > 0) {}
  first
}

fn build_request(addr: SocketAddr, path: &str, body: &str) -> String {
  format!(
    "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
    body.len()
  )
}

// --- criterion benches ---------------------------------------------------

fn bench_routing_decision(c: &mut Criterion) {
  // The direct/proxied delta is the proxy's added work
  // (resolve_model + supervisor lookup + reqwest forward).
  let harness = build_harness();
  let mut group = c.benchmark_group("routing_decision");
  // Loopback is fast; cap measurement at ~5s to keep `cargo bench`
  // tractable without sacrificing convergence.
  group.measurement_time(Duration::from_secs(5));

  let proxy_addr = harness.proxy_addr;
  let upstream_addr = harness.upstream_addr;

  group.bench_function("direct", |b| {
    b.iter(|| {
      let (_d, n) = post_and_drain(upstream_addr, "/v1/chat/completions", BENCH_BODY_NONSTREAM);
      assert!(n > 0, "non-empty response");
    });
  });
  group.bench_function("proxied", |b| {
    b.iter(|| {
      let (_d, n) = post_and_drain(proxy_addr, "/v1/chat/completions", BENCH_BODY_NONSTREAM);
      assert!(n > 0, "non-empty response");
    });
  });
  group.finish();
}

fn bench_first_token(c: &mut Criterion) {
  // Both arms send `stream: true`; the fake emits SSE frames so the
  // first byte lands as soon as the status line is flushed. The <5%
  // streaming first-token overhead target lives here.
  let harness = build_harness();
  let mut group = c.benchmark_group("first_token");
  group.measurement_time(Duration::from_secs(5));

  let proxy_addr = harness.proxy_addr;
  let upstream_addr = harness.upstream_addr;

  group.bench_function("direct", |b| {
    b.iter(|| {
      let _d = post_first_byte(upstream_addr, "/v1/chat/completions", BENCH_BODY_STREAM);
    });
  });
  group.bench_function("proxied", |b| {
    b.iter(|| {
      let _d = post_first_byte(proxy_addr, "/v1/chat/completions", BENCH_BODY_STREAM);
    });
  });
  group.finish();
}

fn bench_throughput(c: &mut Criterion) {
  // The <2% throughput overhead target lives here.
  //
  // We measure one full request per iteration — the fake server's
  // canonical SSE response is ~200 bytes, so this is genuinely a
  // bytes-per-second number rather than "round-trips per second".
  let harness = build_harness();
  let mut group = c.benchmark_group("throughput");
  group.measurement_time(Duration::from_secs(5));

  let proxy_addr = harness.proxy_addr;
  let upstream_addr = harness.upstream_addr;

  // Measure one direct round-trip up front to size criterion's
  // Throughput counter. Both arms hit the same upstream so the size
  // is identical.
  let (_d, payload_bytes) =
    post_and_drain(upstream_addr, "/v1/chat/completions", BENCH_BODY_STREAM);
  assert!(payload_bytes > 0, "fake emitted a non-empty SSE stream");
  group.throughput(Throughput::Bytes(payload_bytes as u64));

  group.bench_function("direct", |b| {
    b.iter(|| {
      let (_d, n) = post_and_drain(upstream_addr, "/v1/chat/completions", BENCH_BODY_STREAM);
      assert!(n > 0);
    });
  });
  group.bench_function("proxied", |b| {
    b.iter(|| {
      let (_d, n) = post_and_drain(proxy_addr, "/v1/chat/completions", BENCH_BODY_STREAM);
      assert!(n > 0);
    });
  });
  group.finish();
}

criterion_group!(
  benches,
  bench_routing_decision,
  bench_first_token,
  bench_throughput
);
criterion_main!(benches);
