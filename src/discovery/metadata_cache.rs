//! Per-file metadata cache keyed by `(canonical path, mtime, size)`.
//!
//! The scanner reads + parses the GGUF header for every `.gguf` it
//! encounters during a scan. On large model trees (HF cache + Ollama
//! blobs + LM Studio plus user paths can easily exceed a hundred
//! files) every watcher event would otherwise re-parse the lot, even
//! though only one file actually changed.
//!
//! This cache turns the steady-state into "parse once, reuse on every
//! subsequent scan where mtime and size are unchanged". A bumped
//! mtime *or* a bumped size invalidates — both signals matter
//! because a tool can write with the same mtime (rare but allowed)
//! or with the same size (e.g., re-quantising in place keeps mtime
//! current but file size shifts).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::gguf::metadata::ModelMetadata;

/// A parsed-once snapshot of one file's metadata. Either the parse
/// succeeded (`metadata`) or it failed (`parse_error`); both shapes
/// are cached so we don't keep re-parsing a file that's known-bad
/// every time the watcher fires.
#[derive(Clone, Debug, Default)]
pub struct CachedParse {
  pub metadata: Option<ModelMetadata>,
  pub parse_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CacheEntry {
  mtime: Option<SystemTime>,
  size: u64,
  parse: CachedParse,
  /// Monotonic counter, bumped on every access. Smallest value is the
  /// LRU victim when we evict.
  last_access: u64,
}

/// Thread-safe LRU cache. Cheap to clone — the inner state is held
/// in an `Arc<RwLock<…>>` so a single cache instance can be shared
/// between the scanner and the discovery task.
#[derive(Debug, Clone)]
pub struct MetadataCache {
  inner: Arc<RwLock<MetadataCacheInner>>,
  capacity: usize,
}

#[derive(Debug, Default)]
struct MetadataCacheInner {
  entries: BTreeMap<PathBuf, CacheEntry>,
  access_counter: u64,
}

impl MetadataCache {
  /// New cache with the supplied capacity. `capacity == 0` is treated
  /// as a degenerate (no-cache) configuration — gets always miss.
  pub fn new(capacity: usize) -> Self {
    Self {
      inner: Arc::new(RwLock::new(MetadataCacheInner::default())),
      capacity,
    }
  }

  /// Sensible default for a v1 install: 2048 entries comfortably
  /// covers the HF cache + Ollama + LM Studio of a power user
  /// without bloating RAM. Plan didn't fix a specific number, so
  /// this is the implementation choice.
  pub fn default_capacity() -> Self {
    Self::new(2048)
  }

  /// Returns the cached parse for `path` *if and only if* the
  /// on-disk mtime and size still match the cached probe. Bumps the
  /// LRU access counter on a hit. `None` covers miss + invalidation.
  pub async fn get(
    &self,
    path: &Path,
    mtime: Option<SystemTime>,
    size: u64,
  ) -> Option<CachedParse> {
    if self.capacity == 0 {
      return None;
    }
    let mut guard = self.inner.write().await;
    let mtime_matches;
    let size_matches;
    {
      let probe = guard.entries.get(path)?;
      mtime_matches = probe.mtime == mtime;
      size_matches = probe.size == size;
    }
    if !(mtime_matches && size_matches) {
      return None;
    }
    guard.access_counter = guard.access_counter.saturating_add(1);
    let new_access = guard.access_counter;
    let live = guard
      .entries
      .get_mut(path)
      .expect("entry exists; second borrow after counter bump");
    live.last_access = new_access;
    Some(live.parse.clone())
  }

  /// Insert or replace the parse result for `path`. Evicts the LRU
  /// entry if the cache is at capacity.
  pub async fn put(&self, path: PathBuf, mtime: Option<SystemTime>, size: u64, parse: CachedParse) {
    if self.capacity == 0 {
      return;
    }
    let mut guard = self.inner.write().await;
    guard.access_counter = guard.access_counter.saturating_add(1);
    let last_access = guard.access_counter;
    guard.entries.insert(
      path,
      CacheEntry {
        mtime,
        size,
        parse,
        last_access,
      },
    );
    if guard.entries.len() > self.capacity {
      // Identify the LRU victim by smallest `last_access`. O(n) in
      // capacity, which is fine for our bound (a few thousand).
      let victim = guard
        .entries
        .iter()
        .min_by_key(|(_, e)| e.last_access)
        .map(|(p, _)| p.clone());
      if let Some(p) = victim {
        guard.entries.remove(&p);
      }
    }
  }

  /// Current entry count. Useful in tests.
  pub async fn len(&self) -> usize {
    self.inner.read().await.entries.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.entries.is_empty()
  }
}

impl Default for MetadataCache {
  fn default() -> Self {
    Self::default_capacity()
  }
}

/// Best-effort `(mtime, size)` probe for `path`. Returns
/// `(None, 0)` on metadata failure rather than failing the lookup,
/// since a torn read can't recover anyway — the scanner will retry
/// on the next pass.
pub fn probe(path: &Path) -> (Option<SystemTime>, u64) {
  match std::fs::metadata(path) {
    Ok(m) => (m.modified().ok(), m.len()),
    Err(_) => (None, 0),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::gguf::metadata::{ModeHint, Quant};

  fn fake_parse() -> CachedParse {
    CachedParse {
      metadata: Some(ModelMetadata {
        arch: Some("llama".to_string()),
        total_parameters: Some(7_000_000_000),
        parameter_label: Some("7B".to_string()),
        quant: Quant::Q4_K,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: None,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
      }),
      parse_error: None,
    }
  }

  #[tokio::test]
  async fn hit_when_mtime_and_size_match() {
    let cache = MetadataCache::new(8);
    let now = SystemTime::now();
    cache
      .put(PathBuf::from("/m/a.gguf"), Some(now), 1024, fake_parse())
      .await;
    let got = cache.get(Path::new("/m/a.gguf"), Some(now), 1024).await;
    assert!(got.is_some(), "exact-match probe must hit");
  }

  #[tokio::test]
  async fn miss_when_mtime_changes() {
    let cache = MetadataCache::new(8);
    let t1 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
    let t2 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2);
    cache
      .put(PathBuf::from("/m/a.gguf"), Some(t1), 1024, fake_parse())
      .await;
    let got = cache.get(Path::new("/m/a.gguf"), Some(t2), 1024).await;
    assert!(got.is_none(), "mtime bump must invalidate");
  }

  #[tokio::test]
  async fn miss_when_size_changes() {
    let cache = MetadataCache::new(8);
    let now = SystemTime::now();
    cache
      .put(PathBuf::from("/m/a.gguf"), Some(now), 1024, fake_parse())
      .await;
    let got = cache.get(Path::new("/m/a.gguf"), Some(now), 2048).await;
    assert!(got.is_none(), "size bump must invalidate");
  }

  #[tokio::test]
  async fn lru_eviction_drops_oldest_entry() {
    let cache = MetadataCache::new(2);
    let now = SystemTime::now();
    cache
      .put(PathBuf::from("/m/a.gguf"), Some(now), 1, fake_parse())
      .await;
    cache
      .put(PathBuf::from("/m/b.gguf"), Some(now), 1, fake_parse())
      .await;
    // Touch a → it becomes most-recently-used.
    let _ = cache.get(Path::new("/m/a.gguf"), Some(now), 1).await;
    // Insert c → b is the LRU victim.
    cache
      .put(PathBuf::from("/m/c.gguf"), Some(now), 1, fake_parse())
      .await;
    assert_eq!(cache.len().await, 2);
    assert!(cache
      .get(Path::new("/m/b.gguf"), Some(now), 1)
      .await
      .is_none());
    assert!(cache
      .get(Path::new("/m/a.gguf"), Some(now), 1)
      .await
      .is_some());
    assert!(cache
      .get(Path::new("/m/c.gguf"), Some(now), 1)
      .await
      .is_some());
  }

  #[tokio::test]
  async fn zero_capacity_disables_cache() {
    let cache = MetadataCache::new(0);
    cache
      .put(
        PathBuf::from("/m/a.gguf"),
        Some(SystemTime::now()),
        1,
        fake_parse(),
      )
      .await;
    assert_eq!(cache.len().await, 0);
    assert!(cache
      .get(Path::new("/m/a.gguf"), Some(SystemTime::now()), 1)
      .await
      .is_none());
  }
}
