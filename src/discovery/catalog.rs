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
use crate::launch::resolve::{CatalogRow, MtpCapability};

/// Shared, cheap-to-clone catalog of every model discovery has seen.
///
/// Reads come in two shapes. [`ModelCatalog::snapshot`] deep-clones the
/// rows (what the `list_models` IPC handler, which serialises them
/// anyway, wants); the proxy hot path instead takes a shared [`Arc`] of a
/// pre-built view, so a request costs one
/// refcount bump instead of one allocation per row per request. The views are
/// rebuilt on write, and the only production writer is the discovery task's
/// per-scan [`ModelCatalog::replace_all`] — a write pays the rebuild so the hot
/// read does not, and `rows` is a projection of the metadata the resolver reads,
/// not a second copy of every row's templates. [`Self::upsert`] and
/// [`Self::remove`] rebuild per call and so are for single-row test and dev use,
/// not a per-model production path. What a shared view costs is that a row sits
/// in the map and in the view until the next rebuild: memory in proportion to how
/// many models are on disk, never time on the request path. If that ever matters
/// the fix is an `Arc` per row rather than a second `Vec`.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
  inner: Arc<RwLock<CatalogSlot>>,
}

/// The map plus the read views handed out to proxy readers.
#[derive(Debug, Default)]
struct CatalogSlot {
  models: BTreeMap<PathBuf, DiscoveredModel>,
  /// Rows in canonical-path order, shared with every reader that wants
  /// [`DiscoveredModel`].
  view: Arc<Vec<DiscoveredModel>>,
  /// The same rows projected to [`CatalogRow`] — the shape the model
  /// resolver speaks — so the proxy resolves without rebuilding a row
  /// per model on every request.
  rows: Arc<Vec<CatalogRow>>,
}

impl CatalogSlot {
  /// Rebuild the shared views from `models`. Called under the write lock
  /// after every mutation, so a view can never disagree with the map.
  fn rebuild(&mut self) {
    self.view = Arc::new(self.models.values().cloned().collect());
    self.rows = Arc::new(self.models.values().map(catalog_row).collect());
  }
}

impl ModelCatalog {
  pub fn new() -> Self {
    Self::default()
  }

  /// Insert or replace a model by its canonical path, rebuilding the shared
  /// views. A single row costs a whole rebuild, so this is test surface; the
  /// discovery task writes the whole set at once with [`Self::replace_all`],
  /// including on a watcher event and on the periodic backstop rescan.
  pub async fn upsert(&self, model: DiscoveredModel) {
    let key = model.path.clone();
    let mut slot = self.inner.write().await;
    slot.models.insert(key, model);
    slot.rebuild();
  }

  /// Drop a model by canonical path, rebuilding the shared views. No production
  /// path calls this: a file that disappears simply stops appearing in the next
  /// rescan. Same per-call cost as [`Self::upsert`].
  pub async fn remove(&self, path: &Path) {
    let mut slot = self.inner.write().await;
    slot.models.remove(path);
    slot.rebuild();
  }

  /// Replace the entire catalog atomically. Used after a full rescan
  /// to drop rows for files that no longer exist on disk.
  pub async fn replace_all(&self, models: Vec<DiscoveredModel>) {
    let mut slot = self.inner.write().await;
    slot.models.clear();
    for m in models {
      slot.models.insert(m.path.clone(), m);
    }
    slot.rebuild();
  }

  /// Number of models currently surfaced.
  pub async fn len(&self) -> usize {
    self.inner.read().await.models.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.models.is_empty()
  }

  /// Snapshot of every model, sorted by canonical path. Used by the
  /// `list_models` IPC handler and by inline tests.
  pub async fn snapshot(&self) -> Vec<DiscoveredModel> {
    self.inner.read().await.models.values().cloned().collect()
  }

  /// Shared, path-ordered row view. A clone is one refcount bump, so a
  /// proxy request can read the whole catalog without copying it.
  pub async fn shared_view(&self) -> Arc<Vec<DiscoveredModel>> {
    self.inner.read().await.view.clone()
  }

  /// Shared, path-ordered [`CatalogRow`] view — the resolver's input
  /// shape, pre-built on write for the same reason as
  /// [`Self::shared_view`].
  pub async fn shared_rows(&self) -> Arc<Vec<CatalogRow>> {
    self.inner.read().await.rows.clone()
  }

  /// Both shared views out of one read lock, for a caller that resolves against
  /// the rows and then reads metadata out of the models: two separate reads can
  /// straddle a rescan, and a row that resolves while its metadata view is from
  /// before the rescan reads back as a model with no metadata at all.
  pub async fn shared_pair(&self) -> (Arc<Vec<DiscoveredModel>>, Arc<Vec<CatalogRow>>) {
    let slot = self.inner.read().await;
    (slot.view.clone(), slot.rows.clone())
  }

  /// Serialise the catalog into the JSON shape `list_models` returns.
  /// Pulled out of the dispatcher so it can be unit-tested with
  /// hand-built fixtures. The wire shape lives entirely in `CatalogRow`'s
  /// serde impl — this only maps `DiscoveredModel` → `CatalogRow`.
  pub async fn to_list_response(&self, available_routed: &BTreeSet<String>) -> Value {
    let snap = self.snapshot().await;
    let rows: Vec<CatalogRow> = snap.iter().map(|m| list_row(m, available_routed)).collect();
    json!({ "models": rows })
  }
}

/// A `list_models` row: [`catalog_row`] plus the primary backend badge
/// (R14 / R13 routing), the highest-priority supported backend that is
/// available, else the source's default. Names no backend.
fn list_row(m: &DiscoveredModel, available_routed: &BTreeSet<String>) -> CatalogRow {
  let backend = m
    .supported_backends
    .iter()
    .find(|rb| available_routed.contains(*rb))
    .cloned()
    .unwrap_or_else(|| m.source.backend_id().to_string());
  CatalogRow {
    backend: Some(backend),
    ..catalog_row(m)
  }
}

/// Map one `DiscoveredModel` into the transport-agnostic [`CatalogRow`]. All
/// JSON shaping lives in `CatalogRow`'s serde impl (the single definition of
/// the `list_models` wire shape, which agents pin against); this is the only
/// `DiscoveredModel` → row projection in the tree. `backend` is left unset:
/// only `list_models` shows it.
pub(crate) fn catalog_row(m: &DiscoveredModel) -> CatalogRow {
  let md = m.metadata.as_ref();
  CatalogRow {
    path: m.path.to_string_lossy().into_owned(),
    model_id: None,
    parent: m.parent.to_string_lossy().into_owned(),
    source: m.source.label().to_string(),
    arch: md.and_then(|d| d.arch.clone()),
    // A non-GGUF row has no GGML tag, so the verbatim `quant_label` a backend
    // overlaid (AWQ / GPTQ / FP8) wins where present. Without this the field
    // renders the `Unknown` placeholder for every safetensors row.
    quant: md.map(|d| d.quant_display()),
    native_ctx: md.and_then(|d| d.native_ctx),
    mode_hint: md.map(|d| mode_hint_label(d.mode_hint).to_string()),
    parameter_label: md.and_then(|d| d.parameter_label.clone()),
    weights_bytes: md.and_then(|d| d.weights_bytes),
    display_label: m.display_label.clone(),
    parse_error: m.parse_error.clone(),
    split_siblings: m
      .split_siblings
      .iter()
      .map(|p| p.to_string_lossy().into_owned())
      .collect(),
    has_chat_template: md.map(|d| d.chat_template.is_some()).unwrap_or(false),
    has_reasoning_hint: md.map(|d| d.reasoning_hint).unwrap_or(false),
    tokenizer_kind: md.and_then(|d| d.tokenizer_kind.clone()),
    total_parameters: md.and_then(|d| d.total_parameters),
    backend: None,
    supported_backends: m.supported_backends.clone(),
    multimodal: m.multimodal,
    mtp: m.mtp_capable().then(|| MtpCapability {
      embedded_layers: md.and_then(|d| d.mtp),
      separate_head: m.mtp_head.is_some(),
    }),
  }
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
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }
  }

  /// The `list_models` wire `Value` for one model — via the real
  /// `list_row` → `CatalogRow` serde path the daemon uses.
  fn row_json(m: &DiscoveredModel, available_routed: &BTreeSet<String>) -> Value {
    serde_json::to_value(list_row(m, available_routed)).unwrap()
  }

  #[test]
  fn model_row_tags_backend_by_source() {
    // Disk (GGUF) rows report the direct llama.cpp backend — the R14 badge /
    // R13 routing tag. GGUF JSON is otherwise unchanged (additive field). A
    // backend-registry source adds its own tag.
    let none = BTreeSet::new();
    let gguf = row_json(&fake_model("/m/a.gguf", ModelSource::UserPath), &none);
    assert_eq!(gguf["backend"], "llamacpp");
    assert_eq!(gguf["path"], "/m/a.gguf");
    // A model that supports some backend reports *that* backend when it is in
    // the available set — the badge echoes the highest-priority available entry
    // of `supported_backends`, so it is backend-agnostic (a synthetic id proves
    // the mechanism, not a specific engine).
    let mut routed = fake_model("/m/a.gguf", ModelSource::UserPath);
    routed.supported_backends = vec!["some-engine".to_string(), "llamacpp".to_string()];
    let available = BTreeSet::from(["some-engine".to_string()]);
    assert_eq!(row_json(&routed, &available)["backend"], "some-engine");
    // The full supported list is emitted for the column / badges.
    assert_eq!(
      row_json(&routed, &available)["supported_backends"],
      json!(["some-engine", "llamacpp"])
    );
    // ...but the badge is not "some-engine" when it's unavailable — falls back
    // to the source default.
    assert_eq!(row_json(&routed, &none)["backend"], "llamacpp");
  }

  #[test]
  fn model_row_mtp_block_reflects_capability() {
    let none = BTreeSet::new();
    // Not MTP-capable → `mtp` is null.
    let plain = row_json(&fake_model("/m/a.gguf", ModelSource::UserPath), &none);
    assert!(
      plain["mtp"].is_null(),
      "non-capable model omits the mtp block"
    );

    // Embedded head (metadata.mtp = Some(n)) → embedded_layers set, no head.
    let mut embedded = fake_model("/m/a.gguf", ModelSource::UserPath);
    embedded.metadata.as_mut().unwrap().mtp = Some(1);
    let embedded_row = row_json(&embedded, &none);
    assert_eq!(embedded_row["mtp"]["embedded_layers"], 1);
    assert_eq!(embedded_row["mtp"]["separate_head"], false);

    // Separate head only (no embedded) → embedded_layers null, head true.
    let mut sep = fake_model("/m/a.gguf", ModelSource::UserPath);
    sep.metadata.as_mut().unwrap().mtp = None;
    sep.mtp_head = Some(PathBuf::from("/m/mtp-a.gguf"));
    let sep_row = row_json(&sep, &none);
    assert!(sep_row["mtp"]["embedded_layers"].is_null());
    assert_eq!(sep_row["mtp"]["separate_head"], true);
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
  async fn shared_views_track_the_map() {
    let cat = ModelCatalog::new();
    cat
      .upsert(fake_model("/m/a.gguf", ModelSource::UserPath))
      .await;
    cat.remove(Path::new("/m/gone.gguf")).await;

    let view = cat.shared_view().await;
    let rows = cat.shared_rows().await;
    assert_eq!(view.len(), 1);
    assert_eq!(
      rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
      vec!["/m/a.gguf".to_string()]
    );
    // A second read hands out the same allocation, not a copy of it.
    assert!(Arc::ptr_eq(&view, &cat.shared_view().await));

    // Every mutation path rebuilds the views, so a reader can never see
    // a row that is no longer on disk.
    cat
      .replace_all(vec![fake_model("/m/b.gguf", ModelSource::Ollama)])
      .await;
    let refreshed = cat.shared_view().await;
    assert_eq!(refreshed.len(), 1);
    assert_eq!(refreshed[0].path, PathBuf::from("/m/b.gguf"));
    assert_eq!(
      cat
        .shared_rows()
        .await
        .iter()
        .map(|r| r.path.clone())
        .collect::<Vec<_>>(),
      vec!["/m/b.gguf".to_string()]
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
    assert_eq!(meta["has_reasoning_hint"], json!(true));
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
      mtp_head: None,
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
