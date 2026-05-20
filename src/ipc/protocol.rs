//! JSON-RPC 2.0 types shared by the daemon (server) and the client.
//!
//! We stick to the minimum spec slice needed for our methods: a single
//! Request, a single Response (either result or error), no batching. The
//! standard reserved error codes (`-32700..=-32600`) are encoded as
//! variants of `ErrorCode`; llamastash-specific codes live in the same enum
//! to keep the wire shape uniform.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker for the `"jsonrpc"` field. Always the string `"2.0"`.
pub const JSONRPC_VERSION: &str = "2.0";

/// A single JSON-RPC request as it arrives on the wire.
///
/// `id` is `Option<...>` because notifications (the spec's name for
/// fire-and-forget requests) omit it. Unit 2 doesn't have any
/// notification-style methods, but accepting `null`/missing here keeps us
/// compatible with any client that follows the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
  pub jsonrpc: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id: Option<Value>,
  pub method: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
}

impl Request {
  /// Build a well-formed JSON-RPC 2.0 request with the conventional
  /// integer id. Useful for the client and tests.
  pub fn new(id: i64, method: impl Into<String>, params: Option<Value>) -> Self {
    Self {
      jsonrpc: JSONRPC_VERSION.into(),
      id: Some(Value::from(id)),
      method: method.into(),
      params,
    }
  }
}

/// A single JSON-RPC response. Exactly one of `result` or `error` is set
/// per the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
  pub jsonrpc: String,
  /// Echoed from the request. Always present in our responses (we don't
  /// generate spontaneous server-pushed messages in Unit 2).
  pub id: Value,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub result: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error: Option<ErrorObject>,
}

impl Response {
  pub fn ok(id: Value, result: Value) -> Self {
    Self {
      jsonrpc: JSONRPC_VERSION.into(),
      id,
      result: Some(result),
      error: None,
    }
  }

  pub fn err(id: Value, error: ErrorObject) -> Self {
    Self {
      jsonrpc: JSONRPC_VERSION.into(),
      id,
      result: None,
      error: Some(error),
    }
  }
}

/// JSON-RPC error object. `data` is optional context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
  pub code: i32,
  pub message: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

impl ErrorObject {
  pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
    Self {
      code: code.as_i32(),
      message: message.into(),
      data: None,
    }
  }
}

/// JSON-RPC error codes. The first five (`ParseError`..`InternalError`)
/// are the reserved spec codes; `UnauthorizedPeer` is llamastash's
/// application-specific code returned when peercred verification fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
  ParseError,
  InvalidRequest,
  MethodNotFound,
  InvalidParams,
  InternalError,
  /// Server rejected the connection because the peer UID didn't match the
  /// daemon's UID. Application-defined code (must be outside the reserved
  /// `-32768..=-32000` server-error range).
  UnauthorizedPeer,
}

impl ErrorCode {
  pub fn as_i32(self) -> i32 {
    match self {
      Self::ParseError => -32700,
      Self::InvalidRequest => -32600,
      Self::MethodNotFound => -32601,
      Self::InvalidParams => -32602,
      Self::InternalError => -32603,
      Self::UnauthorizedPeer => -32001,
    }
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn request_roundtrips_through_json() {
    let req = Request::new(7, "ping", None);
    let s = serde_json::to_string(&req).expect("serialize");
    let back: Request = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(back.jsonrpc, "2.0");
    assert_eq!(back.method, "ping");
    assert_eq!(back.id, Some(json!(7)));
    assert!(back.params.is_none());
  }

  #[test]
  fn request_with_params_roundtrips() {
    let req = Request::new(1, "start", Some(json!({"model": "qwen"})));
    let s = serde_json::to_string(&req).expect("serialize");
    let back: Request = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(back.params, Some(json!({"model": "qwen"})));
  }

  #[test]
  fn response_ok_omits_error_field() {
    let resp = Response::ok(json!(1), json!("pong"));
    let s = serde_json::to_string(&resp).expect("serialize");
    assert!(!s.contains("\"error\""), "OK response must omit error: {s}");
    assert!(s.contains("\"result\":\"pong\""));
  }

  #[test]
  fn response_err_omits_result_field() {
    let resp = Response::err(
      json!(1),
      ErrorObject::new(ErrorCode::MethodNotFound, "no such method: foo"),
    );
    let s = serde_json::to_string(&resp).expect("serialize");
    assert!(
      !s.contains("\"result\""),
      "err response must omit result: {s}"
    );
    assert!(s.contains("\"code\":-32601"));
    assert!(s.contains("no such method: foo"));
  }

  #[test]
  fn error_codes_match_jsonrpc_spec() {
    assert_eq!(ErrorCode::ParseError.as_i32(), -32700);
    assert_eq!(ErrorCode::InvalidRequest.as_i32(), -32600);
    assert_eq!(ErrorCode::MethodNotFound.as_i32(), -32601);
    assert_eq!(ErrorCode::InvalidParams.as_i32(), -32602);
    assert_eq!(ErrorCode::InternalError.as_i32(), -32603);
    // UnauthorizedPeer is an "implementation-defined server-error" per the
    // JSON-RPC 2.0 spec, which reserves -32000..=-32099 for that purpose.
    // Anything in that band is valid and not a pre-defined code.
    let code = ErrorCode::UnauthorizedPeer.as_i32();
    assert!(
      (-32099..=-32000).contains(&code),
      "UnauthorizedPeer code {code} should live in the server-defined band"
    );
    let predefined = [-32700, -32600, -32601, -32602, -32603];
    assert!(
      !predefined.contains(&code),
      "UnauthorizedPeer code {code} must not collide with a pre-defined code"
    );
  }

  #[test]
  fn id_field_accepts_null_and_string() {
    // `id: null` and a missing `id` field both deserialize to None — both
    // forms mean "notification" per the spec, and we collapse them on the
    // way in. The semantic distinction (which the spec preserves) isn't
    // useful for our handlers, so we don't replicate it on this struct.
    let null_id: Request =
      serde_json::from_str(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).expect("null id parses");
    assert!(null_id.id.is_none());

    let str_id: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","id":"abc","method":"x"}"#)
      .expect("string id parses");
    assert_eq!(str_id.id, Some(json!("abc")));

    let int_id: Request =
      serde_json::from_str(r#"{"jsonrpc":"2.0","id":42,"method":"x"}"#).expect("int id parses");
    assert_eq!(int_id.id, Some(json!(42)));
  }
}
