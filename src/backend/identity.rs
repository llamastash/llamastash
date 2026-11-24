//! Generalized model identity at the backend seam.
//!
//! [`crate::gguf::identity::ModelId`] is `(canonical path, BLAKE3 of header)`
//! — it assumes a **local GGUF file**. A backend that names models from a
//! remote registry has rows with no local path or header.
//! [`ModelIdentity`] is the seam-level union that lets both coexist.
//!
//! # Why a wrapper enum, not "make `ModelId` an enum"
//!
//! `ModelId` (the GGUF struct) is a persisted key threaded through
//! `state.json`, the proxy MRU, the failure tracker, the router, and the
//! CLI/JSON wire shapes (~30 sites that read `id.path`). Turning *that*
//! struct into an enum would break every one of them. So the generalization
//! lives here, at the [`crate::backend::Backend::identify`] boundary, leaving
//! the GGUF `ModelId` — and therefore `state.json` — byte-for-byte unchanged.

use serde::{Deserialize, Serialize};

use crate::gguf::identity::ModelId;

/// Identity of a model served by a backend-managed registry (no local
/// GGUF), e.g. `("example", "Qwen2.5-7B-Instruct-GGUF")`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BackendModelId {
  /// The owning backend's stable id (e.g. `"example"`).
  pub backend: String,
  /// The model name as the backend's registry / API knows it.
  pub name: String,
}

/// A model's identity, generalized across the shapes a backend serves.
///
/// Serialized `#[serde(untagged)]` so the [`Gguf`](ModelIdentity::Gguf)
/// variant is wire-identical to today's bare `ModelId`
/// (`{ "path", "header_blake3" }`) and a legacy `state.json` row
/// deserializes straight into it — no schema break, no migration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelIdentity {
  /// A local GGUF file (llama.cpp). Wraps the existing `ModelId` identity.
  /// Declared first so an ambiguous-free legacy `{path, header_blake3}`
  /// object matches here during untagged deserialization.
  Gguf(ModelId),
  /// A backend-registry model with no local file.
  Backend(BackendModelId),
}

impl ModelIdentity {
  /// The wrapped GGUF identity, if this is a local-file model.
  pub fn as_gguf(&self) -> Option<&ModelId> {
    match self {
      ModelIdentity::Gguf(id) => Some(id),
      ModelIdentity::Backend(_) => None,
    }
  }

  /// The backend-registry identity, if this is a managed-registry model.
  pub fn as_backend(&self) -> Option<&BackendModelId> {
    match self {
      ModelIdentity::Backend(id) => Some(id),
      ModelIdentity::Gguf(_) => None,
    }
  }

  /// A human-readable name for logs / status, regardless of shape: the
  /// GGUF file stem, or the backend registry name.
  pub fn display_name(&self) -> String {
    match self {
      ModelIdentity::Gguf(id) => id
        .path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.path.to_string_lossy().into_owned()),
      ModelIdentity::Backend(id) => id.name.clone(),
    }
  }
}

impl From<ModelId> for ModelIdentity {
  fn from(id: ModelId) -> Self {
    ModelIdentity::Gguf(id)
  }
}

impl From<BackendModelId> for ModelIdentity {
  fn from(id: BackendModelId) -> Self {
    ModelIdentity::Backend(id)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gguf::identity::compute;

  #[test]
  fn gguf_variant_is_wire_identical_to_bare_model_id() {
    let id = compute("/m/model.gguf", b"GGUF\x03\x00\x00\x00 header");
    let identity = ModelIdentity::Gguf(id.clone());
    let from_enum = serde_json::to_value(&identity).unwrap();
    let from_struct = serde_json::to_value(&id).unwrap();
    assert_eq!(
      from_enum, from_struct,
      "untagged Gguf must serialize exactly like a bare ModelId"
    );
    assert!(from_enum.get("path").is_some());
    assert!(from_enum.get("header_blake3").is_some());
  }

  #[test]
  fn legacy_state_json_row_deserializes_into_gguf() {
    // A bare ModelId state.json row. Must land in the Gguf variant
    // unchanged — the backward-compat guarantee when a second backend lands.
    let legacy = serde_json::json!({
      "path": "/models/qwen.gguf",
      "header_blake3": "a".repeat(64),
    });
    let identity: ModelIdentity = serde_json::from_value(legacy).unwrap();
    let gguf = identity
      .as_gguf()
      .expect("legacy row must be a Gguf identity");
    assert_eq!(gguf.path.to_string_lossy(), "/models/qwen.gguf");
  }

  #[test]
  fn backend_variant_round_trips() {
    let identity = ModelIdentity::Backend(BackendModelId {
      backend: "example".into(),
      name: "Qwen2.5-7B-Instruct-GGUF".into(),
    });
    let json = serde_json::to_string(&identity).unwrap();
    let back: ModelIdentity = serde_json::from_str(&json).unwrap();
    assert_eq!(identity, back);
    assert_eq!(back.as_backend().unwrap().backend, "example");
    assert!(back.as_gguf().is_none());
  }

  #[test]
  fn display_name_for_each_shape() {
    let gguf = ModelIdentity::Gguf(compute("/m/Qwen2.5.gguf", b"abc"));
    assert_eq!(gguf.display_name(), "Qwen2.5.gguf");
    let backend = ModelIdentity::Backend(BackendModelId {
      backend: "example".into(),
      name: "Llama-3.1-8B".into(),
    });
    assert_eq!(backend.display_name(), "Llama-3.1-8B");
  }

  #[test]
  fn backend_ids_differ_by_name_and_backend() {
    let a = BackendModelId {
      backend: "example".into(),
      name: "m1".into(),
    };
    let b = BackendModelId {
      backend: "example".into(),
      name: "m2".into(),
    };
    let c = BackendModelId {
      backend: "example".into(),
      name: "m1".into(),
    };
    assert_ne!(a, b);
    assert_eq!(a, c);
  }
}
