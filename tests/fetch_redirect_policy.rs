//! Integration tests for the `FetchClient` refusal paths that run
//! before any network I/O: HTTP scheme refusal, non-allowlisted
//! hosts, and loopback / RFC1918 / IMDS IP-literal refusal. These
//! branches run entirely on URL parsing + allowlist checks, so they
//! are testable without a server.
//!
//! Not covered here (needs a TLS-loopback mock, which would add
//! test-only deps):
//!   * `Content-Length > max_bytes` upfront refusal
//!   * Streaming body overflow mid-read
//!   * HTTP 429 / 403 "x-ratelimit-remaining: 0" → `RateLimited`
//!   * `Authorization` header never present on api.github.com calls
//!     even when `GITHUB_TOKEN` is in the parent env
//!
//! These run against the real CDNs during a real `init` run.

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
