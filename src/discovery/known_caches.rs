//! Resolve the set of scan roots discovery should walk by default,
//! merging user-configured paths with well-known cache locations
//! (HuggingFace hub, Ollama, LM Studio).
//!
//! Per-cache disables in `config.disable_default_cache_paths` let
//! users opt out of any single source without having to enumerate
//! the remaining roots by hand. A global `no_scan` flag (set by
//! `--no-scan` or `LLAMASTASH_NO_SCAN=1`) skips everything except
//! explicitly-passed `--model-path` roots, so agent invocations can
//! pin their scan surface.
//!
//! Path resolution (env vars + platform defaults) lives in
//! [`crate::util::model_caches`] so writes (`init::download`) and
//! scans (this module) stay in lockstep — see that module's docs for
//! the full precedence chain.

use std::path::{Path, PathBuf};

use crate::config::CachePathsConfig;
use crate::discovery::lm_studio;
use crate::discovery::scanner::ScanRoot;
use crate::discovery::ModelSource;
use crate::util::model_caches;

/// Inputs to [`default_set`]. `user_paths` are unconditional — they
/// participate even when `no_scan` is set, since the user asked for
/// them explicitly. `disable` and `no_scan` shape the default cache
/// roots.
#[derive(Debug, Clone)]
pub struct RootResolution<'a> {
  pub user_paths: &'a [PathBuf],
  pub disable: &'a CachePathsConfig,
  pub no_scan: bool,
  /// Resolved `$HOME` (or equivalent). The platform default-cache
  /// paths are anchored under this. Discovery passes `dirs::home_dir`
  /// in production; tests pass a temp dir.
  pub home: Option<&'a Path>,
}

/// Merge user-configured roots with default cache roots into the
/// canonical ordered scan list. Duplicate paths are collapsed in
/// insertion order so a `--model-path` that overlaps an HF cache is
/// listed once.
pub fn default_set(res: RootResolution<'_>) -> Vec<ScanRoot> {
  let mut out: Vec<ScanRoot> = Vec::new();
  let mut seen = std::collections::BTreeSet::new();

  for p in res.user_paths {
    push_unique(
      &mut out,
      &mut seen,
      ScanRoot {
        path: canonical_or_raw(p),
        source: ModelSource::UserPath,
      },
    );
  }

  if res.no_scan {
    // Everything else is suppressed: agents that want deterministic
    // scope rely on this.
    return out;
  }

  if !res.disable.huggingface {
    for p in model_caches::huggingface_hub_dirs(res.home) {
      push_unique(
        &mut out,
        &mut seen,
        ScanRoot {
          path: p,
          source: ModelSource::HuggingFace,
        },
      );
    }
  }
  if !res.disable.ollama {
    for p in model_caches::ollama_models_dirs(res.home) {
      push_unique(
        &mut out,
        &mut seen,
        ScanRoot {
          path: p,
          source: ModelSource::Ollama,
        },
      );
    }
  }
  if !res.disable.lm_studio {
    // LM Studio's defaults and settings.json override both anchor on
    // the home directory, so skip them entirely when home is missing.
    if let Some(home) = res.home {
      for p in model_caches::lm_studio_models_dirs(home) {
        push_unique(
          &mut out,
          &mut seen,
          ScanRoot {
            path: p,
            source: ModelSource::LmStudio,
          },
        );
      }
      // Honour the GUI-configured `paths.models` override the user set
      // in `~/.lmstudio/settings.json`. The resolver only returns
      // *existing* directories, so this won't generate phantom roots
      // when LM Studio isn't installed.
      for p in lm_studio::resolve_models_dirs(home) {
        push_unique(
          &mut out,
          &mut seen,
          ScanRoot {
            path: p,
            source: ModelSource::LmStudio,
          },
        );
      }
    }
  }
  out
}

fn canonical_or_raw(p: &Path) -> PathBuf {
  crate::util::paths::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn push_unique(
  out: &mut Vec<ScanRoot>,
  seen: &mut std::collections::BTreeSet<PathBuf>,
  root: ScanRoot,
) {
  if seen.insert(root.path.clone()) {
    out.push(root);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn disable_default() -> CachePathsConfig {
    CachePathsConfig {
      huggingface: false,
      ollama: false,
      lm_studio: false,
    }
  }

  #[test]
  fn no_scan_keeps_only_user_paths() {
    let user = vec![PathBuf::from("/explicit/user/path")];
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: true,
      home: Some(&home),
    });
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].source, ModelSource::UserPath);
  }

  #[test]
  fn default_includes_all_three_caches_when_enabled() {
    let user: Vec<PathBuf> = Vec::new();
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
    assert!(sources.contains(&ModelSource::HuggingFace));
    assert!(sources.contains(&ModelSource::Ollama));
    assert!(sources.contains(&ModelSource::LmStudio));
  }

  #[test]
  fn per_cache_disable_drops_only_that_source() {
    let user: Vec<PathBuf> = Vec::new();
    let home = PathBuf::from("/home/user");
    let mut disable = disable_default();
    disable.ollama = true;
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable,
      no_scan: false,
      home: Some(&home),
    });
    let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
    assert!(sources.contains(&ModelSource::HuggingFace));
    assert!(!sources.contains(&ModelSource::Ollama), "ollama disabled");
    assert!(sources.contains(&ModelSource::LmStudio));
  }

  #[test]
  fn default_set_surfaces_hf_hub_cache_override_as_huggingface_root() {
    // End-to-end: a user with `HF_HUB_CACHE` set must see that path in
    // the rescanner's root list, otherwise `llamastash list` misses
    // models downloaded by `llamastash pull` under the same override.
    // This is the regression test for the discovery / download
    // asymmetry: before the fix, downloads landed at the override but
    // discovery scanned only `~/.cache/huggingface/hub`.
    let _lock = crate::cli::test_lock::serialize();
    let saved_hub = std::env::var_os("HF_HUB_CACHE");
    let saved_home = std::env::var_os("HF_HOME");

    std::env::set_var("HF_HUB_CACHE", "/mnt/relocated-hf-hub");
    std::env::remove_var("HF_HOME");

    let user: Vec<PathBuf> = Vec::new();
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let hf_roots: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::HuggingFace)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      hf_roots.contains(&Path::new("/mnt/relocated-hf-hub")),
      "HF_HUB_CACHE override must appear among HF roots, got {hf_roots:?}",
    );

    match saved_hub {
      Some(v) => std::env::set_var("HF_HUB_CACHE", v),
      None => std::env::remove_var("HF_HUB_CACHE"),
    }
    match saved_home {
      Some(v) => std::env::set_var("HF_HOME", v),
      None => std::env::remove_var("HF_HOME"),
    }
  }

  #[test]
  fn user_path_overlapping_a_default_cache_dedupes() {
    // The hf-hub default lives under ~/.cache/huggingface/hub. If the
    // user passes that exact path via --model-path, we want it listed
    // once, not once-as-user-and-once-as-huggingface.
    let user = vec![PathBuf::from("/home/user/.cache/huggingface/hub")];
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let count_for_path: usize = roots
      .iter()
      .filter(|r| r.path == Path::new("/home/user/.cache/huggingface/hub"))
      .count();
    assert_eq!(count_for_path, 1);
    // The user-supplied entry wins because it was inserted first.
    assert_eq!(roots[0].source, ModelSource::UserPath);
  }

  /// On Linux, `ollama_models_dirs` always appends the systemd
  /// installer's path, regardless of env / home. Tests that build
  /// `expected` paths use this to stay platform-agnostic.
  fn expected_ollama_system_path() -> Option<&'static Path> {
    if cfg!(target_os = "linux") {
      Some(Path::new("/usr/share/ollama/.ollama/models"))
    } else {
      None
    }
  }

  #[test]
  fn missing_home_dir_yields_user_paths_and_only_unanchored_defaults() {
    // With home=None and no env overrides, HF / LM Studio contribute
    // nothing (their defaults all need a home). Ollama is special: on
    // Linux the official systemd installer drops models under
    // `/usr/share/ollama/.ollama/models`, which is reachable without
    // any home, so that path surfaces unconditionally on Linux.
    let _lock = crate::cli::test_lock::serialize();
    let saved: Vec<(&str, _)> = [
      "HF_HUB_CACHE",
      "HUGGINGFACE_HUB_CACHE",
      "HF_HOME",
      "XDG_CACHE_HOME",
      "OLLAMA_MODELS",
    ]
    .iter()
    .map(|k| (*k, std::env::var_os(k)))
    .collect();
    for (k, _) in &saved {
      std::env::remove_var(k);
    }

    let user = vec![PathBuf::from("/explicit/path")];
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: None,
    });
    assert_eq!(roots[0].source, ModelSource::UserPath);
    assert_eq!(roots[0].path, Path::new("/explicit/path"));
    match expected_ollama_system_path() {
      Some(p) => {
        assert_eq!(roots.len(), 2, "Linux: UserPath + Ollama system path");
        assert_eq!(roots[1].source, ModelSource::Ollama);
        assert_eq!(roots[1].path, p);
      }
      None => {
        assert_eq!(roots.len(), 1, "non-Linux: UserPath only when home=None");
      }
    }

    for (k, v) in saved {
      match v {
        Some(val) => std::env::set_var(k, val),
        None => std::env::remove_var(k),
      }
    }
  }

  #[test]
  fn missing_home_dir_still_surfaces_env_overrides() {
    // With home=None, HF_HUB_CACHE / OLLAMA_MODELS overrides must still
    // appear so a user with a relocated cache and no resolvable $HOME
    // (rare but possible in sandboxes) doesn't lose visibility. LM Studio
    // is skipped — it has no env override and its defaults need home.
    // On Linux the Ollama systemd path is appended unconditionally.
    let _lock = crate::cli::test_lock::serialize();
    let saved_hf = std::env::var_os("HF_HUB_CACHE");
    let saved_hf_hub = std::env::var_os("HUGGINGFACE_HUB_CACHE");
    let saved_hf_home = std::env::var_os("HF_HOME");
    let saved_xdg = std::env::var_os("XDG_CACHE_HOME");
    let saved_ollama = std::env::var_os("OLLAMA_MODELS");
    std::env::set_var("HF_HUB_CACHE", "/relocated/hub");
    std::env::set_var("OLLAMA_MODELS", "/relocated/ollama");
    // Clear the other env vars that huggingface_hub_dirs checks, so
    // only HF_HUB_CACHE drives the result — the test verifies that a
    // relocated override wins even when home is None.
    std::env::remove_var("HF_HOME");
    std::env::remove_var("XDG_CACHE_HOME");
    std::env::remove_var("HUGGINGFACE_HUB_CACHE");

    let user: Vec<PathBuf> = Vec::new();
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: None,
    });
    let by_source: std::collections::BTreeMap<ModelSource, Vec<&Path>> =
      roots
        .iter()
        .fold(std::collections::BTreeMap::new(), |mut m, r| {
          m.entry(r.source).or_default().push(r.path.as_path());
          m
        });
    assert_eq!(
      by_source
        .get(&ModelSource::HuggingFace)
        .map(|v| v.as_slice()),
      Some(&[Path::new("/relocated/hub")][..]),
    );
    let mut expected_ollama = vec![Path::new("/relocated/ollama")];
    if let Some(sys) = expected_ollama_system_path() {
      expected_ollama.push(sys);
    }
    assert_eq!(
      by_source.get(&ModelSource::Ollama).map(|v| v.as_slice()),
      Some(expected_ollama.as_slice()),
    );
    assert!(
      !by_source.contains_key(&ModelSource::LmStudio),
      "LM Studio has no env override; must not appear without home",
    );

    match saved_hf {
      Some(v) => std::env::set_var("HF_HUB_CACHE", v),
      None => std::env::remove_var("HF_HUB_CACHE"),
    }
    match saved_ollama {
      Some(v) => std::env::set_var("OLLAMA_MODELS", v),
      None => std::env::remove_var("OLLAMA_MODELS"),
    }
    match saved_hf_hub {
      Some(v) => std::env::set_var("HUGGINGFACE_HUB_CACHE", v),
      None => std::env::remove_var("HUGGINGFACE_HUB_CACHE"),
    }
    match saved_hf_home {
      Some(v) => std::env::set_var("HF_HOME", v),
      None => std::env::remove_var("HF_HOME"),
    }
    match saved_xdg {
      Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
      None => std::env::remove_var("XDG_CACHE_HOME"),
    }
  }

  #[test]
  fn lm_studio_settings_override_surfaces_as_lm_studio_root() {
    // When a real LM Studio install advertises a non-default
    // `paths.models` in `~/.lmstudio/settings.json`, that directory
    // must show up among the LM Studio roots — not get silently
    // dropped in favour of the hard-coded `~/.lmstudio/models`.
    use std::fs;
    let home = std::env::temp_dir().join(format!(
      "llamastash-known-caches-lmstudio-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    fs::create_dir_all(home.join(".lmstudio")).unwrap();
    let custom = home.join("Models/LmStudio");
    fs::create_dir_all(&custom).unwrap();
    let settings = serde_json::json!({"paths": {"models": custom.to_string_lossy()}});
    fs::write(
      home.join(".lmstudio/settings.json"),
      serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();

    let user: Vec<PathBuf> = Vec::new();
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let lm_paths: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::LmStudio)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      lm_paths.contains(&custom.as_path()),
      "LM Studio override `{}` must appear among roots, got {:?}",
      custom.display(),
      lm_paths
    );
    fs::remove_dir_all(&home).ok();
  }
}
