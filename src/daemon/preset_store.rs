//! In-memory config-preset store with write-through to `config.yaml`.
//!
//! Loaded once at daemon start from `Config.presets`, held in memory, and
//! the single read/write surface the IPC `presets_*` handlers call. A
//! save/delete mutates memory **and** atomically patches the one node in
//! `config.yaml` (via the comment-safe [`crate::config::presets_writer`]),
//! so app-driven changes are live without a restart; hand-edits to
//! `config.yaml` are only picked up on the next daemon start.
//!
//! Writes target **per-model keys only** (the model's basename); arch-level
//! presets are hand-authored and read-only here. Classification and the
//! per-model ∪ arch merge live in [`crate::launch::presets`], called by the
//! handlers with this store's snapshot plus the live catalog.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::writer::WriteError;
use crate::config::{presets_writer, ConfigPresetBlock, PresetBody};

/// Cheap-to-clone handle (`Arc` inside) to the daemon's live config-preset
/// map plus the path used for write-through.
#[derive(Clone)]
pub struct ConfigPresetStore {
  inner: Arc<Mutex<BTreeMap<String, ConfigPresetBlock>>>,
  /// `None` disables write-through (mutations stay in-memory) — used by
  /// catalog-only / ephemeral tests that don't own a config file.
  config_path: Option<PathBuf>,
}

impl ConfigPresetStore {
  /// Seed the store from the loaded config blocks. `config_path` is where
  /// write-through patches land; `None` keeps mutations in-memory only.
  pub fn new(presets: BTreeMap<String, ConfigPresetBlock>, config_path: Option<PathBuf>) -> Self {
    Self {
      inner: Arc::new(Mutex::new(presets)),
      config_path,
    }
  }

  /// An empty, write-through-disabled store for tests / catalog-only daemons.
  pub fn empty() -> Self {
    Self::new(BTreeMap::new(), None)
  }

  /// Clone of the current config-preset map. The handlers pair this with
  /// the live catalog to compute a model's effective preset set.
  pub async fn snapshot(&self) -> BTreeMap<String, ConfigPresetBlock> {
    self.inner.lock().await.clone()
  }

  /// Upsert `name` under the per-model `model_key`. Writes through to
  /// `config.yaml` first so a write failure leaves memory untouched, then
  /// commits to memory. Returns the previous body when this replaced one.
  pub async fn save(
    &self,
    model_key: &str,
    name: &str,
    body: PresetBody,
  ) -> Result<Option<PresetBody>, WriteError> {
    let mut guard = self.inner.lock().await;
    if let Some(path) = &self.config_path {
      presets_writer::upsert_preset(path, model_key, name, &body)?;
    }
    Ok(
      guard
        .entry(model_key.to_string())
        .or_default()
        .entries
        .insert(name.to_string(), body),
    )
  }

  /// Remove `name` under `model_key`. No-op (returns `None`) when the
  /// daemon's view holds no such entry. Otherwise writes through, then
  /// prunes the entry from memory — dropping the model key entirely when
  /// it becomes empty and carries no hand-authored `default`. Returns the
  /// removed body.
  pub async fn delete(
    &self,
    model_key: &str,
    name: &str,
  ) -> Result<Option<PresetBody>, WriteError> {
    let mut guard = self.inner.lock().await;
    let prev = guard
      .get(model_key)
      .and_then(|b| b.entries.get(name))
      .cloned();
    if prev.is_none() {
      return Ok(None);
    }
    if let Some(path) = &self.config_path {
      presets_writer::remove_preset(path, model_key, name)?;
    }
    if let Some(block) = guard.get_mut(model_key) {
      block.entries.remove(name);
      if block.entries.is_empty() && block.default.is_none() {
        guard.remove(model_key);
      }
    }
    Ok(prev)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::Path;

  fn body(ctx: u32) -> PresetBody {
    PresetBody {
      knobs: crate::knobset! {
        ctx: ctx
      },
      ..PresetBody::default()
    }
  }

  fn temp_config(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("preset-store-{label}")).join("config.yaml")
  }

  fn cleanup(path: &Path) {
    if let Some(dir) = path.parent() {
      std::fs::remove_dir_all(dir).ok();
    }
  }

  #[tokio::test]
  async fn save_writes_through_and_is_readable_back() {
    let path = temp_config("save");
    let store = ConfigPresetStore::new(BTreeMap::new(), Some(path.clone()));
    let prev = store.save("coder.gguf", "long", body(65536)).await.unwrap();
    assert!(prev.is_none(), "first save has no previous body");
    // In-memory.
    let snap = store.snapshot().await;
    assert_eq!(
      snap["coder.gguf"].entries["long"]
        .knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(65536)
    );
    // On disk — a fresh daemon would load this.
    let cfg = crate::config::load_config_from_path(&path).config;
    assert_eq!(
      cfg.presets["coder.gguf"].entries["long"]
        .knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(65536)
    );
    cleanup(&path);
  }

  #[tokio::test]
  async fn save_over_existing_returns_previous_body() {
    let path = temp_config("replace");
    let store = ConfigPresetStore::new(BTreeMap::new(), Some(path.clone()));
    store.save("m", "p", body(1)).await.unwrap();
    let prev = store.save("m", "p", body(2)).await.unwrap();
    assert_eq!(
      prev
        .unwrap()
        .knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(1)
    );
    assert_eq!(
      store.snapshot().await["m"].entries["p"]
        .knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(2)
    );
    cleanup(&path);
  }

  #[tokio::test]
  async fn delete_removes_and_prunes_empty_model_key() {
    let path = temp_config("delete");
    let store = ConfigPresetStore::new(BTreeMap::new(), Some(path.clone()));
    store.save("m", "only", body(1)).await.unwrap();
    let removed = store.delete("m", "only").await.unwrap();
    assert_eq!(
      removed
        .unwrap()
        .knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(1)
    );
    assert!(
      !store.snapshot().await.contains_key("m"),
      "empty model key pruned in memory"
    );
    let cfg = crate::config::load_config_from_path(&path).config;
    assert!(!cfg.presets.contains_key("m"), "and on disk");
    cleanup(&path);
  }

  #[tokio::test]
  async fn delete_absent_is_a_noop_returning_none() {
    let path = temp_config("delete-absent");
    let store = ConfigPresetStore::new(BTreeMap::new(), Some(path.clone()));
    assert!(store.delete("m", "nope").await.unwrap().is_none());
    cleanup(&path);
  }

  #[tokio::test]
  async fn empty_store_skips_write_through() {
    let store = ConfigPresetStore::empty();
    store.save("m", "p", body(1)).await.unwrap();
    assert_eq!(
      store.snapshot().await["m"].entries["p"]
        .knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(1)
    );
  }
}
