//! A `rows_from` entry mints its rows from the engine's own listing, so the
//! model store lives in one place. The `fake_llama_server` fixture stands in
//! for the engine in both roles: `--print-listing` is its model store, and the
//! same binary is the server each minted row launches.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use llamastash::backend::BackendConfig;
use llamastash::config::ProxyConfig;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::ipc::Client;
use serde_json::{json, Value};

fn fake() -> String {
  env!("CARGO_BIN_EXE_fake_llama_server").to_string()
}

fn listing() -> String {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/listing_engine.json")
    .to_string_lossy()
    .into_owned()
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-rows", label)
}

fn backend_config() -> BackendConfig {
  let yaml = format!(
    r#"
servers:
  - name: listed
    binary: {bin}
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv]
    ready: /health
    memory_gib: 1
    rows_from:
      command: [{bin}, --print-listing, {listing}]
      items: models
      name: model
      when: installed
      arch: details.family
      params: details.parameter_size
      quant: details.quantization_level
      ctx: default_context_length
      memory_gib: footprint
  - name: listed-broken
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    memory_gib: 1
    rows_from:
      command: [{bin}, --print-listing, /nonexistent/listing.json]
      name: model
"#,
    bin = fake(),
    listing = listing()
  );
  BackendConfig {
    generic: yaml_serde::from_str(&yaml).unwrap(),
    ..BackendConfig::default()
  }
}

fn opts(state: PathBuf, proxy_port: Option<u16>) -> DaemonOptions {
  let base = DaemonOptions::rooted_at(state);
  DaemonOptions {
    binary: Some(PathBuf::from(fake())),
    port_range: llamastash::test_support::allocate_port_range(8),
    metrics_interval: Duration::from_secs(60),
    backend: backend_config(),
    proxy: ProxyConfig {
      enabled: proxy_port.is_some(),
      port: proxy_port,
      ..ProxyConfig::default()
    },
    ..base
  }
}

async fn boot(
  opts: DaemonOptions,
) -> (
  Client,
  tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
) {
  let dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  let deadline = Instant::now() + Duration::from_secs(30);
  loop {
    if let Ok(c) = Client::connect(&dir).await {
      return (c, daemon);
    }
    assert!(Instant::now() < deadline, "daemon never came up");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn shutdown(
  mut client: Client,
  daemon: tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
) {
  let _ = client.call("shutdown", None).await;
  let _ = daemon.await;
}

/// The minted rows, by display name, as `list_models` publishes them.
async fn minted_rows(client: &mut Client) -> Vec<Value> {
  let deadline = Instant::now() + Duration::from_secs(15);
  loop {
    let rows: Vec<Value> = client.call("list_models", None).await.unwrap()["models"]
      .as_array()
      .unwrap()
      .iter()
      .filter(|r| {
        r["path"]
          .as_str()
          .is_some_and(|p| p.starts_with("generic://listed/"))
      })
      .cloned()
      .collect();
    if !rows.is_empty() || Instant::now() >= deadline {
      return rows;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_from_mints_one_row_per_reported_model() {
  let (mut client, daemon) = boot(opts(unique_temp("mint"), None)).await;
  let rows = minted_rows(&mut client).await;
  let names: Vec<&str> = rows
    .iter()
    .map(|r| r["display_label"].as_str().unwrap_or(""))
    .collect();
  assert_eq!(names, ["listed:one", "listed:three"], "{rows:?}");
  for row in &rows {
    assert_eq!(row["backend"], "generic", "a minted row runs generically");
    assert_eq!(row["source"], "config");
  }
  shutdown(client, daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_row_carries_the_item_s_own_context_and_size() {
  let (mut client, daemon) = boot(opts(unique_temp("info"), None)).await;
  let rows = minted_rows(&mut client).await;
  let one = &rows[0]["metadata"];
  assert_eq!(one["native_ctx"], json!(32768), "{one}");
  assert_eq!(one["arch"], json!("fam-one"));
  assert_eq!(one["parameter_label"], json!("0.6B"));
  assert_eq!(one["quant"], json!("Q4_1"));
  assert_eq!(one["weights_bytes"], json!((0.7 * 1024f64.powi(3)) as u64));
  let three = &rows[1]["metadata"];
  assert_eq!(three["native_ctx"], json!(448), "{three}");
  assert_eq!(three["arch"], json!("fam-three"));
  shutdown(client, daemon).await;
}

/// The point of minting rows instead of aliasing one: `{name}` is the minted
/// tag, so the same entry launches whichever model the row names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_row_launches_with_its_tag_as_the_name() {
  let (mut client, daemon) = boot(opts(unique_temp("launch"), None)).await;
  assert!(!minted_rows(&mut client).await.is_empty());
  let resp = client
    .call(
      "start_model",
      Some(json!({"model": "listed:three", "model_path": "generic://listed/listed:three"})),
    )
    .await
    .expect("start_model");
  let id = resp["launch_id"].as_str().unwrap();
  let deadline = Instant::now() + Duration::from_secs(30);
  loop {
    let status = client.call("status", None).await.unwrap();
    let row = status["models"]
      .as_array()
      .unwrap()
      .iter()
      .find(|m| m["launch_id"] == json!(id))
      .cloned()
      .unwrap_or(Value::Null);
    if row["state"]["state"] == "ready" {
      break;
    }
    assert!(Instant::now() < deadline, "minted row never came up: {row}");
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  let deadline = Instant::now() + Duration::from_secs(5);
  let argv = loop {
    let body = client
      .call("logs_tail", Some(json!({"launch_id": id, "lines": 200})))
      .await
      .unwrap();
    let lines: Vec<String> = body["lines"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|l| l.as_str())
      .filter(|l| l.contains("argv --port"))
      .map(str::to_string)
      .collect();
    if !lines.is_empty() || Instant::now() >= deadline {
      break (lines, body);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  };
  let (argv, body) = argv;
  assert!(
    argv.iter().any(|l| l.contains("-m listed:three")),
    "`{{name}}` must be the minted tag: {argv:?} / {body}"
  );
  shutdown(client, daemon).await;
}

/// A named launch qualifies the published id (`tag@coder`) so two launches of
/// one model stay addressable, but the engine only knows the bare tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_minted_launch_still_sends_the_bare_tag() {
  let (mut client, daemon) = boot(opts(unique_temp("named"), None)).await;
  assert!(!minted_rows(&mut client).await.is_empty());
  let resp = client
    .call(
      "start_model",
      Some(json!({
        "model": "listed:three",
        "model_path": "generic://listed/listed:three",
        "name": "coder",
      })),
    )
    .await
    .expect("start_model");
  let id = resp["launch_id"].as_str().unwrap();
  let deadline = Instant::now() + Duration::from_secs(30);
  let argv = loop {
    let status = client.call("status", None).await.unwrap();
    let ready = status["models"]
      .as_array()
      .unwrap()
      .iter()
      .any(|m| m["launch_id"] == json!(id) && m["state"]["state"] == "ready");
    if ready {
      break log_lines(&mut client, id).await;
    }
    assert!(
      Instant::now() < deadline,
      "named minted row never came up: {status}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  };
  assert!(
    argv.iter().any(|l| l.contains("-m listed:three ")),
    "the tag is sent bare, not `listed:three@coder`: {argv:?}"
  );
  shutdown(client, daemon).await;
}

/// The log tail of a launch, one helper for both launch tests.
async fn log_lines(client: &mut Client, launch_id: &str) -> Vec<String> {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let body = client
      .call(
        "logs_tail",
        Some(json!({"launch_id": launch_id, "lines": 200})),
      )
      .await
      .unwrap();
    let lines: Vec<String> = body["lines"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|l| l.as_str())
      .filter(|l| l.contains("argv --port"))
      .map(str::to_string)
      .collect();
    if !lines.is_empty() || Instant::now() >= deadline {
      return lines;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_listing_mints_nothing_and_keeps_the_rest() {
  let (mut client, daemon) = boot(opts(unique_temp("broken"), None)).await;
  let rows = minted_rows(&mut client).await;
  assert!(!rows.is_empty(), "the working entry still mints");
  let all = client.call("list_models", None).await.unwrap().to_string();
  assert!(
    !all.contains("listed-broken/"),
    "a listing that cannot be read mints no rows: {all}"
  );
  shutdown(client, daemon).await;
}

/// No `proxy.aliases` entry: the minted row's name is the engine's tag, and the
/// proxy resolves the request to it like any other row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_routes_a_minted_row_without_an_alias() {
  let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
    .unwrap()
    .local_addr()
    .unwrap()
    .port();
  let (mut client, daemon) = boot(opts(unique_temp("proxy"), Some(proxy_port))).await;
  assert!(!minted_rows(&mut client).await.is_empty());

  let http = reqwest::Client::new();
  let resp = http
    .post(format!("http://127.0.0.1:{proxy_port}/v1/chat/completions"))
    .json(&json!({
      "model": "listed:one",
      "messages": [{"role": "user", "content": "hi"}],
    }))
    .timeout(Duration::from_secs(60))
    .send()
    .await
    .expect("proxy request");
  assert!(resp.status().is_success(), "status {}", resp.status());

  let status = client.call("status", None).await.unwrap();
  assert!(
    status["models"]
      .as_array()
      .unwrap()
      .iter()
      .any(|m| m["id"]["path"] == "generic://listed/listed:one"),
    "auto-started the minted row: {status}"
  );
  shutdown(client, daemon).await;
}
