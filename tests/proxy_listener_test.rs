//! Daemon-level integration tests for the OpenAI-compat proxy listener.
//!
//! Inline unit tests under `src/proxy/server.rs` cover the
//! `/health` / 501 / keep-alive / port-in-use surface in isolation.
//! These integration tests exercise the same scenarios with the
//! full `run_foreground` daemon up so config wiring + the spawn
//! ordering in `src/daemon/mod.rs` are exercised end-to-end.

#![cfg(feature = "test-fixtures")]

use std::{
  net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
  path::PathBuf,
  time::Duration,
};

use llamastash::config::loader::ProxyConfig;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::ipc::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-px", label)
}

/// Pick a free loopback port by binding-and-dropping. There's still
/// a TOCTOU window between drop and the daemon's bind, but the
/// tests run on ephemeral kernel-assigned ports so contention is
/// vanishingly unlikely.
fn pick_free_port() -> u16 {
  let l = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral");
  l.local_addr().expect("local_addr").port()
}

async fn wait_for_socket(path: &std::path::Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  loop {
    if std::time::Instant::now() > deadline {
      panic!(
        "daemon did not become connectable within 10s: {}",
        path.display()
      );
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    sleep(Duration::from_millis(20)).await;
  }
}

/// Resolve the proxy's actual bound address via the daemon's IPC
/// `status` surface. The address may differ from the one the test
/// configured: there's a TOCTOU window between `pick_free_port`
/// (which drops its listener) and the daemon's bind, and the
/// listener will scan `port..=port+5` looking for a free slot if the
/// originally-chosen port has since been claimed by another process.
async fn wait_for_proxy(socket_path: &std::path::Path) -> SocketAddr {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("proxy did not reach 'listening' status within 10s");
    }
    if let Ok(mut client) = Client::connect(socket_path).await {
      if let Ok(resp) = client.call("status", None).await {
        if let Some(proxy) = resp.get("proxy") {
          if proxy.get("status").and_then(|v| v.as_str()) == Some("listening") {
            if let Some(listen) = proxy.get("listen").and_then(|v| v.as_str()) {
              return listen.parse().expect("parse listen addr");
            }
          }
        }
      }
    }
    sleep(Duration::from_millis(20)).await;
  }
}

/// One bare-socket request against the daemon's own proxy, `(status, body)`.
async fn http_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
  let (status, _, body) = llamastash::test_support::http_get(addr, path, &[]).await;
  (status, body)
}

async fn http_post(addr: SocketAddr, path: &str, body: &str) -> (u16, Vec<u8>) {
  let (status, _, body) = llamastash::test_support::http_post(addr, path, body, &[]).await;
  (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_starts_with_proxy_enabled_and_health_returns_ok() {
  let dir = unique_temp_dir("health-shape");
  let mut opts = DaemonOptions::rooted_at(dir.clone());
  let port = pick_free_port();
  opts.proxy = ProxyConfig {
    enabled: true,
    port: Some(port),
    ..ProxyConfig::default()
  };
  let socket_path = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  wait_for_socket(&socket_path).await;
  let proxy_addr = wait_for_proxy(&socket_path).await;

  let (status, body) = http_get(proxy_addr, "/health").await;
  assert_eq!(status, 200);
  let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(parsed["status"], "ok", "body shape: {parsed}");
  assert!(parsed["models_loaded"].is_u64(), "models_loaded shape");
  assert!(
    parsed["models_discovered"].is_u64(),
    "models_discovered shape"
  );

  // A body without `model` short-circuits at `RouteDecision::ModelRequired`.
  let (s2, b2) = http_post(proxy_addr, "/v1/chat/completions", "{}").await;
  assert_eq!(s2, 400);
  let parsed2: serde_json::Value = serde_json::from_slice(&b2).expect("json2");
  assert_eq!(parsed2["error"]["type"], "invalid_request");
  assert_eq!(parsed2["error"]["code"], "model_required");
  assert_eq!(parsed2["error"]["param"], "model");

  // `proxy.request_log_file` is off by default: no file, and `status` says so.
  let mut client = Client::connect(&socket_path).await.expect("connect daemon");
  let status_body = client.call("status", None).await.expect("status");
  assert_eq!(
    status_body["proxy"]["request_log_file"],
    serde_json::Value::Null
  );
  assert!(!dir.join("logs").join("requests.jsonl").exists());

  // Shutdown.
  let _ = client.call("shutdown", None).await.expect("shutdown");
  let _ = timeout(Duration::from_secs(3), handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// `proxy.request_log_file: true` end to end: the daemon starts the file
/// writer, a finished request lands in `<log dir>/requests.jsonl`, and
/// `status` reports the path with nothing dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_file_option_writes_finished_requests_and_shows_in_status() {
  let dir = unique_temp_dir("request-log-file");
  let mut opts = DaemonOptions::rooted_at(dir.clone());
  opts.proxy = ProxyConfig {
    enabled: true,
    port: Some(pick_free_port()),
    request_log_file: true,
    ..ProxyConfig::default()
  };
  let log_file = opts.log_dir.join("requests.jsonl");
  let socket_path = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  wait_for_socket(&socket_path).await;
  let proxy_addr = wait_for_proxy(&socket_path).await;
  let (status, _) = http_post(proxy_addr, "/v1/chat/completions", "{}").await;
  assert_eq!(status, 400);

  // The writer runs on its own task; wait for the line to land.
  let mut text = String::new();
  for _ in 0..200 {
    text = std::fs::read_to_string(&log_file).unwrap_or_default();
    if text.ends_with('\n') {
      break;
    }
    sleep(Duration::from_millis(10)).await;
  }
  let rows: Vec<serde_json::Value> = text
    .lines()
    .map(|line| serde_json::from_str(line).expect("one JSON row per line"))
    .collect();
  assert_eq!(rows.len(), 1, "{text}");
  assert_eq!(rows[0]["status"], 400);
  assert_eq!(rows[0]["error"], "model_required");
  assert_eq!(rows[0]["route"], "/v1/chat/completions");

  let mut client = Client::connect(&socket_path).await.expect("connect daemon");
  let status_body = client.call("status", None).await.expect("status");
  assert_eq!(
    status_body["proxy"]["request_log_file"],
    serde_json::json!({ "path": log_file, "dropped": 0 })
  );

  let _ = client.call("shutdown", None).await.expect("shutdown");
  let _ = timeout(Duration::from_secs(3), handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_starts_without_proxy_when_disabled() {
  let dir = unique_temp_dir("disabled");
  let mut opts = DaemonOptions::rooted_at(dir.clone());
  let port = pick_free_port();
  opts.proxy = ProxyConfig {
    enabled: false,
    port: Some(port),
    ..ProxyConfig::default()
  };
  let socket_path = opts.state_dir.clone();
  let proxy_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  wait_for_socket(&socket_path).await;

  // Wait a short beat to let any (incorrect) listener bind, then
  // confirm nothing is answering on the configured port.
  sleep(Duration::from_millis(200)).await;
  let connect_attempt = TcpStream::connect(proxy_addr).await;
  assert!(
    connect_attempt.is_err(),
    "proxy must not be listening when proxy.enabled = false; got {connect_attempt:?}"
  );

  let mut client = Client::connect(&socket_path).await.expect("connect daemon");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  let _ = timeout(Duration::from_secs(3), handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_keeps_running_when_proxy_port_already_in_use() {
  let dir = unique_temp_dir("port-in-use");
  // Camp on a port first using std (synchronous) so the daemon
  // observes a guaranteed EADDRINUSE.
  let camp = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("camp bind");
  let camp_addr: SocketAddr = camp.local_addr().expect("local_addr");
  let port = camp_addr.port();

  let mut opts = DaemonOptions::rooted_at(dir.clone());
  opts.proxy = ProxyConfig {
    enabled: true,
    port: Some(port),
    ..ProxyConfig::default()
  };
  let socket_path = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  // Daemon must reach a connectable IPC socket even though the
  // proxy listener bind failed.
  wait_for_socket(&socket_path).await;

  // A second-level smoke: the IPC `ping` works.
  let mut client = Client::connect(&socket_path).await.expect("connect");
  let pong = client.call("ping", None).await.expect("ping");
  assert_eq!(pong, serde_json::json!("pong"));

  let _ = client.call("shutdown", None).await.expect("shutdown");
  let _ = timeout(Duration::from_secs(3), handle).await;
  drop(camp);
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http11_keep_alive_serves_two_health_requests_on_one_connection() {
  let dir = unique_temp_dir("keepalive");
  let mut opts = DaemonOptions::rooted_at(dir.clone());
  let port = pick_free_port();
  opts.proxy = ProxyConfig {
    enabled: true,
    port: Some(port),
    ..ProxyConfig::default()
  };
  let socket_path = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });

  wait_for_socket(&socket_path).await;
  let proxy_addr = wait_for_proxy(&socket_path).await;

  let mut sock = TcpStream::connect(proxy_addr).await.expect("connect");
  for which in 0..2u8 {
    let req =
      format!("GET /health HTTP/1.1\r\nHost: {proxy_addr}\r\nConnection: keep-alive\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write");
    // Read until we have CRLFCRLF + the Content-Length body.
    let body = read_one_response(&mut sock).await;
    assert!(
      body.contains("\"status\":\"ok\""),
      "request #{which} body: {body}"
    );
  }

  let mut client = Client::connect(&socket_path).await.expect("connect daemon");
  let _ = client.call("shutdown", None).await.expect("shutdown");
  let _ = timeout(Duration::from_secs(3), handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// Read one complete HTTP/1.1 response off `sock` (headers + body
/// according to `Content-Length`). Panics if the socket closes or
/// the response is malformed.
async fn read_one_response(sock: &mut TcpStream) -> String {
  let mut buf = Vec::new();
  let mut tmp = [0u8; 1024];
  loop {
    let n = sock.read(&mut tmp).await.expect("read");
    if n == 0 {
      panic!("connection closed mid-response: {buf:?}");
    }
    buf.extend_from_slice(&tmp[..n]);
    if let Some(consumed) = try_consume_one(&buf) {
      let text = String::from_utf8_lossy(&buf[..consumed]).to_string();
      // Discard the consumed bytes; leave any pipelined tail in
      // place for the next caller. The keep-alive test only reads
      // one response at a time so the tail is empty in practice.
      buf.drain(..consumed);
      return text;
    }
  }
}

fn try_consume_one(buf: &[u8]) -> Option<usize> {
  let needle = b"\r\n\r\n";
  let split = buf.windows(needle.len()).position(|w| w == needle)?;
  let head = std::str::from_utf8(&buf[..split]).ok()?;
  let mut content_length: usize = 0;
  for line in head.split("\r\n") {
    if let Some(v) = line
      .strip_prefix("Content-Length:")
      .or_else(|| line.strip_prefix("content-length:"))
    {
      content_length = v.trim().parse().ok()?;
    }
  }
  let body_start = split + needle.len();
  if buf.len() < body_start + content_length {
    return None;
  }
  Some(body_start + content_length)
}
