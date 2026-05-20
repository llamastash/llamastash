//! HTTPS fetch client governed by the v2 fetch contract.
//!
//! Owns a single `reqwest::Client` configured with:
//! - host allowlist + redirect-chain inheritance (every hop checked),
//! - redirect cap (3),
//! - HTTPS-only,
//! - per-call body-size cap with streaming + `Content-Length` pre-check,
//! - minimal `User-Agent` = `llamastash/<CARGO_PKG_VERSION>`,
//! - no opportunistic `Authorization` header. `GITHUB_TOKEN` /
//!   `GH_TOKEN` env vars are never read; callers add bearer auth
//!   explicitly (e.g. Unit 9 for `HF_TOKEN`).
//!
//! `FetchClient::offline()` returns a stub that fails every call with
//! `FetchError::Offline`; the wizard's step-resolver consumes that to
//! emit "skipped, see hint" rather than mid-fetch panics.

use std::time::Duration;

use futures::StreamExt;
use reqwest::redirect::Policy;
use serde::de::DeserializeOwned;

use crate::init::fetch_policy::{check_url, HostAllowlist, UrlRefusal, MAX_REDIRECTS};

/// What may go wrong on a fetch. Stable surface — agent consumers
/// branch on the variant.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
  #[error("network egress is disabled (LLAMASTASH_OFFLINE / --offline)")]
  Offline,
  #[error("URL host `{host}` is not on the allowlist; refused before connecting")]
  HostNotAllowed { host: String },
  #[error("URL scheme `{got}` is not allowed (HTTPS only)")]
  HttpNotAllowed { got: String },
  #[error("URL is missing a host component")]
  MissingHost,
  #[error("redirect chain exceeded {MAX_REDIRECTS} hops")]
  TooManyRedirects,
  #[error("response body exceeded the {cap}-byte cap")]
  BodyOverflow { cap: u64 },
  #[error("rate-limited by remote (status {status}); retry after backoff")]
  RateLimited { status: u16 },
  #[error("remote returned status {status}")]
  RemoteStatus { status: u16 },
  #[error("transport error: {0}")]
  Transport(String),
  #[error("body decode (JSON): {0}")]
  JsonDecode(String),
}

impl From<UrlRefusal> for FetchError {
  fn from(r: UrlRefusal) -> Self {
    match r {
      UrlRefusal::Scheme { got } => FetchError::HttpNotAllowed { got },
      UrlRefusal::MissingHost => FetchError::MissingHost,
      UrlRefusal::HostNotAllowed { host } => FetchError::HostNotAllowed { host },
    }
  }
}

/// Build-time inputs for [`FetchClient::new`]. Defaults mirror the
/// v2 fetch contract; the wizard / snapshot fetcher / GH Releases
/// installer all construct one of these and inject the result into
/// `hf-hub` for HF traffic (Unit 9).
#[derive(Debug, Clone)]
pub struct FetchClientConfig {
  pub allowlist: HostAllowlist,
  /// Per-request connect timeout. Short enough to fail fast on a
  /// rate-limited or wedged endpoint without dragging the wizard.
  pub connect_timeout: Duration,
  /// Total wall-clock per request, including streamed body read.
  /// Long enough for a few-hundred-megabyte download; Unit 9 sets a
  /// per-call cap when it knows the expected shard size.
  pub request_timeout: Duration,
  /// User-Agent header value. Defaults to `llamastash/<version>`;
  /// Unit 9 may attach a discriminator suffix.
  pub user_agent: String,
}

impl Default for FetchClientConfig {
  fn default() -> Self {
    Self {
      allowlist: HostAllowlist::default_v2(),
      connect_timeout: Duration::from_secs(10),
      request_timeout: Duration::from_secs(120),
      user_agent: format!("llamastash/{}", env!("CARGO_PKG_VERSION")),
    }
  }
}

/// The fetch client. Cheap to clone: the inner `reqwest::Client` is
/// `Arc`-backed.
#[derive(Debug, Clone)]
pub struct FetchClient {
  inner: Mode,
}

#[derive(Debug, Clone)]
enum Mode {
  Online {
    client: reqwest::Client,
    allowlist: HostAllowlist,
  },
  Offline,
}

impl FetchClient {
  /// Build a live client. The `reqwest::Client` uses the supplied
  /// allowlist for every redirect attempt — a redirect to a host not
  /// on the allowlist aborts the chain with `FetchError::HostNotAllowed`.
  pub fn new(cfg: FetchClientConfig) -> Result<Self, FetchError> {
    let allowlist_for_policy = cfg.allowlist.clone();
    let redirect_policy = Policy::custom(move |attempt| {
      if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error("redirect cap reached (fetch contract)");
      }
      if let Err(r) = check_url(attempt.url(), &allowlist_for_policy) {
        let reason: FetchError = r.into();
        return attempt.error(reason.to_string());
      }
      attempt.follow()
    });
    let client = reqwest::Client::builder()
      .user_agent(cfg.user_agent.clone())
      .connect_timeout(cfg.connect_timeout)
      .timeout(cfg.request_timeout)
      .redirect(redirect_policy)
      .https_only(true)
      .build()
      .map_err(|e| FetchError::Transport(e.to_string()))?;
    Ok(Self {
      inner: Mode::Online {
        client,
        allowlist: cfg.allowlist,
      },
    })
  }

  /// Build an offline stub. Every method returns `FetchError::Offline`.
  pub fn offline() -> Self {
    Self {
      inner: Mode::Offline,
    }
  }

  /// `true` when this client refuses to make network calls.
  pub fn is_offline(&self) -> bool {
    matches!(self.inner, Mode::Offline)
  }

  /// Expose the underlying `reqwest::Client` so Unit 9 can inject it
  /// into `hf-hub::HFClientBuilder::client()` (the with-client method
  /// confirmed in the Unit 1 hf-hub-client-injection spike).
  /// Returns `None` for offline clients; the wizard's HF download
  /// step short-circuits in that case before reaching the injection
  /// point.
  pub fn reqwest_client(&self) -> Option<reqwest::Client> {
    match &self.inner {
      Mode::Online { client, .. } => Some(client.clone()),
      Mode::Offline => None,
    }
  }

  /// GET `url`, return body bytes (capped at `max_bytes`). Refuses
  /// the request up-front if the URL fails the fetch contract. A
  /// `Content-Length` larger than `max_bytes` is refused before any
  /// body is read; streaming reads enforce the cap mid-flight too.
  pub async fn get_bytes(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
    let (bytes, _) = self.get_bytes_with_headers(url, max_bytes).await?;
    Ok(bytes)
  }

  /// Same contract as [`Self::get_bytes`] but also returns the response
  /// headers. Used by `init::hf_api` to read the `Link` header for
  /// cursor-based pagination.
  pub async fn get_bytes_with_headers(
    &self,
    url: &str,
    max_bytes: u64,
  ) -> Result<(Vec<u8>, reqwest::header::HeaderMap), FetchError> {
    let (client, allowlist) = match &self.inner {
      Mode::Online { client, allowlist } => (client, allowlist),
      Mode::Offline => return Err(FetchError::Offline),
    };
    let parsed =
      reqwest::Url::parse(url).map_err(|e| FetchError::Transport(format!("URL parse: {e}")))?;
    check_url(&parsed, allowlist)?;
    let response = client
      .get(parsed)
      .send()
      .await
      .map_err(translate_send_error)?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
      || (status == reqwest::StatusCode::FORBIDDEN
        && response
          .headers()
          .get("x-ratelimit-remaining")
          .and_then(|v| v.to_str().ok())
          == Some("0"))
    {
      return Err(FetchError::RateLimited {
        status: status.as_u16(),
      });
    }
    if !status.is_success() {
      return Err(FetchError::RemoteStatus {
        status: status.as_u16(),
      });
    }
    if let Some(len) = response.content_length() {
      if len > max_bytes {
        return Err(FetchError::BodyOverflow { cap: max_bytes });
      }
    }
    let headers = response.headers().clone();
    let mut total = 0_u64;
    let mut buf: Vec<u8> = Vec::with_capacity(
      response
        .content_length()
        .map(|n| n.min(max_bytes) as usize)
        .unwrap_or(0),
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
      let chunk = chunk.map_err(|e| FetchError::Transport(e.to_string()))?;
      total = total.saturating_add(chunk.len() as u64);
      if total > max_bytes {
        return Err(FetchError::BodyOverflow { cap: max_bytes });
      }
      buf.extend_from_slice(&chunk);
    }
    Ok((buf, headers))
  }

  /// GET `url`, parse the body as JSON into `T`. Body capped at
  /// `max_bytes` (typical JSON payloads fit well under 1 MiB; callers
  /// pass a conservative value so a misbehaving endpoint can't blow
  /// the heap).
  pub async fn get_json<T: DeserializeOwned>(
    &self,
    url: &str,
    max_bytes: u64,
  ) -> Result<T, FetchError> {
    let bytes = self.get_bytes(url, max_bytes).await?;
    serde_json::from_slice(&bytes).map_err(|e| FetchError::JsonDecode(e.to_string()))
  }

  /// Same as [`Self::get_json`] but also returns the response headers
  /// so callers can read `Link` (pagination), `ETag` (caching), etc.
  pub async fn get_json_with_headers<T: DeserializeOwned>(
    &self,
    url: &str,
    max_bytes: u64,
  ) -> Result<(T, reqwest::header::HeaderMap), FetchError> {
    let (bytes, headers) = self.get_bytes_with_headers(url, max_bytes).await?;
    let parsed: T =
      serde_json::from_slice(&bytes).map_err(|e| FetchError::JsonDecode(e.to_string()))?;
    Ok((parsed, headers))
  }
}

fn translate_send_error(e: reqwest::Error) -> FetchError {
  // reqwest's redirect-policy errors `Display` to "error following
  // redirect for url (…)" and hide the inner reason behind `.source()`.
  // Walk the chain so an allowlist refusal or cap-hit at hop N surfaces
  // as its semantic variant rather than an opaque transport error.
  use std::error::Error as _;
  let top = e.to_string();
  let mut combined = top.clone();
  let mut cur: Option<&dyn std::error::Error> = e.source();
  while let Some(src) = cur {
    combined.push_str(" :: ");
    combined.push_str(&src.to_string());
    cur = src.source();
  }
  if combined.contains("redirect cap reached") {
    FetchError::TooManyRedirects
  } else if combined.contains("not on the allowlist") {
    // The refusal string is `URL host \`X\` is not on the allowlist; …`
    // Pull out X for a precise error.
    let host = combined
      .split_once("host `")
      .and_then(|(_, rest)| rest.split_once('`'))
      .map(|(h, _)| h.to_string())
      .unwrap_or_default();
    FetchError::HostNotAllowed { host }
  } else if e.is_timeout() {
    FetchError::Transport(format!("timeout: {top}"))
  } else {
    FetchError::Transport(top)
  }
}

/// Honour `LLAMASTASH_OFFLINE=1` / `--offline` by returning an offline
/// stub. Resolution rule: explicit `cli_offline == true` wins; otherwise
/// the env var is consulted (`"1"`, `"true"`, `"yes"` are truthy).
pub fn offline_requested(cli_offline: bool) -> bool {
  if cli_offline {
    return true;
  }
  match std::env::var("LLAMASTASH_OFFLINE") {
    Ok(v) => {
      let v = v.to_ascii_lowercase();
      v == "1" || v == "true" || v == "yes"
    }
    Err(_) => false,
  }
}

/// Resolve a `FetchClient` honouring `--offline` / `LLAMASTASH_OFFLINE`.
/// On error the caller surfaces `INIT_ABORTED` — a `reqwest::Client`
/// builder failure is fatal because we can't ship a half-configured
/// client that bypasses the contract.
pub fn build_with_offline_check(
  cli_offline: bool,
  cfg: FetchClientConfig,
) -> Result<FetchClient, FetchError> {
  if offline_requested(cli_offline) {
    return Ok(FetchClient::offline());
  }
  FetchClient::new(cfg)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn offline_client_refuses_every_call() {
    let c = FetchClient::offline();
    assert!(c.is_offline());
    let bytes_result = c.get_bytes("https://api.github.com/", 1024).await;
    assert!(matches!(bytes_result, Err(FetchError::Offline)));
    let json_result: Result<serde_json::Value, _> =
      c.get_json("https://api.github.com/", 1024).await;
    assert!(matches!(json_result, Err(FetchError::Offline)));
  }

  #[tokio::test]
  async fn online_client_refuses_non_allowlisted_host_up_front() {
    let c = FetchClient::new(FetchClientConfig::default()).expect("build");
    let r = c.get_bytes("https://evil.example.com/", 1024).await;
    assert!(
      matches!(r, Err(FetchError::HostNotAllowed { .. })),
      "expected HostNotAllowed, got {r:?}"
    );
  }

  #[tokio::test]
  async fn online_client_refuses_http_scheme_up_front() {
    let c = FetchClient::new(FetchClientConfig::default()).expect("build");
    let r = c.get_bytes("http://api.github.com/", 1024).await;
    assert!(
      matches!(r, Err(FetchError::HttpNotAllowed { .. })),
      "expected HttpNotAllowed, got {r:?}"
    );
  }

  #[test]
  fn offline_requested_honours_env_truthy_values() {
    use std::sync::Mutex;
    // `set_var` is process-global; lock so concurrent tests in the
    // same `cargo test` invocation don't race on the env var.
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap();
    let saved = std::env::var_os("LLAMASTASH_OFFLINE");
    for v in ["1", "true", "TRUE", "yes", "Yes"] {
      std::env::set_var("LLAMASTASH_OFFLINE", v);
      assert!(offline_requested(false), "value `{v}` should be truthy");
    }
    for v in ["0", "false", "no", ""] {
      std::env::set_var("LLAMASTASH_OFFLINE", v);
      assert!(!offline_requested(false), "value `{v}` should be falsy");
    }
    // cli_offline=true always wins regardless of env.
    std::env::set_var("LLAMASTASH_OFFLINE", "0");
    assert!(offline_requested(true));
    // Restore.
    match saved {
      Some(s) => std::env::set_var("LLAMASTASH_OFFLINE", s),
      None => std::env::remove_var("LLAMASTASH_OFFLINE"),
    }
  }

  #[tokio::test]
  async fn build_with_offline_check_returns_offline_stub_under_flag() {
    let c = build_with_offline_check(true, FetchClientConfig::default()).expect("build");
    assert!(c.is_offline());
    let r = c.get_bytes("https://api.github.com/", 1024).await;
    assert!(matches!(r, Err(FetchError::Offline)));
  }

  #[tokio::test]
  async fn reqwest_client_is_none_for_offline_some_for_online() {
    let online = FetchClient::new(FetchClientConfig::default()).expect("build");
    assert!(online.reqwest_client().is_some());
    let offline = FetchClient::offline();
    assert!(offline.reqwest_client().is_none());
  }
}
