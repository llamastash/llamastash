//! HTTP `/health` probe for a launched `llama-server`.
//!
//! Polls `http://127.0.0.1:<port>/health` every 500 ms until a 200
//! response arrives or the timeout fires. Status 503 (the canonical
//! "model still loading" shape) keeps us polling. Anything else is
//! still a miss — `llama-server` only returns 200 once it's fully
//! ready to serve requests.
//!
//! The probe is hand-rolled HTTP/1.1 (the request is constant, the
//! response decoding is just "find the status line") to avoid a
//! `reqwest` / `hyper` dep just for this. Real `llama-server`
//! supports keep-alive, but the probe always sends `Connection:
//! close` so we don't fight pipelining.

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Outcome of a probe sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
  /// `/health` responded `200` within the timeout.
  Ready,
  /// Timeout elapsed without a 200. The last observation is
  /// captured for the supervisor's error cause string.
  Timeout { last_status: Option<u16> },
}

/// Tunables. Defaults mirror the plan: 500 ms poll interval, 120 s
/// timeout.
#[derive(Debug, Clone, Copy)]
pub struct ProbeOptions {
  pub interval: Duration,
  pub timeout: Duration,
}

impl Default for ProbeOptions {
  fn default() -> Self {
    Self {
      interval: Duration::from_millis(500),
      timeout: Duration::from_secs(120),
    }
  }
}

/// Poll `/health` on the supplied port until 200 OK or the timeout
/// fires.
pub async fn poll_until_ready(port: u16, opts: ProbeOptions) -> ProbeOutcome {
  let deadline = Instant::now() + opts.timeout;
  let mut last_status: Option<u16> = None;
  loop {
    match probe_once(port, opts.interval).await {
      Ok(200) => return ProbeOutcome::Ready,
      Ok(status) => last_status = Some(status),
      Err(_) => {
        // Connect refused / read error keeps the previous
        // observation (if any) so the `Timeout` payload still
        // distinguishes "we never connected" from "we got 503
        // forever".
      }
    }
    if Instant::now() >= deadline {
      return ProbeOutcome::Timeout { last_status };
    }
    tokio::time::sleep(opts.interval).await;
  }
}

/// One probe attempt. Returns the HTTP status code on success;
/// connect / read errors come back as `Err`.
async fn probe_once(port: u16, op_timeout: Duration) -> std::io::Result<u16> {
  let connect = TcpStream::connect(("127.0.0.1", port));
  let mut sock = tokio::time::timeout(op_timeout, connect)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))??;
  let req = b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
  let write = sock.write_all(req);
  tokio::time::timeout(op_timeout, write)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout"))??;
  let mut buf = [0u8; 256];
  let n = tokio::time::timeout(op_timeout, sock.read(&mut buf))
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timeout"))??;
  parse_status(&buf[..n])
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed status line"))
}

/// Extract the numeric status code from an HTTP response prefix.
fn parse_status(bytes: &[u8]) -> Option<u16> {
  let prefix = std::str::from_utf8(bytes).ok()?;
  let first_line = prefix.lines().next()?;
  // "HTTP/1.1 200 OK"
  let mut iter = first_line.split_whitespace();
  let _version = iter.next()?;
  let status = iter.next()?;
  status.parse().ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_status_handles_canonical_response() {
    assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
    assert_eq!(
      parse_status(b"HTTP/1.1 503 Service Unavailable\r\n"),
      Some(503)
    );
    assert!(parse_status(b"not http").is_none());
  }

  #[tokio::test]
  async fn poll_until_ready_times_out_against_unreachable_port() {
    // No listener on this port; probe should fail to connect on every
    // attempt and surface a `Timeout` with `last_status = None`.
    let opts = ProbeOptions {
      interval: Duration::from_millis(20),
      timeout: Duration::from_millis(120),
    };
    // Pick a port unlikely to be open — 1 is privileged on Linux.
    let outcome = poll_until_ready(1, opts).await;
    match outcome {
      ProbeOutcome::Timeout { last_status } => {
        assert!(
          last_status.is_none(),
          "no responses observed; last_status should be None"
        );
      }
      ProbeOutcome::Ready => panic!("port 1 should not be ready"),
    }
  }
}
