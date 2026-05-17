//! Locate LM Studio's models directory.
//!
//! Unlike Ollama, LM Studio stores models as plain `.gguf` files under
//! a single `models/` directory the user can relocate via the LM
//! Studio UI. The on-disk filename is the GGUF's own name (no content
//! addressing), so the regular [`scanner`](super::scanner) walk
//! handles the *enumeration*; this module only resolves *where* to
//! point it.
//!
//! Resolution order, per the v1 plan:
//! 1. `~/.lmstudio/models` (the documented default).
//! 2. `~/.cache/lm-studio/models` (older installs).
//! 3. `~/.lmstudio/settings.json` → `paths.models` (or similar key) —
//!    user override surfaced through the LM Studio GUI.
//!
//! Returns every existing candidate so the merged scan-root list
//! covers users who keep historical caches around.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Return every existing LM Studio models directory under `home`,
/// in resolution-priority order. An empty vec means LM Studio is
/// not installed for this user (or all candidate paths are missing).
pub fn resolve_models_dirs(home: &Path) -> Vec<PathBuf> {
  let mut candidates: Vec<PathBuf> = Vec::new();
  candidates.push(home.join(".lmstudio/models"));
  candidates.push(home.join(".cache/lm-studio/models"));
  if let Some(p) = read_settings_models_dir(home) {
    candidates.push(p);
  }
  // De-duplicate by canonicalised path while preserving order; existing
  // dirs only.
  let mut seen = std::collections::BTreeSet::new();
  let mut out = Vec::new();
  for c in candidates {
    if !c.exists() {
      continue;
    }
    let key = std::fs::canonicalize(&c).unwrap_or_else(|_| c.clone());
    if seen.insert(key) {
      out.push(c);
    }
  }
  out
}

/// LM Studio's settings file. The shape varies between releases; the
/// relevant key shifts across releases:
/// - `paths.models` — older builds.
/// - `modelsDirectory` — even older.
/// - `downloadsFolder` — current top-level key (observed in
///   LM Studio 0.3.x). Set whenever the user relocates the model
///   directory via the GUI; without this key the resolver missed
///   real user installs that pointed at an external drive.
///
/// All three are accepted in priority order.
#[derive(Debug, Deserialize, Default)]
struct LmStudioSettings {
  #[serde(default)]
  paths: Option<PathsSection>,
  #[serde(rename = "modelsDirectory", default)]
  models_directory_legacy: Option<PathBuf>,
  #[serde(rename = "downloadsFolder", default)]
  downloads_folder: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
struct PathsSection {
  #[serde(default)]
  models: Option<PathBuf>,
}

fn read_settings_models_dir(home: &Path) -> Option<PathBuf> {
  let candidates = [
    home.join(".lmstudio/settings.json"),
    home.join(".config/lm-studio/settings.json"),
  ];
  for path in candidates {
    let raw = match std::fs::read_to_string(&path) {
      Ok(s) => s,
      Err(_) => continue,
    };
    let settings: LmStudioSettings = match serde_json::from_str(&raw) {
      Ok(s) => s,
      Err(e) => {
        log::warn!("lm-studio settings parse error at {}: {e}", path.display());
        continue;
      }
    };
    if let Some(p) = settings.paths.and_then(|s| s.models) {
      return Some(p);
    }
    if let Some(p) = settings.models_directory_legacy {
      return Some(p);
    }
    if let Some(p) = settings.downloads_folder {
      return Some(p);
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_home(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamadash-lmstudio-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp home");
    p
  }

  #[test]
  fn returns_default_path_when_only_default_exists() {
    let home = temp_home("default-only");
    fs::create_dir_all(home.join(".lmstudio/models")).unwrap();
    let dirs = resolve_models_dirs(&home);
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0], home.join(".lmstudio/models"));
    fs::remove_dir_all(&home).ok();
  }

  #[test]
  fn returns_empty_when_lm_studio_not_installed() {
    let home = temp_home("missing");
    assert!(resolve_models_dirs(&home).is_empty());
    fs::remove_dir_all(&home).ok();
  }

  #[test]
  fn returns_legacy_cache_dir_when_present() {
    let home = temp_home("legacy");
    fs::create_dir_all(home.join(".cache/lm-studio/models")).unwrap();
    let dirs = resolve_models_dirs(&home);
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].ends_with("lm-studio/models"));
    fs::remove_dir_all(&home).ok();
  }

  #[test]
  fn reads_user_override_from_settings_paths_models() {
    let home = temp_home("override-new");
    let custom = home.join("Custom/Models");
    fs::create_dir_all(&custom).unwrap();
    fs::create_dir_all(home.join(".lmstudio")).unwrap();
    let settings = serde_json::json!({
      "paths": { "models": custom.to_string_lossy() }
    });
    fs::write(
      home.join(".lmstudio/settings.json"),
      serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let dirs = resolve_models_dirs(&home);
    assert!(
      dirs.iter().any(|d| d == &custom),
      "expected override path in {dirs:?}"
    );
    fs::remove_dir_all(&home).ok();
  }

  #[test]
  fn downloads_folder_key_resolves_when_others_absent() {
    // LM Studio 0.3.x writes `downloadsFolder` (not `paths.models`)
    // when the user relocates the model directory via the GUI.
    // Without this key the resolver missed real user installs that
    // pointed at an external drive — observed on the test author's
    // Strix Halo box where the default `.lmstudio/models` had a
    // stale 2-model snapshot and `downloadsFolder` pointed at a
    // 100+-model collection on a separate disk.
    let home = temp_home("downloads-folder");
    let custom = home.join("ExternalDrive/lmstudio-models");
    fs::create_dir_all(&custom).unwrap();
    fs::create_dir_all(home.join(".lmstudio")).unwrap();
    let settings = serde_json::json!({
      "downloadsFolder": custom.to_string_lossy(),
    });
    fs::write(
      home.join(".lmstudio/settings.json"),
      serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let dirs = resolve_models_dirs(&home);
    assert!(
      dirs.iter().any(|d| d == &custom),
      "downloadsFolder must resolve, got {dirs:?}"
    );
    fs::remove_dir_all(&home).ok();
  }

  #[test]
  fn legacy_models_directory_key_still_recognised() {
    let home = temp_home("override-legacy");
    let custom = home.join("OldLocation");
    fs::create_dir_all(&custom).unwrap();
    fs::create_dir_all(home.join(".lmstudio")).unwrap();
    let settings = serde_json::json!({
      "modelsDirectory": custom.to_string_lossy()
    });
    fs::write(
      home.join(".lmstudio/settings.json"),
      serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let dirs = resolve_models_dirs(&home);
    assert!(
      dirs.iter().any(|d| d == &custom),
      "legacy key must still resolve, got {dirs:?}"
    );
    fs::remove_dir_all(&home).ok();
  }
}
