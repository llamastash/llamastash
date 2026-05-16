//! Walk one or more scan roots, group split-GGUF shards, parse each
//! launchable file's header on a bounded pool, and stream results to
//! the caller over an `mpsc` channel (origin: R1, R5, R9).
//!
//! The walk uses the `ignore` crate so `.gitignore` rules and the
//! caller's exclude globs are honoured for free. CPU-bound parsing
//! runs on `tokio::task::spawn_blocking` so the scan tasks don't
//! starve the runtime when the user has hundreds of GGUFs on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::{self, StreamExt};
use ignore::WalkBuilder;
use tokio::sync::mpsc;

use crate::discovery::metadata_cache::{self, CachedParse, MetadataCache};
use crate::discovery::split_gguf::{group, DiscoveredEntry};
use crate::discovery::{DiscoveredModel, ModelSource};
use crate::gguf::{read_path, summarise_metadata, GgufError, HeaderReadOptions};

/// One root to scan plus how to label files found beneath it.
#[derive(Debug, Clone)]
pub struct ScanRoot {
  pub path: PathBuf,
  pub source: ModelSource,
}

/// Options for [`scan`]. `excludes` are appended to the gitignore-
/// derived ignores; absolute or relative-to-root globs both work.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
  pub excludes: Vec<String>,
  /// Capacity for the streaming channel. The TUI is usually faster
  /// than disk, but a tiny capacity makes back-pressure visible in
  /// tests; production defaults to a comfortable buffer.
  pub channel_capacity: Option<usize>,
  /// Optional per-file parse cache. When `Some`, unchanged
  /// `(canonical path, mtime, size)` triples reuse the cached
  /// parse instead of re-reading + re-parsing the header. The
  /// daemon's discovery task wires a shared cache so successive
  /// watcher-driven re-scans don't re-parse the whole tree.
  pub metadata_cache: Option<MetadataCache>,
}

impl ScanOptions {
  pub fn channel_capacity(&self) -> usize {
    self.channel_capacity.unwrap_or(64)
  }
}

/// Begin a scan across `roots`. Returns the receiver immediately; the
/// scan runs in the background and closes the channel when every root
/// has been walked.
///
/// Errors per-file (unreadable directories, parse failures) are
/// surfaced via `DiscoveredModel.parse_error` rather than aborting the
/// whole scan — a single bad model file should not blind the user to
/// the rest of their library (origin: R9 "scan continues with other
/// roots").
pub fn scan(roots: Vec<ScanRoot>, opts: ScanOptions) -> mpsc::Receiver<DiscoveredModel> {
  let (tx, rx) = mpsc::channel(opts.channel_capacity());
  let excludes = Arc::new(opts.excludes);
  let cache = opts.metadata_cache;
  tokio::spawn(async move {
    for root in roots {
      walk_root(root, Arc::clone(&excludes), cache.clone(), tx.clone()).await;
    }
    // dropping `tx` here closes the receiver
  });
  rx
}

async fn walk_root(
  root: ScanRoot,
  excludes: Arc<Vec<String>>,
  cache: Option<MetadataCache>,
  tx: mpsc::Sender<DiscoveredModel>,
) {
  let path = root.path.clone();
  let source = root.source;
  let excludes_for_walk = Arc::clone(&excludes);
  let paths = tokio::task::spawn_blocking(move || collect_gguf_paths(&path, &excludes_for_walk))
    .await
    .unwrap_or_else(|join_err| {
      log::warn!(
        "scan walker task for {} panicked: {join_err}",
        root.path.display()
      );
      Vec::new()
    });

  // Parse files in parallel on the blocking pool. The per-file
  // `build_discovered_model` already pushes its CPU-bound work onto
  // `spawn_blocking`; `buffer_unordered` just lets several of those
  // happen at once instead of strict one-at-a-time await. On a cold
  // HF cache with hundreds of GGUFs this cuts first-scan latency from
  // serial-disk-bound to parallel-disk-bound. We deliberately do NOT
  // use rayon — the work is mostly waiting on disk, and we want the
  // tokio scheduler to interleave with the rest of the daemon.
  let entries: Vec<_> = group(paths);
  let cache_ref = cache.clone();
  let mut stream = stream::iter(entries.into_iter().map(|entry| {
    let cache_ref = cache_ref.clone();
    async move { build_discovered_model(entry, source, cache_ref.as_ref()).await }
  }))
  .buffer_unordered(parallel_parse_limit());
  while let Some(model) = stream.next().await {
    if tx.send(model).await.is_err() {
      return;
    }
  }
}

/// Concurrency cap for [`walk_root`]'s per-file parse. Default to
/// `num_cpus()`-flavoured but capped — too many parallel
/// `spawn_blocking` calls land everything on the blocking pool
/// regardless. Empirically 8 saturates a single NVMe.
fn parallel_parse_limit() -> usize {
  std::thread::available_parallelism()
    .map(|n| n.get().clamp(2, 8))
    .unwrap_or(4)
}

/// Synchronous file-system walk. Returns every `.gguf` file under
/// `root` honouring gitignore semantics and the caller's exclude
/// globs. Unreadable subdirectories are logged and skipped rather
/// than aborting the walk.
fn collect_gguf_paths(root: &Path, excludes: &[String]) -> Vec<PathBuf> {
  if !root.exists() {
    log::warn!("scan root does not exist: {}", root.display());
    return Vec::new();
  }
  let mut builder = WalkBuilder::new(root);
  builder
    .standard_filters(true)
    .require_git(false)
    // Follow symlinks so users who alias a GGUF into a scan root
    // (e.g., `ln -s /big-disk/model.gguf ~/models/`) still see the
    // model. The `ignore` walker detects cycles, so following links
    // doesn't expose us to symlink loops. A hostile symlink pointing
    // at a non-GGUF file is bounded by (a) the `.gguf` extension
    // gate below and (b) the GGUF parser's BadMagic short-circuit
    // and 4 MiB header cap — opening such a file reads at most a
    // few KB and surfaces as a parse error.
    .follow_links(true)
    .hidden(false);
  if !excludes.is_empty() {
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pat in excludes {
      // `ignore`'s override globs treat a leading `!` as include-back,
      // so prefix every user exclude with `!` to mean "exclude this".
      // A plain `*.tmp` glob would otherwise be interpreted as
      // "include only files matching this".
      if let Err(e) = overrides.add(&format!("!{pat}")) {
        log::warn!("invalid scan exclude glob {pat:?}: {e}");
      }
    }
    match overrides.build() {
      Ok(o) => {
        builder.overrides(o);
      }
      Err(e) => log::warn!("scan exclude globs failed to compile: {e}"),
    }
  }

  let mut out = Vec::new();
  let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
  for result in builder.build() {
    match result {
      Ok(entry) => {
        let p = entry.path();
        // Skip `.gguf.part` (mid-download) and only emit regular files
        // ending in `.gguf`. With `follow_links(true)` above, an entry
        // pointing at a symlink reports the *target*'s file type.
        if p.extension().and_then(|s| s.to_str()) == Some("gguf")
          && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
        {
          // Canonicalise before dedup so a real file and a symlink to
          // it collapse to a single row. Falling back to the raw path
          // if canonicalisation fails (broken symlink, permission
          // denied) keeps the row visible — the user can investigate.
          let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
          if seen.insert(canonical.clone()) {
            out.push(canonical);
          }
        }
      }
      Err(e) => log::warn!("scan walker error under {}: {e}", root.display()),
    }
  }
  out
}

async fn build_discovered_model(
  entry: DiscoveredEntry,
  source: ModelSource,
  cache: Option<&MetadataCache>,
) -> DiscoveredModel {
  match entry {
    DiscoveredEntry::Single(path) => parse_into_model(path, source, Vec::new(), cache).await,
    DiscoveredEntry::Split(group) => {
      // Siblings exclude the launch file itself so the field's purpose
      // ("sibling shards") matches its content.
      let siblings = group
        .shards
        .into_iter()
        .filter(|p| *p != group.launch_path)
        .collect();
      parse_into_model(group.launch_path, source, siblings, cache).await
    }
  }
}

async fn parse_into_model(
  path: PathBuf,
  source: ModelSource,
  siblings: Vec<PathBuf>,
  cache: Option<&MetadataCache>,
) -> DiscoveredModel {
  let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
  let probe_path = path.clone();
  let (mtime, size) = tokio::task::spawn_blocking(move || metadata_cache::probe(&probe_path))
    .await
    .unwrap_or((None, 0));

  // Cache lookup first. A hit short-circuits the header read entirely.
  if let Some(c) = cache {
    if let Some(hit) = c.get(&path, mtime, size).await {
      return DiscoveredModel {
        path,
        parent,
        source,
        metadata: hit.metadata,
        parse_error: hit.parse_error,
        split_siblings: siblings,
      };
    }
  }

  let path_for_parse = path.clone();
  let parsed: Result<_, GgufError> =
    tokio::task::spawn_blocking(move || read_path(&path_for_parse, HeaderReadOptions::default()))
      .await
      .unwrap_or_else(|join_err| {
        Err(GgufError::Io(std::io::Error::other(format!(
          "parser task panicked: {join_err}"
        ))))
      });
  let cached = match parsed {
    Ok(read) => CachedParse {
      metadata: Some(summarise_metadata(&read.header)),
      parse_error: None,
    },
    Err(e) => CachedParse {
      metadata: None,
      parse_error: Some(e.to_string()),
    },
  };
  if let Some(c) = cache {
    c.put(path.clone(), mtime, size, cached.clone()).await;
  }
  DiscoveredModel {
    path,
    parent,
    source,
    metadata: cached.metadata,
    parse_error: cached.parse_error,
    split_siblings: siblings,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  use crate::gguf::test_fixtures::build_minimal_gguf;

  fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let dir = std::env::temp_dir().join(format!(
      "llamadash-scanner-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
  }

  #[test]
  fn collect_gguf_paths_skips_part_files() {
    let dir = temp_dir("part");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("a.gguf.part"), b"in-progress").unwrap();
    let paths = collect_gguf_paths(&dir, &[]);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("a.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn collect_gguf_paths_honours_exclude_globs() {
    let dir = temp_dir("excl");
    fs::create_dir_all(dir.join("keep")).unwrap();
    fs::create_dir_all(dir.join("skip")).unwrap();
    fs::write(dir.join("keep/a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("skip/b.gguf"), build_minimal_gguf("llama")).unwrap();
    let paths = collect_gguf_paths(&dir, &["skip/**".to_string()]);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].to_string_lossy().contains("keep"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn nonexistent_root_returns_empty_without_panic() {
    let bogus = PathBuf::from("/nonexistent/scan-root-llamadash");
    assert!(collect_gguf_paths(&bogus, &[]).is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn symlinked_gguf_is_canonicalised_and_deduped() {
    let dir = temp_dir("symlinks");
    fs::write(dir.join("real.gguf"), build_minimal_gguf("llama")).unwrap();
    // Alias the real file with a sibling symlink under the same root.
    let alias = dir.join("alias.gguf");
    std::os::unix::fs::symlink(dir.join("real.gguf"), &alias).unwrap();

    let paths = collect_gguf_paths(&dir, &[]);
    // One canonical row, not two — the symlink collapses onto the
    // real file via `canonicalize` + dedup.
    assert_eq!(
      paths.len(),
      1,
      "real + symlink should collapse to one canonical row, got {paths:?}"
    );
    // The emitted path is the canonical (target) path, not the alias.
    let canon_real = fs::canonicalize(dir.join("real.gguf")).unwrap();
    assert_eq!(paths[0], canon_real);
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn symlink_to_gguf_outside_root_is_followed_once() {
    // The target file lives outside `root`; a symlink under `root`
    // points at it. follow_links must surface the row.
    let outside = temp_dir("symlinks-outside-target");
    let target = outside.join("target.gguf");
    fs::write(&target, build_minimal_gguf("llama")).unwrap();
    let root = temp_dir("symlinks-outside-root");
    std::os::unix::fs::symlink(&target, root.join("aliased.gguf")).unwrap();

    let paths = collect_gguf_paths(&root, &[]);
    assert_eq!(paths.len(), 1, "symlink target outside root must surface");
    assert_eq!(paths[0], fs::canonicalize(&target).unwrap());
    fs::remove_dir_all(&outside).ok();
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn scan_streams_discovered_models_with_metadata() {
    let dir = temp_dir("stream");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("b.gguf"), build_minimal_gguf("qwen3")).unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let mut got = Vec::new();
    while let Some(m) = rx.recv().await {
      got.push(m);
    }
    assert_eq!(got.len(), 2);
    for m in &got {
      assert!(m.metadata.is_some(), "minimal gguf should parse");
      assert_eq!(m.source, ModelSource::UserPath);
      assert!(m.split_siblings.is_empty());
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn scan_surfaces_parse_failure_without_dropping_row() {
    let dir = temp_dir("badparse");
    fs::write(dir.join("bad.gguf"), b"this is not a GGUF").unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let m = rx.recv().await.expect("one model surfaced");
    assert!(rx.recv().await.is_none(), "only one file in dir");
    assert!(m.metadata.is_none(), "invalid file → no metadata");
    assert!(m.parse_error.is_some(), "diagnostic must accompany failure");
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn scan_groups_split_shards_into_one_entry() {
    let dir = temp_dir("split");
    let bytes = build_minimal_gguf("llama");
    fs::write(dir.join("model-00001-of-00003.gguf"), &bytes).unwrap();
    fs::write(dir.join("model-00002-of-00003.gguf"), &bytes).unwrap();
    fs::write(dir.join("model-00003-of-00003.gguf"), &bytes).unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let m = rx.recv().await.expect("one grouped entry");
    assert!(
      rx.recv().await.is_none(),
      "shard set should collapse to one"
    );
    assert_eq!(m.split_siblings.len(), 2, "shard 1 plus 2 siblings");
    assert!(m
      .path
      .to_string_lossy()
      .ends_with("model-00001-of-00003.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn metadata_cache_reuses_parse_for_unchanged_file() {
    use crate::discovery::metadata_cache::MetadataCache;

    let dir = temp_dir("cache-hit");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    let cache = MetadataCache::new(8);
    let opts = ScanOptions {
      metadata_cache: Some(cache.clone()),
      ..ScanOptions::default()
    };
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];

    async fn drain(mut rx: mpsc::Receiver<DiscoveredModel>) {
      while rx.recv().await.is_some() {}
    }

    // First scan: cache empty → one miss → one entry inserted.
    drain(scan(roots.clone(), opts.clone())).await;
    assert_eq!(cache.len().await, 1, "first scan populates cache");

    // Second scan: cache hit → still one entry, parse skipped.
    drain(scan(roots.clone(), opts.clone())).await;
    assert_eq!(cache.len().await, 1, "second scan does not duplicate");
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn metadata_cache_invalidates_when_size_changes() {
    use crate::discovery::metadata_cache::MetadataCache;
    use crate::gguf::test_fixtures::FixtureBuilder;

    let dir = temp_dir("cache-invalid");
    let path = dir.join("a.gguf");
    // First write: minimal arch=llama header.
    fs::write(&path, build_minimal_gguf("llama")).unwrap();
    let cache = MetadataCache::new(8);
    let opts = ScanOptions {
      metadata_cache: Some(cache.clone()),
      ..ScanOptions::default()
    };
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];

    // Prime the cache.
    let mut first_rx = scan(roots.clone(), opts.clone());
    let first = first_rx.recv().await.expect("one model");
    while first_rx.recv().await.is_some() {}
    let first_arch = first
      .metadata
      .as_ref()
      .and_then(|m| m.arch.clone())
      .unwrap();
    assert_eq!(first_arch, "llama");

    // Mutate the file: different arch, different total tensor count
    // → different on-disk size and (typically) different mtime.
    let updated_bytes = FixtureBuilder::new()
      .with_arch("phi3")
      .with_context_length(4096)
      .with_tensor("blk.0.attn_q.weight", &[10, 10], 1)
      .build();
    fs::write(&path, &updated_bytes).unwrap();

    // Force the on-disk mtime to advance even on filesystems whose
    // mtime resolution is coarse (some CI tmpfs sets mtime to whole
    // seconds and rewrites within the same second can otherwise look
    // unchanged). Size *also* changed above, which alone invalidates
    // the cache; this just makes the invalidation independent of fs
    // mtime granularity.
    let _ = std::process::Command::new("touch")
      .arg("-t")
      .arg("203601011200")
      .arg(&path)
      .status();

    let mut second_rx = scan(roots, opts);
    let second = second_rx.recv().await.expect("one model");
    while second_rx.recv().await.is_some() {}
    let second_arch = second
      .metadata
      .as_ref()
      .and_then(|m| m.arch.clone())
      .unwrap();
    assert_eq!(
      second_arch, "phi3",
      "size change must invalidate cache and re-parse"
    );
  }
}
