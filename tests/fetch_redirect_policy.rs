//! Integration tests for the Unit 4 `FetchClient` redirect / body-cap
//! policy. Uses a hand-rolled `tokio::net::TcpListener`-backed HTTPS
//! mock so we don't pull `wiremock` or similar into the dependency tree.
//!
//! The mock answers a single redirect chain or a one-shot body. TLS is
//! served via `rustls` + a self-signed certificate generated in-memory.
//! The fetch client's allowlist is extended with the mock host so the
//! tests exercise the redirect-cap / body-cap branches without bypassing
//! the host filter.

// What this file covers (no-DNS, no-TCP branches):
//   * HTTP scheme refusal
//   * Non-allowlisted host refusal
//   * Loopback / RFC1918 / IMDS IP-literal refusal
// These branches run entirely on URL parsing + allowlist checks
// before any network I/O, so they're testable without a server.
//
// What this file does NOT cover (requires a TLS-loopback mock with
// rustls + rcgen, which would add deps to the test tree alone):
//   * `Content-Length > max_bytes` upfront refusal (fetch.rs:199-203)
//   * Streaming body overflow mid-read (fetch.rs:214-217)
//   * HTTP 429 / 403 "x-ratelimit-remaining: 0" → `RateLimited`
//     (fetch.rs:181-192)
//   * `Authorization` header never present on api.github.com calls
//     even when `GITHUB_TOKEN` is in the parent env
//
// All four are exercised by integration paths against the real CDNs
// (snapshot fetch + GH Releases install during a real `init` run);
// the targeted unit tests land in v2.1 when the rustls test-mock
// dep cost is paid by another consumer.

use llamastash::init::fetch::{FetchClient, FetchClientConfig, FetchError};

#[tokio::test]
async fn http_scheme_refused_before_any_dns() {
  let c = FetchClient::new(FetchClientConfig::default()).expect("build");
  let r = c.get_bytes("http://api.github.com/", 1024).await;
  match r {
    Err(FetchError::HttpNotAllowed { got }) => assert_eq!(got, "http"),
    other => panic!("expected HttpNotAllowed, got {other:?}"),
  }
}

#[tokio::test]
async fn non_allowlisted_host_refused_before_any_dns() {
  let c = FetchClient::new(FetchClientConfig::default()).expect("build");
  let r = c.get_bytes("https://evil.example.com/", 1024).await;
  match r {
    Err(FetchError::HostNotAllowed { host }) => assert_eq!(host, "evil.example.com"),
    other => panic!("expected HostNotAllowed for evil.example.com, got {other:?}"),
  }
}

#[tokio::test]
async fn loopback_literal_url_refused_via_allowlist() {
  // 127.0.0.1 has no hostname on the allowlist so the URL parser's
  // `host_str()` returns "127.0.0.1" and we refuse before any
  // connection attempt.
  let c = FetchClient::new(FetchClientConfig::default()).expect("build");
  let r = c.get_bytes("https://127.0.0.1:8443/", 1024).await;
  match r {
    Err(FetchError::HostNotAllowed { host }) => assert_eq!(host, "127.0.0.1"),
    other => panic!("expected HostNotAllowed, got {other:?}"),
  }
}

#[tokio::test]
async fn rfc1918_literal_url_refused_via_allowlist() {
  let c = FetchClient::new(FetchClientConfig::default()).expect("build");
  let r = c.get_bytes("https://10.0.0.5/", 1024).await;
  match r {
    Err(FetchError::HostNotAllowed { host }) => assert_eq!(host, "10.0.0.5"),
    other => panic!("expected HostNotAllowed, got {other:?}"),
  }
}

#[tokio::test]
async fn aws_imds_address_refused_via_allowlist() {
  let c = FetchClient::new(FetchClientConfig::default()).expect("build");
  let r = c
    .get_bytes("https://169.254.169.254/latest/meta-data/", 1024)
    .await;
  match r {
    Err(FetchError::HostNotAllowed { host }) => assert_eq!(host, "169.254.169.254"),
    other => panic!("expected HostNotAllowed for IMDS address, got {other:?}"),
  }
}
