//! Single-shot JSON-RPC client over a Unix-domain socket.
//!
//! Owned by both the TUI (Unit 6) and the non-interactive CLI subcommands
//! (Unit 8). Holds a `UnixStream` and an integer id counter; each `call`
//! writes one request frame and reads one response frame back. There is
//! no pipelining or multiplexing — connections are cheap and the daemon
//! is local. If a method needs streaming progress (e.g. `pull`), it will
//! land in a later unit as a paired `pull_status` polling method, not as
//! server-pushed messages.

use std::{io, path::Path, time::Duration};

use serde_json::Value;
use tokio::{net::UnixStream, time::timeout};

use super::framing::{read_frame, write_frame, FrameError};
use super::protocol::{ErrorObject, Request, Response};

/// Default timeout for a single `call`. Long enough for warm-cache
/// operations like `ping` but short enough that a wedged daemon doesn't
/// hang an agent script.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors a caller of `Client::call` may see.
#[derive(Debug)]
pub enum ClientError {
  /// `connect()` failed — daemon socket missing or unreachable.
  Connect(io::Error),
  /// Frame-level transport problem.
  Frame(FrameError),
  /// Response body wasn't valid JSON-RPC.
  Decode(serde_json::Error),
  /// Daemon returned a JSON-RPC error object.
  Remote(ErrorObject),
  /// Local request body couldn't be serialised.
  Encode(serde_json::Error),
  /// Call exceeded the supplied timeout.
  Timeout(Duration),
}

impl std::fmt::Display for ClientError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Connect(e) => write!(f, "could not connect to daemon socket: {e}"),
      Self::Frame(e) => write!(f, "ipc frame error: {e}"),
      Self::Decode(e) => write!(f, "could not decode daemon response: {e}"),
      Self::Encode(e) => write!(f, "could not encode request body: {e}"),
      Self::Remote(err) => write!(f, "daemon error {}: {}", err.code, err.message),
      Self::Timeout(d) => write!(f, "ipc call exceeded {:?}", d),
    }
  }
}

impl std::error::Error for ClientError {}

impl From<FrameError> for ClientError {
  fn from(e: FrameError) -> Self {
    Self::Frame(e)
  }
}

/// JSON-RPC client. Owns one open `UnixStream`. Drop the value to close
/// the connection. Cloning is intentionally not supported — share via
/// `tokio::sync::Mutex` or open a new connection per task.
pub struct Client {
  stream: UnixStream,
  next_id: i64,
  /// Once a `call` is cancelled mid-frame by the timeout, the underlying
  /// byte stream is in an undefined position. Mark the client poisoned
  /// so every subsequent `call` short-circuits with a clear error and
  /// the user is forced to reconnect, instead of silently corrupting
  /// the next request with leftover payload.
  poisoned: bool,
}

impl Client {
  /// Open a fresh connection to the daemon at `socket_path`. The daemon
  /// must already be running; auto-spawn lives one layer up (CLI / TUI
  /// orchestration) so callers can honour `--no-spawn`.
  pub async fn connect(socket_path: &Path) -> Result<Self, ClientError> {
    let stream = UnixStream::connect(socket_path)
      .await
      .map_err(ClientError::Connect)?;
    Ok(Self {
      stream,
      next_id: 1,
      poisoned: false,
    })
  }

  /// Returns true if the client has been poisoned by a prior timeout
  /// and must be reconnected before further use.
  pub fn is_poisoned(&self) -> bool {
    self.poisoned
  }

  /// Issue one JSON-RPC call with the default timeout. Returns the
  /// `result` field on success or the structured `error` on protocol
  /// failure. Transport problems surface as `ClientError::Frame`.
  pub async fn call(&mut self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
    self
      .call_with_timeout(method, params, DEFAULT_CALL_TIMEOUT)
      .await
  }

  /// Same as `call` but with a caller-supplied timeout.
  ///
  /// On `ClientError::Timeout`, the in-flight `write_frame` / `read_frame`
  /// may have transferred a partial frame, so the byte stream's framing
  /// is undefined. The client is poisoned in that case and all
  /// subsequent calls return `Timeout` until the caller drops it and
  /// reconnects.
  pub async fn call_with_timeout(
    &mut self,
    method: &str,
    params: Option<Value>,
    deadline: Duration,
  ) -> Result<Value, ClientError> {
    if self.poisoned {
      return Err(ClientError::Timeout(deadline));
    }
    let id = self.next_id;
    self.next_id = self.next_id.wrapping_add(1);
    let req = Request::new(id, method, params);
    let body = serde_json::to_vec(&req).map_err(ClientError::Encode)?;

    let interaction = async {
      write_frame(&mut self.stream, &body).await?;
      let resp_bytes = read_frame(&mut self.stream).await?;
      let resp: Response = serde_json::from_slice(&resp_bytes).map_err(ClientError::Decode)?;
      if let Some(err) = resp.error {
        return Err(ClientError::Remote(err));
      }
      Ok(resp.result.unwrap_or(Value::Null))
    };

    match timeout(deadline, interaction).await {
      Ok(result) => result,
      Err(_) => {
        self.poisoned = true;
        Err(ClientError::Timeout(deadline))
      }
    }
  }
}
