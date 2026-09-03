//! OS-aware path resolution for state, config, cache, and logs.
//!
//! Linux follows XDG; macOS follows the Apple convention (everything under
//! `~/Library/...`). `state_dir` falls back to `data_dir` on macOS because
//! the `directories` crate only exposes a distinct state directory on Linux.
//!
//! Env-var overrides (consulted as resolution step #1, all empty values
//! treated as unset):
//!
//! * `LLAMASTASH_STATE_DIR` — verbatim override for `state_dir()`. The
//!   daemon's `runtime.json` (URL + bearer token) lives here, so clients
//!   attaching to a non-default daemon set this env var rather than
//!   passing a socket path.
//! * `LLAMASTASH_CONFIG_DIR` — verbatim override for `config_dir()`;
//!   `user_config_file()` inherits because it is
//!   `config_dir().join("config.yaml")`.
//! * `LLAMASTASH_CACHE_DIR` — verbatim override for `cache_dir()`; `log_dir()`
//!   inherits because it is `cache_dir().join("logs")`.
//! * `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` — direct overrides
//!   for the IPC client's URL+bearer attach (see [`crate::ipc::client`]).
//!   Both must be set together when used; partial overrides are
//!   rejected.
//! * HuggingFace, Ollama, and LM Studio model-cache paths are resolved
//!   by [`crate::util::model_caches`] (single source of truth for both
//!   writes via `init::download` and scans via `discovery::known_caches`).
//!   That module honors `HF_HUB_CACHE`, `HUGGINGFACE_HUB_CACHE`,
//!   `HF_HOME`, `XDG_CACHE_HOME`, and `OLLAMA_MODELS`. Not a `paths::*`
//!   concern, but worth naming here because callers isolating a child
//!   process need to clear or override them to fully sandbox the HF
//!   cache alongside state, config, and cache/logs.

use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "llamastash";

pub fn project_dirs() -> Option<ProjectDirs> {
  ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
}

/// Human-friendly short label derived from a model path. Falls back to
/// `"model"` when the path has no readable `file_stem`. Used by every TUI
/// surface that needs a short tag for the focused model (chat-tab `model`
/// field, right-pane title, launch picker, logs) — the shared fallback when
/// the live catalog has no `display_label` for the path.
///
/// A Lemonade registry path (`lemonade://qwen3.5-4b-FLM`) is synthetic: the
/// whole string after the scheme is the name, so `file_stem` is wrong there —
/// it mistakes the `.5-4b-FLM` for a file extension and returns `qwen3`. Return
/// the full registry name for those, so a Lemonade model reads identically
/// whether or not the catalog match landed (it transiently misses while the
/// umbrella is mid-load / errored).
pub fn model_display_name(path: &Path) -> String {
  if let Some(name) = crate::backend::lemonade::registry_name_from_path(path) {
    return name.to_string();
  }
  let label = model_file_label(path);
  Path::new(&label)
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model")
    .to_string()
}

/// The **file-shaped** label a model is shown by: the path's basename with a
/// split set's `-NNNNN-of-NNNNN` marker collapsed, so four shards read as one
/// model (`Qwen3.8-Flash-Next-UD-Q4_K_XL.gguf`, not `…-00001-of-00004.gguf`).
///
/// Also the per-model preset key the CLI and TUI write under, and the
/// catalog-fallback name wherever a model is not in the live catalog. It has
/// to collapse for those too: the `default:` preset silently stopped applying
/// to split models when the key was derived raw here but collapsed elsewhere.
///
/// This is the single place the shard collapse happens. `CatalogRow::name`,
/// the CLI's running / external rows, and [`model_display_name`] (which is
/// this minus the extension) all delegate here, so a split model cannot read
/// one way in `list` and another in `status` — it did, before they each
/// derived a basename of their own.
pub fn model_file_label(path: &Path) -> String {
  let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
    return path.to_string_lossy().into_owned();
  };
  crate::discovery::split_gguf::collapsed_file_name(file_name)
    .unwrap_or_else(|| file_name.to_string())
}

/// The one user-visible identifier for a model: `display_label` when the
/// source supplies one, otherwise [`model_display_name`] over the path.
///
/// A GGUF row has no label, so it reads as the file stem
/// (`Qwen2.5-Coder-7B-Q4_K_M`); a safetensors repo, an Ollama blob, and a
/// Lemonade registry entry all have opaque paths and carry a label instead
/// (`Qwen/Qwen3-0.6B`, `llama3:8b`, `qwen3.5-4b-FLM`). This is the `id`
/// `/v1/models` publishes and the string external tool configs must send
/// back, so both go through here rather than deriving a name each.
pub fn model_public_id(path: &Path, display_label: Option<&str>) -> String {
  match display_label {
    Some(label) => label.to_string(),
    None => model_display_name(path),
  }
}

/// The repo-qualified form of [`model_public_id`]:
/// `unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M`. Used to publish a model
/// whose bare id collides with another row's, so both stay reachable.
///
/// A source that supplies its own `display_label` already carries a
/// repo-scoped id (`Qwen/Qwen3-0.6B`, `llama3:8b`), so it is returned
/// unchanged — qualifying it would only stutter.
pub fn model_qualified_id(path: &Path, display_label: Option<&str>) -> String {
  if let Some(label) = display_label {
    return label.to_string();
  }
  match model_group_label(path) {
    Some(group) => format!("{group}/{}", model_display_name(path)),
    None => model_display_name(path),
  }
}

/// The source-qualified form of [`model_qualified_id`]:
/// `huggingface/unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M`, where the head
/// is a [`ModelSource`](crate::discovery::ModelSource) label.
///
/// The rung the repo qualifier cannot cover: one repo cached by two different
/// tools derives the *same* `owner/repo` from two different roots, so
/// `models--lmstudio-community--gemma-4-E2B-it-GGUF` in the HF cache and
/// `lmstudio-community/gemma-4-E2B-it-GGUF` under LM Studio qualify
/// identically. That is the ordinary shape of a duplicate, not a corner case,
/// and without this rung both fell through to absolute paths.
///
/// `display_label` sources are returned unchanged, like the rung below. An
/// empty `source` (a row projected without one) degrades to that rung rather
/// than publishing a leading `/`.
pub fn model_source_qualified_id(path: &Path, display_label: Option<&str>, source: &str) -> String {
  let qualified = model_qualified_id(path, display_label);
  if display_label.is_some() || source.is_empty() {
    return qualified;
  }
  format!("{source}/{qualified}")
}

/// The forms one model can publish under, shortest first, with duplicates
/// collapsed so a row never counts the same string twice.
fn id_ladder(path: &Path, display_label: Option<&str>, source: &str) -> Vec<String> {
  let mut forms = vec![model_public_id(path, display_label)];
  for next in [
    model_qualified_id(path, display_label),
    model_source_qualified_id(path, display_label, source),
  ] {
    if !forms.contains(&next) {
      forms.push(next);
    }
  }
  forms
}

/// Published ids for a whole catalog, keyed by canonical path.
///
/// Each model takes the shortest form of its id ladder that no other
/// model in the catalog claims — plain [`model_public_id`], else
/// [`model_qualified_id`], else [`model_source_qualified_id`] — and falls
/// back to the canonical path, unique by construction, when even the longest
/// collides. So every row `/v1/models` lists is reachable: before this, two
/// same-named GGUFs in different roots published one id that 400'd with
/// `ambiguous_model` for both, and an id already written into a tool config
/// keeps working for as long as it stays unambiguous.
///
/// Every form shares one namespace, so a longer form can never shadow some
/// other row's shorter one.
pub fn published_id_index<'a, I>(models: I) -> std::collections::HashMap<String, String>
where
  I: IntoIterator<Item = (&'a Path, Option<&'a str>, &'a str)>,
{
  use std::collections::HashMap;
  let ladders: Vec<(&Path, Vec<String>)> = models
    .into_iter()
    .map(|(path, label, source)| (path, id_ladder(path, label, source)))
    .collect();
  let mut taken: HashMap<&str, usize> = HashMap::new();
  for (_, forms) in &ladders {
    for form in forms {
      *taken.entry(form.as_str()).or_insert(0) += 1;
    }
  }
  ladders
    .iter()
    .map(|(path, forms)| {
      let id = forms
        .iter()
        .find(|f| taken.get(f.as_str()).copied().unwrap_or(0) == 1)
        .cloned()
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
      (path.to_string_lossy().into_owned(), id)
    })
    .collect()
}

/// Follow `path` through any symlinks to the real file that would be
/// written.
///
/// Atomic writes go tempfile-then-rename, and a rename over a symlink
/// **replaces the link** with a regular file. Dotfile setups routinely
/// symlink these configs into a managed repo (`~/.pi/agent/settings.json`
/// → `~/dotfiles/...`), so writing without resolving first would silently
/// detach the user's config from the repo managing it.
///
/// Relative link targets resolve against the link's own directory. A
/// broken or missing link resolves to the last path that existed, so the
/// caller creates a file where the user pointed. Hops are capped so a
/// loop returns rather than spins.
pub fn resolve_symlinks(path: &Path) -> PathBuf {
  let mut current = path.to_path_buf();
  for _ in 0..SYMLINK_HOPS {
    let Ok(meta) = std::fs::symlink_metadata(&current) else {
      return current;
    };
    if !meta.file_type().is_symlink() {
      return current;
    }
    let Ok(target) = std::fs::read_link(&current) else {
      return current;
    };
    current = if target.is_absolute() {
      target
    } else {
      match current.parent() {
        // Normalise the `..` a relative link introduces, so the path we
        // report and write is the one the user would type.
        Some(dir) => canonicalize(dir.join(&target)).unwrap_or_else(|_| dir.join(target)),
        None => target,
      }
    };
  }
  current
}

/// Cap on symlink hops in [`resolve_symlinks`]. Linux's own limit is 40;
/// a config path nested deeper than this is a loop, not a setup.
const SYMLINK_HOPS: usize = 8;

/// Best-effort home directory resolution. Returns `None` only when the
/// platform can't supply one (i.e. broken `$HOME` and no equivalent in
/// the password database) — every realistic developer machine has one.
/// Discovery uses this to anchor `~/.cache/huggingface/hub`,
/// `~/.ollama/models`, and `~/.lmstudio/models`.
pub fn home_dir() -> Option<PathBuf> {
  BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Canonicalize `path`, stripping the Windows verbatim (`\\?\`) prefix
/// that `std::fs::canonicalize` prepends. User-facing surfaces (status
/// `server_path`, model paths, favorites, `--render`, the written
/// `config.yaml`) then show `C:\Users\…` instead of `\\?\C:\Users\…`.
/// On non-Windows this is a thin wrapper around `std::fs::canonicalize`.
///
/// The verbatim and stripped forms refer to the same file, and model
/// identity is anchored on the header hash (not the path), so stripping
/// keeps path matching consistent while cleaning up display.
pub fn canonicalize<P: AsRef<Path>>(path: P) -> std::io::Result<PathBuf> {
  std::fs::canonicalize(path).map(strip_verbatim_prefix)
}

/// Strip the Windows extended-length / verbatim prefix from a path:
/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share` → `\\server\share`.
/// No-op on non-Windows, on paths without the prefix, and on paths
/// whose stripped form would exceed the legacy `MAX_PATH` (260) limit —
/// those genuinely need the verbatim form for filesystem access.
pub fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
  #[cfg(windows)]
  {
    if let Some(s) = path.to_str() {
      if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        if rest.len() + 2 < 260 {
          return PathBuf::from(format!(r"\\{rest}"));
        }
      } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Only strip a normal drive path (`\\?\X:\…`); leave device
        // namespaces and other verbatim forms untouched.
        let b = rest.as_bytes();
        let is_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        if is_drive && rest.len() < 260 {
          return PathBuf::from(rest);
        }
      }
    }
  }
  path
}

pub fn state_dir() -> Option<PathBuf> {
  if let Some(p) = env_path_override("LLAMASTASH_STATE_DIR") {
    return Some(p);
  }
  project_dirs().map(|d| {
    d.state_dir()
      .map_or_else(|| d.data_dir().to_path_buf(), PathBuf::from)
  })
}

pub fn config_dir() -> Option<PathBuf> {
  if let Some(p) = env_path_override("LLAMASTASH_CONFIG_DIR") {
    return Some(p);
  }
  project_dirs().map(|d| d.config_dir().to_path_buf())
}

pub fn cache_dir() -> Option<PathBuf> {
  if let Some(p) = env_path_override("LLAMASTASH_CACHE_DIR") {
    return Some(p);
  }
  project_dirs().map(|d| d.cache_dir().to_path_buf())
}

/// Read an env var as a `PathBuf`, treating unset and empty values
/// identically. Used by `state_dir` / `config_dir` / `cache_dir` to
/// keep override semantics uniform.
fn env_path_override(name: &str) -> Option<PathBuf> {
  let raw = std::env::var_os(name)?;
  let p = PathBuf::from(raw);
  if p.as_os_str().is_empty() {
    None
  } else {
    Some(p)
  }
}

pub fn log_dir() -> Option<PathBuf> {
  cache_dir().map(|d| d.join("logs"))
}

/// Convenience: return the canonical user config-file path.
pub fn user_config_file() -> Option<PathBuf> {
  config_dir().map(|d| d.join("config.yaml"))
}

/// Convenience: state-store path (favorites, presets, last-params, running snapshot).
pub fn state_file() -> Option<PathBuf> {
  state_dir().map(|d| d.join("state.json"))
}

/// Convenience: PID lockfile path used by the daemon to enforce single-instance.
pub fn daemon_pidfile() -> Option<PathBuf> {
  state_dir().map(|d| d.join("daemon.pid"))
}

/// Convenience: init-wizard snapshot file path. Sibling of
/// `state.json` under the state dir; written and consumed only by the
/// init wizard and `llamastash doctor` — the daemon ignores it.
pub fn init_snapshot_file() -> Option<PathBuf> {
  state_dir().map(|d| d.join("init_snapshot.json"))
}

/// Substitute `$HOME` for `~` when `path` is under the user's home
/// directory, so right-pane and list-pane headers stay scannable.
/// Returns the path verbatim when no home dir is available or the
/// path is outside it.
pub fn abbreviate_with_home(path: &Path) -> String {
  if let Some(home) = home_dir() {
    if let Ok(rest) = path.strip_prefix(&home) {
      if rest.as_os_str().is_empty() {
        return "~".to_string();
      }
      return format!("~/{}", rest.display());
    }
  }
  path.display().to_string()
}

fn trailing_path_label(path: &Path, keep_segments: usize) -> String {
  // Only Normal components contribute to the label — RootDir on Unix
  // (`/`) and RootDir+Prefix on Windows (`\`, `C:`) would otherwise
  // count as a segment and produce `…/m/x` instead of `m/x` for
  // `Path::new("/m/x")` on Windows.
  let parts: Vec<String> = path
    .components()
    .filter_map(|comp| match comp {
      std::path::Component::Normal(s) => s.to_str().map(ToOwned::to_owned),
      _ => None,
    })
    .filter(|part| !part.is_empty())
    .collect();
  if parts.is_empty() {
    return abbreviate_with_home(path);
  }
  let start = parts.len().saturating_sub(keep_segments);
  let tail = parts[start..].join("/");
  if start == 0 {
    tail
  } else {
    format!("…/{tail}")
  }
}

/// Derive a short, human-friendly label for a model-directory parent
/// path. Used by the list pane's section headers in place of the full
/// raw parent so the user reads `owner/repo` instead of a 100-cell
/// cache path. Detects three common cache layouts:
///
/// * HuggingFace — `…/huggingface/hub/models--<owner>--<repo>/snapshots/<hash>`
///   → `<owner>/<repo>`
/// * LM Studio — `…/lmstudio-models/<owner>/<repo>` or
///   `…/.lmstudio/models/<owner>/<repo>` → `<owner>/<repo>`
/// * Ollama — `…/registry.ollama.ai/library/<name>/<tag>` → `<name>:<tag>`
///
/// Anything that doesn't match falls back to the last two path segments
/// (`…/models/qwen`) so user-configured `model_paths` stay scannable
/// instead of burning the whole left-pane width on an absolute path.
pub fn friendly_group_label(parent: &Path) -> String {
  cache_repo_label(parent).unwrap_or_else(|| trailing_path_label(parent, 2))
}

/// The `owner/repo`-shaped label for a path under a recognized model cache,
/// or `None` for anything else. Split out of [`friendly_group_label`] so the
/// id qualifier can fall back to a bare directory name instead of the
/// elided `…/a/b` form, which a user cannot type back.
fn cache_repo_label(parent: &Path) -> Option<String> {
  for comp in parent.components() {
    if let Some(s) = comp.as_os_str().to_str() {
      if let Some(rest) = s.strip_prefix("models--") {
        if let Some((owner, repo)) = rest.split_once("--") {
          return Some(format!("{owner}/{repo}"));
        }
      }
    }
  }
  let s = parent.to_string_lossy();
  // Ollama blob storage — every model lives in `<root>/blobs/sha256-…`
  // as a content-addressed file, so all rows share one parent dir.
  // The per-model `display_label` carries the resolved `<name>:<tag>`,
  // so the group header only needs to advertise the cache.
  let last = parent.file_name().and_then(|n| n.to_str());
  if last == Some("blobs") && (s.contains("ollama") || s.contains(".ollama")) {
    return Some("Ollama".to_string());
  }
  if let Some(idx) = s.find("registry.ollama.ai/library/") {
    let tail = &s[idx + "registry.ollama.ai/library/".len()..];
    let mut parts = tail.split('/').filter(|p| !p.is_empty());
    let name = parts.next().unwrap_or("");
    let tag = parts.next().unwrap_or("");
    if !name.is_empty() {
      return Some(if tag.is_empty() {
        name.to_string()
      } else {
        format!("{name}:{tag}")
      });
    }
  }
  for marker in ["lmstudio-models/", ".lmstudio/models/"] {
    if let Some(idx) = s.find(marker) {
      let tail = &s[idx + marker.len()..];
      let mut parts = tail.split('/').filter(|p| !p.is_empty());
      let owner = parts.next().unwrap_or("");
      let repo = parts.next().unwrap_or("");
      if !owner.is_empty() && !repo.is_empty() {
        return Some(format!("{owner}/{repo}"));
      }
    }
  }
  None
}

/// The repo-shaped label for where a **model file** lives: `owner/repo` for
/// a recognized cache layout, otherwise the name of its own parent
/// directory. `None` when the path has no parent segment to name.
///
/// This is the `list` REPO column and the qualifier that disambiguates a
/// published model id. Deliberately not [`friendly_group_label`]: that one
/// elides a long generic path to `…/a/b`, which reads fine as a TUI section
/// header but cannot be typed back as part of a model id.
pub fn model_group_label(path: &Path) -> Option<String> {
  let parent = path.parent()?;
  if let Some(repo) = cache_repo_label(parent) {
    return Some(repo);
  }
  parent
    .file_name()
    .and_then(|s| s.to_str())
    .filter(|s| !s.is_empty())
    .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
  use std::{
    path::Path,
    sync::{Mutex, OnceLock},
  };

  use super::*;

  #[test]
  fn model_display_name_stems_real_files_but_keeps_lemonade_registry_names() {
    // A real GGUF: drop the `.gguf` extension, keep the dotted version tag.
    assert_eq!(
      model_display_name(Path::new("/models/Qwen2.5-Coder-7B-Q4_K_M.gguf")),
      "Qwen2.5-Coder-7B-Q4_K_M"
    );
    // A Lemonade synthetic path: the dotted registry name is the whole label —
    // `file_stem` would truncate `qwen3.5-4b-FLM` to `qwen3`.
    assert_eq!(
      model_display_name(Path::new("lemonade://qwen3.5-4b-FLM")),
      "qwen3.5-4b-FLM"
    );
  }

  /// Serialize the tests that actually mutate process-global env vars
  /// (a small handful covering the `LLAMASTASH_STATE_DIR` /
  /// `LLAMASTASH_CACHE_DIR` resolution chain). Every other test in this
  /// module reads paths without touching env state so they stay
  /// parallel-safe.
  fn env_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
  }

  /// Save-and-restore guard for a single env var. Restores the previous
  /// value (including "unset") on drop so a panicking test does not
  /// leak state into siblings sharing the env mutex.
  struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
  }

  impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
      let previous = std::env::var_os(key);
      std::env::set_var(key, value);
      Self { key, previous }
    }

    fn unset(key: &'static str) -> Self {
      let previous = std::env::var_os(key);
      std::env::remove_var(key);
      Self { key, previous }
    }
  }

  impl Drop for EnvVarGuard {
    fn drop(&mut self) {
      match self.previous.take() {
        Some(v) => std::env::set_var(self.key, v),
        None => std::env::remove_var(self.key),
      }
    }
  }

  #[test]
  fn project_dirs_resolves_on_this_platform() {
    // Smoke test: directories crate must produce a project root on every
    // platform we support. If this returns None the test environment has no
    // resolvable home directory, which is a broader problem than we want to
    // hide here.
    assert!(
      project_dirs().is_some(),
      "ProjectDirs::from should resolve on a normal developer machine"
    );
  }

  #[test]
  fn all_dir_helpers_return_some() {
    assert!(state_dir().is_some());
    assert!(config_dir().is_some());
    assert!(cache_dir().is_some());
    assert!(log_dir().is_some());
    assert!(user_config_file().is_some());
    assert!(state_file().is_some());
    assert!(daemon_pidfile().is_some());
  }

  #[test]
  fn all_dir_helpers_contain_llamastash_segment() {
    // Stronger than `is_some()`: every resolved path must live under
    // a `llamastash/` directory regardless of platform. Catches a
    // regression where the `directories` crate dependency or our
    // `APPLICATION` constant changes and silently re-roots the
    // daemon's files outside of its namespace.
    //
    // This checks the platform-default resolution, so hold the env mutex
    // and clear the dir overrides first: a concurrent override-setting
    // test could otherwise flip `LLAMASTASH_*_DIR` to a path like
    // `/tmp/uat-state-override` (no `llamastash` segment) between our
    // reads and fail the assertion spuriously.
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _state = EnvVarGuard::unset("LLAMASTASH_STATE_DIR");
    let _config = EnvVarGuard::unset("LLAMASTASH_CONFIG_DIR");
    let _cache = EnvVarGuard::unset("LLAMASTASH_CACHE_DIR");

    let llamastash = std::ffi::OsStr::new("llamastash");
    for path in [
      state_dir().unwrap(),
      config_dir().unwrap(),
      cache_dir().unwrap(),
      log_dir().unwrap(),
    ] {
      assert!(
        path.components().any(|c| c.as_os_str() == llamastash),
        "{} does not contain a `llamastash` segment",
        path.display()
      );
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_state_dir_uses_xdg_or_home_dot_local() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _override = EnvVarGuard::unset("LLAMASTASH_STATE_DIR");
    // On Linux the resolved state_dir must live under either
    // `$XDG_STATE_HOME/llamastash` or the conventional fallback
    // `~/.local/state/llamastash`. Clear the explicit override first so
    // the assertion checks the platform/XDG path rather than a developer's
    // shell-local `LLAMASTASH_STATE_DIR`.
    let path = state_dir().unwrap().display().to_string();
    assert!(
      path.contains(".local/state/llamastash") || path.contains("/llamastash"),
      "unexpected state_dir on linux: {path}"
    );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn macos_state_dir_lives_under_library_application_support() {
    // The `directories` crate maps state_dir to data_dir on macOS;
    // both live under `~/Library/Application Support/llamastash`.
    let path = state_dir().unwrap().display().to_string();
    assert!(
      path.contains("Library/Application Support/llamastash"),
      "unexpected state_dir on macOS: {path}"
    );
  }

  #[test]
  fn log_dir_is_logs_under_cache_dir() {
    // Hold env_mutex so a concurrent override-setting test can't flip
    // LLAMASTASH_CACHE_DIR between our two reads and split the answer.
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let log = log_dir().unwrap();
    let cache = cache_dir().unwrap();
    assert_eq!(log, cache.join("logs"));
  }

  #[test]
  fn user_config_file_lives_under_config_dir() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let path = user_config_file().unwrap();
    assert_eq!(path, config_dir().unwrap().join("config.yaml"));
  }

  #[test]
  fn state_file_lives_under_state_dir() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let path = state_file().unwrap();
    assert_eq!(path, state_dir().unwrap().join("state.json"));
  }

  #[test]
  fn daemon_pidfile_lives_under_state_dir() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let path = daemon_pidfile().unwrap();
    assert_eq!(path, state_dir().unwrap().join("daemon.pid"));
  }

  #[test]
  fn init_snapshot_file_lives_under_state_dir() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let path = init_snapshot_file().unwrap();
    assert_eq!(path, state_dir().unwrap().join("init_snapshot.json"));
  }

  #[test]
  fn strip_verbatim_prefix_is_noop_without_prefix() {
    // A plain path is returned unchanged on every platform.
    let p = PathBuf::from("/home/u/models/x.gguf");
    assert_eq!(strip_verbatim_prefix(p.clone()), p);
  }

  #[cfg(windows)]
  #[test]
  fn strip_verbatim_prefix_strips_drive_and_unc_on_windows() {
    assert_eq!(
      strip_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\d\m.gguf")),
      PathBuf::from(r"C:\Users\d\m.gguf")
    );
    assert_eq!(
      strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\m.gguf")),
      PathBuf::from(r"\\server\share\m.gguf")
    );
    // Non-drive verbatim forms are left untouched.
    let dev = PathBuf::from(r"\\?\Volume{abc}\m.gguf");
    assert_eq!(strip_verbatim_prefix(dev.clone()), dev);
  }

  #[test]
  fn model_group_label_is_the_repo_for_a_cache_and_the_parent_dir_otherwise() {
    assert_eq!(
      model_group_label(Path::new(
        "/home/a/.cache/huggingface/hub/models--unsloth--Qwen3.8-27B-GGUF/snapshots/1/UD-Q4_K_XL/w.gguf"
      ))
      .as_deref(),
      Some("unsloth/Qwen3.8-27B-GGUF"),
      "a quant subdir still reports its repo"
    );
    assert_eq!(
      model_group_label(Path::new(
        "/home/a/.lmstudio/models/lmstudio-community/Phi-4-GGUF/p.gguf"
      ))
      .as_deref(),
      Some("lmstudio-community/Phi-4-GGUF")
    );
    // Not a known cache: the parent's own name, never the elided `…/a/b`
    // form — the qualifier has to be typeable back as part of a model id.
    assert_eq!(
      model_group_label(Path::new("/very/long/model/cache/custom/qwen/w.gguf")).as_deref(),
      Some("qwen")
    );
    assert_eq!(model_group_label(Path::new("w.gguf")), None);
  }

  #[test]
  fn model_qualified_id_prefixes_a_gguf_but_leaves_a_labelled_row_alone() {
    let hf = Path::new(
      "/c/huggingface/hub/models--unsloth--Qwen3.8-27B-GGUF/snapshots/1/Qwen3.8-27B-Q4_K_M.gguf",
    );
    assert_eq!(
      model_qualified_id(hf, None),
      "unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M"
    );
    // An Ollama blob's label is already a repo-scoped id; qualifying it with
    // the content-addressed blob dir would only stutter.
    assert_eq!(
      model_qualified_id(Path::new("/o/blobs/sha256-abc"), Some("llama3:8b")),
      "llama3:8b"
    );
  }

  #[test]
  fn published_id_index_qualifies_only_what_collides() {
    let ids = published_id_index([
      (
        Path::new("/hf/gemma-4-E2B-it-Q4_K_M.gguf"),
        None,
        "huggingface",
      ),
      (
        Path::new("/lms/gemma-4-E2B-it-Q4_K_M.gguf"),
        None,
        "lm-studio",
      ),
      (Path::new("/m/llama.gguf"), None, "user"),
      (
        Path::new("/o/blobs/sha256-abc"),
        Some("llama3:8b"),
        "ollama",
      ),
    ]);
    assert_eq!(ids["/m/llama.gguf"], "llama", "unique id is left alone");
    assert_eq!(
      ids["/o/blobs/sha256-abc"], "llama3:8b",
      "a labelled row whose forms are all identical still reads as unique"
    );
    assert_eq!(
      ids["/hf/gemma-4-E2B-it-Q4_K_M.gguf"], "hf/gemma-4-E2B-it-Q4_K_M",
      "the repo qualifier already separates these, so the source rung is not spent"
    );
    assert_eq!(
      ids["/lms/gemma-4-E2B-it-Q4_K_M.gguf"],
      "lms/gemma-4-E2B-it-Q4_K_M"
    );
  }

  #[test]
  fn published_id_index_adds_the_source_when_the_repo_label_is_shared() {
    // The ordinary duplicate: one repo cached by both tools. Each root
    // derives the *same* `owner/repo`, so the repo rung cannot separate
    // them and the source rung has to — these published as raw absolute
    // paths before it existed.
    let hf = Path::new(
      "/w/huggingface/hub/models--lmstudio-community--gemma-4-E2B-it-GGUF/snapshots/1/gemma-4-E2B-it-Q4_K_M.gguf",
    );
    let lms = Path::new(
      "/w/lmstudio-models/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M.gguf",
    );
    assert_eq!(
      model_qualified_id(hf, None),
      model_qualified_id(lms, None),
      "the premise: both qualify identically"
    );
    let ids = published_id_index([(hf, None, "huggingface"), (lms, None, "lm-studio")]);
    assert_eq!(
      ids[&hf.to_string_lossy().to_string()],
      "huggingface/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M"
    );
    assert_eq!(
      ids[&lms.to_string_lossy().to_string()],
      "lm-studio/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M"
    );
  }

  #[test]
  fn published_id_index_falls_back_to_the_path_when_every_rung_collides() {
    // Same basename in two subdirs of one repo, reached through one source:
    // every rung of the ladder is identical, so the only remaining unique
    // handle is the path.
    let a = Path::new("/c/hub/models--o--r/snapshots/1/UD-Q4_K_XL/w.gguf");
    let b = Path::new("/c/hub/models--o--r/snapshots/1/Q8_0/w.gguf");
    let ids = published_id_index([(a, None, "huggingface"), (b, None, "huggingface")]);
    assert_eq!(ids[&a.to_string_lossy().to_string()], a.to_string_lossy());
    assert_eq!(ids[&b.to_string_lossy().to_string()], b.to_string_lossy());
  }

  #[test]
  fn model_source_qualified_id_degrades_without_a_source() {
    let p = Path::new("/m/x.gguf");
    assert_eq!(model_source_qualified_id(p, None, "user"), "user/m/x");
    assert_eq!(
      model_source_qualified_id(p, None, ""),
      model_qualified_id(p, None),
      "no leading slash when the row carries no source"
    );
    assert_eq!(
      model_source_qualified_id(Path::new("/o/blobs/sha"), Some("llama3:8b"), "ollama"),
      "llama3:8b"
    );
  }

  #[test]
  fn friendly_group_label_collapses_generic_paths_to_tail_segments() {
    assert_eq!(
      friendly_group_label(Path::new("/very/long/model/cache/custom/qwen")),
      "…/custom/qwen"
    );
  }

  #[test]
  fn friendly_group_label_keeps_hf_owner_repo_short_form() {
    assert_eq!(
      friendly_group_label(Path::new(
        "/home/alice/.cache/huggingface/hub/models--bartowski--Qwen2.5-Coder-7B-Instruct-GGUF/snapshots/1234"
      )),
      "bartowski/Qwen2.5-Coder-7B-Instruct-GGUF"
    );
  }

  #[test]
  fn env_path_override_returns_pathbuf_when_set() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_TEST_PATH_OVERRIDE_A", "/tmp/uat-abc");
    assert_eq!(
      env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_A"),
      Some(PathBuf::from("/tmp/uat-abc"))
    );
  }

  #[test]
  fn env_path_override_returns_none_when_unset() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::unset("LLAMASTASH_TEST_PATH_OVERRIDE_B");
    assert_eq!(env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_B"), None);
  }

  #[test]
  fn env_path_override_treats_empty_string_as_unset() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_TEST_PATH_OVERRIDE_C", "");
    assert_eq!(env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_C"), None);
  }

  #[test]
  fn env_path_override_preserves_unicode_and_spaces_verbatim() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let raw = "/tmp/uat 🦙 spaces";
    let _e = EnvVarGuard::set("LLAMASTASH_TEST_PATH_OVERRIDE_D", raw);
    assert_eq!(
      env_path_override("LLAMASTASH_TEST_PATH_OVERRIDE_D"),
      Some(PathBuf::from(raw))
    );
  }

  #[test]
  fn state_dir_honors_llamastash_state_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "/tmp/uat-state-override");
    assert_eq!(state_dir(), Some(PathBuf::from("/tmp/uat-state-override")));
  }

  #[test]
  fn cache_dir_honors_llamastash_cache_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "/tmp/uat-cache-override");
    assert_eq!(cache_dir(), Some(PathBuf::from("/tmp/uat-cache-override")));
  }

  #[test]
  fn config_dir_honors_llamastash_config_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CONFIG_DIR", "/tmp/uat-config-override");
    assert_eq!(
      config_dir(),
      Some(PathBuf::from("/tmp/uat-config-override"))
    );
  }

  #[test]
  fn user_config_file_inherits_config_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CONFIG_DIR", "/tmp/uat-config-override");
    assert_eq!(
      user_config_file(),
      Some(PathBuf::from("/tmp/uat-config-override/config.yaml"))
    );
  }

  #[test]
  fn config_dir_empty_override_falls_through_to_project_dirs() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CONFIG_DIR", "");
    let p = config_dir().expect("config_dir() resolves on this platform");
    assert!(
      p.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("llamastash")),
      "{} should contain a `llamastash` segment when override is empty",
      p.display()
    );
  }

  #[test]
  fn log_dir_inherits_cache_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "/tmp/uat-cache-override");
    // log_dir = cache_dir().join("logs"); confirms the chain.
    assert_eq!(
      log_dir(),
      Some(PathBuf::from("/tmp/uat-cache-override/logs"))
    );
  }

  #[test]
  fn state_dir_empty_override_falls_through_to_project_dirs() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "");
    // Empty override is treated as unset; resolution returns the
    // platform-default path which still carries the `llamastash` segment.
    let p = state_dir().expect("state_dir() resolves on this platform");
    assert!(
      p.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("llamastash")),
      "{} should contain a `llamastash` segment when override is empty",
      p.display()
    );
  }

  #[test]
  fn cache_dir_empty_override_falls_through_to_project_dirs() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "");
    let p = cache_dir().expect("cache_dir() resolves on this platform");
    assert!(
      p.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("llamastash")),
      "{} should contain a `llamastash` segment when override is empty",
      p.display()
    );
  }

  #[test]
  fn state_and_cache_overrides_are_independent() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _state = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "/tmp/uat-state");
    let _cache = EnvVarGuard::set("LLAMASTASH_CACHE_DIR", "/tmp/uat-cache");
    assert_eq!(state_dir(), Some(PathBuf::from("/tmp/uat-state")));
    assert_eq!(cache_dir(), Some(PathBuf::from("/tmp/uat-cache")));
    // log_dir inherits cache, NOT state.
    assert_eq!(log_dir(), Some(PathBuf::from("/tmp/uat-cache/logs")));
  }

  #[test]
  fn state_file_and_pidfile_follow_state_dir_override() {
    let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _e = EnvVarGuard::set("LLAMASTASH_STATE_DIR", "/tmp/uat-state-x");
    assert_eq!(
      state_file(),
      Some(PathBuf::from("/tmp/uat-state-x/state.json"))
    );
    assert_eq!(
      daemon_pidfile(),
      Some(PathBuf::from("/tmp/uat-state-x/daemon.pid"))
    );
    assert_eq!(
      init_snapshot_file(),
      Some(PathBuf::from("/tmp/uat-state-x/init_snapshot.json"))
    );
  }
}
