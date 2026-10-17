//! Byte-pipe forwarding of `/v1/...` requests to a Ready upstream
//! `llama-server`.
//!
//! Once [`super::route::decide`] produces a [`RouteDecision::ReadyAt`],
//! the router hands the buffered body + the target port off to
//! [`forward_to_upstream`]. The contract is intentionally minimal:
//!
//! - Mirror inbound method, path, query, and headers (stripping
//!   hop-by-hop entries per RFC 7230).
//! - Forward the **buffered body bytes** with at most two surgical edits:
//!   `body.model` when the launch pins a request-model name, and whatever the
//!   serving backend needs for its engine
//!   ([`crate::backend::Backend::rewrite_request_body`]). Both copy every
//!   untouched entry as the client's own bytes — the body is never re-encoded.
//!   The plan's Risks row "rewrites body.model" makes the case for this; the
//!   `received_body` echo assertions in `tests/proxy_routing.rs` keep it
//!   honest.
//! - Stream the upstream response body through `http_body_util::StreamBody`
//!   so SSE chunks land at the client as they arrive. No buffering,
//!   no per-chunk parse.
//! - Stamp `x-llamastash-served-by` + `x-llamastash-fallback-reason`
//!   only when [`RouteDecision::fallback == true`] — set by the
//!   family-MRU fallback path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::TryStreamExt;
use http_body_util::{combinators::BoxBody, BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};

use super::request_log::{RequestRecord, RequestState};
use super::router::{BodyError, ProxyResponse};
use super::state::ProxyState;
use super::usage_tap::{carries_token, ResponseTap, Usage};

/// Hop-by-hop header set — RFC 7230 §6.1. Stripped on both the
/// outbound request (so we don't leak the inbound peer's keep-alive
/// state into the upstream connection) and the inbound response (so
/// the client doesn't see contradictory framing). Lower-case keys
/// because `HeaderName::as_str()` is lower-case canonical.
const HOP_BY_HOP: &[&str] = &[
  "connection",
  "keep-alive",
  "transfer-encoding",
  "te",
  "trailers",
  "upgrade",
  "proxy-authorization",
  "proxy-authenticate",
];

/// Inbound-request slice consumed by [`forward_to_upstream`]. Bundled
/// so the forwarding fn stays under clippy's argument limit and so
/// the call-site reads as "here is the inbound request, here is the
/// target" rather than five positional strings.
pub(crate) struct InboundRequest {
  pub method: Method,
  pub uri: hyper::Uri,
  pub headers: HeaderMap,
  pub body_bytes: Bytes,
}

/// Target the inbound request should be forwarded to. Populated by
/// [`super::route::decide`] into a [`super::route::RouteDecision::ReadyAt`]
/// variant; the forwarding fn lifts the fields off the variant.
pub(crate) struct Target<'a> {
  pub port: u16,
  pub served_model_id: &'a str,
  /// Canonical id of the supervisor that owns `port` at decision
  /// time. Used to re-verify the binding immediately before send so
  /// a Ready→Stopping→port-reuse race can't silently route to a
  /// different model.
  pub served_model_key: &'a crate::gguf::identity::ModelId,
  /// Upstream path prefix prepended to the inbound path before the
  /// request is sent (`None` for direct llama.cpp, which serves OpenAI
  /// at `/v1/...`; `Some("/api")` for an umbrella that serves
  /// serves it at `/api/v1/...`). Lets one forward path target backends
  /// whose OpenAI surface lives under different roots.
  pub upstream_path_prefix: Option<&'a str>,
  pub fallback: bool,
  pub fallback_reason: Option<&'a str>,
}

/// Forward an inbound `hyper::Request` to the upstream
/// `llama-server` on `target.port`, stream the response back to the
/// caller.
///
/// `inbound.body_bytes` is the already-buffered (and length-checked)
/// inbound body; the caller has run
/// [`super::route::buffer_and_extract`] so we don't repeat the cap
/// enforcement here.
///
/// `record` is the request's log row. `None` for traffic that is not
/// logged (a `/ui` asset, `/props`, the stream lookup).
pub(crate) async fn forward_to_upstream(
  state: &Arc<ProxyState>,
  inbound: InboundRequest,
  target: Target<'_>,
  mut record: Option<RequestRecord>,
) -> ProxyResponse {
  let InboundRequest {
    method: inbound_method,
    uri: inbound_uri,
    headers: inbound_headers,
    body_bytes,
  } = inbound;
  let Target {
    port,
    served_model_id,
    served_model_key,
    upstream_path_prefix,
    fallback,
    fallback_reason,
  } = target;
  // Re-verify the supervisor at `port` is still the one we picked,
  // and take an in-flight guard on the matching ManagedModel in the
  // same snapshot walk so concurrent eviction can't tear down the
  // supervisor between our snapshot read and the body forward. The
  // guard's `Drop` decrements the inflight counter — covers happy-
  // path body completion, abandoned client connections, and upstream
  // errors uniformly because the response body owns the guard.
  let (inflight_guard, launch_id, request_model, backend) =
    match acquire_inflight_guard(state, port, served_model_key).await {
      Some(g) => g,
      None => {
        return unreachable_response(record, "model exited before forwarding could begin");
      }
    };
  if let Some(record) = record.as_mut() {
    record.set_launch(launch_id.as_str());
    record.publish();
  }
  // Compose upstream URL: path + query from the original request,
  // host always 127.0.0.1 (loopback only — see plan §Scope Boundaries).
  let path_and_query = inbound_uri
    .path_and_query()
    .map(|p| p.as_str())
    .unwrap_or("/");
  // A multiplexer umbrella can serve OpenAI under `/api/v1/...`; llama.cpp
  // The prefix (if any) is prepended so the same forward path reaches both.
  let prefix = upstream_path_prefix.unwrap_or("");
  let upstream_url = format!("http://127.0.0.1:{port}{prefix}{path_and_query}");

  // Translate hyper::Method into reqwest::Method. Both crates share
  // the underlying `http` types so this is a structural conversion
  // rather than a string round-trip — and hyper has already validated
  // the inbound method before we reach this point, so the parse can't
  // fail in practice.
  let upstream_method = reqwest::Method::from_bytes(inbound_method.as_str().as_bytes())
    .expect("hyper-validated method round-trips to reqwest");

  // Forwarded headers: drop hop-by-hop entries and anything named in
  // the inbound `Connection: <list>` header (RFC 7230 §6.1 extends
  // the hop-by-hop set per-request).
  let connection_listed = collect_connection_listed(&inbound_headers);
  let mut outbound_headers = reqwest::header::HeaderMap::new();
  for (name, value) in inbound_headers.iter() {
    let n = name.as_str();
    if HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(n)) {
      continue;
    }
    if connection_listed.iter().any(|h| h.eq_ignore_ascii_case(n)) {
      continue;
    }
    // `host` would mislabel the upstream-side virtual host; reqwest
    // computes the correct host from the URL we hand it.
    if n.eq_ignore_ascii_case("host") {
      continue;
    }
    // `content-length` is recomputed by reqwest from the body we
    // pass in; skip the inbound value (in particular when it's `0`
    // for an empty body, the upstream still gets the right framing).
    if n.eq_ignore_ascii_case("content-length") {
      continue;
    }
    // Convert reqwest <- hyper. Both libs use the `http` crate's
    // `HeaderName` / `HeaderValue` so the bytes round-trip cleanly.
    let outbound_name = match reqwest::header::HeaderName::from_bytes(n.as_bytes()) {
      Ok(n) => n,
      Err(_) => continue,
    };
    let outbound_value = match reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
      Ok(v) => v,
      Err(_) => continue,
    };
    outbound_headers.append(outbound_name, outbound_value);
  }

  // Umbrella upstreams (prefixed path) use the unpooled client so no
  // keep-alive connection ever idles against the umbrella port — see
  // `ProxyState::umbrella_client` for the restart-wedge this avoids.
  let client = if upstream_path_prefix.is_some() {
    &state.umbrella_client
  } else {
    &state.http_client
  };
  // Two surgical body edits, both of which copy every untouched entry as the
  // client's own bytes: the launch's request-model name, then whatever the
  // serving backend needs the engine to understand (see
  // [`crate::backend::Backend::rewrite_request_body`]).
  let body = outbound_body(
    &state.ctx,
    body_bytes,
    inbound_uri.path(),
    &backend,
    request_model.as_deref(),
  );
  let request = client
    .request(upstream_method, &upstream_url)
    .headers(outbound_headers)
    .body(body);

  let sent = Instant::now();
  let upstream = match request.send().await {
    Ok(r) => r,
    Err(err) => {
      // Connect refused / DNS / mid-handshake error before the
      // status line came back. The model was Ready a moment ago but
      // the kernel disagrees — surface as 502 with a recognisable
      // OpenAI body so clients can branch on it.
      return unreachable_response(
        record,
        &format!("failed to reach upstream llama-server: {err}"),
      );
    }
  };

  build_streaming_response(
    upstream,
    served_model_id,
    fallback,
    fallback_reason,
    inflight_guard,
    record,
    sent,
  )
}

/// The 502 for an upstream that is gone, logged on `record` when there is one.
fn unreachable_response(record: Option<RequestRecord>, message: &str) -> ProxyResponse {
  let response =
    super::router::error_response(StatusCode::BAD_GATEWAY, "upstream_unreachable", message);
  match record {
    Some(record) => super::router::answered(record, response),
    None => response,
  }
}

/// Find the supervisor that owns `expected_id` on `port`, take an
/// inflight guard, and return it. Returns `None` when no Ready
/// supervisor matches — same condition the legacy `verify_port_binding`
/// caught, just folded into one snapshot walk so the gate and the
/// guard acquisition can't race against an eviction landing in
/// between.
async fn acquire_inflight_guard(
  state: &Arc<ProxyState>,
  port: u16,
  expected_id: &crate::gguf::identity::ModelId,
) -> Option<(
  crate::daemon::supervisor::InflightGuard,
  crate::daemon::registry::LaunchId,
  Option<String>,
  crate::backend::Backends,
)> {
  let snap = state.ctx.supervisors.snapshot().await;
  for (launch_id, model) in snap {
    if model.port() != port {
      continue;
    }
    if model.id() != expected_id {
      continue;
    }
    if !matches!(
      model.state().await,
      crate::daemon::supervisor::ManagedState::Ready
    ) {
      continue;
    }
    let request_model = model
      .params()
      .launch_config
      .get(crate::backend::REQUEST_MODEL_KEY)
      .cloned();
    return Some((
      model.inflight_guard(),
      launch_id,
      request_model,
      model.backend().clone(),
    ));
  }
  None
}

/// The request body as sent upstream: `body` with the launch's request-model
/// name, then the serving backend's own rewrite for `endpoint`.
fn outbound_body(
  ctx: &crate::daemon::context::MethodContext,
  body: Bytes,
  endpoint: &str,
  backend: &crate::backend::Backends,
  request_model: Option<&str>,
) -> Bytes {
  use crate::backend::Backend as _;
  // Two surgical edits, each copying every untouched entry as the client's own
  // bytes.
  let body = match request_model {
    Some(model) => with_model(body, model),
    None => body,
  };
  match backend.rewrite_request_body(ctx, endpoint, &body) {
    Some(rewritten) => Bytes::from(rewritten),
    None => body,
  }
}

/// `body` with its top-level `model` set to `model`. Every other entry is
/// copied as its raw bytes, in order, so key order and nested content survive
/// and no `Value` tree is built for a multi-MB image body. A body that is not a
/// JSON object carrying `model` is returned unchanged.
fn with_model(body: Bytes, model: &str) -> Bytes {
  let Some(entries) = crate::util::json_body::entries(&body) else {
    return body;
  };
  if !entries.iter().any(|(key, _)| key == "model") {
    return body;
  }
  // Encoding a `&str` cannot fail.
  let model_value = serde_json::to_vec(&model).expect("string encodes");
  Bytes::from(crate::util::json_body::write_object(entries.iter().map(
    |(key, value)| {
      (
        key.as_str(),
        if key == "model" {
          model_value.as_slice()
        } else {
          value.get().as_bytes()
        },
      )
    },
  )))
}

/// `body` with a top-level `model` added when it carries none, for a client
/// that names no model (the bundled page does not in single-model mode, and
/// some engines answer 400 without one). Same raw-entry approach as
/// [`with_model`]: untouched entries keep their bytes and key order. A body
/// that is not a JSON object — a bodyless GET included — is returned
/// unchanged.
pub(crate) fn with_model_added(body: Bytes, model: &str) -> Bytes {
  let Some(entries) = crate::util::json_body::entries(&body) else {
    return body;
  };
  if entries.iter().any(|(key, _)| key == "model") {
    return body;
  }
  // Encoding a `&str` cannot fail.
  let model_value = serde_json::to_vec(&model).expect("string encodes");
  Bytes::from(crate::util::json_body::write_object(
    entries
      .iter()
      .map(|(key, value)| (key.as_str(), value.get().as_bytes()))
      .chain(std::iter::once(("model", model_value.as_slice()))),
  ))
}

/// Translate `reqwest::Response` into `hyper::Response`, preserving
/// status + headers (minus hop-by-hop) and piping the body chunks
/// through `StreamBody` so SSE chunks reach the client as they
/// arrive upstream.
fn build_streaming_response(
  upstream: reqwest::Response,
  served_model_id: &str,
  fallback: bool,
  fallback_reason: Option<&str>,
  inflight_guard: crate::daemon::supervisor::InflightGuard,
  record: Option<RequestRecord>,
  sent: Instant,
) -> ProxyResponse {
  let status = upstream.status();
  let inbound_headers = upstream.headers().clone();
  let logged = record.map(|mut record| {
    record.set_status(status.as_u16());
    record.publish();
    LoggedResponse {
      record,
      // A compressed body cannot be read for token counts.
      tap: (!inbound_headers.contains_key(reqwest::header::CONTENT_ENCODING))
        .then(ResponseTap::default),
      content_length: upstream.content_length(),
      seen: 0,
      ended: false,
      errored: false,
      streamed: inbound_headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream")),
      sent,
      generating_since: sent,
      first_token: None,
    }
  });

  // `bytes_stream()` yields `Result<Bytes, reqwest::Error>`. Wrap
  // each `Bytes` in a `Frame::data` and box the reqwest error into
  // the proxy's wider `BodyError` type. When the upstream errors
  // mid-stream the StreamBody surfaces it as a frame error; hyper
  // drops the client connection in turn, which is the desired
  // "mid-stream upstream death" behaviour the plan calls for.
  let stream = upstream
    .bytes_stream()
    .map_ok(Frame::data)
    .map_err(|e| -> BodyError {
      log::debug!("proxy: upstream stream error: {e}");
      Box::new(e)
    });
  let stream_body = StreamBody::new(stream);
  let inner_body: BoxBody<Bytes, BodyError> = stream_body.boxed();
  // Attach the inflight guard to the streamed body. When the body is
  // dropped — happy-path completion, client disconnect, or upstream
  // error — the guard's `Drop` decrements the inflight counter so the
  // idle-TTL sweeper sees `inflight == 0` and can evict the
  // supervisor at the next sweep tick.
  let body: BoxBody<Bytes, BodyError> = GuardedBody {
    inner: inner_body,
    _guard: inflight_guard,
    logged,
  }
  .boxed();

  // Strip the static hop-by-hop set AND anything named in the upstream's
  // own `Connection: <list>` header — RFC 7230 §6.1 extends hop-by-hop
  // per-message. Mirrors the request-side stripping above.
  let upstream_connection_listed = collect_connection_listed(&inbound_headers);
  let mut builder = Response::builder().status(status_to_hyper(status));
  if let Some(map) = builder.headers_mut() {
    for (name, value) in inbound_headers.iter() {
      let n = name.as_str();
      if HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(n)) {
        continue;
      }
      if upstream_connection_listed
        .iter()
        .any(|h| h.eq_ignore_ascii_case(n))
      {
        continue;
      }
      let Ok(hyper_name) = HeaderName::from_bytes(n.as_bytes()) else {
        continue;
      };
      let Ok(hyper_value) = HeaderValue::from_bytes(value.as_bytes()) else {
        continue;
      };
      map.append(hyper_name, hyper_value);
    }
    if fallback {
      // Sanitize served_model_id for the HeaderValue alphabet (visible
      // ASCII): non-ASCII bytes are replaced with `_` so the header
      // can never silently drop on a model with CJK/emoji in its name.
      let sanitized = sanitize_header_value(served_model_id);
      if let Ok(v) = HeaderValue::from_str(&sanitized) {
        map.insert(HeaderName::from_static("x-llamastash-served-by"), v);
      }
      if let Some(reason) = fallback_reason {
        if let Ok(v) = HeaderValue::from_str(reason) {
          map.insert(HeaderName::from_static("x-llamastash-fallback-reason"), v);
        }
      }
    }
  }

  Ok(builder.body(body).expect("static headers always parse"))
}

/// Body wrapper that holds an `InflightGuard` next to the streamed
/// upstream body. When hyper drops the response body — end-of-stream,
/// client disconnect, or pipeline tear-down — the guard field drops
/// with it and decrements the supervisor's inflight counter. This is
/// the single ownership chain that ties "request is being served" to
/// "supervisor is not idle"; the idle-TTL sweeper reads `inflight`
/// straight off the supervisor and skips eviction while it's > 0.
///
/// It is also where the request log learns how the response went: every
/// frame passes through [`LoggedResponse::observe`], and the drop that
/// releases the guard finishes the log row.
struct GuardedBody {
  inner: BoxBody<Bytes, BodyError>,
  _guard: crate::daemon::supervisor::InflightGuard,
  logged: Option<LoggedResponse>,
}

/// The log row of a response that is streaming, and what has been seen of
/// the body so far.
struct LoggedResponse {
  record: RequestRecord,
  tap: Option<ResponseTap>,
  /// Upstream `Content-Length`, when it sent one.
  content_length: Option<u64>,
  seen: u64,
  ended: bool,
  errored: bool,
  /// The response is an event stream.
  streamed: bool,
  /// When the request went upstream, before the response began. The prefill
  /// estimate is measured from here, so routing and an auto-start's model load
  /// stay out of it.
  sent: Instant,
  /// Where the proxy's clock starts timing generation, for the tok/s
  /// estimate: a stream's first token, or its first byte when no piece of it
  /// showed a token, or the moment the request went upstream for a response
  /// that arrives whole. The last two include prompt processing, so those
  /// estimates read low.
  generating_since: Instant,
  /// When a stream's first token arrived, which is when its prompt was done.
  /// Its first byte can be a status event sent before the prompt is computed.
  first_token: Option<Instant>,
}

impl LoggedResponse {
  fn observe(&mut self, polled: &Option<Result<Frame<Bytes>, BodyError>>) {
    match polled {
      Some(Ok(frame)) => {
        let Some(data) = frame.data_ref().filter(|d| !d.is_empty()) else {
          return;
        };
        if self.seen == 0 {
          self.record.mark_first_byte();
          self.record.publish();
          if self.streamed {
            self.generating_since = Instant::now();
          }
        }
        if self.streamed && self.first_token.is_none() && carries_token(data) {
          let now = Instant::now();
          self.first_token = Some(now);
          self.generating_since = now;
        }
        self.seen += data.len() as u64;
        if let Some(tap) = self.tap.as_mut() {
          tap.push(data);
        }
      }
      Some(Err(_)) => self.errored = true,
      None => self.ended = true,
    }
  }
}

impl Drop for LoggedResponse {
  fn drop(&mut self) {
    // hyper stops polling a body with a `Content-Length` once it has
    // written that many bytes, so such a body never yields its final
    // `None`. Having seen every byte counts as complete.
    let complete = self.ended || self.content_length == Some(self.seen);
    let state = if self.errored {
      RequestState::UpstreamError
    } else if complete {
      RequestState::Done
    } else {
      RequestState::ClientClosed
    };
    // Read whatever the tap holds, however the response ended: a client
    // that closes on `data: [DONE]` can beat the upstream's end of body,
    // and its final chunk is already here.
    let mut usage = self
      .tap
      .as_ref()
      .map(ResponseTap::usage)
      .unwrap_or_default();
    if state == RequestState::Done {
      // The window has to be read before the estimate lands on the usage, and
      // it only exists where the engine's own generation speed says how much
      // of the wait was not prompt processing.
      if let Some(window) = self.prefill_window(&usage) {
        usage.estimate_prefill(window);
      }
      usage.estimate_speed(self.generating_since.elapsed());
    }
    self.record.finish(state, usage);
  }
}

impl LoggedResponse {
  /// How long the engine spent computing the prompt, by the proxy's clock, or
  /// `None` when this response leaves no window to measure.
  ///
  /// A queue wait inside the engine stays in the window, so a request that had
  /// to wait for a slot estimates low.
  fn prefill_window(&self, usage: &Usage) -> Option<Duration> {
    if self.streamed {
      // A stream with no token the tap could see has no window: its first byte
      // may have been sent before the prompt was computed.
      return self.first_token?.checked_duration_since(self.sent);
    }
    // A response that arrives whole is one wait, and the engine's own speed
    // says how much of it was generation. An estimated speed already spans the
    // whole wait, which leaves nothing here.
    let generated = usage.completion_tokens? as f64 / usage.tokens_per_second?;
    // The count and the speed are the engine's, so their quotient can be any
    // magnitude, and this runs in a `Drop`. A window too large for a `Duration`
    // is no window.
    let generated = Duration::try_from_secs_f64(generated).ok()?;
    self.sent.elapsed().checked_sub(generated)
  }
}

impl hyper::body::Body for GuardedBody {
  type Data = Bytes;
  type Error = BodyError;

  fn poll_frame(
    self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
    // `GuardedBody` is `Unpin` (every field is: `BoxBody` is a
    // `Pin<Box<…>>` wrapper, `InflightGuard` holds only an `Arc`,
    // `LoggedResponse` is plain data), so `get_mut` is safe and
    // `Pin::new(inner)` re-pins for the inner body's `poll_frame` contract.
    let this = self.get_mut();
    let polled = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
    if let (Some(logged), std::task::Poll::Ready(item)) = (this.logged.as_mut(), &polled) {
      logged.observe(item);
    }
    polled
  }

  fn is_end_stream(&self) -> bool {
    self.inner.is_end_stream()
  }

  fn size_hint(&self) -> hyper::body::SizeHint {
    self.inner.size_hint()
  }
}

/// reqwest exposes `StatusCode` from the `http` crate; hyper too.
/// They're the same type but re-exported, so we go via the wire
/// number for safety.
fn status_to_hyper(s: reqwest::StatusCode) -> hyper::StatusCode {
  hyper::StatusCode::from_u16(s.as_u16()).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR)
}

/// Parse a `Connection: <list>` header into a lower-case list of
/// header names that should be stripped per RFC 7230 §6.1.
fn collect_connection_listed(headers: &HeaderMap) -> Vec<String> {
  headers
    .get_all(hyper::header::CONNECTION)
    .iter()
    .filter_map(|v| v.to_str().ok())
    .flat_map(|s| s.split(','))
    .map(|tok| tok.trim().to_ascii_lowercase())
    .filter(|tok| !tok.is_empty())
    .collect()
}

/// Coerce an arbitrary string into something `HeaderValue::from_str`
/// will accept (visible ASCII, no control characters). Non-ASCII
/// bytes and ASCII controls are replaced with `_` so a model with
/// CJK / emoji / whitespace in its display name still produces a
/// usable `x-llamastash-served-by` header instead of silently
/// dropping it.
fn sanitize_header_value(input: &str) -> String {
  input
    .chars()
    .map(|c| {
      if (' '..='~').contains(&c) && c != '\u{007f}' {
        c
      } else {
        '_'
      }
    })
    .collect()
}

/// Helper to massage a hyper::Request<Incoming> into the parts the
/// forwarding fn wants. Pulled out so the router's match arms stay
/// short.
pub(crate) fn deconstruct(
  req: Request<hyper::body::Incoming>,
) -> (Method, hyper::Uri, HeaderMap, hyper::body::Incoming) {
  let (parts, body) = req.into_parts();
  (parts.method, parts.uri, parts.headers, body)
}

#[cfg(test)]
mod tests {
  use super::super::request_log::RequestRow;
  use super::*;
  use http_body_util::BodyExt;

  #[test]
  fn with_model_replaces_only_a_top_level_model() {
    let out = with_model(
      Bytes::from(r#"{"model":"q@other","messages":[{"model":"keep"}],"stream":true}"#),
      "q",
    );
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["model"], "q");
    assert_eq!(v["messages"][0]["model"], "keep");
    assert_eq!(v["stream"], true);

    assert_eq!(
      with_model(Bytes::from(r#"{"z":1,"model":"a","b":{"y":1,"x":2}}"#), "q"),
      r#"{"z":1,"model":"q","b":{"y":1,"x":2}}"#.as_bytes(),
      "key order survives the rewrite"
    );

    for untouched in [r#"{"messages":[]}"#, "not json", "[1]", ""] {
      assert_eq!(
        with_model(Bytes::from(untouched), "q"),
        untouched.as_bytes()
      );
    }
  }

  #[test]
  fn with_model_added_fills_only_a_body_that_names_none() {
    let out = with_model_added(
      Bytes::from(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#),
      "solo",
    );
    assert_eq!(
      out,
      r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"solo"}"#.as_bytes(),
      "the name is appended and the client's own bytes and order survive"
    );

    // A body that names its model stays the client's bytes: this is an
    // insertion, not the request-model rewrite `with_model` does.
    let named = r#"{"model":"other","stream":true}"#;
    assert_eq!(
      with_model_added(Bytes::from(named), "solo"),
      named.as_bytes()
    );

    for not_object in ["", " ", "not json", "[1]", "1"] {
      assert_eq!(
        with_model_added(Bytes::from(not_object), "solo"),
        not_object.as_bytes(),
        "{not_object} is not an object body"
      );
    }
  }

  #[test]
  fn outbound_body_forwards_client_bytes_when_there_is_nothing_to_rewrite() {
    // No effort field and no request-model pin, so every registered backend
    // forwards the client's exact bytes. What a backend does rewrite is that
    // backend's own test, in its own module.
    use crate::backend::{Backend as _, Backends};
    let ctx = test_ctx();
    for body in [
      r#"{"model":"q","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
      r#"{"thinking":{"type":"adaptive"},"stream": true ,"output_config":{"format":{}}}"#,
    ] {
      for backend in Backends::all() {
        assert_eq!(
          outbound_body(&ctx, Bytes::from(body), "/v1/messages", &backend, None),
          body.as_bytes(),
          "{} on {body}",
          backend.id(),
        );
      }
    }
  }

  #[test]
  fn outbound_body_pins_the_launch_request_model() {
    // The request-model pin is what a config-declared server that checks
    // `body.model` launches with, so the client's own name never reaches it.
    let ctx = test_ctx();
    assert_eq!(
      outbound_body(
        &ctx,
        Bytes::from(r#"{"model":"client-name","stream":true}"#),
        "/v1/chat/completions",
        &test_backend(crate::backend::DEFAULT_BACKEND_ID),
        Some("launch-name"),
      ),
      r#"{"model":"launch-name","stream":true}"#.as_bytes(),
    );
  }

  fn test_ctx() -> crate::daemon::context::MethodContext {
    crate::daemon::context::MethodContext::new(crate::daemon::shutdown::ShutdownToken::new())
  }

  fn test_backend(id: &str) -> crate::backend::Backends {
    crate::backend::Backends::from_id(id).expect("registered backend")
  }

  #[test]
  fn sanitize_header_value_replaces_non_visible_ascii() {
    // Visible ASCII passes through unchanged.
    assert_eq!(sanitize_header_value("gemma-3-4b"), "gemma-3-4b");
    // CJK, emoji, and ASCII controls all collapse to `_` so the header
    // stays `HeaderValue::from_str`-acceptable.
    assert_eq!(sanitize_header_value("模型"), "__");
    assert_eq!(sanitize_header_value("a\tb\nc"), "a_b_c");
    // DEL (0x7f) is explicitly excluded from the visible range.
    assert_eq!(sanitize_header_value("x\u{007f}y"), "x_y");
    // The result is always a valid header value.
    assert!(HeaderValue::from_str(&sanitize_header_value("模型 🚀")).is_ok());
  }

  #[test]
  fn collect_connection_listed_splits_and_lowercases() {
    let mut headers = HeaderMap::new();
    headers.insert(
      hyper::header::CONNECTION,
      HeaderValue::from_static("Keep-Alive, X-Custom"),
    );
    let listed = collect_connection_listed(&headers);
    assert_eq!(
      listed,
      vec!["keep-alive".to_string(), "x-custom".to_string()]
    );
  }

  #[test]
  fn collect_connection_listed_drops_empty_tokens() {
    // A trailing comma / stray whitespace must not yield empty entries
    // that would then strip a header named "".
    let mut headers = HeaderMap::new();
    headers.insert(
      hyper::header::CONNECTION,
      HeaderValue::from_static("close, , upgrade"),
    );
    let listed = collect_connection_listed(&headers);
    assert_eq!(listed, vec!["close".to_string(), "upgrade".to_string()]);
  }

  #[test]
  fn collect_connection_listed_empty_when_header_absent() {
    assert!(collect_connection_listed(&HeaderMap::new()).is_empty());
  }

  #[test]
  fn status_to_hyper_round_trips_known_status() {
    assert_eq!(
      status_to_hyper(reqwest::StatusCode::NOT_FOUND),
      hyper::StatusCode::NOT_FOUND
    );
    assert_eq!(
      status_to_hyper(reqwest::StatusCode::OK),
      hyper::StatusCode::OK
    );
  }

  #[tokio::test]
  async fn unreachable_response_builds_openai_shaped_body() {
    // The 502 forwarding-arm error must carry the OpenAI `{error:{...}}`
    // envelope shape so SDK clients surface it as a structured error.
    let resp =
      unreachable_response(None, "model exited before forwarding could begin").expect("infallible");
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = resp
      .into_body()
      .collect()
      .await
      .expect("collect")
      .to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["error"]["type"], "upstream_unreachable");
    assert_eq!(
      v["error"]["message"],
      "model exited before forwarding could begin"
    );
  }

  /// A response to a request that went upstream at `sent`, with nothing of
  /// its body seen yet.
  fn logged_response(
    log: &crate::proxy::request_log::RequestLog,
    streamed: bool,
    sent: Instant,
  ) -> LoggedResponse {
    LoggedResponse {
      record: log.begin("/v1/chat/completions", None),
      tap: Some(ResponseTap::default()),
      content_length: None,
      seen: 0,
      ended: false,
      errored: false,
      streamed,
      sent,
      generating_since: sent,
      first_token: None,
    }
  }

  fn data(body: &'static str) -> Option<Result<Frame<Bytes>, BodyError>> {
    Some(Ok(Frame::data(Bytes::from_static(body.as_bytes()))))
  }

  fn ten_seconds_ago() -> Instant {
    Instant::now()
      .checked_sub(std::time::Duration::from_secs(10))
      .expect("the clock has run for ten seconds")
  }

  fn only_row(log: &crate::proxy::request_log::RequestLog) -> RequestRow {
    log
      .tail(crate::proxy::request_log::LogScope::All, 1)
      .rows
      .into_iter()
      .next()
      .expect("a row")
  }

  /// An Anthropic stream's first event, which engines send before they compute
  /// the prompt.
  const MESSAGE_START: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","usage":{"input_tokens":1000,"output_tokens":0}}}

"#;
  const TOKEN: &str = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

"#;

  /// The tok/s estimated for a response that reported 100 generated tokens and
  /// no speed, and whose request went upstream ten seconds ago.
  fn estimated_speed(streamed: bool) -> f64 {
    let log = crate::proxy::request_log::RequestLog::new();
    let mut logged = logged_response(&log, streamed, ten_seconds_ago());
    logged.observe(&data(
      r#"{"usage":{"prompt_tokens":5,"completion_tokens":100}}"#,
    ));
    // This body shows no token, so a stream's clock is its first byte. Here
    // that byte arrived a second ago rather than in this instant, which would
    // divide 100 tokens by microseconds, past the ceiling.
    if streamed {
      logged.generating_since = Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .expect("the clock has run for a second");
    }
    logged.observe(&None);
    drop(logged);
    let row = only_row(&log);
    assert!(row.tokens_per_second_estimated);
    row.tokens_per_second.expect("an estimate")
  }

  #[test]
  fn the_speed_estimate_times_a_stream_from_where_its_clock_started() {
    // Timed from the send, 100 tokens in ten seconds is 10 tok/s at most. A
    // stream is timed from its clock, a second ago: 100 tok/s, not the 10 the
    // whole wait would say.
    let stream = estimated_speed(true);
    assert!((90.0..=110.0).contains(&stream), "{stream}");
    // A response that arrives whole is timed from the send.
    let whole = estimated_speed(false);
    assert!((9.0..=10.0).contains(&whole), "{whole}");
  }

  #[test]
  fn a_stream_starts_its_clock_at_its_first_token_not_its_first_byte() {
    let log = crate::proxy::request_log::RequestLog::new();
    let sent = ten_seconds_ago();
    let mut logged = logged_response(&log, true, sent);
    logged.observe(&data(MESSAGE_START));
    assert_eq!(logged.first_token, None);
    assert_eq!(logged.prefill_window(&Usage::default()), None);
    let first_byte = logged.generating_since;
    assert!(first_byte > sent);

    logged.observe(&data(TOKEN));
    let first_token = logged.first_token.expect("the token starts the clock");
    assert!(first_token >= first_byte);
    assert_eq!(logged.generating_since, first_token);
    assert_eq!(
      logged.prefill_window(&Usage::default()),
      first_token.checked_duration_since(sent)
    );
    logged.observe(&data(TOKEN));
    assert_eq!(logged.first_token, Some(first_token));

    // A response that arrives whole is timed from the send, whatever it holds.
    let mut whole = logged_response(&log, false, sent);
    whole.observe(&data(TOKEN));
    assert_eq!((whole.first_token, whole.generating_since), (None, sent));
  }

  /// A response whose request went upstream ten seconds ago and whose body is
  /// `frames`, all arriving just now. `ended` is whether the body reached its
  /// end; a body that did not is a client that hung up.
  fn prefill_row(streamed: bool, frames: &[&'static str], ended: bool) -> RequestRow {
    let log = crate::proxy::request_log::RequestLog::new();
    let mut logged = logged_response(&log, streamed, ten_seconds_ago());
    for frame in frames {
      logged.observe(&data(frame));
    }
    if ended {
      logged.observe(&None);
    }
    drop(logged);
    only_row(&log)
  }

  /// A 1000-token prompt with 600 of it cached, 100 generated tokens, and no
  /// speed from the engine.
  const NO_ENGINE_SPEED: &str = r#"{"usage":{"prompt_tokens":1000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":600}}}"#;

  #[test]
  fn a_stream_measures_prefill_from_the_send_to_its_first_token() {
    // The 400 uncached tokens over the ten seconds before the first token.
    let row = prefill_row(true, &[MESSAGE_START, TOKEN, NO_ENGINE_SPEED], true);
    assert_eq!(row.prefill_tokens, Some(400));
    assert!(row.prefill_estimated);
    let est = row.prefill_tokens_per_second.expect("an estimate");
    assert!((39.0..=40.0).contains(&est), "{est}");
  }

  #[test]
  fn a_whole_response_subtracts_the_generation_it_reports() {
    // The whole wait was ten seconds and the engine says it generated its 100
    // tokens at 50 tok/s, so two of those seconds were generation: 400
    // uncached tokens over the other eight is 50 tok/s.
    let body = r#"{"usage":{"prompt_tokens":1000,"completion_tokens":100,"completion_tokens_per_second":50,"prompt_tokens_details":{"cached_tokens":600}}}"#;
    let row = prefill_row(false, &[body], true);
    assert!(row.prefill_estimated);
    let est = row.prefill_tokens_per_second.expect("an estimate");
    assert!((49.0..=50.0).contains(&est), "{est}");
  }

  #[test]
  fn an_absurd_engine_generation_speed_leaves_no_estimate() {
    // The whole-response window is the wait minus the generation the engine says
    // it did. Both bodies put that beyond a `Duration`, and this runs in a
    // `Drop`, where a panic would strand the row in flight forever.
    let too_many_tokens = r#"{"usage":{"prompt_tokens":1000,"completion_tokens":1e19,"completion_tokens_per_second":0.5,"prompt_tokens_details":{"cached_tokens":600}}}"#;
    let too_slow = r#"{"usage":{"prompt_tokens":1000,"completion_tokens":100,"completion_tokens_per_second":5e-324,"prompt_tokens_details":{"cached_tokens":600}}}"#;
    for body in [too_many_tokens, too_slow] {
      let row = prefill_row(false, &[body], true);
      assert_eq!(row.state, RequestState::Done, "{body}");
      assert!(!row.prefill_estimated, "{body}");
      assert_eq!(row.prefill_tokens_per_second, None, "{body}");
    }
  }

  #[test]
  fn a_whole_response_with_no_engine_speed_gets_no_prefill_estimate() {
    // Its estimated generation speed already spans the whole wait, so there is
    // no prefill window left to measure.
    let row = prefill_row(false, &[NO_ENGINE_SPEED], true);
    assert!(!row.prefill_estimated);
    assert_eq!(row.prefill_tokens_per_second, None);
  }

  #[test]
  fn a_stream_with_no_token_gets_no_prefill_estimate() {
    // Its first byte is a status event, which can arrive before the engine has
    // looked at the prompt, so it says nothing about how long the prompt took.
    let row = prefill_row(true, &[MESSAGE_START, NO_ENGINE_SPEED], true);
    assert_eq!(row.state, RequestState::Done);
    assert!(!row.prefill_estimated);
    assert_eq!(row.prefill_tokens_per_second, None);
  }

  #[test]
  fn a_row_that_never_finished_gets_no_prefill_estimate() {
    let row = prefill_row(true, &[TOKEN, NO_ENGINE_SPEED], false);
    assert_eq!(row.state, RequestState::ClientClosed);
    assert!(!row.prefill_estimated);
    assert_eq!(row.prefill_tokens_per_second, None);
  }
}
