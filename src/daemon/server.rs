//! UnixListener accept loop.
//!
//! Responsibilities, in order of arrival:
//! 1. Pop a connection from the listener.
//! 2. Read peer credentials. Reject anything that isn't the daemon's UID
//!    *before* parsing a request frame.
//! 3. Spawn a per-connection task that runs a serial request loop:
//!    `read_frame -> parse JSON-RPC -> dispatch -> write_frame`.
//! 4. Track live-connection count for `version` reporting.
//! 5. Honour the shutdown token: stop accepting on trigger; drain
//!    in-flight tasks up to the supplied deadline.

use std::{
  sync::{atomic::Ordering, Arc, Mutex as StdMutex},
  time::Duration,
};

use anyhow::Result;
use serde_json::Value;
use tokio::{net::UnixListener, task::JoinHandle, time::Instant};

use super::peercred::read_peer_credentials;
use crate::ipc::{
  framing::{read_frame, write_frame, FrameError},
  methods::{dispatch_request, MethodContext},
  protocol::{ErrorCode, ErrorObject, Request, Response},
};

/// Maximum time to wait for in-flight requests after shutdown is
/// triggered before dropping them.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Run the daemon's accept loop until `ctx.shutdown` is triggered, then
/// drain in-flight tasks for up to `DRAIN_TIMEOUT`. On drain timeout
/// we explicitly abort the outstanding per-connection task handles
/// rather than relying on runtime drop to do it for us, so the
/// behaviour is predictable across tokio versions.
pub async fn serve(listener: UnixListener, ctx: MethodContext) -> Result<()> {
  let tracker = Arc::new(ConnectionTracker {
    counter: ctx.active_connections.clone(),
    notify: tokio::sync::Notify::new(),
    handles: StdMutex::new(Vec::new()),
  });

  loop {
    tokio::select! {
      _ = ctx.shutdown.wait_until_triggered() => {
        log::info!("shutdown signalled; closing listener");
        break;
      }
      accept = listener.accept() => {
        match accept {
          Ok((stream, _addr)) => {
            // Peercred check happens synchronously before we hand the
            // connection off — a rejected peer should never see a single
            // byte of our protocol.
            match read_peer_credentials(&stream) {
              Ok(cred) if (ctx.peer_authorizer)(cred) => {
                let conn_ctx = ctx.clone();
                let conn_tracker = tracker.clone();
                conn_tracker.counter.fetch_add(1, Ordering::SeqCst);
                let task_tracker = conn_tracker.clone();
                let handle = tokio::spawn(async move {
                  serve_connection(stream, conn_ctx).await;
                  let prev = task_tracker.counter.fetch_sub(1, Ordering::SeqCst);
                  if prev <= 1 {
                    task_tracker.notify.notify_waiters();
                  }
                });
                if let Ok(mut handles) = conn_tracker.handles.lock() {
                  // Garbage-collect finished handles opportunistically so
                  // the vector doesn't grow unboundedly.
                  handles.retain(|h| !h.is_finished());
                  handles.push(handle);
                }
              }
              Ok(cred) => {
                log::warn!(
                  "rejecting connection: peer uid {} doesn't match daemon uid",
                  cred.uid
                );
                drop(stream);
              }
              Err(e) => {
                log::warn!("peercred read failed; closing connection: {e}");
                drop(stream);
              }
            }
          }
          Err(e) => {
            log::warn!("listener.accept failed: {e}");
            // Don't exit the loop on transient accept failures; the
            // listener stays bound and the next iteration retries.
          }
        }
      }
    }
  }

  // Drain phase: wait until the counter reaches zero, capped at DRAIN_TIMEOUT.
  let deadline = Instant::now() + DRAIN_TIMEOUT;
  while tracker.counter.load(Ordering::SeqCst) > 0 {
    let remaining = deadline.checked_duration_since(Instant::now());
    let Some(timeout) = remaining else {
      let still_active = tracker.counter.load(Ordering::SeqCst);
      log::warn!(
        "drain deadline reached with {still_active} connection(s) still active; aborting"
      );
      // Explicitly abort outstanding tasks so any partial frame in
      // their write buffers is dropped along with the task rather
      // than appearing on the wire after subsequent reconnects.
      if let Ok(mut handles) = tracker.handles.lock() {
        for h in handles.drain(..) {
          h.abort();
        }
      }
      break;
    };
    let notified = tracker.notify.notified();
    if tracker.counter.load(Ordering::SeqCst) == 0 {
      break;
    }
    let _ = tokio::time::timeout(timeout, notified).await;
  }
  Ok(())
}

struct ConnectionTracker {
  counter: Arc<std::sync::atomic::AtomicUsize>,
  notify: tokio::sync::Notify,
  handles: StdMutex<Vec<JoinHandle<()>>>,
}

/// Run the per-connection request loop. Returns when the peer closes the
/// socket, sends a malformed frame, or the daemon shuts down.
async fn serve_connection(stream: tokio::net::UnixStream, ctx: MethodContext) {
  let (read_half, write_half) = stream.into_split();
  let mut reader = tokio::io::BufReader::new(read_half);
  let mut writer = tokio::io::BufWriter::new(write_half);

  loop {
    let frame = tokio::select! {
      _ = ctx.shutdown.wait_until_triggered() => return,
      f = read_frame(&mut reader) => f,
    };
    let body = match frame {
      Ok(b) => b,
      Err(FrameError::PeerClosed) => return, // graceful close
      Err(e) => {
        log::debug!("connection dropped on frame error: {e}");
        return;
      }
    };

    let response: Response = match serde_json::from_slice::<Request>(&body) {
      Ok(req) => dispatch_request(&ctx, req).await,
      Err(e) => Response::err(
        Value::Null,
        ErrorObject::new(
          ErrorCode::ParseError,
          format!("invalid json-rpc request: {e}"),
        ),
      ),
    };

    let response_bytes = match serde_json::to_vec(&response) {
      Ok(b) => b,
      Err(e) => {
        log::error!("could not encode response: {e}; closing connection");
        return;
      }
    };

    if let Err(e) = write_frame(&mut writer, &response_bytes).await {
      log::debug!("write_frame failed: {e}; closing connection");
      return;
    }
  }
}
