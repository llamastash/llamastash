//! Shared helpers for tests: the integration suites under `tests/`
//! and the inline `#[cfg(test)]` modules that need the same isolation.
//!
//! Gated behind the `test-fixtures` feature so consumer builds of the
//! library don't carry test-only utilities.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a test waits for an in-process daemon to come up.
///
/// Startup runs the real GPU probe. On a host with `nvidia-smi`, a dozen
/// daemons booting at once under nextest queue on the driver and the probe
/// takes ~4 s each (measured on an RTX 3050 Ti); alone it takes 10 ms.
pub const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Unique temp directory for an integration test.
///
/// macOS `sun_path` is 104 bytes; the default `temp_dir()` already
/// eats ~50 of those, so we trim the time-based suffix and add a
/// process-local atomic counter. Two tests running on the same
/// millisecond used to share a directory (and a daemon, and a
/// runtime.json), which surfaced as periodic Connect-error flakes in
/// the chat smoke tests. `prefix` should be 2-5 chars.
pub fn unique_temp_dir(prefix: &str, label: &str) -> PathBuf {
  static SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = SEQ.fetch_add(1, Ordering::Relaxed);
  let suffix = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_millis()
    % 0xFFFF_FFFF;
  let dir = std::env::temp_dir().join(format!(
    "{prefix}-{label}-{}-{suffix:x}-{seq:x}",
    std::process::id()
  ));
  std::fs::create_dir_all(&dir).expect("temp dir creation");
  dir
}

/// Test launch ranges are drawn from 20000-31999, below every OS's default
/// ephemeral range (Linux starts at 32768, macOS and Windows at 49152). A
/// `bind(0)` therefore never hands out a pool port, so no other test can take
/// one between the probe here and the fake server's bind.
const PORT_POOL: std::ops::Range<u16> = 20_000..32_000;

const PORT_POOL_ATTEMPTS: u32 = 64;

/// Window starts already handed out in this process, so two tests in one
/// binary cannot draw the same window. A caller in another binary still can:
/// each process keeps its own set.
static HANDSED_OUT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

fn claim_start(start: u16) -> bool {
  let mut taken = HANDSED_OUT
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  if taken.contains(&start) {
    return false;
  }
  taken.push(start);
  true
}

/// A launch-pool port range for a test daemon: `width` contiguous ports drawn
/// from `PORT_POOL`, every one of them bindable on `127.0.0.1` when this
/// returns. Some callers use `.start` alone as a listener port, so the whole
/// window is probed, not just the first port; the rest give `ports::allocate`
/// (which walks the range linearly) somewhere to land if a port goes busy.
/// A process that asks for more windows than the pool holds runs out and
/// panics; no test comes close.
pub fn allocate_port_range(width: usize) -> crate::config::loader::PortRange {
  use rand::TryRngCore;
  let width = width.max(1) as u32;
  let pool = PORT_POOL.end as u32 - PORT_POOL.start as u32;
  assert!(width <= pool, "width {width} exceeds the port pool");
  let starts = pool - width + 1;
  for _ in 0..PORT_POOL_ATTEMPTS {
    let offset = rand::rngs::OsRng
      .try_next_u32()
      .expect("OsRng must be available for test port allocation")
      % starts;
    let start = (PORT_POOL.start as u32 + offset) as u16;
    let end = start + width as u16 - 1;
    if (start..=end).all(crate::daemon::ports::try_bind_probe) && claim_start(start) {
      return crate::config::loader::PortRange { start, end };
    }
  }
  panic!(
    "no {}-port window in {}-{} bound on 127.0.0.1 in {PORT_POOL_ATTEMPTS} tries",
    width,
    PORT_POOL.start,
    PORT_POOL.end - 1
  );
}

/// Best-effort **synchronous** daemon shutdown, for test `Drop` guards (Drop
/// runs during unwind and can't drive an async client). Hand-rolls an
/// HTTP/1.0 `POST /rpc` carrying the JSON-RPC `shutdown` envelope against the
/// URL + token recorded in `runtime.json`. That trips the daemon's shutdown
/// token, so `run_foreground` runs its `stop_all_managed` step — which is
/// where every `setsid`-detached supervised child (`fake_llama_server`) gets
/// SIGTERM/SIGKILLed. Without it those children become init-owned orphans, the
/// historical source of leaked test fixtures. No-op when `runtime.json` is
/// absent (daemon already gone).
pub fn sync_shutdown_daemon(state_dir: &std::path::Path) -> std::io::Result<()> {
  use std::io::{Read, Write};
  use std::net::TcpStream;
  use std::time::Duration;
  let info = match crate::daemon::runtime_file::load(state_dir) {
    Ok(Some(i)) => i,
    _ => return Ok(()),
  };
  // The daemon binds loopback only, so the URL is always `http://127.0.0.1:<port>`.
  let host_port = info
    .ipc_url
    .strip_prefix("http://")
    .unwrap_or(info.ipc_url.as_str());
  let mut stream = TcpStream::connect(host_port)?;
  stream.set_write_timeout(Some(Duration::from_secs(1)))?;
  stream.set_read_timeout(Some(Duration::from_secs(1)))?;
  let body = br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#;
  let req = format!(
    "POST /rpc HTTP/1.0\r\n\
     Host: {host_port}\r\n\
     Authorization: Bearer {token}\r\n\
     Content-Type: application/json\r\n\
     Content-Length: {len}\r\n\
     Connection: close\r\n\r\n",
    token = info.ipc_token,
    len = body.len(),
  );
  stream.write_all(req.as_bytes())?;
  stream.write_all(body)?;
  // Drain the response so the daemon's writer doesn't block on a full peer
  // buffer; the content doesn't matter — only that the token was tripped.
  let mut sink = [0u8; 512];
  let _ = stream.read(&mut sink);
  Ok(())
}

/// A [`RunningSnapshot`](crate::daemon::state_store::RunningSnapshot) for
/// tests, with every field pre-filled and one setter per field a test
/// actually varies.
///
/// `RunningSnapshot` is constructed in a dozen inline test modules and four
/// integration suites. Every field added to it used to mean editing each of
/// those literals; here it means one default in one place.
///
/// ```ignore
/// let row = running_row("/m/a.gguf").name("coder").launch_id("L2").build();
/// ```
pub struct RunningRow(crate::daemon::state_store::RunningSnapshot);

/// Start a [`RunningRow`] for a GGUF at `path`: launch `L1` on port 41100,
/// unnamed, chat mode, on the default backend.
pub fn running_row(path: &str) -> RunningRow {
  use crate::daemon::registry::LaunchId;
  use crate::daemon::state_store::RunningSnapshot;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;
  RunningRow(RunningSnapshot {
    id: crate::backend::identity::ModelIdentity::Gguf(crate::gguf::identity::ModelId {
      path: PathBuf::from(path),
      header_blake3: [7u8; 32],
    }),
    pid: 1,
    port: 41100,
    started_at: 0,
    launch_id: Some(LaunchId("L1".to_string())),
    name: None,
    preset: None,
    params: LaunchParams::new(PathBuf::from(path), LaunchMode::Chat),
    actuals: Default::default(),
    resolved_backend: crate::backend::DEFAULT_BACKEND_ID.to_string(),
    projected_demand_bytes: None,
    origin: None,
  })
}

impl RunningRow {
  /// Replace the GGUF identity with a backend (registry) one.
  pub fn identity(mut self, id: crate::backend::identity::ModelIdentity) -> Self {
    self.0.id = id;
    self
  }

  pub fn name(mut self, name: &str) -> Self {
    self.0.name = Some(name.to_string());
    self
  }

  /// The launch name as an `Option`, for a test that parameterises over both.
  pub fn maybe_name(mut self, name: Option<&str>) -> Self {
    self.0.name = name.map(str::to_string);
    self
  }

  /// The preset the launch resolved, as on a preset-backed running row.
  pub fn preset(mut self, preset: &str) -> Self {
    self.0.preset = Some(preset.to_string());
    self
  }

  pub fn launch_id(mut self, id: &str) -> Self {
    self.0.launch_id = Some(crate::daemon::registry::LaunchId(id.to_string()));
    self
  }

  /// Drop the launch id, as on a row adopted from a `state.json` written
  /// before the stamp existed.
  pub fn unstamped(mut self) -> Self {
    self.0.launch_id = None;
    self
  }

  pub fn port(mut self, port: u16) -> Self {
    self.0.port = port;
    self
  }

  pub fn pid(mut self, pid: i32) -> Self {
    self.0.pid = pid;
    self
  }

  pub fn started_at(mut self, secs: u64) -> Self {
    self.0.started_at = secs;
    self
  }

  pub fn params(mut self, params: crate::launch::params::LaunchParams) -> Self {
    self.0.params = params;
    self
  }

  /// The demand the admission gate priced this launch at — what make-room
  /// credits it for when it is unloaded.
  pub fn projected_demand(mut self, bytes: u64) -> Self {
    self.0.projected_demand_bytes = Some(bytes);
    self
  }

  /// How the launch came to be running — the sweep and make-room only ever
  /// consider an `AutoStart` row.
  pub fn origin(mut self, origin: crate::daemon::supervisor::LaunchOrigin) -> Self {
    self.0.origin = Some(origin);
    self
  }

  pub fn resolved_backend(mut self, backend: &str) -> Self {
    self.0.resolved_backend = backend.to_string();
    self
  }

  pub fn build(self) -> crate::daemon::state_store::RunningSnapshot {
    self.0
  }
}

/// The id of the registered backend that declares knob `knob_id`, for tests
/// that need a real non-default backend without naming one.
pub fn backend_declaring(knob_id: &str) -> &'static str {
  use crate::backend::Backend;
  crate::backend::Backends::all()
    .into_iter()
    .find(|b| b.knobs().iter().any(|k| k.id == knob_id))
    .map(|b| b.id())
    .unwrap_or_else(|| panic!("no backend declares knob `{knob_id}`"))
}

/// The newest request-log row once it satisfies `done`. The proxy finishes
/// a row when hyper drops the response body, which can trail the client's
/// read by a scheduler tick.
pub async fn newest_request_when(
  log: &crate::proxy::request_log::RequestLog,
  done: impl Fn(&crate::proxy::request_log::RequestRow) -> bool,
) -> crate::proxy::request_log::RequestRow {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
  loop {
    match log
      .tail(crate::proxy::request_log::LogScope::All, 1)
      .rows
      .into_iter()
      .next()
    {
      Some(row) if done(&row) => return row,
      other => {
        assert!(
          std::time::Instant::now() < deadline,
          "log row never reached the expected state; last seen: {other:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    }
  }
}

/// The newest request-log row once its response has ended.
pub async fn finished_request(
  log: &crate::proxy::request_log::RequestLog,
) -> crate::proxy::request_log::RequestRow {
  newest_request_when(log, |r| {
    r.state != crate::proxy::request_log::RequestState::InFlight
  })
  .await
}

/// A `ModelMetadata` for a 7B chat GGUF of architecture `arch`, for proxy and
/// supervisor tests that need a row rather than a parsed file.
pub fn fake_metadata(arch: &str) -> crate::gguf::metadata::ModelMetadata {
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
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

/// Write a minimal parseable GGUF of `arch` into `dir` under `name` and return
/// its canonical path, the shape a discovery row expects.
pub fn write_gguf(dir: &std::path::Path, name: &str, arch: &str) -> PathBuf {
  let path = dir.join(name);
  std::fs::write(&path, crate::gguf::test_fixtures::build_minimal_gguf(arch)).expect("write gguf");
  crate::util::paths::canonicalize(&path).expect("canonicalize")
}

/// Start the proxy serve loop on an ephemeral loopback port, wait until it
/// reports `Listening`, and hand back the bound address with the pieces the
/// caller needs to stop it ([`shutdown_listener`]).
pub async fn spawn_listener(
  state: std::sync::Arc<crate::proxy::state::ProxyState>,
) -> (
  std::net::SocketAddr,
  crate::daemon::shutdown::ShutdownToken,
  tokio::task::JoinHandle<()>,
) {
  spawn_listener_with_options(state, crate::proxy::server::ServeOptions::default()).await
}

/// The same listener with explicit `ServeOptions`, for the suites that
/// exercise a non-default serve knob (a short header-read timeout).
pub async fn spawn_listener_with_options(
  state: std::sync::Arc<crate::proxy::state::ProxyState>,
  options: crate::proxy::server::ServeOptions,
) -> (
  std::net::SocketAddr,
  crate::daemon::shutdown::ShutdownToken,
  tokio::task::JoinHandle<()>,
) {
  use crate::proxy::server::{loopback_addr, new_status_cell, serve_with_options};
  let token = crate::daemon::shutdown::ShutdownToken::new();
  let status = new_status_cell();
  let bind_addr = loopback_addr(0);
  let token_for_task = token.clone();
  let status_for_task = std::sync::Arc::clone(&status);
  let handle = tokio::spawn(async move {
    serve_with_options(state, bind_addr, token_for_task, status_for_task, options)
      .await
      .expect("proxy serve returns Ok");
  });
  let bound = wait_for_listening(&status, std::time::Duration::from_secs(2))
    .await
    .expect("listener reaches Listening");
  (bound, token, handle)
}

/// Trigger shutdown and join the serve task with a generous budget. Catches a
/// hung serve loop instead of leaving a detached task that would otherwise be
/// silently torn down when the runtime exits.
pub async fn shutdown_listener(
  shutdown: crate::daemon::shutdown::ShutdownToken,
  handle: tokio::task::JoinHandle<()>,
) {
  shutdown.trigger();
  tokio::time::timeout(std::time::Duration::from_secs(5), handle)
    .await
    .expect("proxy serve loop must exit after shutdown.trigger()")
    .expect("proxy serve task must not panic");
}

/// Poll `status` for up to `budget` for the `Listening` address, or `None`.
async fn wait_for_listening(
  status: &crate::proxy::server::StatusCell,
  budget: std::time::Duration,
) -> Option<std::net::SocketAddr> {
  use crate::proxy::server::ProxyStatus;
  let deadline = std::time::Instant::now() + budget;
  while std::time::Instant::now() < deadline {
    if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
      return Some(addr);
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
  }
  None
}

/// One raw HTTP/1.1 exchange: connect, send `request`, read to EOF, split the
/// reply into `(status, headers, body)` with header names lowercased.
/// `Connection: close` keeps it one-shot, so a caller never has to frame a body
/// to know the response ended.
async fn http_round_trip(
  addr: std::net::SocketAddr,
  request: String,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
  sock.write_all(request.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  // Bound the read: a server that accepts and never answers should fail the
  // test, not hang the suite until the runner's own timeout. Kept generous so
  // a request that waits on a real model load still finishes on a loaded CI box.
  let read = tokio::time::timeout(
    std::time::Duration::from_secs(60),
    sock.read_to_end(&mut buf),
  )
  .await
  .expect("response within 60s");
  read.expect("read");
  let needle = b"\r\n\r\n";
  let split = buf
    .windows(needle.len())
    .position(|w| w == needle)
    .expect("CRLFCRLF terminator");
  let head = std::str::from_utf8(&buf[..split]).expect("utf8 headers");
  let mut lines = head.split("\r\n");
  let status: u16 = lines
    .next()
    .expect("status line")
    .split_whitespace()
    .nth(1)
    .expect("status code")
    .parse()
    .expect("parse status");
  let headers = lines
    .filter_map(|line| line.split_once(':'))
    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
    .collect();
  (status, headers, buf[split + needle.len()..].to_vec())
}

fn head_lines(host: std::net::SocketAddr, extra_headers: &[(&str, &str)]) -> String {
  let mut req = format!("Host: {host}\r\nConnection: close\r\n");
  for (k, v) in extra_headers {
    req.push_str(&format!("{k}: {v}\r\n"));
  }
  req
}

/// A bare-socket `GET` with optional extra headers: `(status, headers, body)`.
pub async fn http_get(
  addr: std::net::SocketAddr,
  path: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let request = format!(
    "GET {path} HTTP/1.1\r\n{}{}",
    head_lines(addr, extra_headers),
    "\r\n"
  );
  http_round_trip(addr, request).await
}

/// A bare-socket `HEAD` with optional extra headers: `(status, headers, body)`.
pub async fn http_head(
  addr: std::net::SocketAddr,
  path: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let request = format!(
    "HEAD {path} HTTP/1.1\r\n{}{}",
    head_lines(addr, extra_headers),
    "\r\n"
  );
  http_round_trip(addr, request).await
}

/// A bare-socket JSON `POST` with optional extra headers:
/// `(status, headers, body)`.
pub async fn http_post(
  addr: std::net::SocketAddr,
  path: &str,
  body: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let request = format!(
    "POST {path} HTTP/1.1\r\n{}Content-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
    head_lines(addr, extra_headers),
    body.len()
  );
  http_round_trip(addr, request).await
}

/// One header value from an [`http_get`] / [`http_post`] reply; `name` is
/// matched case-insensitively.
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
  headers
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn allocate_port_range_returns_the_requested_width_inside_the_pool() {
    let mut starts = Vec::new();
    for _ in 0..50 {
      let range = allocate_port_range(8);
      assert_eq!(range.end as u32 - range.start as u32 + 1, 8);
      assert!(
        range.start >= PORT_POOL.start,
        "start {} too low",
        range.start
      );
      assert!(range.end < PORT_POOL.end, "end {} too high", range.end);
      assert!(
        (range.start..=range.end).all(crate::daemon::ports::try_bind_probe),
        "window {}-{} must be bindable",
        range.start,
        range.end
      );
      starts.push(range.start);
    }
    starts.sort_unstable();
    starts.dedup();
    assert_eq!(starts.len(), 50, "two calls in one process shared a window");
  }
}
