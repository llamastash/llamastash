//! Daemon-side orchestration of discovery.
//!
//! Spawns one long-running task per daemon that:
//! 1. Runs the scanner once at startup, populating the [`ModelCatalog`].
//! 2. Enumerates Ollama-managed models alongside, since their blobs
//!    don't surface through the regular `.gguf` extension filter.
//! 3. Starts the filesystem watcher and re-scans whenever a watcher
//!    event fires (debounced by `notify-debouncer-mini` at the
//!    watcher layer) or whenever the 5-minute periodic backstop
//!    ticks.
//!
//! The IPC layer's `list_models` handler reads from the same
//! catalog, so changes on disk reflect in the `list_models` response
//! within ~1 second of the file landing (per the plan's integration
//! verification).

use tokio::sync::mpsc;

use crate::discovery::catalog::ModelCatalog;
use crate::discovery::metadata_cache::MetadataCache;
use crate::discovery::ollama;
use crate::discovery::scanner::{scan, ScanOptions, ScanRoot};
use crate::discovery::watcher::{self, WatchEvent, WatchMode, WatchRoot, WatcherOptions};
use crate::discovery::{DiscoveredModel, ModelSource};

/// Inputs the daemon hands to [`spawn`]. Anything that needs config
/// resolution belongs on the caller side — by the time we land in
/// this module the roots are already final.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
  pub scan_roots: Vec<ScanRoot>,
  pub scan: ScanOptions,
  pub watcher: WatcherOptions,
  /// When `Some(port)`, each rescan also enumerates the `lemond` umbrella
  /// on that loopback port and merges its models as Lemonade-tagged catalog
  /// rows (R11, list-only). `None` (the default, and whenever the Lemonade
  /// backend is disabled) skips it entirely — no `lemond` contact.
  pub lemonade_port: Option<u16>,
  /// Backend config + force map, consulted on **every** rescan to decide which
  /// backends project rows from the shared safetensors / HF-repo enumerator.
  ///
  /// Re-read each pass rather than resolved once at boot: `status` evaluates
  /// availability live, so a backend installed after the daemon started used
  /// to report itself enabled while the walk stayed permanently skipped — a
  /// dead state the shipped diagnostic confirmed as healthy. None projecting
  /// skips the walk entirely, so an install with no safetensors-capable
  /// backend pays nothing and its catalog stays byte-identical. This module
  /// names no backend.
  pub backend: crate::backend::BackendConfig,
  pub backend_force: std::collections::BTreeMap<String, bool>,
}

impl DiscoveryOptions {
  /// Backends that project HF-repo rows right now.
  fn hf_repo_projectors(&self) -> Vec<crate::backend::Backends> {
    crate::backend::Backends::all()
      .into_iter()
      .filter(|b| {
        crate::backend::Backend::projects_hf_repos(b)
          && crate::backend::Backend::enabled_in_config(b, &self.backend, &self.backend_force)
      })
      .collect()
  }

  pub fn new(roots: Vec<ScanRoot>) -> Self {
    // Production builds default to the standard-capacity metadata
    // cache so successive watcher-driven rescans don't re-parse
    // headers for unchanged files. Tests that want a no-cache or
    // small-cache configuration can replace this on the returned
    // value.
    let scan = ScanOptions {
      metadata_cache: Some(MetadataCache::default_capacity()),
      ..ScanOptions::default()
    };
    Self {
      scan_roots: roots,
      scan,
      watcher: WatcherOptions::default(),
      lemonade_port: None,
      backend: Default::default(),
      backend_force: Default::default(),
    }
  }
}

/// Spawn the discovery orchestration task. The returned `JoinHandle`
/// completes when the watcher channel closes (catalog drops, or the
/// daemon shuts down). The catalog is populated as soon as the
/// initial scan finishes.
pub fn spawn(catalog: ModelCatalog, opts: DiscoveryOptions) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    run(catalog, opts).await;
  })
}

async fn run(catalog: ModelCatalog, opts: DiscoveryOptions) {
  // Initial scan: drain every scanner row into the catalog.
  full_rescan(&catalog, &opts).await;

  // Build per-root watcher descriptors. HuggingFace's hub layout is
  // deeply nested (`models--<owner>--<repo>/snapshots/<rev>/blobs/`
  // plus a `refs/` tree), so a recursive watch would burn through
  // inotify slots and tank steady-state memory. Use a `Shallow`
  // watch on HF roots: new top-level `models--*` dirs still fire
  // events; deeper changes are caught by the 5-minute periodic
  // rescan backstop. Other sources stay recursive.
  let watch_roots: Vec<WatchRoot> = opts
    .scan_roots
    .iter()
    .map(|r| WatchRoot {
      path: r.path.clone(),
      mode: watch_mode_for(r.source),
    })
    .collect();
  let (handle, mut rx) = match watcher::start(watch_roots, opts.watcher) {
    Ok(v) => v,
    Err(e) => {
      log::warn!(
        "discovery: filesystem watcher failed to start: {e}; running in static-catalog mode"
      );
      // Without the watcher, the initial scan is all the catalog
      // ever sees — but list_models still works. This matches the
      // plan's "scan continues with other roots" resilience posture.
      return;
    }
  };

  while let Some(event) = rx.recv().await {
    match event {
      WatchEvent::Changed { paths } => {
        // We could do surgical updates per-path, but the simplest
        // correct thing is a full re-scan; the scanner is fast on
        // disk-cached trees and the catalog replacement is atomic.
        log::debug!(
          "discovery: re-scanning after watcher event ({} paths)",
          paths.len()
        );
        full_rescan(&catalog, &opts).await;
      }
      WatchEvent::PeriodicRescan => {
        log::debug!("discovery: periodic rescan tick");
        full_rescan(&catalog, &opts).await;
      }
    }
  }

  // Keep `handle` alive for the loop's lifetime. Dropping it here
  // tears down the filesystem watcher and the periodic ticker.
  drop(handle);
}

/// Drain `scanner::scan` plus the Ollama enumerator into a fresh
/// vector and atomically replace the catalog. Errors per-file
/// surface as `DiscoveredModel { metadata: None, parse_error: Some
/// }` rows; the scan never aborts.
async fn full_rescan(catalog: &ModelCatalog, opts: &DiscoveryOptions) {
  let scanner_rx = scan(opts.scan_roots.clone(), opts.scan.clone());

  // The Ollama enumerator runs alongside the regular scanner because
  // Ollama's content-addressed blob layout doesn't surface through
  // `.gguf` extension filtering. We launch one per Ollama root.
  let ollama_rxs: Vec<mpsc::Receiver<DiscoveredModel>> = opts
    .scan_roots
    .iter()
    .filter(|r| r.source == ModelSource::Ollama)
    .map(|r| ollama::enumerate(r.path.clone(), opts.scan.metadata_cache.clone()))
    .collect();

  let mut new_models: Vec<DiscoveredModel> = Vec::new();

  let mut scanner_rx = scanner_rx;
  while let Some(m) = scanner_rx.recv().await {
    new_models.push(m);
  }
  for mut rx in ollama_rxs {
    while let Some(m) = rx.recv().await {
      new_models.push(m);
    }
  }

  // Lemonade models (R11, opt-in, list-only). Best-effort: an unreachable
  // umbrella yields no rows and never aborts the disk scan above.
  if let Some(port) = opts.lemonade_port {
    new_models.extend(crate::backend::lemonade::discovery::enumerate(port).await);
  }

  // Safetensors / HF-repo rows. One walk, shared across every projecting
  // backend, and skipped outright when none is enabled.
  let projectors = opts.hf_repo_projectors();
  if !projectors.is_empty() {
    // Walk the *configured* roots, not a re-derivation from `$HOME`. Deriving
    // them independently escaped the discovery scope in both directions:
    // `--no-scan` still got HF-cache rows it had excluded, and an HF-layout
    // tree under a configured `model_paths` root was never seen. A root with
    // no `models--*` children contributes nothing, so passing them all is safe.
    let roots: Vec<std::path::PathBuf> = opts.scan_roots.iter().map(|r| r.path.clone()).collect();
    // Synchronous `read_dir` + `serde_json` per repo, unlike every other
    // producer here (all channel-fed or genuinely async). Inline it and a
    // large cache blocks a runtime worker on each debounced watcher event.
    // Projection belongs on the same side of the fence: it stats every weight
    // shard through the cache's symlinks, which is the costlier half.
    let projected = tokio::task::spawn_blocking(move || {
      let candidates = crate::discovery::hf_repos::enumerate_repos(&roots);
      projectors
        .iter()
        .flat_map(|b| crate::backend::Backend::project_hf_repos(b, &candidates))
        .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_else(|e| {
      log::warn!("hf-repo discovery task panicked: {e}");
      Vec::new()
    });
    new_models.extend(projected);
  }

  catalog.replace_all(new_models).await;
}

/// Choose the watcher depth for a scan root based on its provenance.
/// HF hub trees go non-recursive; everything else stays recursive.
fn watch_mode_for(source: ModelSource) -> WatchMode {
  match source {
    ModelSource::HuggingFace => WatchMode::Shallow,
    // Lemonade models come from the `lemond` API, not a filesystem root, so
    // this is never reached for them in practice; default to recursive.
    ModelSource::Ollama | ModelSource::LmStudio | ModelSource::UserPath | ModelSource::Lemonade => {
      WatchMode::Recursive
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::path::PathBuf;
  use std::time::{Duration, SystemTime, UNIX_EPOCH};

  use crate::gguf::test_fixtures::build_minimal_gguf;

  fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-discovery-task-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
  }

  fn fast_watcher() -> WatcherOptions {
    // The periodic rescan is intentionally short here so the test is
    // robust under heavy parallel `cargo test` load: even if the
    // watcher's event firing slips beyond the inline poll budget, the
    // periodic tick drives a re-scan and the catalog still converges
    // within seconds.
    WatcherOptions {
      debounce: Duration::from_millis(50),
      periodic_rescan: Duration::from_secs(1),
      channel_capacity: 16,
    }
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn initial_scan_populates_catalog_before_watcher_runs() {
    let dir = temp_dir("initial");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("b.gguf"), build_minimal_gguf("qwen3")).unwrap();
    let catalog = ModelCatalog::new();
    let opts = DiscoveryOptions {
      scan_roots: vec![ScanRoot {
        path: dir.clone(),
        source: ModelSource::UserPath,
      }],
      scan: ScanOptions::default(),
      watcher: fast_watcher(),
      lemonade_port: None,
      backend: Default::default(),
      backend_force: Default::default(),
    };
    let _task = spawn(catalog.clone(), opts);

    // Initial scan is sync within the task; poll briefly.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while catalog.len().await < 2 {
      if std::time::Instant::now() > deadline {
        panic!(
          "initial scan never populated catalog; size = {}",
          catalog.len().await
        );
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(catalog.len().await, 2);
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn watcher_event_re_scans_catalog_to_include_new_file() {
    let dir = temp_dir("watch");
    fs::write(dir.join("seed.gguf"), build_minimal_gguf("llama")).unwrap();
    let catalog = ModelCatalog::new();
    let opts = DiscoveryOptions {
      scan_roots: vec![ScanRoot {
        path: dir.clone(),
        source: ModelSource::UserPath,
      }],
      scan: ScanOptions::default(),
      watcher: fast_watcher(),
      lemonade_port: None,
      backend: Default::default(),
      backend_force: Default::default(),
    };
    let _task = spawn(catalog.clone(), opts);

    // Wait for the initial scan.
    let initial_deadline = std::time::Instant::now() + Duration::from_secs(3);
    while catalog.len().await < 1 {
      if std::time::Instant::now() > initial_deadline {
        panic!("initial scan never populated catalog");
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Drop a new file and verify the catalog grows.
    fs::write(dir.join("added.gguf"), build_minimal_gguf("phi3")).unwrap();
    let watch_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while catalog.len().await < 2 {
      if std::time::Instant::now() > watch_deadline {
        panic!(
          "watcher never picked up added.gguf; catalog still at {}",
          catalog.len().await
        );
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let snap = catalog.snapshot().await;
    assert!(
      snap.iter().any(|m| m.path.ends_with("added.gguf")),
      "expected added.gguf in {snap:?}"
    );
    fs::remove_dir_all(&dir).ok();
  }
}
