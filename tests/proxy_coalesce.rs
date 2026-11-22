//! Single-flight coalescing for Unit 4's auto-start.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 4
//! Test scenarios — "two concurrent requests for the same dormant
//! model" and "three concurrent requests for three different
//! dormant models").
//!
//! Instrumentation: we observe single-flight by counting supervisor
//! registry entries after both requests complete. If coalescing
//! works, exactly one `compose_and_spawn` call ran — there is
//! exactly one supervisor for the path. Without coalescing, two
//! `compose_and_spawn` calls race on `reserve_port`; depending on
//! the port range, either one would error out (and the test fails
//! the 200 assertion) or both would succeed and the registry would
//! contain two entries on the same path. Either failure mode is
//! detected here.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use llamastash::config::loader::PortRange;
use llamastash::daemon::context::{LaunchEnv, MethodContext};
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use llamastash::test_support::{fake_metadata, shutdown_listener, spawn_listener, write_gguf};

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-pc", label)
}

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(30),
    timeout: Duration::from_secs(15),
  }
}

/// A range for `slots` simultaneous launches, plus headroom: the allocator
/// probes for a free port, and no assertion here needs the range tight.
fn allocate_port_range(slots: usize) -> PortRange {
  llamastash::test_support::allocate_port_range(slots + 16)
}

fn discovered(path: &Path, display_label: Option<&str>, arch: &str) -> DiscoveredModel {
  let parent = path.parent().expect("parent").to_path_buf();
  DiscoveredModel {
    path: path.to_path_buf(),
    parent,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata(arch)),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: display_label.map(str::to_string),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

async fn build_state(
  models: Vec<DiscoveredModel>,
  log_dir: &Path,
  port_range: PortRange,
) -> (Arc<ProxyState>, MethodContext) {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let token = ShutdownToken::new();
  let env = LaunchEnv {
    binary: Some(fake_binary()),
    port_range,
    log_dir: log_dir.to_path_buf(),
    probe: fast_probe(),
    arch_defaults: BTreeMap::new(),
    servers: Default::default(),
    default_launch_mode: Default::default(),
  };
  let ctx = MethodContext::with_catalog(token, catalog)
    .with_supervisors(SupervisorRegistry::new())
    .with_launch_env(env);
  let state = ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES);
  (state, ctx)
}

async fn http_post(addr: SocketAddr, path: &str, body: &str) -> u16 {
  let (status, _, _) = llamastash::test_support::http_post(addr, path, body, &[]).await;
  status
}

async fn stop_all(ctx: &MethodContext) {
  let snap = ctx.supervisors.snapshot().await;
  for (_, m) in snap {
    let _ = m.stop(Duration::from_secs(3)).await;
  }
}

// ---- Scenario 3: two concurrent requests for the same dormant model ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_requests_for_same_model_share_one_launch() {
  let dir = unique_temp("same");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let model_path = write_gguf(&dir, "qwen3.gguf", "qwen3");
  // Use a single-port range — if coalescing fails, the second
  // launch will error on port allocation and the test catches
  // either the failed 200 or the duplicate supervisor.
  let (state, ctx) = build_state(
    vec![discovered(&model_path, Some("qwen3"), "qwen3")],
    &log_dir,
    allocate_port_range(1),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let body = r#"{"model":"qwen3","messages":[]}"#.to_string();
  let body_a = body.clone();
  let body_b = body.clone();
  let (a, b) = tokio::join!(
    tokio::spawn(async move { http_post(addr, "/v1/chat/completions", &body_a).await }),
    tokio::spawn(async move { http_post(addr, "/v1/chat/completions", &body_b).await }),
  );
  let a = a.expect("join a");
  let b = b.expect("join b");
  assert_eq!(a, 200, "first request succeeds");
  assert_eq!(b, 200, "second request succeeds");

  let snap = ctx.supervisors.snapshot().await;
  assert_eq!(
    snap.len(),
    1,
    "single-flight: exactly one supervisor for the shared model id; got {}",
    snap.len()
  );

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// ---- Scenario 4: three concurrent requests for three different models -

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_concurrent_requests_for_distinct_models_each_launch() {
  let dir = unique_temp("distinct");
  let log_dir = dir.join("logs");
  std::fs::create_dir_all(&log_dir).unwrap();
  let a = write_gguf(&dir, "a.gguf", "llama");
  let b = write_gguf(&dir, "b.gguf", "qwen3");
  let c = write_gguf(&dir, "c.gguf", "bert");
  let (state, ctx) = build_state(
    vec![
      discovered(&a, Some("a"), "llama"),
      discovered(&b, Some("b"), "qwen3"),
      discovered(&c, Some("c"), "bert"),
    ],
    &log_dir,
    allocate_port_range(3),
  )
  .await;
  let (addr, shutdown, listener_handle) = spawn_listener(state).await;

  let r_a = tokio::spawn(async move {
    http_post(
      addr,
      "/v1/chat/completions",
      r#"{"model":"a","messages":[]}"#,
    )
    .await
  });
  let r_b = tokio::spawn(async move {
    http_post(
      addr,
      "/v1/chat/completions",
      r#"{"model":"b","messages":[]}"#,
    )
    .await
  });
  let r_c = tokio::spawn(async move {
    http_post(
      addr,
      "/v1/chat/completions",
      r#"{"model":"c","messages":[]}"#,
    )
    .await
  });
  let (sa, sb, sc) = tokio::try_join!(r_a, r_b, r_c).expect("all complete");
  assert_eq!(sa, 200);
  assert_eq!(sb, 200);
  assert_eq!(sc, 200);

  let snap = ctx.supervisors.snapshot().await;
  assert_eq!(snap.len(), 3, "three distinct supervisors launched");

  stop_all(&ctx).await;
  shutdown_listener(shutdown, listener_handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
