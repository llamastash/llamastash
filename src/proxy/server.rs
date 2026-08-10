// Echo-verification:
//
// Pass-through forwarding relies on the upstream `llama-server`
// echoing `request.body.model` back in `response.body.model`. The
// inline tests below post a JSON request carrying
// `"model":"sentinel-x42"` to `fake_llama_server` and assert the
// echo on the response body. The fake fixture mirrors the real
// `llama-server` behavior in this respect — see
// `tests/fixtures/fake_llama_server.rs` (`/v1/chat/completions`).
//
// If the assertion ever breaks — `llama-server` rewriting
// `body.model` in its response instead of echoing — forwarding would
// have to JSON-parse / rewrite each SSE frame on fallback instead of
// byte-piping.

//! TCP accept loop + per-connection hyper service.
//!
//! Mirrors the shape of [`crate::daemon::control_plane::serve`]: a
//! `tokio::select!` between `listener.accept()` and the daemon's
//! [`ShutdownToken`], with a bounded drain phase on shutdown.
//! Unlike the IPC control plane we don't bearer-authenticate —
//! loopback-only by default, the proxy is reachable by every
//! same-host process by design.

use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::{atomic::Ordering, Arc, Mutex as StdMutex, RwLock},
  time::Duration,
};

use anyhow::Result;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinHandle, time::Instant};

use super::router::route;
use super::state::ProxyState;
use crate::daemon::shutdown::ShutdownToken;

/// Maximum time to wait for in-flight connections after shutdown is
/// triggered before dropping them. Mirrors
/// [`crate::daemon::control_plane::DRAIN_TIMEOUT`] so the two
/// listeners drain on the same budget — useful when both are stopped
/// by one SIGINT.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Externally visible proxy listener state. The IPC `status` response
/// and the CLI / TUI surfaces read this cell; the proxy task writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyStatus {
  /// `proxy.enabled: false` in config; no listener was attempted.
  Disabled,
  /// Listener bound successfully and is accepting connections.
  /// `auth_enforced` is `true` when a bearer key is configured (the
  /// data routes require `Authorization: Bearer`), `false` for the
  /// keyless loopback default.
  Listening {
    addr: SocketAddr,
    auth_enforced: bool,
  },
  /// `EADDRINUSE` — the configured port is held by another process.
  /// Status surface reports the *attempted* address.
  PortInUse { addr: SocketAddr },
  /// Bind failed for any other reason (EACCES, EADDRNOTAVAIL, ...).
  /// `bind_error` is the OS error message; the status surface
  /// passes it through to the user.
  Unbound {
    addr: SocketAddr,
    bind_error: String,
  },
  /// A non-loopback bind was requested with no `api_key` and without
  /// `insecure_no_auth`, so the daemon refused to expose the proxy
  /// unauthenticated. The daemon and control plane keep running; `addr`
  /// is the address it would have bound. Resolve by setting a key (the
  /// CLI auto-provisions one on `daemon start --proxy-host`) or pass
  /// `--insecure-no-auth` to opt into an unauthenticated LAN proxy.
  RefusedInsecure { addr: SocketAddr },
}

/// Cheap-to-clone handle to the proxy's current status. The proxy
/// task writes to it on every transition; the IPC `status` handler
/// reads from it.
pub type StatusCell = Arc<RwLock<ProxyStatus>>;

/// Construct a fresh cell seeded with `Disabled`. The daemon
/// overrides it immediately if `proxy.enabled` is true.
pub fn new_status_cell() -> StatusCell {
  Arc::new(RwLock::new(ProxyStatus::Disabled))
}

/// Default header-read timeout used by tests + callers that don't
/// thread a real `ProxyConfig` through. Production wiring in
/// `run_foreground` passes the user-configured value via
/// [`serve_with_options`] instead.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How many ports above the configured base port the listener will
/// probe for a free slot before giving up. Six attempts total
/// (`port..=port+5`) — the default mode starts at `11435` and lands
/// within `11435..=11440`; Ollama-compat mode starts at `11434` and
/// lands within `11434..=11439`. The window is intentionally narrow
/// (rather than e.g. ephemeral-range-wide) so a long-running daemon
/// stays on a predictable port the user can pin into agent configs.
pub const DEFAULT_PORT_SCAN_MAX_OFFSET: u16 = 5;

/// Tunable knobs for the listener. Kept as a struct so future
/// additions don't churn the [`serve`] signature.
#[derive(Clone, Copy, Debug)]
pub struct ServeOptions {
  pub header_read_timeout: Duration,
  /// Maximum offset above the configured port that the listener will
  /// try when the lower ports are all `AddrInUse`. `0` reverts to
  /// strict single-port behaviour (used by the port-collision test).
  /// See [`DEFAULT_PORT_SCAN_MAX_OFFSET`] for the production default.
  pub port_scan_max_offset: u16,
}

impl Default for ServeOptions {
  fn default() -> Self {
    Self {
      header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
      port_scan_max_offset: DEFAULT_PORT_SCAN_MAX_OFFSET,
    }
  }
}

/// Run the proxy listener until `shutdown` is triggered.
///
/// Returns without panicking on bind failure: the caller is the
/// daemon's `run_foreground`, which has stricter availability
/// guarantees than the proxy itself. `status` reflects the outcome
/// so the IPC surface can report it.
///
/// Defaults to [`ServeOptions::default`] (30 s header-read timeout).
/// Production callers pass a `ProxyConfig`-derived value via
/// [`serve_with_options`].
pub async fn serve(
  state: Arc<ProxyState>,
  addr: SocketAddr,
  shutdown: ShutdownToken,
  status: StatusCell,
) -> Result<()> {
  serve_with_options(state, addr, shutdown, status, ServeOptions::default()).await
}

/// Outcome of [`bind_with_scan`]. The two failure variants split
/// `AddrInUse` from every other bind error because the IPC `status`
/// surface reports the two cases as distinct states (`PortInUse` vs
/// `Unbound { bind_error }`).
enum BindOutcome {
  Bound(TcpListener),
  AllPortsInUse {
    /// The highest port we actually attempted. Surfaced via `status`
    /// so the user knows which port the listener gave up on.
    last_addr: SocketAddr,
  },
  Failed {
    addr: SocketAddr,
    error: std::io::Error,
  },
}

/// A bind error that should advance the scan to the next candidate
/// port rather than abort the whole listener.
///
/// - `AddrInUse`: something already holds this port.
/// - `PermissionDenied`: on Windows this is `WSAEACCES` (os error
///   10013), which a *specific* port returns when it falls inside an
///   OS-reserved/excluded TCP range — Hyper-V / WSL2 / Docker Desktop
///   carve out random dynamic ranges, and a port in one fails to bind
///   even though nothing is listening. The reservation is per-port, so
///   the next candidate usually binds fine. (On Unix EACCES is the
///   privileged-port case, but the scan window is all > 1024, so this
///   only meaningfully changes Windows; advancing is harmless either
///   way because an exhausted scan still reports the error.)
fn is_retriable_bind_error(e: &std::io::Error) -> bool {
  matches!(
    e.kind(),
    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
  )
}

/// Walk `[port, port + max_offset]` looking for a free slot.
/// Retriable bind errors (see [`is_retriable_bind_error`]) advance to
/// the next port; any other error is fatal (no point retrying e.g. an
/// invalid-address error on the next port). A zero `max_offset`
/// collapses to a single bind attempt — same shape as the v0 code
/// path.
///
/// If the whole window is exhausted by retriable errors, the outcome
/// distinguishes a pure `AddrInUse` sweep ([`BindOutcome::AllPortsInUse`]
/// → `PortInUse` status) from one where a `PermissionDenied`
/// (Windows excluded-range) miss occurred ([`BindOutcome::Failed`] →
/// `Unbound` status), so the user sees the accurate cause.
async fn bind_with_scan(base: SocketAddr, max_offset: u16) -> BindOutcome {
  let mut last_addr = base;
  let mut last_denied: Option<(SocketAddr, std::io::Error)> = None;
  for offset in 0..=max_offset {
    let Some(port) = base.port().checked_add(offset) else {
      break;
    };
    let candidate = SocketAddr::new(base.ip(), port);
    last_addr = candidate;
    match TcpListener::bind(&candidate).await {
      Ok(l) => return BindOutcome::Bound(l),
      Err(e) if is_retriable_bind_error(&e) => {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
          last_denied = Some((candidate, e));
        }
        continue;
      }
      Err(error) => {
        return BindOutcome::Failed {
          addr: candidate,
          error,
        }
      }
    }
  }
  // The window is exhausted. A permission denial is a more actionable
  // cause than "port in use", so surface it when one occurred.
  match last_denied {
    Some((addr, error)) => BindOutcome::Failed { addr, error },
    None => BindOutcome::AllPortsInUse { last_addr },
  }
}

/// `serve` plus per-listener tuning knobs. Same semantics as
/// [`serve`]; lifted out so the daemon can forward
/// `proxy.header_read_timeout_secs` without forcing every caller
/// (tests, benches) to construct a full options bundle.
pub async fn serve_with_options(
  state: Arc<ProxyState>,
  addr: SocketAddr,
  shutdown: ShutdownToken,
  status: StatusCell,
  options: ServeOptions,
) -> Result<()> {
  let listener = match bind_with_scan(addr, options.port_scan_max_offset).await {
    BindOutcome::Bound(l) => l,
    BindOutcome::AllPortsInUse { last_addr } => {
      let base = addr.port();
      let high = base.saturating_add(options.port_scan_max_offset);
      if base == high {
        log::warn!(
          "proxy listener: port {base} already in use; daemon continues without the proxy"
        );
      } else {
        log::warn!(
          "proxy listener: ports {base}..={high} all in use; daemon continues without the proxy"
        );
      }
      write_status(&status, ProxyStatus::PortInUse { addr: last_addr });
      return Ok(());
    }
    BindOutcome::Failed { addr, error } => {
      log::warn!(
        "proxy listener: failed to bind {addr}: {error}; daemon continues without the proxy"
      );
      // Cap the bind_error string so a pathological OS message cannot
      // bloat the IPC status payload. 256 chars is roomy for typical
      // io::Error strings ("permission denied", etc.). A Windows
      // excluded-range denial (WSAEACCES) gets an actionable hint
      // appended, since the raw OS text doesn't tell the user the port
      // is reserved or how to move off it.
      let mut bind_error: String = error.to_string().chars().take(256).collect();
      if error.kind() == std::io::ErrorKind::PermissionDenied {
        bind_error.push_str(
          "; port is reserved by the OS (often Hyper-V/WSL2/Docker on Windows) — set a different proxy.port",
        );
      }
      write_status(&status, ProxyStatus::Unbound { addr, bind_error });
      return Ok(());
    }
  };
  // Re-resolve the bound address (the kernel may have promoted a 0
  // port — useful in tests, harmless in production).
  let bound = listener.local_addr().unwrap_or(addr);
  let auth_enforced = state.auth.enforced();
  write_status(
    &status,
    ProxyStatus::Listening {
      addr: bound,
      auth_enforced,
    },
  );
  log::info!(
    "proxy listener bound on http://{bound} (auth {})",
    if auth_enforced { "enforced" } else { "off" }
  );

  let tracker = Arc::new(ConnectionTracker {
    active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    handles: StdMutex::new(Vec::new()),
  });

  loop {
    tokio::select! {
      _ = shutdown.wait_until_triggered() => {
        log::info!("proxy listener: shutdown signalled");
        break;
      }
      accept = listener.accept() => {
        match accept {
          Ok((stream, _peer)) => {
            let conn_state = Arc::clone(&state);
            let conn_tracker = Arc::clone(&tracker);
            conn_tracker.active.fetch_add(1, Ordering::SeqCst);
            // Proxy traffic never touches the control plane, so without
            // this an agent hammering /v1/chat/completions still reads
            // as an idle daemon. Stamped at both ends of the connection.
            let activity = conn_state.ctx.activity.clone();
            activity.mark();
            let task_tracker = Arc::clone(&conn_tracker);
            let header_read_timeout = options.header_read_timeout;
            let handle = tokio::spawn(async move {
              serve_connection(stream, conn_state, header_read_timeout).await;
              activity.mark();
              task_tracker.active.fetch_sub(1, Ordering::SeqCst);
            });
            push_handle(&conn_tracker, handle);
          }
          Err(e) => {
            // Mirrors the IPC server's posture: a transient accept
            // failure is not fatal; loop and retry.
            log::warn!("proxy listener: accept failed: {e}");
          }
        }
      }
    }
  }

  drain(tracker).await;
  Ok(())
}

/// Bounded poll-and-abort drain phase. Identical shape to the
/// IPC server's drain so the two listeners exit on the same
/// schedule.
async fn drain(tracker: Arc<ConnectionTracker>) {
  let deadline = Instant::now() + DRAIN_TIMEOUT;
  let poll_interval = Duration::from_millis(50);
  while tracker.active.load(Ordering::SeqCst) > 0 {
    let remaining = deadline.checked_duration_since(Instant::now());
    let Some(time_left) = remaining else {
      let still_active = tracker.active.load(Ordering::SeqCst);
      log::warn!(
        "proxy drain deadline reached with {still_active} connection(s) still active; aborting"
      );
      let mut handles = tracker.handles.lock().unwrap_or_else(|e| e.into_inner());
      for h in handles.drain(..) {
        h.abort();
      }
      break;
    };
    tokio::time::sleep(poll_interval.min(time_left)).await;
  }
}

struct ConnectionTracker {
  active: Arc<std::sync::atomic::AtomicUsize>,
  handles: StdMutex<Vec<JoinHandle<()>>>,
}

fn push_handle(tracker: &Arc<ConnectionTracker>, handle: JoinHandle<()>) {
  let mut handles = tracker.handles.lock().unwrap_or_else(|e| e.into_inner());
  handles.retain(|h| !h.is_finished());
  handles.push(handle);
}

async fn serve_connection(
  stream: tokio::net::TcpStream,
  state: Arc<ProxyState>,
  header_read_timeout: Duration,
) {
  // `TokioIo` bridges `tokio::net::TcpStream` (which implements the
  // tokio `AsyncRead`/`AsyncWrite` traits) onto hyper 1.x's `Io`
  // traits without a `tower-service` dep. Owned by the connection
  // future so it lives exactly as long as the connection does.
  let io = TokioIo::new(stream);
  let service = service_fn(move |req| {
    let state = Arc::clone(&state);
    async move { route(state, req).await }
  });
  let mut builder = http1::Builder::new();
  builder
    // `keep_alive(true)` is the default but documenting it
    // explicitly: the keep-alive smoke test below depends on it.
    .keep_alive(true)
    // Bound header-read on the inbound stream so partial-request
    // clients don't hold the serve_connection future indefinitely.
    .timer(hyper_util::rt::TokioTimer::new())
    .header_read_timeout(header_read_timeout);
  if let Err(e) = builder.serve_connection(io, service).await {
    log::debug!("proxy: connection ended with: {e}");
  }
}

fn write_status(cell: &StatusCell, next: ProxyStatus) {
  let mut guard = cell.write().unwrap_or_else(|e| e.into_inner());
  *guard = next;
}

/// Build the `SocketAddr` the proxy listener binds from a `host` and
/// `port`. `127.0.0.1` keeps the loopback-only posture; a routable
/// address (`0.0.0.0`, a NIC IP, or IPv6) opts the proxy data plane
/// into LAN exposure — gated by the bearer key in the daemon wiring,
/// not here. The port-scan in `bind_with_scan` carries this host
/// through unchanged.
pub fn listen_addr(host: IpAddr, port: u16) -> SocketAddr {
  SocketAddr::new(host, port)
}

/// Loopback `SocketAddr` for a port — `listen_addr(127.0.0.1, port)`.
/// Retained for tests and callers that always bind loopback.
pub fn loopback_addr(port: u16) -> SocketAddr {
  listen_addr(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::daemon::context::MethodContext;
  use std::time::Duration;

  #[test]
  fn listen_addr_builds_from_host_and_port() {
    // loopback wrapper keeps the historical address.
    assert_eq!(
      loopback_addr(11435),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 11435)
    );
    // A routable IPv4 host is carried through verbatim.
    let v4: IpAddr = "0.0.0.0".parse().unwrap();
    assert_eq!(listen_addr(v4, 11435), SocketAddr::new(v4, 11435));
    // IPv6 too.
    let v6: IpAddr = "::".parse().unwrap();
    assert_eq!(listen_addr(v6, 8080), SocketAddr::new(v6, 8080));
  }

  #[test]
  fn retriable_bind_errors_advance_the_scan() {
    use std::io::{Error, ErrorKind};
    // Both an occupied port and a Windows excluded-range denial
    // (WSAEACCES, surfaced as PermissionDenied) must advance the scan.
    assert!(is_retriable_bind_error(&Error::from(ErrorKind::AddrInUse)));
    assert!(is_retriable_bind_error(&Error::from(
      ErrorKind::PermissionDenied
    )));
    // The raw Windows code path: os error 10013 classifies as
    // PermissionDenied, so a synthetic one is retriable too.
    let wsaeacces = Error::from_raw_os_error(10013);
    if wsaeacces.kind() == ErrorKind::PermissionDenied {
      assert!(is_retriable_bind_error(&wsaeacces));
    }
    // Errors that won't fare better on the next port stay fatal.
    assert!(!is_retriable_bind_error(&Error::from(
      ErrorKind::AddrNotAvailable
    )));
    assert!(!is_retriable_bind_error(&Error::from(
      ErrorKind::InvalidInput
    )));
  }

  /// Spin up the proxy on an ephemeral port and return the bound
  /// address + shutdown token + the JoinHandle. The caller drives
  /// shutdown via `shutdown.trigger()` and then `shutdown_proxy` so
  /// a hung serve loop fails the test loudly instead of leaving a
  /// detached task behind.
  async fn spawn_proxy_on_ephemeral_port() -> (SocketAddr, ShutdownToken, StatusCell, JoinHandle<()>)
  {
    let ctx = MethodContext::new(ShutdownToken::new());
    let state = ProxyState::from_context(&ctx, false, true);
    let token = ctx.shutdown.clone();
    let status = new_status_cell();
    let status_for_task = Arc::clone(&status);
    let token_for_task = token.clone();
    // Bind to port 0 so the kernel picks a free port; sidesteps
    // the well-known port collision with anything else on the box.
    let bind_addr = loopback_addr(0);
    let handle = tokio::spawn(async move {
      serve(state, bind_addr, token_for_task, status_for_task)
        .await
        .expect("proxy serve returns Ok even on bind failure");
    });
    // Wait for the listener to record its bound address.
    let bound = wait_for_listening(&status, Duration::from_secs(2))
      .await
      .expect("proxy must reach Listening within 2s");
    (bound, token, status, handle)
  }

  /// Like [`spawn_proxy_on_ephemeral_port`] but with bearer auth
  /// enforced on the data routes via `api_key`.
  async fn spawn_proxy_with_key(
    api_key: &str,
  ) -> (SocketAddr, ShutdownToken, StatusCell, JoinHandle<()>) {
    let ctx = MethodContext::new(ShutdownToken::new());
    let state = ProxyState::from_context_with_auth(&ctx, false, true, Some(api_key.to_string()));
    let token = ctx.shutdown.clone();
    let status = new_status_cell();
    let status_for_task = Arc::clone(&status);
    let token_for_task = token.clone();
    let bind_addr = loopback_addr(0);
    let handle = tokio::spawn(async move {
      serve(state, bind_addr, token_for_task, status_for_task)
        .await
        .expect("proxy serve returns Ok even on bind failure");
    });
    let bound = wait_for_listening(&status, Duration::from_secs(2))
      .await
      .expect("proxy must reach Listening within 2s");
    (bound, token, status, handle)
  }

  /// One-shot GET with an optional `Authorization` header on a fresh
  /// `Connection: close` socket — keeps the auth assertions free of
  /// keep-alive bookkeeping.
  async fn http_get_auth(addr: SocketAddr, path: &str, auth: Option<&str>) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr)
      .await
      .expect("connect to proxy");
    let host = addr.to_string();
    let auth_line = match auth {
      Some(v) => format!("Authorization: {v}\r\n"),
      None => String::new(),
    };
    let req =
      format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth_line}Connection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
      let n = sock.read(&mut tmp).await.expect("read");
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&tmp[..n]);
      if let Some(body) = extract_body_when_complete(&buf) {
        return body;
      }
    }
    extract_body_when_complete(&buf).unwrap_or((0, buf))
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn auth_gates_data_routes_but_not_health_or_identity() {
    let key = "sk-llamastash-testkey123";
    let (addr, shutdown, _status, handle) = spawn_proxy_with_key(key).await;

    // Liveness / identity probes stay open with no Authorization.
    assert_eq!(http_get_auth(addr, "/", None).await.0, 200, "GET / exempt");
    assert_eq!(
      http_get_auth(addr, "/health", None).await.0,
      200,
      "GET /health exempt"
    );

    // Data route with no bearer → 401 + OpenAI auth envelope.
    let (status, body) = http_get_auth(addr, "/v1/models", None).await;
    assert_eq!(status, 401, "no bearer → 401");
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(parsed["error"]["code"], "invalid_api_key");

    // Wrong bearer → 401.
    assert_eq!(
      http_get_auth(addr, "/v1/models", Some("Bearer wrong"))
        .await
        .0,
      401,
      "wrong bearer → 401"
    );

    // Correct bearer passes the gate and the route runs (200 + list).
    let (ok_status, _) = http_get_auth(addr, "/v1/models", Some(&format!("Bearer {key}"))).await;
    assert_eq!(ok_status, 200, "correct bearer → 200");

    shutdown_proxy(shutdown, handle).await;
  }

  /// Trigger shutdown and join the serve task with a generous budget.
  /// Catches the failure mode where a serve loop never exits — without
  /// this, `drop(handle)` would silently leave a detached task and the
  /// hang would go unnoticed.
  async fn shutdown_proxy(shutdown: ShutdownToken, handle: JoinHandle<()>) {
    shutdown.trigger();
    // DRAIN_TIMEOUT + 1 s slack for the spawn-task to wind down.
    let budget = DRAIN_TIMEOUT + Duration::from_secs(1);
    tokio::time::timeout(budget, handle)
      .await
      .expect("proxy serve loop must exit after shutdown.trigger()")
      .expect("proxy serve task must not panic");
  }

  async fn wait_for_listening(status: &StatusCell, budget: Duration) -> Option<SocketAddr> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
      if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
        return Some(addr);
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
  }

  /// Minimal HTTP/1.1 client used by the inline tests. We don't
  /// want to pull `reqwest` into the unit tests just for two
  /// request/response cycles — and the keep-alive test needs
  /// explicit control over the TCP socket anyway.
  async fn http_get_keepalive(
    sock: &mut tokio::net::TcpStream,
    host: &str,
    path: &str,
  ) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write");
    // Read until we have headers + body. We control both sides so a
    // simple read-until-content-length-bytes loop is enough.
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
      let n = sock.read(&mut tmp).await.expect("read");
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&tmp[..n]);
      // Stop once the body is complete (Content-Length present).
      if let Some(body) = extract_body_when_complete(&buf) {
        return body;
      }
    }
    panic!("connection closed before body arrived; got: {buf:?}");
  }

  fn extract_body_when_complete(buf: &[u8]) -> Option<(u16, Vec<u8>)> {
    let needle = b"\r\n\r\n";
    let split = buf.windows(needle.len()).position(|w| w == needle)?;
    let head = std::str::from_utf8(&buf[..split]).ok()?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let mut content_length: Option<usize> = None;
    for line in lines {
      if let Some(v) = line
        .strip_prefix("Content-Length:")
        .or_else(|| line.strip_prefix("content-length:"))
      {
        content_length = v.trim().parse().ok();
      }
    }
    let body_start = split + needle.len();
    let need = content_length?;
    if buf.len() < body_start + need {
      return None;
    }
    let body = buf[body_start..body_start + need].to_vec();
    Some((status, body))
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn health_returns_ok_shape() {
    let (addr, shutdown, _status, handle) = spawn_proxy_on_ephemeral_port().await;
    let mut sock = tokio::net::TcpStream::connect(addr)
      .await
      .expect("connect to proxy");
    let host = addr.to_string();
    let (status, body) = http_get_keepalive(&mut sock, &host, "/health").await;
    assert_eq!(status, 200, "health returns 200");
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(parsed["status"], "ok");
    assert!(parsed["models_loaded"].is_u64(), "models_loaded shape");
    assert!(
      parsed["models_discovered"].is_u64(),
      "models_discovered shape"
    );
    shutdown_proxy(shutdown, handle).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn chat_completions_without_model_field_returns_400() {
    // A body without `model` short-circuits at the
    // `RouteDecision::ModelRequired` arm — 400
    // `invalid_request` / `code: model_required`.
    let (addr, shutdown, _status, handle) = spawn_proxy_on_ephemeral_port().await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let host = addr.to_string();
    let req = format!(
      "POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    sock.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read");
    let head = std::str::from_utf8(&buf).expect("utf8");
    assert!(head.contains("400"), "expected 400 in: {head}");
    assert!(head.contains("model_required"), "body shape: {head}");
    shutdown_proxy(shutdown, handle).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn keep_alive_serves_two_health_requests_on_one_connection() {
    let (addr, shutdown, _status, handle) = spawn_proxy_on_ephemeral_port().await;
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let host = addr.to_string();
    let (s1, b1) = http_get_keepalive(&mut sock, &host, "/health").await;
    assert_eq!(s1, 200, "first request status");
    let p1: serde_json::Value = serde_json::from_slice(&b1).expect("json");
    assert_eq!(p1["status"], "ok");
    // Second GET reuses the same TCP socket.
    let (s2, b2) = http_get_keepalive(&mut sock, &host, "/health").await;
    assert_eq!(s2, 200, "second request status (keep-alive smoke)");
    let p2: serde_json::Value = serde_json::from_slice(&b2).expect("json");
    assert_eq!(p2["status"], "ok");
    shutdown_proxy(shutdown, handle).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn bind_failure_records_port_in_use() {
    // First listener camps on a real port.
    let camp = tokio::net::TcpListener::bind(loopback_addr(0))
      .await
      .expect("camp bind");
    let camp_addr = camp.local_addr().expect("camp addr");
    // Now ask the proxy to bind the same port — must surface
    // PortInUse and exit cleanly. Strict single-port mode
    // (`port_scan_max_offset: 0`) so the scan can't silently land on
    // an adjacent free port and turn the assertion green for the
    // wrong reason.
    let ctx = MethodContext::new(ShutdownToken::new());
    let state = ProxyState::from_context(&ctx, false, true);
    let token = ctx.shutdown.clone();
    let status = new_status_cell();
    serve_with_options(
      state,
      camp_addr,
      token,
      Arc::clone(&status),
      ServeOptions {
        port_scan_max_offset: 0,
        ..ServeOptions::default()
      },
    )
    .await
    .expect("bind failure must not error");
    let observed = status.read().unwrap().clone();
    match observed {
      ProxyStatus::PortInUse { addr } => assert_eq!(addr, camp_addr),
      other => panic!("expected PortInUse, got {other:?}"),
    }
    drop(camp);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn bind_scans_to_next_free_port_when_base_taken() {
    // Camp on a real port; the proxy will try to bind the same port
    // and must land one slot up. Bound to localhost ephemeral so the
    // test never collides with whatever's listening on 11434 on the
    // dev box.
    let camp = tokio::net::TcpListener::bind(loopback_addr(0))
      .await
      .expect("camp bind");
    let camp_addr = camp.local_addr().expect("camp addr");
    let ctx = MethodContext::new(ShutdownToken::new());
    let state = ProxyState::from_context(&ctx, false, true);
    let token = ctx.shutdown.clone();
    let status = new_status_cell();
    // Run the listener under a shutdown so the test can clean up; the
    // scan should land on `camp_addr.port() + 1` (or higher if that
    // is also taken on the test host, but the assertion below only
    // requires "different from camp").
    let token_for_serve = token.clone();
    let status_clone = Arc::clone(&status);
    let handle = tokio::spawn(async move {
      serve(state, camp_addr, token_for_serve, status_clone)
        .await
        .expect("proxy serve returns Ok even on bind failure");
    });
    // Poll the status cell until Listening lands — the scan may sleep
    // a few ms across a few syscalls on a loaded box.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let bound_addr = loop {
      if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
        break addr;
      }
      if std::time::Instant::now() > deadline {
        panic!(
          "proxy never reached Listening; status = {:?}",
          status.read().unwrap().clone()
        );
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_ne!(
      bound_addr, camp_addr,
      "scan should not have re-bound the camp port"
    );
    assert_eq!(
      bound_addr.ip(),
      camp_addr.ip(),
      "scan must stay on the same loopback IP"
    );
    let offset = bound_addr.port() - camp_addr.port();
    assert!(
      (1..=DEFAULT_PORT_SCAN_MAX_OFFSET).contains(&offset),
      "scan landed at +{offset}, expected within 1..={DEFAULT_PORT_SCAN_MAX_OFFSET}"
    );
    shutdown_proxy(token, handle).await;
    drop(camp);
  }

  // Echo verification against `fake_llama_server` lives in
  // tests/proxy_echo_verification.rs — inline `#[cfg(test)] mod
  // tests` cannot reach `CARGO_BIN_EXE_fake_llama_server` (cargo
  // sets that env var only when building integration tests under
  // `tests/`).
}
