//! Fixture-backed integration coverage for the ds4 (DwarfStar) backend,
//! driven through the production `run_foreground` daemon against the
//! `fake_ds4_server` fixture (which mirrors the real load-before-listen +
//! fixed-alias + no-`/health` contract).
//!
//! Covers the ds4 lifecycle a unit test can't reach: a ds4-compatible GGUF
//! auto-routes to ds4, spawns `ds4-server`, reaches Ready via the
//! `/v1/models` alias body (not the file path), reports the deepseek4
//! KV-blind advisory, and — with ds4 unavailable — falls back to llama.cpp
//! with no error.
//!
//! Built only under `--features test-fixtures`.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use llamastash::backend::BackendConfig;
use llamastash::config::{Ds4Config, PortRange};
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::gguf::test_fixtures::FixtureBuilder;
use llamastash::ipc::Client;
use serde_json::{json, Value};

fn fake_llama_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fake_ds4_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_ds4_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-ds4", label)
}

/// A synthetic **ds4-compatible** GGUF: `deepseek4` arch + the per-tensor
/// quant contract (routed experts IQ2_XXS, everything else Q8_0 / F16 / I32).
fn ds4_gguf_bytes() -> Vec<u8> {
  const IQ2_XXS: u32 = 16;
  const Q8_0: u32 = 8;
  const F16: u32 = 1;
  const I32: u32 = 26;
  FixtureBuilder::new()
    .with_arch("deepseek4")
    .with_context_length(8192)
    .with_tensor("blk.0.ffn_gate_exps.weight", &[512, 512], IQ2_XXS)
    .with_tensor("blk.0.ffn_down_exps.weight", &[512, 512], IQ2_XXS)
    .with_tensor("blk.0.ffn_gate_tid2eid.weight", &[512], I32)
    .with_tensor("blk.0.attn_q.weight", &[512, 512], Q8_0)
    .with_tensor("token_embd.weight", &[512, 512], F16)
    .build()
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(30);
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

/// Poll `status` until the single model row reaches `ready` / `error` or the
/// budget runs out. Returns the terminal row.
async fn wait_ready(client: &mut Client) -> Value {
  // The `status` model row nests its lifecycle under `state`:
  // `{ "state": { "state": "ready", "port": N, ... } }`.
  let deadline = std::time::Instant::now() + Duration::from_secs(20);
  loop {
    let status = client.call("status", None).await.expect("status");
    if let Some(row) = status
      .get("models")
      .and_then(|m| m.as_array())
      .and_then(|a| a.first())
    {
      let state = row
        .get("state")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("");
      if state == "ready" || state == "error" {
        return row.clone();
      }
    }
    if std::time::Instant::now() > deadline {
      let status = client.call("status", None).await.expect("status");
      panic!("model never settled; status={status}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

/// Read the nested `state.state` label from a `status` model row.
fn row_state(row: &Value) -> &str {
  row
    .get("state")
    .and_then(|s| s.get("state"))
    .and_then(Value::as_str)
    .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ds4_compatible_model_auto_routes_to_ds4_reaches_ready_and_stops() {
  let state = unique_temp("happy");
  let model_dir = unique_temp("happy-models");
  let model_path = model_dir.join("deepseek-v4-flash.gguf");
  std::fs::write(&model_path, ds4_gguf_bytes()).unwrap();

  let base = DaemonOptions::rooted_at(state.clone());
  let opts = DaemonOptions {
    binary: Some(fake_llama_binary()),
    port_range: allocate_port_range(),
    backend: BackendConfig {
      ds4: Ds4Config {
        enabled: Some(true),
        servers: vec![llamastash::backend::ServerConfig {
          binary: fake_ds4_binary(),
          name: None,
        }],
      },
      ..base.backend.clone()
    },
    ..base
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // A ds4-compatible model, no `--backend`: must auto-route to ds4.
  let start = client
    .call(
      "start_model",
      Some(json!({ "model_path": model_path.to_string_lossy() })),
    )
    .await
    .expect("start_model");

  // Reaches Ready via the `/v1/models` alias body probe.
  let row = wait_ready(&mut client).await;
  assert_eq!(
    row_state(&row),
    "ready",
    "ds4 model must reach Ready (row: {row})"
  );
  // The status row reports the honest resolved backend (F5): this launch
  // dispatched to ds4, so a running-row consumer keys on the real backend.
  assert_eq!(
    row.get("backend").and_then(Value::as_str),
    Some("ds4"),
    "running row must report the resolved ds4 backend (row: {row})"
  );

  // The running port answers `/v1/models` with the ds4 alias — proof the
  // ds4-server fixture (not the llama fixture) is behind the port.
  let port = start.get("port").and_then(Value::as_u64).expect("port") as u16;
  let body = reqwest::get(format!("http://127.0.0.1:{port}/v1/models"))
    .await
    .expect("GET /v1/models")
    .text()
    .await
    .unwrap();
  assert!(
    body.contains("deepseek-v4-flash"),
    "ds4 /v1/models must advertise the alias, got {body}"
  );

  // Stop cleanly (SIGTERM path — the fixture has no `/health`).
  let launch_id = start.get("launch_id").and_then(Value::as_str).unwrap();
  client
    .call("stop_model", Some(json!({ "launch_id": launch_id })))
    .await
    .expect("stop_model");

  client.call("shutdown", None).await.ok();
  let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ds4_compatible_model_falls_back_to_llamacpp_when_ds4_unavailable() {
  let state = unique_temp("fallback");
  let model_dir = unique_temp("fallback-models");
  let model_path = model_dir.join("deepseek-v4-flash.gguf");
  std::fs::write(&model_path, ds4_gguf_bytes()).unwrap();

  // ds4 disabled → the same compatible model must run on llama.cpp (the fake
  // llama server), never refuse.
  let base = DaemonOptions::rooted_at(state.clone());
  let opts = DaemonOptions {
    binary: Some(fake_llama_binary()),
    port_range: allocate_port_range(),
    backend: BackendConfig {
      ds4: Ds4Config {
        enabled: Some(false),
        servers: vec![llamastash::backend::ServerConfig {
          binary: fake_ds4_binary(),
          name: None,
        }],
      },
      ..base.backend.clone()
    },
    ..base
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  let start = client
    .call(
      "start_model",
      Some(json!({ "model_path": model_path.to_string_lossy() })),
    )
    .await
    .expect("start_model must succeed on llama.cpp fallback");
  let row = wait_ready(&mut client).await;
  assert_eq!(
    row_state(&row),
    "ready",
    "fallback to llama.cpp must reach Ready (row: {row})"
  );

  // The llama fixture answers `/health` (ds4 has none) — a quick way to prove
  // the fallback binary, not ds4-server, is behind the port.
  let port = start.get("port").and_then(Value::as_u64).expect("port") as u16;
  let health = reqwest::get(format!("http://127.0.0.1:{port}/health"))
    .await
    .expect("GET /health")
    .status();
  assert!(
    health.is_success(),
    "llama.cpp fallback must answer /health; got {health}"
  );

  let launch_id = start.get("launch_id").and_then(Value::as_str).unwrap();
  client
    .call("stop_model", Some(json!({ "launch_id": launch_id })))
    .await
    .ok();
  client.call("shutdown", None).await.ok();
  let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_pro_half_file_is_refused_pre_spawn() {
  let state = unique_temp("split");
  let model_dir = unique_temp("split-models");
  // A ds4-compatible GGUF named as one half of the PRO distributed pair.
  let model_path = model_dir.join("DeepSeek-V4-Pro-Q4K-Layers00-30.gguf");
  std::fs::write(&model_path, ds4_gguf_bytes()).unwrap();

  let base = DaemonOptions::rooted_at(state.clone());
  let opts = DaemonOptions {
    binary: Some(fake_llama_binary()),
    port_range: allocate_port_range(),
    backend: BackendConfig {
      ds4: Ds4Config {
        enabled: Some(true),
        servers: vec![llamastash::backend::ServerConfig {
          binary: fake_ds4_binary(),
          name: None,
        }],
      },
      ..base.backend.clone()
    },
    ..base
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // No `--backend` (Auto) → the split-half guard refuses pre-spawn, naming
  // distributed mode. This is the one surviving ds4 pre-spawn refusal.
  let err = client
    .call(
      "start_model",
      Some(json!({ "model_path": model_path.to_string_lossy() })),
    )
    .await
    .expect_err("split-half launch must be refused");
  let msg = format!("{err}");
  assert!(
    msg.contains("distributed") || msg.contains("half"),
    "refusal must name ds4 distributed mode; got {msg}"
  );

  client.call("shutdown", None).await.ok();
  let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}

/// Spawn the `fake_ds4_server` fixture directly (not via the daemon) so its
/// two headline behaviours — load-before-listen (`--load-delay-ms`) and a
/// configurable alias (`--alias`) — are actually exercised, together with the
/// readiness probe's `deepseek-v4-` prefix match (F6 forward-compat).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_tolerates_future_alias_and_load_before_listen() {
  use llamastash::daemon::probe::{poll_until_ready_model_id, ProbeOptions, ProbeOutcome};

  let port = {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
  };
  // Advertise a *future* variant id and delay the bind by 400 ms — the port
  // refuses connections during the "load" window, then answers with the
  // alias, exactly like the real ds4-server.
  let mut child = std::process::Command::new(fake_ds4_binary())
    .args([
      "--host",
      "127.0.0.1",
      "--port",
      &port.to_string(),
      "--alias",
      "deepseek-v4-turbo",
      "--load-delay-ms",
      "400",
    ])
    .spawn()
    .expect("spawn fake_ds4_server");

  // The prefix `deepseek-v4-` is what the ds4 readiness contract polls on.
  let opts = ProbeOptions {
    interval: Duration::from_millis(50),
    timeout: Duration::from_secs(10),
  };
  let outcome =
    poll_until_ready_model_id(port, opts, "/v1/models", 200, &["deepseek-v4-".to_string()]).await;
  assert_eq!(
    outcome,
    ProbeOutcome::Ready,
    "a `deepseek-v4-turbo` server behind a load delay must still reach Ready"
  );

  let _ = child.kill();
  let _ = child.wait();
}

/// A foreign server that grabbed the reserved port (wrong `/v1/models` id)
/// must never satisfy readiness — the alias-body gate keeps polling until the
/// timeout, so a bare 200 can't be mistaken for the real backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_rejects_foreign_alias_on_the_port() {
  use llamastash::daemon::probe::{poll_until_ready_model_id, ProbeOptions, ProbeOutcome};

  let port = {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
  };
  let mut child = std::process::Command::new(fake_ds4_binary())
    .args([
      "--host",
      "127.0.0.1",
      "--port",
      &port.to_string(),
      "--alias",
      "some-other-model",
    ])
    .spawn()
    .expect("spawn fake_ds4_server");

  let opts = ProbeOptions {
    interval: Duration::from_millis(40),
    timeout: Duration::from_millis(600),
  };
  let outcome =
    poll_until_ready_model_id(port, opts, "/v1/models", 200, &["deepseek-v4-".to_string()]).await;
  assert!(
    matches!(outcome, ProbeOutcome::Timeout { .. }),
    "a foreign 200 (non-ds4 alias) must not satisfy readiness; got {outcome:?}"
  );

  let _ = child.kill();
  let _ = child.wait();
}
