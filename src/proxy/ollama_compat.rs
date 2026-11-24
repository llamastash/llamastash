//! Ollama-compatible response shapes for the proxy's `/api/*`
//! discovery endpoints. Read-only endpoints that let Ollama-shape
//! discovery libraries (`ollama-python`, IDE plugins probing `GET /api/tags`,
//! `OLLAMA_HOST` environment detection) recognise llamastash as an
//! Ollama-compatible endpoint and fall through to the existing
//! OpenAI-compat surface for inference. The Ollama inference surface
//! (`/api/chat`, `/api/generate`, `/api/embed`) is not implemented: those
//! need request/response body translation and NDJSON-vs-SSE
//! streaming-format handling, which doesn't fit the proxy's byte-pure
//! forward path (TODO §Low priority).
//!
//! Wire-shape references:
//! - <https://github.com/ollama/ollama/blob/main/docs/api.md> §"List Local Models", §"List Running Models", §"Show Model Information", §"Version"
//!
//! Field-level mapping notes (where Ollama's shape doesn't line up
//! with what llamastash has):
//!
//! - `digest`: Ollama uses `sha256:<hex>`. llamastash emits
//!   `blake3:<hex>` derived from the canonical path string of the
//!   discovered file ([`digest_for_path`]). The `blake3:` prefix is
//!   truthful about the algorithm (most clients treat the digest
//!   opaquely; those that *do* validate see the right tag rather
//!   than a misleading `sha256:` prefix on a non-SHA-256 hash). The
//!   value is **stable** across `/api/tags` and `/api/ps` for the
//!   same model — both endpoints feed the same canonical path
//!   through the same hash. It is **not** the GGUF header BLAKE3
//!   that [`crate::gguf::identity::ModelId`] computes: re-reading
//!   ~16 MiB of header on every `/api/tags` request would brick
//!   discovery, and the catalog doesn't cache the header hash
//!   alongside [`ModelMetadata`] today. Lifting the digest to the
//!   truthful header BLAKE3 is tracked in TODO §Low priority ("Ollama-compat
//!   digest from cached header BLAKE3").
//! - `size`: Ollama returns the on-disk file size; we don't currently
//!   stat the file at discovery time. `weights_bytes` from
//!   [`ModelMetadata`] is the GGUF tensor footprint, typically within
//!   a few KiB of the full file size — close enough for the badge
//!   render most clients use it for. Emits `0` when metadata is
//!   absent.
//! - `modified_at`: we don't track file mtime in the catalog. Emit
//!   the Unix epoch (`1970-01-01T00:00:00Z`) as a placeholder; clients
//!   that display this see a sentinel "unknown" value rather than a
//!   misleading current-time stamp.
//! - `expires_at` (on `/api/ps`): Ollama emits the keep-alive
//!   deadline. llamastash unloads on the proxy-wide
//!   `proxy.idle_ttl_secs` sweep instead of a per-launch keep-alive, and
//!   no per-launch deadline is surfaced here, so emit a far-future
//!   timestamp — "no expiry" rather than "expires immediately."
//! - `size_vram` (on `/api/ps`): per-PID VRAM attribution is a TODO
//!   item (TODO §Low priority brainstorm). Emit `0`.
//!
//! These types stay private to the proxy module.

use serde::{Deserialize, Serialize};

// Response types below derive only `Serialize` — they carry
// `&'static str` slots (`format: "gguf"`, capability tags, etc.) that
// would otherwise force a `'de` lifetime parameter and prevent
// borrow-free deserialisation. The proxy only ever writes these on
// the wire; integration tests inspect the JSON via `serde_json::Value`
// rather than re-deserialising into the typed shape.

/// Placeholder `modified_at` value emitted when the catalog has no
/// mtime for a model. Clients that display this see a clearly-not-now
/// timestamp rather than something misleading.
pub const UNKNOWN_MTIME: &str = "1970-01-01T00:00:00Z";

/// Placeholder `expires_at` value emitted on `/api/ps`: no per-launch
/// keep-alive deadline is exposed. Far-future = "no expiry."
pub const FAR_FUTURE_EXPIRY: &str = "9999-12-31T23:59:59Z";

/// Format 32 BLAKE3 bytes as the `blake3:<hex>` string used by
/// Ollama-shape `digest` fields. Algorithm-neutral on what was
/// hashed — see [`digest_for_path`] for the strategy currently used
/// by `/api/tags` and `/api/ps`.
pub fn digest_blake3_hex(bytes: &[u8; 32]) -> String {
  let mut out = String::with_capacity(7 + 64);
  out.push_str("blake3:");
  for byte in bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// Stable Ollama-shape digest for a model identified by its canonical
/// path. Hashes the path string (UTF-8 bytes of
/// `path.to_string_lossy()`) and formats via [`digest_blake3_hex`].
///
/// Both `/api/tags` and `/api/ps` call through this helper so the
/// same model emits the same digest across both endpoints regardless
/// of whether it is currently Ready. The catalog doesn't cache the
/// GGUF header BLAKE3 today and re-reading the header on every
/// `/api/tags` row would brick discovery, so we use the canonical
/// path as a stable stand-in. Lifting this to the truthful header
/// digest is tracked in TODO §Low priority.
pub fn digest_for_path(path: &std::path::Path) -> String {
  let hashed = blake3::hash(path.to_string_lossy().as_bytes());
  digest_blake3_hex(hashed.as_bytes())
}

/// `GET /api/tags` envelope. Ollama returns every model in the local
/// store; llamastash returns the discovered catalog.
#[derive(Debug, Clone, Serialize)]
pub struct TagsResponse {
  pub models: Vec<TagModel>,
}

/// One row of `/api/tags`. Field order matches Ollama's documented
/// shape so a `serde_json::to_value` round-trip lines up
/// alphabetically.
#[derive(Debug, Clone, Serialize)]
pub struct TagModel {
  /// Display name (e.g. `llama3:8b`). Same value the OpenAI-compat
  /// `/v1/models` endpoint emits as `id`, so clients pinning on
  /// either spelling see the same model identifier.
  pub name: String,
  /// Ollama emits both `name` and `model` — historically `name` was
  /// the file-name view and `model` was the tag, but in current
  /// versions they're equal for a discovered local model. We emit
  /// the same string in both slots.
  pub model: String,
  /// RFC3339 mtime. Placeholder ([`UNKNOWN_MTIME`]) when the catalog
  /// has no mtime for the row.
  pub modified_at: String,
  /// On-disk size in bytes. `weights_bytes` projection — see module
  /// docstring for the small approximation gap.
  pub size: u64,
  /// Content digest. `blake3:<hex>` derived from the canonical
  /// path string via [`digest_for_path`] — see the module docstring
  /// for the divergence from Ollama's `sha256:` digest and the
  /// rationale for using a path-derived hash rather than the GGUF
  /// header BLAKE3.
  pub digest: String,
  pub details: ModelDetails,
}

/// `details` block shared between `/api/tags`, `/api/ps`, and
/// `/api/show`. Ollama's documented field set.
#[derive(Debug, Clone, Serialize)]
pub struct ModelDetails {
  /// Empty for llamastash — we don't model parent / merged-model
  /// lineage. Ollama uses this for the `derived-from` Modelfile
  /// concept which we don't surface.
  pub parent_model: String,
  /// Always `"gguf"`. llamastash only supports GGUF in v1.
  pub format: &'static str,
  /// `general.architecture` from the GGUF header (e.g. `"llama"`,
  /// `"qwen2"`). Empty string when discovery couldn't parse the
  /// header.
  pub family: String,
  /// Single-element list of `family`. Ollama uses the multi-element
  /// shape for fine-tuned models built on multiple base architectures;
  /// llamastash always emits one entry.
  pub families: Vec<String>,
  /// Human-readable parameter count (`"7B"`, `"3.2B"`). Empty string
  /// when discovery has no estimate.
  pub parameter_size: String,
  /// Dominant tensor quantisation tag (`"Q4_K_M"`, `"F16"`).
  pub quantization_level: String,
}

/// `GET /api/ps` envelope. Same `models` element type as `/api/tags`
/// plus running-state fields per row.
#[derive(Debug, Clone, Serialize)]
pub struct PsResponse {
  pub models: Vec<PsModel>,
}

/// One row of `/api/ps` — a currently-Ready supervisor projected
/// into Ollama's shape.
#[derive(Debug, Clone, Serialize)]
pub struct PsModel {
  pub name: String,
  pub model: String,
  pub size: u64,
  pub digest: String,
  pub details: ModelDetails,
  /// Keep-alive deadline. Far-future placeholder
  /// ([`FAR_FUTURE_EXPIRY`]): no per-launch deadline is exposed.
  pub expires_at: String,
  /// Per-PID VRAM footprint. `0` until the VRAM attribution TODO
  ///  lands.
  pub size_vram: u64,
}

/// `POST /api/show` request body. Ollama accepts either `name` or
/// `model` for the model reference (the field was renamed historically
/// and both spellings appear in client code).
#[derive(Debug, Clone, Deserialize)]
pub struct ShowRequest {
  /// Modern field name. Wins when both are present.
  #[serde(default)]
  pub model: Option<String>,
  /// Legacy field name kept for older clients.
  #[serde(default)]
  pub name: Option<String>,
}

impl ShowRequest {
  /// Extract the model reference, preferring `model` over `name`.
  /// Returns `None` if both fields are absent or blank — the caller
  /// emits a 400 in that case.
  pub fn reference(&self) -> Option<&str> {
    self
      .model
      .as_deref()
      .or(self.name.as_deref())
      .map(str::trim)
      .filter(|s| !s.is_empty())
  }
}

/// `POST /api/show` response. Mirrors Ollama's documented shape with
/// the slots llamastash can fill from the catalog row.
#[derive(Debug, Clone, Serialize)]
pub struct ShowResponse {
  /// Modelfile body. Empty — llamastash has no Modelfile concept.
  pub modelfile: String,
  /// Stringified parameters block. Empty — last-params persistence
  /// lives on the IPC side.
  pub parameters: String,
  /// Chat template (`tokenizer.chat_template` from the GGUF header).
  /// Empty string when absent.
  pub template: String,
  pub details: ModelDetails,
  /// Flat key/value map of GGUF metadata. Populated with the slots
  /// Ollama clients commonly read (`general.architecture`,
  /// `general.parameter_count`, etc.).
  pub model_info: serde_json::Map<String, serde_json::Value>,
  /// Capability tags Ollama clients inspect to gate UI features
  /// (e.g. whether to show a "chat" button vs an "embed" button).
  /// Derived from [`crate::gguf::metadata::ModeHint`].
  pub capabilities: Vec<&'static str>,
}

/// `GET /api/version` envelope. Ollama returns the daemon's build
/// version; llamastash returns the same value `status.daemon.build`
/// exposes (cargo's `CARGO_PKG_VERSION` at compile time).
#[derive(Debug, Clone, Serialize)]
pub struct VersionResponse {
  pub version: &'static str,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn digest_uses_blake3_prefix_with_64_hex_chars() {
    let digest = digest_blake3_hex(&[0x01u8; 32]);
    assert!(digest.starts_with("blake3:"));
    let hex = digest.strip_prefix("blake3:").unwrap();
    assert_eq!(hex.len(), 64, "32 bytes × 2 hex chars = 64");
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
  }

  #[test]
  fn digest_for_path_is_stable_and_path_dependent() {
    let a1 = digest_for_path(std::path::Path::new("/models/llama.gguf"));
    let a2 = digest_for_path(std::path::Path::new("/models/llama.gguf"));
    let b = digest_for_path(std::path::Path::new("/models/qwen.gguf"));
    assert_eq!(a1, a2, "same path → same digest across calls");
    assert_ne!(a1, b, "different path → different digest");
    assert!(a1.starts_with("blake3:"));
  }

  #[test]
  fn show_request_prefers_model_over_name() {
    let r = ShowRequest {
      model: Some("a".into()),
      name: Some("b".into()),
    };
    assert_eq!(r.reference(), Some("a"));
  }

  #[test]
  fn show_request_falls_back_to_name_when_model_absent() {
    let r = ShowRequest {
      model: None,
      name: Some("legacy".into()),
    };
    assert_eq!(r.reference(), Some("legacy"));
  }

  #[test]
  fn show_request_blank_strings_return_none() {
    let r = ShowRequest {
      model: Some("   ".into()),
      name: Some("".into()),
    };
    assert_eq!(r.reference(), None);
  }

  #[test]
  fn show_request_both_absent_returns_none() {
    let r = ShowRequest {
      model: None,
      name: None,
    };
    assert_eq!(r.reference(), None);
  }

  #[test]
  fn version_response_serialises_with_one_field() {
    let v = VersionResponse { version: "0.0.1" };
    let json: serde_json::Value = serde_json::to_value(&v).unwrap();
    let obj = json.as_object().unwrap();
    assert_eq!(obj.len(), 1);
    assert_eq!(obj.get("version").and_then(|s| s.as_str()), Some("0.0.1"));
  }
}
