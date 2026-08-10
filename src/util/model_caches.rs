//! Single source of truth for HuggingFace / Ollama / LM Studio model
//! cache locations. Used by `init::download` (where to *write* a pulled
//! model) and `discovery::known_caches` (where to *scan* for installed
//! models). Both must agree, otherwise `llamastash pull` lands models in
//! one place and `llamastash list` looks for them in another.
//!
//! ## HuggingFace
//!
//! Precedence mirrors `hf-hub` 1.0.0-rc.1 (the crate driving downloads)
//! and `huggingface_hub` (Python):
//!
//! 1. `HF_HUB_CACHE` — verbatim (most specific override).
//! 2. `HUGGINGFACE_HUB_CACHE` — deprecated alias, still honored by HF
//!    tooling so users with it set in their shell see consistent
//!    behavior across `huggingface-cli`, `transformers`, and llamastash.
//! 3. `$HF_HOME/hub` — moves the whole HF root.
//! 4. `$XDG_CACHE_HOME/huggingface/hub` — Linux XDG compliance, also
//!    honored on macOS when the user has explicitly set it.
//! 5. `$HOME/.cache/huggingface/hub` — the documented default on both
//!    Linux and macOS (HF tooling does *not* use `~/Library/Caches` on
//!    macOS, despite the platform convention).
//!
//! Empty env values are treated as unset so a stray `HF_HOME=` in a
//! shell rc doesn't redirect to the cwd.
//!
//! ## Ollama
//!
//! `OLLAMA_MODELS` (verbatim) → `$HOME/.ollama/models` (user install,
//! all platforms) → `/usr/share/ollama/.ollama/models` (Linux only;
//! the system-install location used by the official `curl | sh`
//! systemd installer). The system path is owned by the `ollama` user
//! and may require group membership to read, but listing it lets us
//! surface those models when permissions permit.
//!
//! ## LM Studio
//!
//! No env var override. Platform-default scan paths only; the user's
//! `~/.lmstudio/settings.json` `paths.models` value is loaded
//! separately by [`crate::discovery::lm_studio::resolve_models_dirs`].

use std::path::{Path, PathBuf};

/// HuggingFace hub cache roots in priority order. Used for both writes
/// (downloads pick `.first()`) and scans (discovery walks the whole
/// list). The full list is returned even when an env override is set so
/// a mid-migration user with stale models at the platform default
/// doesn't lose visibility; `discovery::known_caches::default_set`
/// dedupes overlaps.
///
/// `home` may be `None` when the platform can't supply a home dir; in
/// that case only env-derived paths are returned (empty if no env
/// override is set either).
pub fn huggingface_hub_dirs(home: Option<&Path>) -> Vec<PathBuf> {
  let mut paths = Vec::new();

  if let Some(v) = non_empty_env("HF_HUB_CACHE") {
    paths.push(PathBuf::from(v));
  }
  if let Some(v) = non_empty_env("HUGGINGFACE_HUB_CACHE") {
    paths.push(PathBuf::from(v));
  }
  if let Some(v) = non_empty_env("HF_HOME") {
    paths.push(PathBuf::from(v).join("hub"));
  }
  if let Some(v) = non_empty_env("XDG_CACHE_HOME") {
    paths.push(PathBuf::from(v).join("huggingface/hub"));
  }
  if let Some(h) = home {
    paths.push(h.join(".cache/huggingface/hub"));
  }
  paths
}

/// Primary HuggingFace hub cache location — where a fresh download
/// should land. Equal to the first entry of [`huggingface_hub_dirs`].
/// `None` only when no env override is set *and* no home dir is
/// available (a misconfigured system).
pub fn huggingface_primary_hub_dir(home: Option<&Path>) -> Option<PathBuf> {
  huggingface_hub_dirs(home).into_iter().next()
}

/// Encode a repo id into its HF hub cache folder name: `owner/name` →
/// `models--owner--name`. Matches `huggingface_hub` and `hf-hub`'s
/// `Repo::folder_name`. Inverse of [`repo_id_from_cache_dir`].
pub fn repo_folder_name(repo_id: &str) -> String {
  format!("models--{}", repo_id.replace('/', "--"))
}

/// Decode an HF hub cache folder name back into its repo id:
/// `models--owner--name` → `owner/name`. Inverse of [`repo_folder_name`].
///
/// HF replaces the single `/` in a repo id with `--`, so the owner is the
/// segment before the first `--` and the name is the rest verbatim (preserving
/// any `--` the repo name itself contains). Returns `None` for anything that
/// isn't a `models--` dir or that decodes to an empty owner / name.
///
/// The decoded id is a catalog key a backend turns back into a path
/// (`<scheme>://<repo-id>`), so path-traversal shapes a crafted cache dir name
/// (`models--..--passwd`) could mint are rejected — a real HF owner / repo
/// never contains a separator or `..`.
pub fn repo_id_from_cache_dir(dir_name: &str) -> Option<String> {
  let rest = dir_name.strip_prefix("models--")?;
  let (owner, repo) = rest.split_once("--")?;
  if owner.is_empty() || repo.is_empty() {
    return None;
  }
  if [owner, repo]
    .iter()
    .any(|s| s.contains('/') || s.contains('\\') || s.contains(".."))
  {
    return None;
  }
  Some(format!("{owner}/{repo}"))
}

/// Ollama models directories in priority order. `OLLAMA_MODELS`
/// (verbatim) → `$HOME/.ollama/models` → `/usr/share/ollama/.ollama/
/// models` (Linux only). The env-set path comes first so an explicit
/// relocation wins; the user-install default still appears for the
/// mid-migration case; the Linux system-install path is appended last
/// because most developer machines use the user-install variant.
pub fn ollama_models_dirs(home: Option<&Path>) -> Vec<PathBuf> {
  let mut paths = Vec::new();
  if let Some(v) = non_empty_env("OLLAMA_MODELS") {
    paths.push(PathBuf::from(v));
  }
  if let Some(h) = home {
    paths.push(h.join(".ollama/models"));
  }
  if cfg!(target_os = "linux") {
    paths.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
  }
  paths
}

/// LM Studio default models directories. Probes the documented
/// location first, then the legacy `~/.cache/lm-studio/models` for
/// older installs. The user's settings.json override is layered on by
/// [`crate::discovery::lm_studio::resolve_models_dirs`].
pub fn lm_studio_models_dirs(home: &Path) -> Vec<PathBuf> {
  vec![
    home.join(".lmstudio/models"),
    home.join(".cache/lm-studio/models"),
  ]
}

/// `std::env::var_os` filtered to non-empty values. Empty env vars in
/// HF/Ollama tooling are treated as unset; we follow suit so a stray
/// `HF_HUB_CACHE=` doesn't redirect to the cwd.
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
  let v = std::env::var_os(key)?;
  if v.is_empty() {
    None
  } else {
    Some(v)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Env-var helper: snapshot every relevant cache env var so the test
  /// can clear them, set just the ones it wants, then restore on exit.
  /// All callers serialise through `cli::test_lock` since these vars
  /// are process-global.
  struct EnvSnapshot {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
  }

  impl EnvSnapshot {
    fn take(keys: &[&'static str]) -> Self {
      let saved = keys
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect::<Vec<_>>();
      for k in keys {
        std::env::remove_var(k);
      }
      Self { saved }
    }
  }

  impl Drop for EnvSnapshot {
    fn drop(&mut self) {
      for (k, v) in &self.saved {
        match v {
          Some(val) => std::env::set_var(k, val),
          None => std::env::remove_var(k),
        }
      }
    }
  }

  const HF_KEYS: &[&str] = &[
    "HF_HUB_CACHE",
    "HUGGINGFACE_HUB_CACHE",
    "HF_HOME",
    "XDG_CACHE_HOME",
  ];

  #[test]
  fn repo_folder_name_matches_huggingface_hub_convention() {
    assert_eq!(
      repo_folder_name("Qwen/Qwen2.5-7B"),
      "models--Qwen--Qwen2.5-7B"
    );
    assert_eq!(
      repo_folder_name("bartowski/Llama-3.2-3B-Instruct-GGUF"),
      "models--bartowski--Llama-3.2-3B-Instruct-GGUF"
    );
  }

  #[test]
  fn repo_id_from_cache_dir_preserves_double_dash_in_repo_name() {
    // A repo name that itself contains `--` keeps it (split on first `--`).
    assert_eq!(
      repo_id_from_cache_dir("models--org--foo--bar"),
      Some("org/foo--bar".to_string())
    );
    assert_eq!(repo_id_from_cache_dir("not-a-models-dir"), None);
    assert_eq!(repo_id_from_cache_dir("models--onlyowner"), None);
    // Empty owner / empty repo segment → None.
    assert_eq!(repo_id_from_cache_dir("models----repo"), None);
    assert_eq!(repo_id_from_cache_dir("models--org--"), None);
  }

  #[test]
  fn repo_id_from_cache_dir_rejects_path_traversal_shapes() {
    // A crafted cache dir name must not mint a traversal-shaped repo_id (the
    // catalog key a backend turns back into a path).
    assert_eq!(repo_id_from_cache_dir("models--..--passwd"), None);
    assert_eq!(repo_id_from_cache_dir("models--org--..--etc"), None);
  }

  #[test]
  fn repo_folder_name_round_trips_through_decode() {
    // The encode/decode pair are inverses, so a save→scan cycle can't drift.
    for repo_id in ["Qwen/Qwen2.5-7B", "org/foo--bar", "mlx-community/Repo"] {
      assert_eq!(
        repo_id_from_cache_dir(&repo_folder_name(repo_id)).as_deref(),
        Some(repo_id)
      );
    }
  }

  #[test]
  fn huggingface_priority_hf_hub_cache_first() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(HF_KEYS);

    std::env::set_var("HF_HUB_CACHE", "/explicit/hub");
    std::env::set_var("HUGGINGFACE_HUB_CACHE", "/legacy/hub");
    std::env::set_var("HF_HOME", "/hf-root");
    std::env::set_var("XDG_CACHE_HOME", "/xdg");

    let paths = huggingface_hub_dirs(Some(Path::new("/home/user")));
    assert_eq!(
      paths,
      vec![
        PathBuf::from("/explicit/hub"),
        PathBuf::from("/legacy/hub"),
        PathBuf::from("/hf-root/hub"),
        PathBuf::from("/xdg/huggingface/hub"),
        PathBuf::from("/home/user/.cache/huggingface/hub"),
      ],
    );
  }

  #[test]
  fn huggingface_deprecated_alias_picks_up_when_modern_unset() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(HF_KEYS);

    std::env::set_var("HUGGINGFACE_HUB_CACHE", "/legacy/hub");
    let primary = huggingface_primary_hub_dir(Some(Path::new("/home/user")));
    assert_eq!(primary, Some(PathBuf::from("/legacy/hub")));
  }

  #[test]
  fn huggingface_xdg_cache_home_used_before_home_default() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(HF_KEYS);

    std::env::set_var("XDG_CACHE_HOME", "/custom/xdg");
    let paths = huggingface_hub_dirs(Some(Path::new("/home/user")));
    assert_eq!(paths[0], PathBuf::from("/custom/xdg/huggingface/hub"));
    assert_eq!(paths[1], PathBuf::from("/home/user/.cache/huggingface/hub"),);
  }

  #[test]
  fn huggingface_empty_env_vars_treated_as_unset() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(HF_KEYS);

    std::env::set_var("HF_HUB_CACHE", "");
    std::env::set_var("HF_HOME", "");
    std::env::set_var("XDG_CACHE_HOME", "");
    let paths = huggingface_hub_dirs(Some(Path::new("/home/user")));
    assert_eq!(
      paths,
      vec![PathBuf::from("/home/user/.cache/huggingface/hub")]
    );
  }

  #[test]
  fn huggingface_no_home_returns_env_only_or_empty() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(HF_KEYS);

    assert!(huggingface_hub_dirs(None).is_empty());

    std::env::set_var("HF_HUB_CACHE", "/explicit/hub");
    assert_eq!(
      huggingface_hub_dirs(None),
      vec![PathBuf::from("/explicit/hub")],
    );
  }

  #[test]
  fn huggingface_default_only_when_no_env_set() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(HF_KEYS);

    let paths = huggingface_hub_dirs(Some(Path::new("/home/user")));
    assert_eq!(
      paths,
      vec![PathBuf::from("/home/user/.cache/huggingface/hub")]
    );
  }

  /// Paths the user-install Ollama default sits next to on the current
  /// platform. On Linux the official systemd installer also drops
  /// models at `/usr/share/ollama/.ollama/models`; on macOS that path
  /// doesn't exist. The shared tests use this so they don't have to
  /// repeat `cfg!` checks inline.
  fn ollama_system_paths() -> Vec<PathBuf> {
    if cfg!(target_os = "linux") {
      vec![PathBuf::from("/usr/share/ollama/.ollama/models")]
    } else {
      Vec::new()
    }
  }

  #[test]
  fn ollama_env_var_takes_priority_home_default_appears() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(&["OLLAMA_MODELS"]);

    std::env::set_var("OLLAMA_MODELS", "/mnt/ollama");
    let paths = ollama_models_dirs(Some(Path::new("/home/user")));
    let mut expected = vec![
      PathBuf::from("/mnt/ollama"),
      PathBuf::from("/home/user/.ollama/models"),
    ];
    expected.extend(ollama_system_paths());
    assert_eq!(paths, expected);
  }

  #[test]
  fn ollama_empty_env_falls_through() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(&["OLLAMA_MODELS"]);

    std::env::set_var("OLLAMA_MODELS", "");
    let mut expected = vec![PathBuf::from("/home/user/.ollama/models")];
    expected.extend(ollama_system_paths());
    assert_eq!(ollama_models_dirs(Some(Path::new("/home/user"))), expected,);
  }

  #[test]
  fn ollama_no_home_with_env_returns_env_plus_system_path() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(&["OLLAMA_MODELS"]);

    std::env::set_var("OLLAMA_MODELS", "/mnt/ollama");
    let mut expected = vec![PathBuf::from("/mnt/ollama")];
    expected.extend(ollama_system_paths());
    assert_eq!(ollama_models_dirs(None), expected);
  }

  #[test]
  #[cfg(target_os = "linux")]
  fn ollama_linux_includes_system_install_path() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(&["OLLAMA_MODELS"]);

    let paths = ollama_models_dirs(Some(Path::new("/home/user")));
    assert!(
      paths.contains(&PathBuf::from("/usr/share/ollama/.ollama/models")),
      "Linux system-install path must appear, got {paths:?}",
    );
  }

  #[test]
  #[cfg(not(target_os = "linux"))]
  fn ollama_non_linux_omits_system_install_path() {
    let _lock = crate::cli::test_lock::serialize();
    let _guard = EnvSnapshot::take(&["OLLAMA_MODELS"]);

    let paths = ollama_models_dirs(Some(Path::new("/home/user")));
    assert!(
      !paths.contains(&PathBuf::from("/usr/share/ollama/.ollama/models")),
      "Linux system-install path must not leak onto non-Linux, got {paths:?}",
    );
  }

  #[test]
  fn lm_studio_returns_both_default_paths() {
    let paths = lm_studio_models_dirs(Path::new("/home/user"));
    assert_eq!(
      paths,
      vec![
        PathBuf::from("/home/user/.lmstudio/models"),
        PathBuf::from("/home/user/.cache/lm-studio/models"),
      ],
    );
  }
}
