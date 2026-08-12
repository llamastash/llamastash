//! HTTP readiness probe for a launched backend process.
//!
//! Polls `http://127.0.0.1:<port><path>` every 500 ms until the
//! backend's ready status arrives or the timeout fires. The path + ready
//! status come from the backend's `Readiness` declaration (llama.cpp =
//! `/health` + `200`); a non-ready status (e.g. llama.cpp's `503`
//! "model still loading") keeps us polling until the timeout.
//!
//! The probe is hand-rolled HTTP/1.1 (the request is constant for a
//! given launch — built once and reused across polls — and response
//! decoding is just "find the status line") to avoid a `reqwest` /
//! `hyper` dep just for this. Real `llama-server` supports keep-alive,
//! but the probe always sends `Connection: close` so we don't fight
//! pipelining.

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

impl ProbeOptions {
  /// Bump `timeout` to give a large model enough wall-clock to load
  /// its weights into VRAM before the supervisor declares the probe
  /// failed. The default 120 s budget covers ~10 GB models on warm
  /// NVMe + PCIe Gen4; an 80B Q5_K_M (~53 GB) on ROCm routinely needs
  /// 4-6 minutes and used to hit `health probe timeout (last status
  /// 503)` even though llama-server was still happily loading.
  ///
  /// Formula: assume a conservative 30 MiB/s effective load rate
  /// and add the derived seconds to the base. The 30 MiB/s floor is
  /// where successive calibration rounds settled: a 53 GB Q5_K_M
  /// model on HIP/ROCm needed ~30 min of probe time before `/health`
  /// flipped to 200 — RSS grew at ~95 MiB/s but the load isn't done
  /// when the bytes are resident (VRAM upload + engine prime add
  /// roughly as much again on top). Truly stuck processes still get
  /// killed within the +2 hour cap. Fast NVMe + CUDA users see
  /// generous headroom but the kill path is reachable.
  /// `weights_bytes = 0` keeps the base timeout — typical for
  /// metadata-only rows where the catalog has no estimate.
  pub fn scale_for_model(self, weights_bytes: u64) -> Self {
    const ASSUMED_LOAD_MIB_PER_SEC: u64 = 30;
    const MIB: u64 = 1024 * 1024;
    const MAX_EXTRA_SECS: u64 = 2 * 60 * 60;
    if weights_bytes == 0 {
      return self;
    }
    let extra_secs = ((weights_bytes / MIB) / ASSUMED_LOAD_MIB_PER_SEC).min(MAX_EXTRA_SECS);
    Self {
      interval: self.interval,
      timeout: self.timeout + Duration::from_secs(extra_secs),
    }
  }
}

/// Poll `path` on the supplied port until `ready_status` is observed or
/// the timeout fires. The endpoint + ready status are supplied by the
/// backend's `Readiness` declaration (llama.cpp = `/health` + `200`) so
/// the probe is no longer hardwired to llama-server's surface.
pub async fn poll_until_ready(
  port: u16,
  opts: ProbeOptions,
  path: &str,
  ready_status: u16,
) -> ProbeOutcome {
  let deadline = Instant::now() + opts.timeout;
  let mut last_status: Option<u16> = None;
  // The request is constant for the whole poll loop — build it once
  // rather than re-formatting the same bytes on every attempt.
  let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
  loop {
    match probe_once(port, opts.interval, request.as_bytes()).await {
      Ok(status) if status == ready_status => return ProbeOutcome::Ready,
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

/// Like [`poll_until_ready`], but the ready condition is `ready_status`
/// **and** the response body advertising one of `expect_ids` (ds4's fixed
/// `/v1/models` alias set). A 200 whose body id doesn't match keeps polling:
/// ds4 leaves its reserved port unbound during the multi-minute load, so a
/// bare 200 could be a foreign process that grabbed the port before the real
/// backend bound. Empty `expect_ids` degrades to a status-only check.
pub async fn poll_until_ready_model_id(
  port: u16,
  opts: ProbeOptions,
  path: &str,
  ready_status: u16,
  expect_ids: &[String],
) -> ProbeOutcome {
  let deadline = Instant::now() + opts.timeout;
  let mut last_status: Option<u16> = None;
  let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
  loop {
    match probe_once_body(port, opts.interval, request.as_bytes()).await {
      Ok((status, body)) if status == ready_status => {
        if expect_ids.is_empty() || body_serves_expected_id(&body, expect_ids) {
          return ProbeOutcome::Ready;
        }
        // 200 with a non-matching id — the real backend hasn't bound yet.
        last_status = Some(status);
      }
      Ok((status, _)) => last_status = Some(status),
      Err(_) => {}
    }
    if Instant::now() >= deadline {
      return ProbeOutcome::Timeout { last_status };
    }
    tokio::time::sleep(opts.interval).await;
  }
}

/// Whether the response advertises a model id matching one of `expect_ids`.
///
/// Matching is **prefix against the parsed `data[].id` values**, not a scan of
/// the whole response. Both halves matter:
///
/// - *Parsed*, because `body.contains(id)` matched anywhere in the payload —
///   a header, another model's `root` cache path, a `parent` field. A short
///   derived name (`gpt`, `base`) was then satisfied by any foreign server
///   that grabbed the reserved port during the engine-init window, which is
///   the exact threat this check exists to close.
/// - *Prefix*, because ds4 registers the `deepseek-v4-` family prefix rather
///   than its two exact aliases, so `-flash` / `-pro` / a future `-turbo` all
///   pass without this list chasing upstream.
///
/// A body with no parseable `data` array never matches, so a non-OpenAI
/// responder stays unready instead of being accidentally accepted.
fn body_serves_expected_id(body: &str, expect_ids: &[String]) -> bool {
  served_model_ids(body.as_bytes()).is_some_and(|ids| {
    ids
      .iter()
      .any(|id| expect_ids.iter().any(|want| id.starts_with(want.as_str())))
  })
}

/// The `data[].id` values an OpenAI-compatible `/v1/models` payload
/// advertises, or `None` when the payload is not that shape.
///
/// The one place this wire shape is decoded. Every caller that needs to ask
/// "does this server serve the model I think it does" — the readiness probe
/// here, and both adoption checks in [`crate::daemon::orphans`] — goes through
/// it and applies its own comparison to the returned ids. They differ in how
/// they compare (prefix, exact, path-tolerant), never in how they parse, and
/// none of them may fall back to scanning the raw payload.
///
/// Accepts either a whole HTTP response or a bare body.
pub(crate) fn served_model_ids(body: &[u8]) -> Option<Vec<String>> {
  Some(
    served_model_entries(body)?
      .iter()
      .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
      .collect(),
  )
}

/// The raw `data[]` entries of a `/v1/models` payload, for callers that need a
/// field other than `id` (a backend reading its resolved context window, say).
pub(crate) fn served_model_entries(body: &[u8]) -> Option<Vec<serde_json::Value>> {
  let text = std::str::from_utf8(body).ok()?;
  // Skip the response headers, then decode the first JSON value and ignore
  // whatever follows it (chunked-encoding trailers, a second document).
  let start = text.find('{')?;
  let mut de = serde_json::Deserializer::from_str(&text[start..]);
  if let Ok(value) = <serde_json::Value as serde::Deserialize>::deserialize(&mut de) {
    return Some(value.get("data")?.as_array()?.clone());
  }
  // Truncated at the read cap: the object never closes, so a strict decode
  // fails outright and every id is lost — a server advertising enough models
  // would then never reach Ready, with nothing pointing at the cap. Recover
  // the entries that did arrive by decoding the `data` array element-wise.
  let data_at = text[start..].find("\"data\"")? + start;
  let arr_at = text[data_at..].find('[')? + data_at + 1;
  let mut stream =
    serde_json::Deserializer::from_str(&text[arr_at..]).into_iter::<serde_json::Value>();
  let mut out = Vec::new();
  while let Some(Ok(v)) = stream.next() {
    out.push(v);
  }
  (!out.is_empty()).then_some(out)
}

/// One probe attempt that also captures the response body (for the model-id
/// readiness check). Returns `(status, whole-response-as-lossy-string)`,
/// reading up to a 16 KiB cap — ds4's `/v1/models` list is well under that.
async fn probe_once_body(
  port: u16,
  op_timeout: Duration,
  request: &[u8],
) -> std::io::Result<(u16, String)> {
  const CAP: usize = 16 * 1024;
  let connect = TcpStream::connect(("127.0.0.1", port));
  let mut sock = tokio::time::timeout(op_timeout, connect)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))??;
  let write = sock.write_all(request);
  tokio::time::timeout(op_timeout, write)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout"))??;
  let mut acc: Vec<u8> = Vec::with_capacity(2048);
  let mut chunk = [0u8; 2048];
  loop {
    let n = match tokio::time::timeout(op_timeout, sock.read(&mut chunk)).await {
      Ok(Ok(0)) => break,
      Ok(Ok(n)) => n,
      Ok(Err(e)) => return Err(e),
      Err(_) => break, // read stall — use what we have (status line arrived first)
    };
    acc.extend_from_slice(&chunk[..n]);
    if acc.len() >= CAP {
      break;
    }
  }
  let status = parse_status(&acc)
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed status line"))?;
  Ok((status, String::from_utf8_lossy(&acc).into_owned()))
}

/// One probe attempt. Returns the HTTP status code on success;
/// connect / read errors come back as `Err`.
async fn probe_once(port: u16, op_timeout: Duration, request: &[u8]) -> std::io::Result<u16> {
  let connect = TcpStream::connect(("127.0.0.1", port));
  let mut sock = tokio::time::timeout(op_timeout, connect)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))??;
  let write = sock.write_all(request);
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
  fn served_model_ids_reads_the_data_array_not_the_whole_body() {
    let body = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n\
      {\"object\":\"list\",\"data\":[{\"id\":\"owner/model\",\"root\":\"/cache/x\"}]}";
    assert_eq!(
      served_model_ids(body.as_bytes()).as_deref(),
      Some(&["owner/model".to_string()][..])
    );
  }

  /// The guard this check exists for: the reserved port sits idle for the
  /// whole engine-init window, so a foreign OpenAI-compatible server can
  /// answer first. A substring scan accepted it whenever the expected name
  /// appeared anywhere in the response — including inside another model's
  /// `root` path, which is exactly what a second llamastash proxy would serve.
  #[test]
  fn a_foreign_server_mentioning_the_name_is_not_ready() {
    let expect = vec!["base".to_string()];
    let foreign = "HTTP/1.1 200 OK\r\n\r\n\
      {\"object\":\"list\",\"data\":[{\"id\":\"other-model\",\"root\":\"/models/base/x\"}]}";
    assert!(
      foreign.contains("base"),
      "the old substring check would pass this"
    );
    assert!(
      !body_serves_expected_id(foreign, &expect),
      "a `base` inside another model's path must not count as ready"
    );
  }

  /// ds4 registers the family prefix rather than its exact aliases, so prefix
  /// matching against the parsed ids has to keep working.
  #[test]
  fn a_family_prefix_still_matches_a_real_alias() {
    let body = "HTTP/1.1 200 OK\r\n\r\n\
      {\"object\":\"list\",\"data\":[{\"id\":\"deepseek-v4-flash\",\"object\":\"model\"}]}";
    assert!(body_serves_expected_id(body, &["deepseek-v4-".to_string()]));
    assert!(!body_serves_expected_id(body, &["qwen".to_string()]));
  }

  #[test]
  fn a_non_openai_body_is_never_ready() {
    let expect = vec!["base".to_string()];
    assert_eq!(
      served_model_ids(b"HTTP/1.1 200 OK\r\n\r\n<html>base</html>"),
      None
    );
    assert!(!body_serves_expected_id(
      "HTTP/1.1 200 OK\r\n\r\n<html>base</html>",
      &expect
    ));
    assert!(!body_serves_expected_id(
      "HTTP/1.1 200 OK\r\n\r\n{\"error\":\"nope\"}",
      &expect
    ));
  }

  #[test]
  fn scale_for_model_leaves_base_alone_when_size_unknown() {
    let base = ProbeOptions::default();
    let scaled = base.scale_for_model(0);
    assert_eq!(scaled.timeout, base.timeout);
  }

  #[test]
  fn scale_for_model_adds_seconds_proportional_to_size() {
    let base = ProbeOptions::default();
    // 10 GiB at 30 MiB/s → 10*1024/30 = ~341 extra seconds.
    let small = base.scale_for_model(10u64 * 1024 * 1024 * 1024);
    assert!(
      small.timeout > base.timeout,
      "10 GiB should bump the budget"
    );
    let small_extra = small.timeout - base.timeout;
    assert!(
      small_extra >= Duration::from_secs(330) && small_extra <= Duration::from_secs(360),
      "10 GiB should add ~341s, got {small_extra:?}"
    );
    // 53 GiB (the Qwen3-Next 80B Q5_K_M case) → ~1808 extra seconds.
    // Round-3 calibration: even 18 min wasn't enough with the
    // 50 MiB/s assumption, so the rate is now 30 MiB/s — covers
    // HIP/ROCm + cold disk + VRAM upload + engine prime.
    let big = base.scale_for_model(53u64 * 1024 * 1024 * 1024);
    let big_extra = big.timeout - base.timeout;
    assert!(
      big_extra >= Duration::from_secs(1750) && big_extra <= Duration::from_secs(1900),
      "53 GiB should add ~1808s, got {big_extra:?}"
    );
  }

  #[test]
  fn scale_for_model_caps_extra_at_two_hours() {
    // 1 TiB at 30 MiB/s would otherwise add ~34952 seconds — clamp
    // to the 7200s ceiling so a corrupt weights_bytes can't pin the
    // probe forever.
    let base = ProbeOptions::default();
    let huge = base.scale_for_model(1024u64 * 1024 * 1024 * 1024);
    let extra = huge.timeout - base.timeout;
    assert_eq!(extra, Duration::from_secs(7200));
  }

  #[test]
  fn parse_status_handles_canonical_response() {
    assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
    assert_eq!(
      parse_status(b"HTTP/1.1 503 Service Unavailable\r\n"),
      Some(503)
    );
    assert!(parse_status(b"not http").is_none());
  }

  /// Spawn a loopback responder that answers every connection with
  /// `status` and forwards each request's first line over the channel so
  /// a test can assert which path the probe actually requested. Loops
  /// until the listener is dropped (task end).
  async fn spawn_status_responder(
    status: u16,
  ) -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
      .await
      .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
      loop {
        let Ok((mut sock, _)) = listener.accept().await else {
          break;
        };
        let mut buf = [0u8; 256];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let _ = tx.send(req.lines().next().unwrap_or_default().to_string());
        let resp = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = sock.write_all(resp.as_bytes()).await;
      }
    });
    (port, rx)
  }

  #[tokio::test]
  async fn poll_until_ready_honors_custom_path_and_ready_status() {
    // Backend-declared readiness of /custom/ready + 204 must be used
    // verbatim — proving the probe is no longer hardwired to /health/200.
    let (port, mut rx) = spawn_status_responder(204).await;
    let opts = ProbeOptions {
      interval: Duration::from_millis(20),
      timeout: Duration::from_secs(2),
    };
    let outcome = poll_until_ready(port, opts, "/custom/ready", 204).await;
    assert_eq!(outcome, ProbeOutcome::Ready);
    let request_line = rx.recv().await.expect("responder observed a request");
    assert_eq!(request_line, "GET /custom/ready HTTP/1.1");
  }

  #[tokio::test]
  async fn poll_until_ready_keeps_waiting_until_ready_status_matches() {
    // Responder always answers 200; the declared ready status is 204, so
    // the probe must never flip to Ready — proving ready_status is
    // actually compared, not assumed to be 200.
    let (port, _rx) = spawn_status_responder(200).await;
    let opts = ProbeOptions {
      interval: Duration::from_millis(20),
      timeout: Duration::from_millis(150),
    };
    let outcome = poll_until_ready(port, opts, "/health", 204).await;
    assert_eq!(
      outcome,
      ProbeOutcome::Timeout {
        last_status: Some(200)
      }
    );
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
    let outcome = poll_until_ready(1, opts, "/health", 200).await;
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
