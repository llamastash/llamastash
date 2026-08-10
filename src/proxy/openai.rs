//! OpenAI-compatible response and error shapes used by the proxy.
//!
//! These types stay private to the proxy module. They mirror the
//! documented OpenAI REST shapes for the surfaces llamastash speaks:
//!
//! - [`ModelList`] / [`ModelObject`] — the `/v1/models` listing
//!   (`object: "list"` envelope, each row `object: "model"`).
//! - [`ErrorResponse`] / [`ErrorObject`] — the OpenAI error envelope
//!   the proxy uses for every non-2xx body so clients see a
//!   recognisable payload.
//!
//! [`ErrorObject`] also carries `code` / `param` slots and a `matches`
//! field for the `model_not_found` / `ambiguous_model` cases (clients
//! use it to retry with a tighter reference).

use serde::{Deserialize, Serialize};

/// One row of `/v1/models`. The four documented fields (`id`,
/// `object`, `created`, `owned_by`) — agents pin against this shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelObject {
  /// User-visible identifier the client passes in `body.model`. For
  /// llamastash this is the `display_label` (e.g. Ollama's
  /// `<name>:<tag>`) when present, otherwise `path.file_stem()`.
  /// Matches the rule used by `crate::util::paths::model_display_name`
  /// + the TUI display layer so the same name appears everywhere.
  pub id: String,
  /// Discriminator — always the literal `"model"` for this row type.
  pub object: &'static str,
  /// Seconds-since-Unix-epoch. v1 emits `0` for every row: the
  /// catalog doesn't currently surface a stable creation/discovery
  /// timestamp on `DiscoveredModel`, and inventing one from file
  /// mtime would churn agent caches every time the user re-touches
  /// the file. `0` is the conventional "unknown / not meaningful"
  /// value seen across other OpenAI-compat servers, so clients
  /// tolerate it.
  pub created: u64,
  /// Owner label. Hard-coded to `"llamastash"` — there is no
  /// per-model owner concept in v1.
  pub owned_by: &'static str,
}

impl ModelObject {
  /// Construct a `ModelObject` for a discovered model. `id` is the
  /// caller's responsibility (derived from `display_label`
  /// → `path.file_stem()`); this constructor just stamps the three
  /// fixed fields so callers stay short.
  pub fn new(id: String) -> Self {
    Self {
      id,
      object: "model",
      created: 0,
      owned_by: "llamastash",
    }
  }
}

/// The `/v1/models` envelope. `object: "list"` per the OpenAI spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelList {
  pub object: &'static str,
  pub data: Vec<ModelObject>,
}

impl ModelList {
  /// Wrap a vector of rows in the `{object:"list", data:[...]}`
  /// envelope. Caller sorts `rows` before passing them in — the
  /// envelope itself doesn't impose order.
  pub fn new(rows: Vec<ModelObject>) -> Self {
    Self {
      object: "list",
      data: rows,
    }
  }
}

/// OpenAI's error envelope: `{"error": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
  pub error: ErrorObject,
}

/// One OpenAI-shaped error. `type` is mandatory; the rest mirror
/// the public OpenAI shape for `code` / `param` / `message`.
/// `code` carries the `model_required` case and `matches` carries the
/// `model_not_found` / `ambiguous_model` disambiguation surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
  /// Discriminator (e.g. `"not_implemented"`, `"invalid_request"`,
  /// `"model_not_found"`). Stable wire label.
  #[serde(rename = "type")]
  pub r#type: String,
  /// Human-readable message.
  pub message: String,
  /// Sub-discriminator for client-side switching (e.g.
  /// `"model_required"`). `None` when the `type` field alone is
  /// sufficient.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub code: Option<String>,
  /// Name of the offending field, if applicable.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub param: Option<String>,
  /// Candidate model names returned alongside `model_not_found` /
  /// `ambiguous_model` errors so the client can refine its request.
  /// Always omitted on the wire when empty — keeping the absent
  /// case JSON-shaped the same as a bare error.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub matches: Option<Vec<String>>,
  /// Currently-Ready model names reported alongside `launch_failed`
  /// errors. R155 mandates "no running models" maps to a 503 with
  /// `running: []`; the field is always present on the
  /// `launch_failed` arm (even when empty) so clients can branch on
  /// it without an `Option`-style absence check.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub running: Option<Vec<String>>,
}

impl ErrorObject {
  pub fn new(r#type: impl Into<String>, message: impl Into<String>) -> Self {
    Self {
      r#type: r#type.into(),
      message: message.into(),
      code: None,
      param: None,
      matches: None,
      running: None,
    }
  }

  /// Builder helper: stamp the running-model list (presented to
  /// clients on `launch_failed` 503s). Always wins on the wire,
  /// even when empty — R155 requires the field be present so
  /// clients can see "no running models" explicitly.
  pub fn with_running<I, S>(mut self, names: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    let v: Vec<String> = names.into_iter().map(Into::into).collect();
    self.running = Some(v);
    self
  }

  /// Builder helper: stamp `code` (e.g. `"model_required"`).
  pub fn with_code(mut self, code: impl Into<String>) -> Self {
    self.code = Some(code.into());
    self
  }

  /// Builder helper: stamp `param` (e.g. `"model"`).
  pub fn with_param(mut self, param: impl Into<String>) -> Self {
    self.param = Some(param.into());
    self
  }

  /// Builder helper: stamp the candidate-name list. Empty input
  /// stays `None` so clients see the field omitted rather than a
  /// `[]` they have to special-case.
  pub fn with_matches<I, S>(mut self, matches: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    let v: Vec<String> = matches.into_iter().map(Into::into).collect();
    self.matches = if v.is_empty() { None } else { Some(v) };
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn model_object_serializes_with_documented_fields() {
    let row = ModelObject::new("llama3:8b".to_string());
    let v: serde_json::Value = serde_json::to_value(&row).expect("serialize");
    // Exactly four fields — the OpenAI documented shape.
    let obj = v.as_object().expect("object");
    assert_eq!(obj.len(), 4, "field count: {v}");
    assert_eq!(obj.get("id"), Some(&serde_json::json!("llama3:8b")));
    assert_eq!(obj.get("object"), Some(&serde_json::json!("model")));
    assert_eq!(obj.get("created"), Some(&serde_json::json!(0)));
    assert_eq!(obj.get("owned_by"), Some(&serde_json::json!("llamastash")));
  }

  #[test]
  fn model_list_envelope_shape() {
    let list = ModelList::new(vec![ModelObject::new("a".to_string())]);
    let v: serde_json::Value = serde_json::to_value(&list).expect("serialize");
    assert_eq!(v["object"], serde_json::json!("list"));
    let data = v["data"].as_array().expect("data array");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], serde_json::json!("a"));
  }

  #[test]
  fn empty_model_list_is_an_object_with_empty_data() {
    let list = ModelList::new(Vec::new());
    let v: serde_json::Value = serde_json::to_value(&list).expect("serialize");
    assert_eq!(v["object"], serde_json::json!("list"));
    assert!(v["data"].is_array());
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
  }

  #[test]
  fn error_object_omits_optional_fields_when_unset() {
    let err = ErrorObject::new("not_implemented", "endpoint not implemented yet");
    let v: serde_json::Value = serde_json::to_value(&err).expect("serialize");
    let obj = v.as_object().expect("object");
    assert_eq!(obj.len(), 2, "no code/param when unset: {v}");
    assert_eq!(obj.get("type"), Some(&serde_json::json!("not_implemented")));
    assert!(obj.contains_key("message"));
  }

  #[test]
  fn error_object_emits_code_when_present() {
    let mut err = ErrorObject::new("invalid_request", "model is required");
    err.code = Some("model_required".to_string());
    let v: serde_json::Value = serde_json::to_value(&err).expect("serialize");
    assert_eq!(v["code"], serde_json::json!("model_required"));
  }
}
