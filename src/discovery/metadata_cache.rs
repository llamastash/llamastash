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
use std::sync::atomic::{AtomicU64, Ordering};
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

#[derive(Debug)]
struct CacheEntry {
  mtime: Option<SystemTime>,
  size: u64,
  parse: CachedParse,
  /// Monotonic access counter. Bumped on every `get` hit without
  /// upgrading the outer RwLock from read → write. The smallest
  /// value is the LRU victim at eviction time.
  last_access: AtomicU64,
}

/// Thread-safe LRU cache. Cheap to clone — the inner state is held
/// in an `Arc<RwLock<…>>` so a single cache instance can be shared
/// between the scanner and the discovery task.
#[derive(Debug, Clone)]
pub struct MetadataCache {
  inner: Arc<RwLock<BTreeMap<PathBuf, Arc<CacheEntry>>>>,
  /// Process-wide monotonic counter. Bumping it on a `get` hit only
  /// needs a read lock on `inner`; the write lock is reserved for
  /// inserts and evictions.
  access_counter: Arc<AtomicU64>,
  capacity: usize,
}

impl MetadataCache {
  /// New cache with the supplied capacity. `capacity == 0` is treated
  /// as a degenerate (no-cache) configuration — gets always miss.
  pub fn new(capacity: usize) -> Self {
    Self {
      inner: Arc::new(RwLock::new(BTreeMap::new())),
      access_counter: Arc::new(AtomicU64::new(0)),
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
    // Read-only lookup. The hit path updates `last_access` via
    // atomic store — concurrent scanner reads now scale, where they
    // previously serialised on a write lock.
    let guard = self.inner.read().await;
    let entry = guard.get(path)?;
    if entry.mtime != mtime || entry.size != size {
      return None;
    }
    let next = self.access_counter.fetch_add(1, Ordering::Relaxed) + 1;
    entry.last_access.store(next, Ordering::Relaxed);
    Some(entry.parse.clone())
  }

  /// Insert or replace the parse result for `path`. Evicts the LRU
  /// entry if the cache is at capacity.
  pub async fn put(&self, path: PathBuf, mtime: Option<SystemTime>, size: u64, parse: CachedParse) {
    if self.capacity == 0 {
      return;
    }
    let next = self.access_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let mut guard = self.inner.write().await;
    guard.insert(
      path,
      Arc::new(CacheEntry {
        mtime,
        size,
        parse,
        last_access: AtomicU64::new(next),
      }),
    );
    if guard.len() > self.capacity {
      // Identify the LRU victim by smallest `last_access`. O(n) in
      // capacity, which is fine for our bound (a few thousand entries
      // at most). Documented here so a future move to a multi-million-
      // entry cache knows to swap in a linked-list-keyed LRU.
      let victim = guard
        .iter()
        .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
        .map(|(p, _)| p.clone());
      if let Some(p) = victim {
        guard.remove(&p);
      }
    }
  }

  /// Current entry count. Useful in tests.
  pub async fn len(&self) -> usize {
    self.inner.read().await.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.is_empty()
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
