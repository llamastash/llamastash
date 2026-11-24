//! Control-plane HTTP listener: JSON-RPC over `POST /rpc`, bearer-token
//! authed, on a loopback `TcpListener`.
//!
//! Every route except `GET /health` needs a valid bearer token. The
//! token and URL are written to `runtime.json` at startup so clients
//! can attach. See [`super::auth`] for the token shape and
//! [`super::runtime_file`] for the on-disk handoff.

use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::{atomic::Ordering, Arc, Mutex as StdMutex},
  time::Duration,
};

use anyhow::Result;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle, time::Instant};

use super::auth::IpcToken;
use crate::daemon::context::MethodContext;
use crate::daemon::shutdown::ShutdownToken;
use crate::ipc::methods::dispatch_request;
use crate::ipc::protocol::{
  ErrorCode, ErrorObject, Request as RpcRequest, Response as RpcResponse,
};
use crate::util::http_auth::extract_bearer;

/// Default control-plane port. Above IANA's registered band
/// (1–49151) and below the ephemeral range (49152+ on Linux), so it
/// does not collide with dynamic allocations or common services.
/// Deliberately outside the `11434`–`11440` proxy family, so a proxy
/// scan never walks over the daemon's listener. Clients read the URL
/// from `runtime.json`, not this port.
pub const DEFAULT_CONTROL_PORT: u16 = 48134;

/// How many ports above [`DEFAULT_CONTROL_PORT`] the listener will
/// probe for a free slot before giving up. Matches the proxy
/// listener's scan window so port-collision failure modes stay
/// symmetric between the two listeners.
pub const DEFAULT_PORT_SCAN_MAX_OFFSET: u16 = 5;

/// Maximum request body the control plane will accept on a single
/// request. JSON-RPC envelopes for our methods land under ~10 KiB
/// even with large `start_model` params, so 256 KiB is roomy and
/// bounds memory under a malicious client.
const MAX_RPC_BODY_BYTES: usize = 256 * 1024;

/// Maximum time to wait for in-flight connections after shutdown is
/// triggered before dropping them. The proxy listener uses the same
/// budget (see [`crate::proxy::server::DRAIN_TIMEOUT`]) so the two
/// listeners surrender within one window.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Outcome of [`bind_with_scan`].
enum BindOutcome {
  Bound(TcpListener),
  AllPortsInUse {
    last_addr: SocketAddr,
  },
  Failed {
    addr: SocketAddr,
    error: std::io::Error,
  },
}

/// Walk `[port, port + max_offset]` looking for a free slot.
/// `AddrInUse` advances. Every other error is fatal.
async fn bind_with_scan(base: SocketAddr, max_offset: u16) -> BindOutcome {
  let mut last_addr = base;
  for offset in 0..=max_offset {
    let Some(port) = base.port().checked_add(offset) else {
      break;
    };
    let candidate = SocketAddr::new(base.ip(), port);
    last_addr = candidate;
    match TcpListener::bind(&candidate).await {
      Ok(l) => return BindOutcome::Bound(l),
      Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
      Err(error) => {
        return BindOutcome::Failed {
          addr: candidate,
          error,
        };
      }
    }
  }
  BindOutcome::AllPortsInUse { last_addr }
}

/// Build the canonical loopback `SocketAddr` from a port. The host is
/// fixed at `127.0.0.1` because the control plane never binds LAN. There is
/// deliberately no host knob here: `proxy.host` opts only the *proxy
/// data plane* into LAN exposure (see `crate::proxy::server::listen_addr`).
/// The control plane stays loopback + same-UID-trust regardless.
pub fn loopback_addr(port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Result of binding the listener. The daemon converts the bound
/// `SocketAddr` into an `ipc_url` for `runtime.json`.
pub enum BindResult {
  Bound {
    listener: TcpListener,
    addr: SocketAddr,
  },
  AllPortsInUse {
    last_addr: SocketAddr,
  },
  Failed {
    addr: SocketAddr,
    error: String,
  },
}

/// Bind the control-plane listener with the default scan window
/// applied. When the caller passed port 0, the bound address carries
/// the kernel-assigned port. Caller owns logging and lifecycle.
pub async fn bind(addr: SocketAddr) -> BindResult {
  match bind_with_scan(addr, DEFAULT_PORT_SCAN_MAX_OFFSET).await {
    BindOutcome::Bound(listener) => {
      let bound = listener.local_addr().unwrap_or(addr);
      BindResult::Bound {
        listener,
        addr: bound,
      }
    }
    BindOutcome::AllPortsInUse { last_addr } => BindResult::AllPortsInUse { last_addr },
    BindOutcome::Failed { addr, error } => {
      let msg: String = error.to_string().chars().take(256).collect();
      BindResult::Failed { addr, error: msg }
    }
  }
}

/// Run the control-plane accept loop until `shutdown` is triggered.
pub async fn serve(
  listener: TcpListener,
  token: Arc<IpcToken>,
  ctx: MethodContext,
  shutdown: ShutdownToken,
) -> Result<()> {
  let tracker = Arc::new(ConnectionTracker {
    active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    handles: StdMutex::new(Vec::new()),
  });

  loop {
    tokio::select! {
      _ = shutdown.wait_until_triggered() => {
        log::info!("control plane: shutdown signalled; closing listener");
        break;
      }
      accept = listener.accept() => {
        match accept {
          Ok((stream, _peer)) => {
            let conn_ctx = ctx.clone();
            let conn_token = Arc::clone(&token);
            let conn_tracker = Arc::clone(&tracker);
            conn_tracker.active.fetch_add(1, Ordering::SeqCst);
            // Increment the dispatcher's live-connection gauge so the
            // `version` IPC method reflects active control-plane
            // sessions.
            let counter = ctx.active_connections.clone();
            counter.fetch_add(1, Ordering::SeqCst);
            // Stamped at both ends so a one-shot CLI call that opens and
            // closes between two idle polls still registers as activity.
            let activity = ctx.activity.clone();
            activity.mark();
            let task_tracker = Arc::clone(&conn_tracker);
            let handle = tokio::spawn(async move {
              serve_connection(stream, conn_token, conn_ctx).await;
              activity.mark();
              counter.fetch_sub(1, Ordering::SeqCst);
              task_tracker.active.fetch_sub(1, Ordering::SeqCst);
            });
            push_handle(&conn_tracker, handle);
          }
          Err(e) => {
            log::warn!("control plane: accept failed: {e}");
          }
        }
      }
    }
  }

  drain(tracker).await;
  Ok(())
}

async fn drain(tracker: Arc<ConnectionTracker>) {
  let deadline = Instant::now() + DRAIN_TIMEOUT;
  let poll_interval = Duration::from_millis(50);
  while tracker.active.load(Ordering::SeqCst) > 0 {
    let remaining = deadline.checked_duration_since(Instant::now());
    let Some(time_left) = remaining else {
      let still_active = tracker.active.load(Ordering::SeqCst);
      log::warn!(
        "control plane drain deadline reached with {still_active} connection(s) still active; aborting"
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

async fn serve_connection(stream: tokio::net::TcpStream, token: Arc<IpcToken>, ctx: MethodContext) {
  let io = TokioIo::new(stream);
  let service = service_fn(move |req| {
    let token = Arc::clone(&token);
    let ctx = ctx.clone();
    async move { Ok::<_, hyper::Error>(route(req, token, ctx).await) }
  });
  let mut builder = http1::Builder::new();
  builder
    .keep_alive(true)
    .timer(hyper_util::rt::TokioTimer::new())
    .header_read_timeout(Duration::from_secs(30));
  if let Err(e) = builder.serve_connection(io, service).await {
    log::debug!("control plane: connection ended with: {e}");
  }
}

async fn route(
  req: Request<Incoming>,
  token: Arc<IpcToken>,
  ctx: MethodContext,
) -> Response<Full<Bytes>> {
  match (req.method(), req.uri().path()) {
    (&Method::GET, "/health") => health_response(),
    (&Method::POST, "/rpc") => {
      if let Some(reject) = check_bearer(&req, &token) {
        return reject;
      }
      rpc_handler(req, ctx).await
    }
    _ => not_found(),
  }
}

/// `/health` returns a tiny JSON payload with no secrets. Used by the
/// CLI's daemon-attach handshake: probe before retrying with a bearer
/// header.
fn health_response() -> Response<Full<Bytes>> {
  let body = serde_json::json!({"status": "ok"}).to_string();
  Response::builder()
    .status(StatusCode::OK)
    .header(CONTENT_TYPE, "application/json")
    .body(Full::new(Bytes::from(body)))
    .expect("static response is well-formed")
}

/// Returns `Some(401-response)` to reject and short-circuit, or
/// `None` to let the request proceed. Inverted from a `Result` so the
/// big `Response` type stays out of the `Err` slot (clippy
/// `result_large_err`). Constant-time comparison happens inside
/// [`IpcToken::verify`]; length and presence checks happen here.
fn check_bearer(req: &Request<Incoming>, token: &IpcToken) -> Option<Response<Full<Bytes>>> {
  let Some(header) = req
    .headers()
    .get(AUTHORIZATION)
    .and_then(|h| h.to_str().ok())
  else {
    return Some(unauthorized("missing Authorization header"));
  };
  let Some(candidate) = extract_bearer(header) else {
    return Some(unauthorized("Authorization header is not a Bearer scheme"));
  };
  if !token.verify(candidate) {
    return Some(unauthorized("bearer token rejected"));
  }
  None
}

fn unauthorized(reason: &str) -> Response<Full<Bytes>> {
  let body = serde_json::json!({"error": "unauthorized", "reason": reason}).to_string();
  Response::builder()
    .status(StatusCode::UNAUTHORIZED)
    .header(CONTENT_TYPE, "application/json")
    .body(Full::new(Bytes::from(body)))
    .expect("static response is well-formed")
}

fn not_found() -> Response<Full<Bytes>> {
  Response::builder()
    .status(StatusCode::NOT_FOUND)
    .header(CONTENT_TYPE, "application/json")
    .body(Full::new(Bytes::from(r#"{"error":"not found"}"#)))
    .expect("static response is well-formed")
}

/// `POST /rpc`: parse the bounded body as a JSON-RPC 2.0 request and
/// hand off to `dispatch_request`. The HTTP status is always `200`,
/// even for JSON-RPC-level errors. Clients look at the envelope's
/// `error` field.
async fn rpc_handler(req: Request<Incoming>, ctx: MethodContext) -> Response<Full<Bytes>> {
  let body = match collect_body(req, MAX_RPC_BODY_BYTES).await {
    Ok(b) => b,
    Err(resp) => return *resp,
  };
  let request: RpcRequest = match serde_json::from_slice(&body) {
    Ok(r) => r,
    Err(e) => {
      return json_rpc_response(RpcResponse::err(
        Value::Null,
        ErrorObject::new(
          ErrorCode::ParseError,
          format!("invalid json-rpc request: {e}"),
        ),
      ));
    }
  };
  let response = dispatch_request(&ctx, request).await;
  json_rpc_response(response)
}

fn json_rpc_response(resp: RpcResponse) -> Response<Full<Bytes>> {
  let bytes = match serde_json::to_vec(&resp) {
    Ok(b) => b,
    Err(e) => {
      log::error!("control plane: failed to serialise response envelope: {e}");
      return Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(r#"{"error":"internal"}"#)))
        .expect("static response is well-formed");
    }
  };
  Response::builder()
    .status(StatusCode::OK)
    .header(CONTENT_TYPE, "application/json")
    .body(Full::new(Bytes::from(bytes)))
    .expect("dynamic response with checked headers")
}

/// Read the request body up to `max_bytes`, answering HTTP 413 above
/// it. The bound refuses pathological or malicious payloads, not
/// real callers.
async fn collect_body(
  req: Request<Incoming>,
  max_bytes: usize,
) -> Result<Bytes, Box<Response<Full<Bytes>>>> {
  let collected = match req.into_body().collect().await {
    Ok(c) => c,
    Err(e) => {
      return Err(Box::new(
        Response::builder()
          .status(StatusCode::BAD_REQUEST)
          .header(CONTENT_TYPE, "application/json")
          .body(Full::new(Bytes::from(
            serde_json::json!({"error": "body read failed", "reason": e.to_string()}).to_string(),
          )))
          .expect("static response is well-formed"),
      ));
    }
  };
  let bytes = collected.to_bytes();
  if bytes.len() > max_bytes {
    return Err(Box::new(
      Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(
          serde_json::json!({"error": "payload too large", "max_bytes": max_bytes}).to_string(),
        )))
        .expect("static response is well-formed"),
    ));
  }
  Ok(bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn loopback_addr_is_always_loopback() {
    // Guard for the LAN-exposed-proxy feature: the proxy data plane can
    // bind a routable host, but the control plane must stay loopback no
    // matter what. `loopback_addr` has no host parameter by design.
    for port in [0u16, 1, 48134, 11434, 65535] {
      assert!(
        loopback_addr(port).ip().is_loopback(),
        "control plane must never leave loopback (port {port})"
      );
    }
  }

  #[tokio::test]
  async fn bind_returns_bound_addr_on_ephemeral_port() {
    let addr = loopback_addr(0);
    match bind(addr).await {
      BindResult::Bound { addr, .. } => {
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(addr.port() > 0, "kernel should assign a non-zero port");
      }
      BindResult::AllPortsInUse { .. } | BindResult::Failed { .. } => {
        panic!("ephemeral bind should succeed in tests")
      }
    }
  }

  #[tokio::test]
  async fn bind_returns_all_ports_in_use_when_window_exhausted() {
    // Bind a kernel-assigned port, then bind it again under a
    // 0-offset window so the single-slot scan must fail.
    let occupy = TcpListener::bind("127.0.0.1:0").await.expect("occupy");
    let busy = occupy.local_addr().unwrap();
    match bind_with_scan(busy, 0).await {
      BindOutcome::AllPortsInUse { last_addr } => assert_eq!(last_addr, busy),
      _ => panic!("expected AllPortsInUse on a contended port with 0 offset"),
    }
  }
}
