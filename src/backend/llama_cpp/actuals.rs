//! llama-server post-launch actuals fetch: read what `--fit` chose from the
//! child's `/props` once it reaches Ready.
//!
//! This is the llama-server-specific half of the neutral
//! [`crate::daemon::actuals::Actuals`] result type — the `GET /props` transport
//! and the `default_generation_settings.n_ctx` parse. It lives with the backend
//! (reached through [`crate::backend::Backend::fetch_actuals`]) so the generic
//! supervisor / last-params paths carry no llama-server endpoint.
//!
//! Transport is a hand-rolled `GET /props` over raw TCP with
//! `Connection: close` (so we can read to EOF without a keep-alive dance) — the
//! same no-dep stance as [`crate::daemon::probe`]. We never pull in
//! `reqwest`/`hyper` just for this.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::daemon::actuals::Actuals;

/// Fetch `/props` from the child on `127.0.0.1:<port>` and extract the
/// values `--fit` resolved. Best-effort: any error → empty `Actuals`.
pub(crate) async fn fetch_props_actuals(port: u16, timeout: Duration) -> Actuals {
  match fetch_props_body(port, timeout).await {
    Ok(body) => parse_actuals(&body),
    Err(_) => Actuals::default(),
  }
}

async fn fetch_props_body(port: u16, timeout: Duration) -> std::io::Result<Vec<u8>> {
  let request = format!(
    "GET /props HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
  );
  let fut = async {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await?;
    sock.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(4096);
    // `Connection: close` makes the server hang up after the body, so a
    // read-to-EOF terminates. Cap the buffer so a misbehaving peer can't
    // stream unbounded data into the daemon.
    let mut chunk = [0u8; 4096];
    loop {
      let n = sock.read(&mut chunk).await?;
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&chunk[..n]);
      if buf.len() > 256 * 1024 {
        break;
      }
    }
    Ok::<_, std::io::Error>(buf)
  };
  tokio::time::timeout(timeout, fut)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "props fetch timeout"))?
}

/// Split the HTTP response at the header/body boundary and parse the
/// JSON body for the resolved context window. llama-server carries the
/// per-request `n_ctx` under `default_generation_settings` (and at the
/// top level in some builds); we read it verbatim — see
/// [`extract_n_ctx`] and the `Actuals::resolved_ctx` doc for why
/// `total_slots` is deliberately *not* a factor.
fn parse_actuals(response: &[u8]) -> Actuals {
  let Some(split) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
    return Actuals::default();
  };
  let body = &response[split + 4..];
  let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
    return Actuals::default();
  };
  Actuals {
    resolved_ctx: extract_n_ctx(&v),
    // `/props` can't tell us this; the readiness gate computes it from
    // the floor + the trained window. Always false out of the parser.
    ctx_clamped: false,
  }
}

/// Per-request context window from `/props` — the window one sequence
/// can use. This is `default_generation_settings.n_ctx` read verbatim:
/// per-slot in non-unified mode and the full shared window under the
/// default kv-unified mode, which is the per-request size either way.
/// Multiplying by `total_slots` would double-count under kv-unified and
/// can exceed the model's trained window, so we never do.
fn extract_n_ctx(v: &serde_json::Value) -> Option<u32> {
  v.get("default_generation_settings")
    .and_then(|g| g.get("n_ctx"))
    .and_then(serde_json::Value::as_u64)
    .or_else(|| v.get("n_ctx").and_then(serde_json::Value::as_u64))
    .map(|n| n as u32)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_n_ctx_from_default_generation_settings() {
    let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
      {\"default_generation_settings\":{\"n_ctx\":16384},\"total_slots\":1}";
    assert_eq!(parse_actuals(resp).resolved_ctx, Some(16384));
  }

  #[test]
  fn falls_back_to_top_level_n_ctx() {
    let resp = b"HTTP/1.1 200 OK\r\n\r\n{\"n_ctx\":8192}";
    assert_eq!(parse_actuals(resp).resolved_ctx, Some(8192));
  }

  #[test]
  fn resolved_ctx_reads_n_ctx_verbatim_ignoring_slots() {
    // Non-unified (explicit `--parallel 4`, `-c 8192`): n_ctx is the
    // per-slot 2048 — what one request gets.
    let non_unified = b"HTTP/1.1 200 OK\r\n\r\n\
      {\"default_generation_settings\":{\"n_ctx\":2048},\"total_slots\":4}";
    assert_eq!(parse_actuals(non_unified).resolved_ctx, Some(2048));
    // Unified (auto `--parallel` → kv_unified, the default): `/props`
    // reports the full shared window. Verified live against `-c 8192`
    // auto: n_ctx=8192, total_slots=4.
    let unified = b"HTTP/1.1 200 OK\r\n\r\n\
      {\"default_generation_settings\":{\"n_ctx\":8192},\"total_slots\":4}";
    assert_eq!(parse_actuals(unified).resolved_ctx, Some(8192));
  }

  #[test]
  fn missing_field_yields_none_not_a_crash() {
    let resp = b"HTTP/1.1 200 OK\r\n\r\n{\"model_path\":\"x\"}";
    assert_eq!(parse_actuals(resp).resolved_ctx, None);
  }

  #[test]
  fn malformed_body_yields_none() {
    assert!(parse_actuals(b"HTTP/1.1 200 OK\r\n\r\nnot json").is_empty());
    assert!(parse_actuals(b"no headers no body").is_empty());
  }
}
