//! In-memory snapshot of every model discovery has surfaced so far.
//!
//! The IPC layer's `list_models` method reads from one of these; the
//! daemon's discovery task ([`crate::daemon::discovery_task`]) writes
//! to it after each scan and after each filesystem-watcher event.
//!
//! The catalog is keyed by canonical path so a `mv` of a model file
//! replaces its row in place rather than producing a duplicate.
//! Clone is cheap (`Arc` under the hood) so handler code can hand
//! catalogs around without worrying about lifetimes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::discovery::DiscoveredModel;
use crate::gguf::metadata::ModeHint;

/// Shared, cheap-to-clone catalog of every model discovery has seen.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
  inner: Arc<RwLock<BTreeMap<PathBuf, DiscoveredModel>>>,
}

impl ModelCatalog {
  pub fn new() -> Self {
    Self::default()
  }

  /// Insert or replace a model by its canonical path. Used by the
  /// discovery task as each `DiscoveredModel` streams in.
  pub async fn upsert(&self, model: DiscoveredModel) {
    let key = model.path.clone();
    self.inner.write().await.insert(key, model);
  }

  /// Drop a model by canonical path. Called by the watcher path when
  /// a `.gguf` is deleted under a watched root.
  pub async fn remove(&self, path: &Path) {
    self.inner.write().await.remove(path);
  }

  /// Replace the entire catalog atomically. Used after a full rescan
  /// to drop rows for files that no longer exist on disk.
  pub async fn replace_all(&self, models: Vec<DiscoveredModel>) {
    let mut guard = self.inner.write().await;
    guard.clear();
    for m in models {
      guard.insert(m.path.clone(), m);
    }
  }

  /// Number of models currently surfaced.
  pub async fn len(&self) -> usize {
    self.inner.read().await.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.is_empty()
  }

  /// Snapshot of every model, sorted by canonical path. Used by the
  /// `list_models` IPC handler and by inline tests.
  pub async fn snapshot(&self) -> Vec<DiscoveredModel> {
    self.inner.read().await.values().cloned().collect()
  }

  /// Serialise the catalog into the JSON shape `list_models` returns.
  /// Pulled out of the dispatcher so it can be unit-tested with
  /// hand-built fixtures.
  pub async fn to_list_response(&self, available_routed: &BTreeSet<String>) -> Value {
    let snap = self.snapshot().await;
    let rows: Vec<Value> = snap
      .iter()
      .map(|m| model_row(m, available_routed))
      .collect();
    json!({ "models": rows })
  }
}

/// JSON projection of a single [`DiscoveredModel`] for the
/// `list_models` response. Stable shape — agents pin against this.
///
/// `has_reasoning_hint` is the canonical name for the boolean
/// presence indicator (P2-17). The legacy `reasoning_hint` field
/// is still emitted for backwards compatibility with any caller
/// that pinned against the original name; a future v2 release
/// will drop it.
fn model_row(m: &DiscoveredModel, available_routed: &BTreeSet<String>) -> Value {
  json!({
    "path": m.path,
    "parent": m.parent,
    "source": m.source.label(),
    // Primary backend badge (R14 / R13 routing): the highest-priority supported
    // backend that is available, else the source's default. Reads the
    // precomputed supported list, so this names no backend.
    "backend": m
      .supported_backends
      .iter()
      .find(|rb| available_routed.contains(*rb))
      .map(String::as_str)
      .unwrap_or_else(|| m.source.backend_id()),
    // Every backend that can serve this model, priority-ordered (first =
    // default). The `list` "backend" column + right-pane badges render all.
    "supported_backends": m.supported_backends,
    "split_siblings": m.split_siblings,
    "metadata": m.metadata.as_ref().map(|md| {
      json!({
        "arch": md.arch,
        "total_parameters": md.total_parameters,
        "parameter_label": md.parameter_label,
        "quant": md.quant.label(),
        "native_ctx": md.native_ctx,
        "tokenizer_kind": md.tokenizer_kind,
        "mode_hint": mode_hint_label(md.mode_hint),
        "has_reasoning_hint": md.reasoning_hint,
        // Deprecated alias — kept until v2 to avoid breaking pinned
        // parsers. Same value as `has_reasoning_hint`.
        "reasoning_hint": md.reasoning_hint,
        "has_chat_template": md.chat_template.is_some(),
        "weights_bytes": md.weights_bytes,
      })
    }),
    "parse_error": m.parse_error,
    "display_label": m.display_label,
    // Multimodal projector capability (vision / audio), or null when the
    // model has no mmproj companion. Additive — the TUI renders a glyph
    // after the model title from this field.
    "multimodal": m.multimodal.map(|mm| json!({
      "vision": mm.vision,
      "audio": mm.audio,
    })),
  })
}

fn mode_hint_label(h: ModeHint) -> &'static str {
  match h {
    ModeHint::Chat => "chat",
    ModeHint::Embedding => "embedding",
    ModeHint::Rerank => "rerank",
    ModeHint::Unknown => "unknown",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModelMetadata, Quant};

  fn fake_model(path: &str, source: ModelSource) -> DiscoveredModel {
    DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(path).parent().unwrap().to_path_buf(),
      source,
      metadata: Some(ModelMetadata {
        arch: Some("llama".to_string()),
        total_parameters: Some(7_000_000_000),
        parameter_label: Some("7B".to_string()),
        quant: Quant::Q4_K,
        quant_label: None,
        native_ctx: Some(8192),
        chat_template: Some("{% ... %}".to_string()),
        tokenizer_kind: Some("llama".to_string()),
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: Some(4_000_000_000),
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
    }
  }

  #[test]
  fn model_row_tags_backend_by_source() {
    // Disk (GGUF) rows report the direct llama.cpp backend — the R14 badge /
    // R13 routing tag. GGUF JSON is otherwise unchanged (additive field). A
    // backend-registry source adds its own tag.
    let none = BTreeSet::new();
    let gguf = model_row(&fake_model("/m/a.gguf", ModelSource::UserPath), &none);
    assert_eq!(gguf["backend"], "llamacpp");
    assert_eq!(gguf["path"], "/m/a.gguf");
    // A model that supports some backend reports *that* backend when it is in
    // the available set — the badge echoes the highest-priority available entry
    // of `supported_backends`, so it is backend-agnostic (a synthetic id proves
    // the mechanism, not a specific engine).
    let mut routed = fake_model("/m/a.gguf", ModelSource::UserPath);
    routed.supported_backends = vec!["some-engine".to_string(), "llamacpp".to_string()];
    let available = BTreeSet::from(["some-engine".to_string()]);
    assert_eq!(model_row(&routed, &available)["backend"], "some-engine");
    // The full supported list is emitted for the column / badges.
    assert_eq!(
      model_row(&routed, &available)["supported_backends"],
      json!(["some-engine", "llamacpp"])
    );
    // ...but the badge is not "some-engine" when it's unavailable — falls back
    // to the source default.
    assert_eq!(model_row(&routed, &none)["backend"], "llamacpp");
  }

  #[tokio::test]
  async fn upsert_then_snapshot_round_trips() {
    let cat = ModelCatalog::new();
    cat
      .upsert(fake_model("/m/a.gguf", ModelSource::UserPath))
      .await;
    let snap = cat.snapshot().await;
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].path, PathBuf::from("/m/a.gguf"));
  }

  #[tokio::test]
  async fn upsert_by_same_path_replaces_in_place() {
    let cat = ModelCatalog::new();
    cat
      .upsert(fake_model("/m/a.gguf", ModelSource::UserPath))
      .await;
    cat
      .upsert(fake_model("/m/a.gguf", ModelSource::HuggingFace))
      .await;
    let snap = cat.snapshot().await;
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].source, ModelSource::HuggingFace);
  }

  #[tokio::test]
  async fn remove_drops_by_path() {
    let cat = ModelCatalog::new();
    cat
      .upsert(fake_model("/m/a.gguf", ModelSource::UserPath))
      .await;
    cat
      .upsert(fake_model("/m/b.gguf", ModelSource::Ollama))
      .await;
    cat.remove(Path::new("/m/a.gguf")).await;
    let snap = cat.snapshot().await;
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].path, PathBuf::from("/m/b.gguf"));
  }

  #[tokio::test]
  async fn replace_all_is_atomic() {
    let cat = ModelCatalog::new();
    cat
      .upsert(fake_model("/m/a.gguf", ModelSource::UserPath))
      .await;
    cat
      .replace_all(vec![
        fake_model("/m/b.gguf", ModelSource::HuggingFace),
        fake_model("/m/c.gguf", ModelSource::LmStudio),
      ])
      .await;
    let snap = cat.snapshot().await;
    let paths: Vec<_> = snap.iter().map(|m| m.path.clone()).collect();
    assert_eq!(
      paths,
      vec![PathBuf::from("/m/b.gguf"), PathBuf::from("/m/c.gguf")]
    );
  }

  #[tokio::test]
  async fn to_list_response_emits_documented_fields() {
    let cat = ModelCatalog::new();
    let mut m = fake_model("/m/a.gguf", ModelSource::HuggingFace);
    if let Some(meta) = m.metadata.as_mut() {
      meta.reasoning_hint = true;
    }
    cat.upsert(m).await;

    let v = cat.to_list_response(&BTreeSet::new()).await;
    let models = v.get("models").and_then(Value::as_array).expect("array");
    assert_eq!(models.len(), 1);
    let row = &models[0];
    assert_eq!(row["path"], json!("/m/a.gguf"));
    assert_eq!(row["source"], json!("huggingface"));
    let meta = &row["metadata"];
    assert_eq!(meta["arch"], json!("llama"));
    assert_eq!(meta["quant"], json!("Q4_K"));
    assert_eq!(meta["mode_hint"], json!("chat"));
    assert_eq!(meta["reasoning_hint"], json!(true));
    assert_eq!(meta["has_chat_template"], json!(true));
    assert_eq!(meta["parameter_label"], json!("7B"));
    assert!(row["parse_error"].is_null());
    assert_eq!(row["split_siblings"], json!([]));
  }

  #[tokio::test]
  async fn parse_failure_surfaces_as_null_metadata_plus_error_string() {
    let cat = ModelCatalog::new();
    let m = DiscoveredModel {
      path: PathBuf::from("/m/bad.gguf"),
      parent: PathBuf::from("/m"),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: Some("BadMagic".to_string()),
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
    };
    cat.upsert(m).await;
    let v = cat.to_list_response(&BTreeSet::new()).await;
    let row = &v["models"][0];
    assert!(row["metadata"].is_null());
    assert_eq!(row["parse_error"], json!("BadMagic"));
  }

  #[tokio::test]
  async fn multimodal_serialises_as_object_or_null() {
    use crate::discovery::Multimodal;
    let cat = ModelCatalog::new();
    // No projector → null.
    cat
      .upsert(fake_model("/m/plain.gguf", ModelSource::UserPath))
      .await;
    // Vision projector → object with the two flags.
    let mut vis = fake_model("/m/vision.gguf", ModelSource::UserPath);
    vis.multimodal = Some(Multimodal {
      vision: true,
      audio: false,
    });
    cat.upsert(vis).await;

    let v = cat.to_list_response(&BTreeSet::new()).await;
    let rows = v["models"].as_array().unwrap();
    let plain = rows.iter().find(|r| r["path"] == "/m/plain.gguf").unwrap();
    let vision = rows.iter().find(|r| r["path"] == "/m/vision.gguf").unwrap();
    assert!(plain["multimodal"].is_null());
    assert_eq!(
      vision["multimodal"],
      json!({ "vision": true, "audio": false })
    );
  }
}
