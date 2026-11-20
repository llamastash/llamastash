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

fn listing_rw() -> String {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/listing_engine_rw.json")
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
  - name: rw
    binary: {bin}
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv]
    ready: /health
    memory_gib: 1
    rewrite_model: true
    rows_from:
      command: [{bin}, --print-listing, {listing_rw}]
      items: models
      name: model
      when: installed
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
    listing = listing(),
    listing_rw = listing_rw()
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

/// Boot a daemon, run `body` against it, and shut the daemon down even when
/// `body` panics. A panic that skipped the shutdown left the engines running,
/// and on Windows the runtime then blocked on their pipes until the nextest
/// timeout.
async fn with_daemon<F, Fut>(label: &str, proxy_port: Option<u16>, body: F)
where
  F: FnOnce(Client) -> Fut,
  Fut: std::future::Future<Output = ()> + Send + 'static,
{
  let state = unique_temp(label);
  let (client, daemon) = boot(opts(state.clone(), proxy_port)).await;
  let outcome = tokio::spawn(body(client)).await;
  if let Ok(mut c) = Client::connect(&state).await {
    let _ = c.call("shutdown", None).await;
  }
  let _ = daemon.await;
  if let Err(e) = outcome {
    std::panic::resume_unwind(e.into_panic());
  }
}

fn free_port() -> u16 {
  llamastash::test_support::allocate_port_range(1).start
}

/// The rows `entry` minted, as `list_models` publishes them.
async fn minted_rows(client: &mut Client, entry: &str) -> Vec<Value> {
  let prefix = format!("generic://{entry}/");
  let deadline = Instant::now() + Duration::from_secs(15);
  loop {
    let rows: Vec<Value> = client.call("list_models", None).await.unwrap()["models"]
      .as_array()
      .unwrap()
      .iter()
      .filter(|r| r["path"].as_str().is_some_and(|p| p.starts_with(&prefix)))
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
  with_daemon("mint", None, |mut client| async move {
    let rows = minted_rows(&mut client, "listed").await;
    let names: Vec<&str> = rows
      .iter()
      .map(|r| r["display_label"].as_str().unwrap_or(""))
      .collect();
    assert_eq!(names, ["listed:one", "listed:three"], "{rows:?}");
    for row in &rows {
      assert_eq!(row["backend"], "generic", "a minted row runs generically");
      assert_eq!(row["source"], "config");
    }
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_row_carries_the_item_s_own_context_and_size() {
  with_daemon("info", None, |mut client| async move {
    let rows = minted_rows(&mut client, "listed").await;
    let one = &rows[0]["metadata"];
    assert_eq!(one["native_ctx"], json!(32768), "{one}");
    assert_eq!(one["arch"], json!("fam-one"));
    assert_eq!(one["parameter_label"], json!("0.6B"));
    assert_eq!(one["quant"], json!("Q4_1"));
    assert_eq!(one["weights_bytes"], json!((0.7 * 1024f64.powi(3)) as u64));
    let three = &rows[1]["metadata"];
    assert_eq!(three["native_ctx"], json!(448), "{three}");
    assert_eq!(three["arch"], json!("fam-three"));
  })
  .await;
}

/// Start a model over IPC and wait until its launch reports ready.
async fn start_ready(client: &mut Client, params: Value) -> String {
  let resp = client
    .call("start_model", Some(params))
    .await
    .expect("start_model");
  let id = resp["launch_id"].as_str().unwrap().to_string();
  let deadline = Instant::now() + Duration::from_secs(30);
  loop {
    let status = client.call("status", None).await.unwrap();
    if status["models"]
      .as_array()
      .unwrap()
      .iter()
      .any(|m| m["launch_id"] == json!(id) && m["state"]["state"] == "ready")
    {
      return id;
    }
    assert!(
      Instant::now() < deadline,
      "launch `{id}` never came up: {status}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

/// The point of minting rows instead of aliasing one: `{name}` is the minted
/// tag, so the same entry launches whichever model the row names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_row_launches_with_its_tag_as_the_name() {
  with_daemon("launch", None, |mut client| async move {
    assert!(!minted_rows(&mut client, "listed").await.is_empty());
    let id = start_ready(
      &mut client,
      json!({"model": "listed:three", "model_path": "generic://listed/listed:three"}),
    )
    .await;
    let argv = log_lines(&mut client, &id).await;
    assert!(
      argv.iter().any(|l| l.contains("-m listed:three")),
      "`{{name}}` must be the minted tag: {argv:?}"
    );
  })
  .await;
}

/// A named launch qualifies the published id (`tag@coder`) so two launches of
/// one model stay addressable, but the engine only knows the bare tag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_minted_launch_still_sends_the_bare_tag() {
  with_daemon("named", None, |mut client| async move {
    let id = start_ready(
      &mut client,
      json!({
        "model": "listed:three",
        "model_path": "generic://listed/listed:three",
        "name": "coder",
      }),
    )
    .await;
    let argv = log_lines(&mut client, &id).await;
    assert!(
      argv.iter().any(|l| l.contains("-m listed:three "))
        && !argv.iter().any(|l| l.contains("@coder")),
      "the tag is sent bare, not `listed:three@coder`: {argv:?}"
    );
  })
  .await;
}

/// `rewrite_model` replaces `body.model` for an engine that cannot resolve the
/// catalog id. On a named minted launch the catalog id is `tag@coder`, so the
/// forwarded name has to be the engine's own tag or the engine cannot load it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewrite_model_on_a_named_minted_launch_forwards_the_tag() {
  let proxy_port = free_port();
  with_daemon("rw", Some(proxy_port), |mut client| async move {
    // The proxy resolves `solo:one` against the catalog, which an IPC start by
    // path does not wait for.
    assert!(!minted_rows(&mut client, "rw").await.is_empty());
    start_ready(
      &mut client,
      json!({"model": "solo:one", "model_path": "generic://rw/solo:one", "name": "coder"}),
    )
    .await;
    let resp = reqwest::Client::new()
      .post(format!("http://127.0.0.1:{proxy_port}/v1/chat/completions"))
      .json(&json!({
        "model": "solo:one",
        "messages": [{"role": "user", "content": "hi"}],
      }))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    let status = resp.status();
    // The fixture echoes the `model` it was sent, and replies as SSE.
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "status {status}: {text}");
    assert!(
      text.contains(r#""model":"solo:one""#),
      "the engine is told its own tag: {text}"
    );
    assert!(
      !text.contains("@coder"),
      "the published id leaked into the forwarded body: {text}"
    );
  })
  .await;
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
  with_daemon("broken", None, |mut client| async move {
    let rows = minted_rows(&mut client, "listed").await;
    assert!(!rows.is_empty(), "the working entry still mints");
    let all = client.call("list_models", None).await.unwrap().to_string();
    assert!(
      !all.contains("listed-broken/"),
      "a listing that cannot be read mints no rows: {all}"
    );
  })
  .await;
}

/// No `proxy.aliases` entry: the minted row's name is the engine's tag, and the
/// proxy resolves the request to it like any other row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_routes_a_minted_row_without_an_alias() {
  let proxy_port = free_port();
  with_daemon("proxy", Some(proxy_port), |mut client| async move {
    assert!(!minted_rows(&mut client, "listed").await.is_empty());

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
  })
  .await;
}
