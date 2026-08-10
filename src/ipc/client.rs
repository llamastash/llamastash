//! Single-shot JSON-RPC client over HTTP loopback.
//!
//! The client talks `POST {ipc_url}/rpc` carrying the JSON-RPC 2.0
//! envelope, with the
//! per-daemon bearer token baked into a default `Authorization`
//! header. Attach order: `LLAMASTASH_IPC_URL` +
//! `LLAMASTASH_IPC_TOKEN` env (both required if either set), then
//! `runtime.json` under the state directory, else `Connect` error.
//!
//! This module is the canonical IPC client for every TUI / CLI
//! surface.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use serde_json::Value;

use super::protocol::{ErrorObject, Request, Response};
use crate::daemon::runtime_file;

/// Default timeout for a single `call`. Long enough for warm-cache
/// operations like `ping` but short enough that a wedged daemon doesn't
/// hang an agent script.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Env-var override pair. When **either** is set both must be set or
/// the client treats the configuration as malformed and falls through
/// to `Connect` rather than silently using the partially-overridden
/// values.
pub const ENV_IPC_URL: &str = "LLAMASTASH_IPC_URL";
pub const ENV_IPC_TOKEN: &str = "LLAMASTASH_IPC_TOKEN";

/// Errors a caller of `Client::call` may see.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
  /// `connect()` failed — runtime.json missing / unreadable AND env
  /// overrides absent, OR the URL was unreachable on first probe.
  #[error("could not connect to daemon: {0}")]
  Connect(String),
  /// HTTP transport problem (DNS, TCP, server hangup).
  #[error("ipc transport error: {0}")]
  Transport(String),
  /// Bearer token rejected by the daemon (HTTP 401).
  #[error("daemon rejected the bearer token (401)")]
  Unauthorized,
  /// HTTP non-200/401 status. Carries the status code and the
  /// best-effort body string (truncated). Maps to
  /// `DAEMON_UNREACHABLE` at the exit-code layer today.
  #[error("daemon returned HTTP {status}: {body}")]
  BadStatus { status: u16, body: String },
  /// Response body wasn't valid JSON-RPC.
  #[error("could not decode daemon response: {0}")]
  Decode(#[source] serde_json::Error),
  /// Daemon returned a JSON-RPC error object.
  #[error("daemon error {}: {}", .0.code, .0.message)]
  Remote(ErrorObject),
  /// Local request body couldn't be serialised.
  #[error("could not encode request body: {0}")]
  Encode(#[source] serde_json::Error),
  /// Call exceeded the supplied timeout.
  #[error("ipc call exceeded {0:?}")]
  Timeout(Duration),
}

/// Did the send fail before the daemon produced any response?
///
/// `is_connect` alone only covers a failure to *establish* the
/// connection. A daemon shutting down mid-request — the idle-timeout
/// window — or a stale pooled keep-alive it already closed surfaces as
/// hyper's "connection closed before message completed", with no
/// `io::Error` anywhere in the source chain. Both mean the caller never
/// got an answer, so both must read as "daemon unreachable" instead of
/// flapping the exit code between 65 and 71 on timing. A failure *after*
/// a response arrived stays [`ClientError::Transport`].
fn never_reached_daemon(e: &reqwest::Error) -> bool {
  e.is_connect() || e.is_request()
}

/// JSON-RPC client. Holds a pooled `reqwest::Client` plus the resolved
/// control-plane URL. Cheap to drop and reconnect. Cloning would
/// duplicate the pool, so we discourage it — share via a wrapping
/// `Arc<Mutex<Client>>` if multiple tasks need to call concurrently.
pub struct Client {
  http: reqwest::Client,
  ipc_url: String,
  next_id: i64,
}

impl std::fmt::Debug for Client {
  // Omit `http` (no useful Debug) and `next_id` (internal counter); the
  // URL is the only field worth seeing in a panic / unwrap message.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ipc::Client")
      .field("ipc_url", &self.ipc_url)
      .finish_non_exhaustive()
  }
}

impl Client {
  /// Attach to the daemon by reading the bearer token + URL.
  ///
  /// Resolution order:
  /// 1. `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` env (both required
  ///    when either is set); partial overrides are an error.
  /// 2. `state_dir/runtime.json`, if present.
  /// 3. Otherwise `ClientError::Connect`.
  ///
  /// `path` is interpreted as the daemon's state directory. As a
  /// transitional accommodation, if the caller passes a file path (or
  /// a non-existent path) whose **parent** is a directory holding
  /// `runtime.json`, the parent is used as the state directory. This
  /// lets existing callers that hand in a socket path (e.g.,
  /// `…/daemon.sock`) keep working while the migration to explicit
  /// `state_dir` arguments lands.
  pub async fn connect(path: &Path) -> Result<Self, ClientError> {
    let state_dir = effective_state_dir(path);
    let (ipc_url, ipc_token) = resolve_attach(&state_dir)?;
    Self::with_url_and_token(&ipc_url, &ipc_token)
  }

  /// Lower-level entrypoint that skips the runtime.json + env lookup.
  /// Used by callers that already have the URL+token in hand (the
  /// reconcile path, certain tests). Does not validate that the URL
  /// is reachable — the first `call` surfaces transport failures.
  pub fn with_url_and_token(ipc_url: &str, ipc_token: &str) -> Result<Self, ClientError> {
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
    let mut headers = HeaderMap::new();
    let bearer = format!("Bearer {ipc_token}");
    let mut auth = HeaderValue::from_str(&bearer)
      .map_err(|e| ClientError::Connect(format!("bearer token contains invalid bytes: {e}")))?;
    auth.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let http = reqwest::Client::builder()
      .default_headers(headers)
      .build()
      .map_err(|e| ClientError::Connect(format!("reqwest build failed: {e}")))?;

    Ok(Self {
      http,
      ipc_url: ipc_url.trim_end_matches('/').to_owned(),
      next_id: 1,
    })
  }

  /// Issue one JSON-RPC call with the default timeout. Returns the
  /// `result` field on success or the structured `error` on protocol
  /// failure. Transport problems surface as `ClientError::Transport`.
  pub async fn call(&mut self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
    self
      .call_with_timeout(method, params, DEFAULT_CALL_TIMEOUT)
      .await
  }

  /// Same as `call` but with a caller-supplied timeout.
  pub async fn call_with_timeout(
    &mut self,
    method: &str,
    params: Option<Value>,
    deadline: Duration,
  ) -> Result<Value, ClientError> {
    let id = self.next_id;
    self.next_id = self.next_id.wrapping_add(1);
    let req = Request::new(id, method, params);
    let body = serde_json::to_vec(&req).map_err(ClientError::Encode)?;

    let url = format!("{}/rpc", self.ipc_url);
    let response =
      match tokio::time::timeout(deadline, self.http.post(&url).body(body).send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
          if never_reached_daemon(&e) {
            return Err(ClientError::Connect(e.to_string()));
          }
          return Err(ClientError::Transport(e.to_string()));
        }
        Err(_) => return Err(ClientError::Timeout(deadline)),
      };

    match response.status().as_u16() {
      200 => {}
      401 => return Err(ClientError::Unauthorized),
      other => {
        let error_body = match tokio::time::timeout(deadline, response.text()).await {
          Ok(Ok(s)) => s.chars().take(512).collect::<String>(),
          _ => String::from("<body read failed>"),
        };
        return Err(ClientError::BadStatus {
          status: other,
          body: error_body,
        });
      }
    }

    let resp_bytes = match tokio::time::timeout(deadline, response.bytes()).await {
      Ok(Ok(b)) => b,
      Ok(Err(e)) => return Err(ClientError::Transport(e.to_string())),
      Err(_) => return Err(ClientError::Timeout(deadline)),
    };
    let resp: Response = serde_json::from_slice(&resp_bytes).map_err(ClientError::Decode)?;
    if let Some(err) = resp.error {
      return Err(ClientError::Remote(err));
    }
    Ok(resp.result.unwrap_or(Value::Null))
  }

  /// Borrow the resolved IPC URL — handy for the daemon-status
  /// renderer that surfaces "Daemon at: http://…" alongside the PID.
  pub fn ipc_url(&self) -> &str {
    &self.ipc_url
  }
}

/// Interpret `path` as a state directory. When `path` is a file (or
/// missing), fall back to its parent — the accommodation that
/// lets `Client::connect(&socket_path)` keep working while the bulk
/// rename to explicit `state_dir` arguments lands.
fn effective_state_dir(path: &Path) -> PathBuf {
  if path.is_dir() {
    return path.to_owned();
  }
  path
    .parent()
    .map(Path::to_owned)
    .unwrap_or_else(|| path.to_owned())
}

/// Returns `(ipc_url, ipc_token)` per the documented attach order, or
/// a `Connect` error explaining why neither path resolved.
fn resolve_attach(state_dir: &Path) -> Result<(String, String), ClientError> {
  match (env_var(ENV_IPC_URL), env_var(ENV_IPC_TOKEN)) {
    (Some(url), Some(token)) => Ok((url, token)),
    (Some(_), None) => Err(ClientError::Connect(format!(
      "{ENV_IPC_URL} is set but {ENV_IPC_TOKEN} is missing; both must be set together"
    ))),
    (None, Some(_)) => Err(ClientError::Connect(format!(
      "{ENV_IPC_TOKEN} is set but {ENV_IPC_URL} is missing; both must be set together"
    ))),
    (None, None) => match runtime_file::load(state_dir) {
      Ok(Some(info)) => Ok((info.ipc_url, info.ipc_token)),
      Ok(None) => Err(ClientError::Connect(format!(
        "no runtime.json under {} and no env overrides; daemon may not be running",
        state_dir.display()
      ))),
      Err(e) => Err(ClientError::Connect(format!("read runtime.json: {e}"))),
    },
  }
}

fn env_var(key: &str) -> Option<String> {
  match std::env::var(key) {
    Ok(v) if !v.is_empty() => Some(v),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn with_url_and_token_strips_trailing_slash() {
    let c = Client::with_url_and_token("http://127.0.0.1:48134/", "token").expect("build");
    assert_eq!(c.ipc_url(), "http://127.0.0.1:48134");
  }

  #[test]
  fn with_url_and_token_rejects_bad_header_bytes() {
    // Newlines in a header value cannot be encoded — this guards
    // against an accidentally-pasted token that includes a CR/LF
    // injecting extra headers.
    let err = Client::with_url_and_token("http://127.0.0.1:48134", "bad\nvalue").unwrap_err();
    assert!(matches!(err, ClientError::Connect(_)));
  }

  #[test]
  fn resolve_attach_errors_on_partial_env_override() {
    // We can't reliably manipulate process-global env in unit
    // tests without serialising every test in this file behind a
    // mutex — env var resolution is covered end-to-end in
    // `tests/control_plane_client_test.rs`. Here
    // we only check the error message phrasing.
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    std::env::set_var(ENV_IPC_URL, "http://127.0.0.1:48134");
    std::env::remove_var(ENV_IPC_TOKEN);
    let err = resolve_attach(std::path::Path::new("/no/such/dir")).unwrap_err();
    std::env::remove_var(ENV_IPC_URL);
    match err {
      ClientError::Connect(msg) => {
        assert!(
          msg.contains(ENV_IPC_TOKEN),
          "msg should reference missing token: {msg}"
        );
      }
      _ => panic!("expected Connect error, got {err:?}"),
    }
  }

  /// A daemon that shuts down mid-request (the idle-timeout window) or a
  /// stale pooled keep-alive both surface as a request error wrapping an
  /// io kind, not as `is_connect`. Both must read as "unreachable" so
  /// the exit code doesn't flap between 65 and 71 on timing.
  #[tokio::test]
  async fn a_connection_closed_mid_request_reports_connect_not_transport() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    // Accept, then drop without answering — the client sees the peer
    // close after the connection was established.
    std::thread::spawn(move || {
      if let Ok((stream, _)) = listener.accept() {
        drop(stream);
      }
    });

    let mut client =
      Client::with_url_and_token(&format!("http://127.0.0.1:{port}"), "t").expect("client");
    let err = client
      .call("version", None)
      .await
      .expect_err("a closed connection must not succeed");
    assert!(
      matches!(err, ClientError::Connect(_)),
      "expected Connect, got {err:?}"
    );
  }
}
