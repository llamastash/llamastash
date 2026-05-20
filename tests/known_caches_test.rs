//! End-to-end tests for cache-aware scan-root resolution: HuggingFace
//! hub, Ollama, LM Studio, plus the global `--no-scan` switch.

use std::path::PathBuf;

use llamastash::config::CachePathsConfig;
use llamastash::discovery::known_caches::{default_set, RootResolution};
use llamastash::discovery::ModelSource;

fn all_enabled() -> CachePathsConfig {
  CachePathsConfig {
    huggingface: false,
    ollama: false,
    lm_studio: false,
  }
}

#[test]
fn default_resolution_covers_all_three_known_caches() {
  let user: Vec<PathBuf> = Vec::new();
  let home = PathBuf::from("/home/alice");
  let roots = default_set(RootResolution {
    user_paths: &user,
    disable: &all_enabled(),
    no_scan: false,
    home: Some(&home),
  });
  let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
  assert!(sources.contains(&ModelSource::HuggingFace));
  assert!(sources.contains(&ModelSource::Ollama));
  assert!(sources.contains(&ModelSource::LmStudio));
}

#[test]
fn no_scan_suppresses_default_caches_but_keeps_user_paths() {
  // R4: `--no-scan` / `LLAMASTASH_NO_SCAN=1` must keep only user-supplied
  // paths so agent invocations are scope-deterministic.
  let user = vec![PathBuf::from("/work/models")];
  let home = PathBuf::from("/home/alice");
  let roots = default_set(RootResolution {
    user_paths: &user,
    disable: &all_enabled(),
    no_scan: true,
    home: Some(&home),
  });
  assert_eq!(roots.len(), 1);
  assert_eq!(roots[0].path, PathBuf::from("/work/models"));
  assert_eq!(roots[0].source, ModelSource::UserPath);
}

#[test]
fn disabling_huggingface_keeps_ollama_and_lm_studio() {
  let mut disable = all_enabled();
  disable.huggingface = true;
  let home = PathBuf::from("/home/alice");
  let user: Vec<PathBuf> = Vec::new();
  let roots = default_set(RootResolution {
    user_paths: &user,
    disable: &disable,
    no_scan: false,
    home: Some(&home),
  });
  let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
  assert!(!sources.contains(&ModelSource::HuggingFace));
  assert!(sources.contains(&ModelSource::Ollama));
  assert!(sources.contains(&ModelSource::LmStudio));
}
