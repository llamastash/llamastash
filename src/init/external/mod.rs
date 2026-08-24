//! Init wizard's external-tool config patchers.
//!
//! Each module under `tools/` implements [`ToolPatcher`] for one
//! supported AI dev tool (OpenCode, Aider, Continue, Zed, pi.dev,
//! plus the `env.sh` shell-env writer). The wizard's
//! `run_integrations_step` presents a cliclack multiselect, then
//! calls [`dry_run`] / [`apply`] per chosen patcher.
//!
//! Shared with the llamastash-own config writer:
//! - redaction allowlist + diff rendering from
//!   [`crate::util::config_patch`]
//! - atomic write primitive from [`crate::util::atomic_write`]
//!
//! Per-tool modules only declare: path, format, and the additions
//! `serde_json::Value` to merge in. The merge / read-current /
//! diff / redact / atomic-write plumbing all lives here so a new
//! tool is ~30 lines.

pub mod merge;
pub mod models;
pub mod tools;
pub mod write;

use std::path::PathBuf;

use serde::Serialize;

use crate::util::config_patch::{redact_diff, render_human, RedactedDiffEntry};

/// Serialisation format the patcher's target file uses on disk. We
/// always model the in-memory additions as `serde_json::Value`
/// (JSON is the lowest common denominator); the YAML variant goes
/// through `yaml_serde::to_string` at write time. Reading the
/// current file does the reverse for YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
  Json,
  Yaml,
  /// Patcher manages the whole file body itself — bypasses merge
  /// (used by [`tools::env_sh`] which writes a shell script, not
  /// a merge-target config).
  Raw,
}

/// Inputs every patcher gets when building its additions.
///
/// `proxy_base_url` is the OpenAI-compat endpoint llamastash serves
/// (e.g. `http://127.0.0.1:11435/v1`); each tool's `baseURL` /
/// `api_url` / `apiBase` / `openai-api-base` field maps to it
/// verbatim. `api_key` is the proxy's resolved bearer token when auth
/// is enforced (a real secret — writers that embed it literally use
/// mode `0o600`), or the `llamastash` stub on the keyless loopback
/// default so clients that *require* a non-empty key don't refuse to
/// boot.
///
/// `models` is every model to register with the tool, in preference
/// order: the one `init`'s model step just downloaded first (when it ran),
/// then the user's favorites. Tools whose schema holds a list
/// (OpenCode, Continue, Zed, pi.dev) register all of them; tools with a
/// single model slot (Aider's `model:`, Claude Code's `ANTHROPIC_MODEL`)
/// take [`PatchContext::primary`].
#[derive(Debug, Clone)]
pub struct PatchContext {
  pub proxy_base_url: String,
  pub api_key: String,
  pub models: Vec<PatchModel>,
}

/// Declared when the catalog has no context figure for a model. Small
/// enough to be safe with any model rather than a guess that overflows.
const DEFAULT_CONTEXT_WINDOW: u64 = 32768;

/// One model to register, named the way the proxy publishes it.
///
/// `is_embed`: an embedding model (nomic-embed, snowflake-arctic-embed,
/// …). Patchers that care about the distinction — Continue.dev's `roles`
/// field, pi.dev's `api` field — branch on it; tools that don't
/// differentiate just register the model and let the user wire it up.
#[derive(Debug, Clone)]
pub struct PatchModel {
  /// The `id` `/v1/models` publishes for this model — a GGUF's file stem,
  /// a safetensors repo's `owner/repo`, an Ollama `<name>:<tag>`. Built by
  /// [`crate::util::paths::model_public_id`] so what we write is what the
  /// proxy answers to.
  pub id: String,
  pub is_embed: bool,
  /// The model's trained context window, when the catalog knows it.
  /// Clients size their own history against this — declare 32k for a 262k
  /// model and the tool compacts the conversation long before it needs to.
  /// The launched context is resolved per launch (and `--fit` may size it
  /// down), so the trained window is the honest figure at patch time.
  pub context_window: Option<u64>,
}

impl PatchModel {
  /// Classify by id when there is no catalog row to ask. The name
  /// substring is all a freshly downloaded file offers before the daemon
  /// has rescanned; [`Self::from_catalog_row`] uses the GGUF header's
  /// `mode_hint` instead wherever a row exists.
  pub fn from_id(id: String) -> Self {
    let is_embed = id.to_ascii_lowercase().contains("embed");
    Self {
      id,
      is_embed,
      context_window: None,
    }
  }

  /// Context window to declare, falling back to a conservative 32k when
  /// the catalog has no figure (a parse failure, a registry row).
  pub fn declared_context(&self) -> u64 {
    self.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW)
  }

  /// Project a catalog row. Prefers the parsed `mode_hint` over the name
  /// heuristic — a header that says `embedding` is not a guess.
  pub fn from_catalog_row(row: &crate::launch::resolve::CatalogRow) -> Self {
    let id = row.public_id();
    let is_embed = match row.mode_hint.as_deref() {
      Some(hint) => hint == "embedding",
      None => Self::from_id(id.clone()).is_embed,
    };
    Self {
      id,
      is_embed,
      context_window: row.native_ctx,
    }
  }
}

impl PatchContext {
  /// The model a single-slot tool should point at: the first non-embedding
  /// entry, falling back to the first entry when every model is an
  /// embedder (better to register something than nothing). `None` when no
  /// models resolved at all.
  pub fn primary(&self) -> Option<&PatchModel> {
    self
      .models
      .iter()
      .find(|m| !m.is_embed)
      .or_else(|| self.models.first())
  }
}

/// One supported external-tool patcher. Implementors declare where
/// the tool's config lives, what format it uses, and the JSON
/// additions to merge in.
pub trait ToolPatcher: Send + Sync {
  /// Short stable identifier — used for `--integrations <id>,...`
  /// and for the `tool_id` field on dry-run / apply outcomes.
  fn id(&self) -> &'static str;
  /// Human-readable label for the picker.
  fn display_name(&self) -> &'static str;
  /// Canonical on-disk path for a fresh install. `None` when the
  /// home directory can't be resolved (headless CI without `$HOME`);
  /// the caller surfaces that as [`PatchError::NoHome`].
  fn default_path(&self) -> Option<PathBuf>;
  /// Additional paths to check for an *existing* config before
  /// falling back to [`Self::default_path`]. Returned in priority order:
  /// the first path that actually exists wins. Default impl is
  /// empty — tools that accept multiple filename variants (OpenCode's
  /// `.jsonc` / `.json`, Continue's `.yaml` / `.yml`) override this
  /// to enumerate them, so re-running `init` patches the user's
  /// existing file rather than creating a parallel one.
  fn alt_paths(&self) -> Vec<PathBuf> {
    Vec::new()
  }
  fn format(&self) -> Format;
  /// Build the additions blob to merge into the existing file. For
  /// [`Format::Raw`] patchers this is ignored (the patcher
  /// overrides [`Self::raw_body`] instead).
  fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value;
  /// Override the default object-recursive merge. Default impl is
  /// `merge::merge(current, build_additions(ctx))` (objects recurse,
  /// arrays replace wholesale).
  ///
  /// Tools whose schema includes arrays of named objects — Continue
  /// (`models[]`), Zed (`available_models[]`), pi.dev (`models[]`)
  /// — override this to merge those arrays *by name* so re-running
  /// `init` doesn't wipe model entries the user added manually.
  fn merge_with_current(
    &self,
    current: serde_json::Value,
    ctx: &PatchContext,
  ) -> serde_json::Value {
    merge::merge(current, self.build_additions(ctx))
  }
  /// Name of an environment variable this tool reads the proxy key from,
  /// when it has no way to resolve a credential itself. Declared here so
  /// the wizard can tell the user which of the tools it just patched will
  /// sit there unauthenticated until the variable is exported — the tool
  /// knows its own contract, the wizard should not carry a list.
  fn required_env_var(&self) -> Option<&'static str> {
    None
  }
  /// Name of an environment variable this patcher *defines* (the
  /// sourceable `.sh` writers). Pairs with [`Self::required_env_var`] so
  /// the wizard can point at the file it just wrote instead of telling
  /// the user to invent an export.
  fn provides_env_var(&self) -> Option<&'static str> {
    None
  }
  /// Extra patchers applied whenever this one is, and never offered in
  /// the picker on their own. For a tool whose integration spans two
  /// files — pi.dev's provider block and the `enabledModels` scope that
  /// makes it reachable — the second file is a companion, not a second
  /// integration the user has to know to select.
  fn companions(&self) -> Vec<Box<dyn ToolPatcher>> {
    Vec::new()
  }
  /// For [`Format::Raw`] patchers: produce the full file body to
  /// write. Default implementation returns `None`, which means
  /// merge-based writes are used (Json/Yaml).
  fn raw_body(&self, _ctx: &PatchContext) -> Option<String> {
    None
  }
  /// Unix mode for the on-disk file. Defaults to `0o600` — these
  /// files may carry the proxy's real bearer token (embedded literally,
  /// or exported into a sourceable shell var by the `env.sh` /
  /// `claude-code.sh` writers) and live in `$HOME`, so group/world
  /// read isn't useful.
  fn unix_mode(&self) -> u32 {
    0o600
  }
}

#[cfg(test)]
impl PatchContext {
  /// The loopback proxy + stub key every patcher test patches against.
  /// Model kinds are classified from the ids, same as a real run.
  pub fn fixture(model_ids: &[&str]) -> Self {
    Self {
      proxy_base_url: "http://127.0.0.1:11435/v1".into(),
      api_key: "llamastash".into(),
      models: model_ids
        .iter()
        .map(|id| PatchModel::from_id((*id).to_string()))
        .collect(),
    }
  }
}

/// Preview-only outcome — bytes never hit disk. Returned by
/// [`dry_run`] and embedded in `init --json` so an agent can show
/// the user what would change before they consent.
#[derive(Debug, Clone, Serialize)]
pub struct DryRunOutcome {
  pub tool_id: &'static str,
  pub display_name: &'static str,
  pub path: PathBuf,
  pub diff_human: String,
  pub diff_json: Vec<RedactedDiffEntry>,
}

/// Result of a successful [`apply`]. `written_bytes` is the size of
/// the final file (post-merge); `diff_*` is the redacted view of
/// what changed, identical to the corresponding [`DryRunOutcome`].
#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
  pub tool_id: &'static str,
  pub display_name: &'static str,
  pub path: PathBuf,
  pub written_bytes: u64,
  pub diff_human: String,
  pub diff_json: Vec<RedactedDiffEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
  #[error("no home directory available; cannot resolve {tool_id} default path")]
  NoHome { tool_id: &'static str },
  #[error("{tool_id}: read {}: {error}", path.display())]
  Read {
    tool_id: &'static str,
    path: PathBuf,
    error: String,
  },
  #[error("{tool_id}: parse {} ({format:?}): {error}", path.display())]
  Parse {
    tool_id: &'static str,
    path: PathBuf,
    format: Format,
    error: String,
  },
  #[error("serialise additions: {0}")]
  Serialise(String),
  #[error("{tool_id}: write {}: {error}", path.display())]
  Write {
    tool_id: &'static str,
    path: PathBuf,
    error: String,
  },
}

/// Compute the redacted diff that [`apply`] *would* write, without
/// touching the filesystem. `override_path` lets tests target a
/// tempdir; production callers pass `None` to use the tool's
/// default location.
pub fn dry_run(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  override_path: Option<PathBuf>,
) -> Result<DryRunOutcome, PatchError> {
  let path = resolve_path(patcher, override_path)?;
  let diff_entries = match patcher.format() {
    Format::Json | Format::Yaml => write::compute_diff(patcher, ctx, &path, patcher.format())?,
    Format::Raw => write::compute_raw_diff(patcher, ctx, &path)?,
  };
  let diff_json = redact_diff(&diff_entries);
  let diff_human = render_human(&diff_json);
  Ok(DryRunOutcome {
    tool_id: patcher.id(),
    display_name: patcher.display_name(),
    path,
    diff_human,
    diff_json,
  })
}

/// Apply the patch: read current, merge additions, write atomic.
/// Returns the redacted diff alongside `written_bytes` so the
/// wizard's summary can render it without re-reading the file.
pub fn apply(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  override_path: Option<PathBuf>,
) -> Result<ApplyOutcome, PatchError> {
  let path = resolve_path(patcher, override_path)?;
  let (diff_entries, written_bytes) = match patcher.format() {
    Format::Json | Format::Yaml => write::apply_merge(patcher, ctx, &path, patcher.format())?,
    Format::Raw => write::apply_raw(patcher, ctx, &path)?,
  };
  let diff_json = redact_diff(&diff_entries);
  let diff_human = render_human(&diff_json);
  Ok(ApplyOutcome {
    tool_id: patcher.id(),
    display_name: patcher.display_name(),
    path,
    written_bytes,
    diff_human,
    diff_json,
  })
}

fn resolve_path(
  patcher: &dyn ToolPatcher,
  override_path: Option<PathBuf>,
) -> Result<PathBuf, PatchError> {
  if let Some(p) = override_path {
    return Ok(p);
  }
  let default = patcher.default_path().ok_or(PatchError::NoHome {
    tool_id: patcher.id(),
  })?;
  // Prefer an existing alt path (e.g. opencode.jsonc when the user
  // edits theirs with comments) over creating a parallel canonical
  // file. Falls back to the default for fresh installs.
  for alt in patcher.alt_paths() {
    if alt.exists() {
      return Ok(crate::util::paths::resolve_symlinks(&alt));
    }
  }
  // Through the symlink, not over it: these configs are commonly linked
  // into a dotfiles repo, and an atomic rename would replace the link.
  Ok(crate::util::paths::resolve_symlinks(&default))
}

/// Returns every patcher the wizard knows about. Order is the
/// order the picker displays.
pub fn all_patchers() -> Vec<Box<dyn ToolPatcher>> {
  tools::registered()
}

/// Resolve a patcher by its [`ToolPatcher::id`]. Used by the wizard's
/// `--integrations <id>,...` non-interactive form.
pub fn patcher_by_id(id: &str) -> Option<Box<dyn ToolPatcher>> {
  all_patchers().into_iter().find(|p| p.id() == id)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Trivial test patcher used by the skeleton's own tests. Not
  /// registered with [`all_patchers`].
  struct StubJson;

  impl ToolPatcher for StubJson {
    fn id(&self) -> &'static str {
      "stub-json"
    }
    fn display_name(&self) -> &'static str {
      "Stub JSON"
    }
    fn default_path(&self) -> Option<PathBuf> {
      None
    }
    fn format(&self) -> Format {
      Format::Json
    }
    fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value {
      serde_json::json!({
        "providers": {
          "llamastash": {
            "baseURL": ctx.proxy_base_url,
            "apiKey": ctx.api_key,
          }
        }
      })
    }
  }

  fn ctx() -> PatchContext {
    PatchContext::fixture(&[])
  }

  #[test]
  fn primary_prefers_a_chat_model_over_an_embedder() {
    let ctx = PatchContext::fixture(&["nomic-embed-text-v1.5", "qwen3-coder-30b"]);
    assert_eq!(ctx.primary().expect("primary").id, "qwen3-coder-30b");
  }

  #[test]
  fn primary_falls_back_to_the_first_entry_when_all_are_embedders() {
    let ctx = PatchContext::fixture(&["nomic-embed-text-v1.5", "bge-embed"]);
    assert_eq!(ctx.primary().expect("primary").id, "nomic-embed-text-v1.5");
    assert!(PatchContext::fixture(&[]).primary().is_none());
  }

  #[test]
  fn a_catalog_rows_mode_hint_beats_the_name_heuristic() {
    // A BERT encoder whose name says nothing, and a chat model that happens
    // to have "embed" in its name: the parsed header settles both.
    let mut row = crate::launch::resolve::CatalogRow::for_resolution(
      "/models/bge-large-en.gguf".into(),
      None,
      None,
    );
    row.mode_hint = Some("embedding".into());
    assert!(PatchModel::from_catalog_row(&row).is_embed);
    row.mode_hint = Some("chat".into());
    assert!(!PatchModel::from_catalog_row(&row).is_embed);
  }

  #[test]
  fn the_catalogs_context_window_is_declared_not_a_default() {
    let mut row = crate::launch::resolve::CatalogRow::for_resolution(
      "/models/Qwen3.6-27B-Q8_0.gguf".into(),
      None,
      None,
    );
    row.native_ctx = Some(262_144);
    assert_eq!(
      PatchModel::from_catalog_row(&row).declared_context(),
      262_144
    );
    // No figure in the catalog: fall back rather than guess high.
    row.native_ctx = None;
    assert_eq!(
      PatchModel::from_catalog_row(&row).declared_context(),
      DEFAULT_CONTEXT_WINDOW
    );
  }

  #[test]
  fn model_ids_follow_the_rule_the_proxy_publishes() {
    // GGUF: the file stem. Safetensors / Ollama: the row's label.
    let gguf = crate::launch::resolve::CatalogRow::for_resolution(
      "/models/Qwen3-Coder-30B-Q4_K_M.gguf".into(),
      None,
      None,
    );
    assert_eq!(
      PatchModel::from_catalog_row(&gguf).id,
      "Qwen3-Coder-30B-Q4_K_M"
    );
    let repo = crate::launch::resolve::CatalogRow::for_resolution(
      "/hub/models--Qwen--Qwen3-0.6B/snapshots/abc123".into(),
      Some("Qwen/Qwen3-0.6B".into()),
      None,
    );
    assert_eq!(PatchModel::from_catalog_row(&repo).id, "Qwen/Qwen3-0.6B");
  }

  #[cfg(unix)]
  #[test]
  fn a_symlinked_config_is_written_through_not_replaced() {
    // Dotfile setups link these configs into a managed repo. An atomic
    // rename over the link would swap it for a regular file and detach the
    // user's config from the repo tracking it.
    struct Linked {
      default: PathBuf,
    }
    impl ToolPatcher for Linked {
      fn id(&self) -> &'static str {
        "linked"
      }
      fn display_name(&self) -> &'static str {
        "Linked"
      }
      fn default_path(&self) -> Option<PathBuf> {
        Some(self.default.clone())
      }
      fn format(&self) -> Format {
        Format::Json
      }
      fn build_additions(&self, _ctx: &PatchContext) -> serde_json::Value {
        serde_json::json!({ "patched": true })
      }
    }

    let dir = crate::util::test_temp::unique_temp_dir("ext-symlink");
    let real = dir.join("dotfiles-conf.json");
    let link = dir.join("conf.json");
    std::fs::write(&real, r#"{"user":"kept"}"#).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // A default the test owns: `env::temp_dir()` is `/tmp` on a macOS
    // runner, which is itself a link to `/private/tmp`, so resolving it
    // *does* change the path and says nothing about the no-link case.
    let patcher = Linked {
      default: dir.join("default.json"),
    };
    let resolved = resolve_path(&patcher, None).expect("default path");
    assert_eq!(resolved, dir.join("default.json"), "no link, no change");
    apply(
      &patcher,
      &ctx(),
      Some(crate::util::paths::resolve_symlinks(&link)),
    )
    .expect("apply");

    assert!(
      std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink(),
      "link survived the write"
    );
    let body: serde_json::Value =
      serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
    assert_eq!(body["patched"], serde_json::json!(true));
    assert_eq!(body["user"], "kept");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn dry_run_against_missing_file_reports_additions() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-skeleton-dry");
    let path = dir.join("stub.json");
    let out = dry_run(&StubJson, &ctx(), Some(path.clone())).expect("dry_run");
    assert_eq!(out.tool_id, "stub-json");
    assert_eq!(out.path, path);
    // Whole-subtree Added rows collapse to the top-level new key —
    // same behaviour as the YAML writer (see config::writer::walk_diff).
    let added = out
      .diff_json
      .iter()
      .find(|d| d.path == "providers")
      .expect("providers added row");
    assert!(added.value_yaml.contains("baseURL"));
    assert!(!path.exists(), "dry_run never touches disk");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn dry_run_into_existing_file_reports_only_leaf_changes() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-skeleton-existing");
    let path = dir.join("stub.json");
    // Existing file already has the providers tree but a stale baseURL.
    std::fs::write(
      &path,
      r#"{"providers":{"llamastash":{"baseURL":"http://old/v1","apiKey":"llamastash"}}}"#,
    )
    .unwrap();
    let out = dry_run(&StubJson, &ctx(), Some(path.clone())).expect("dry_run");
    let leaf = out
      .diff_json
      .iter()
      .find(|d| d.path == "providers.llamastash.baseURL")
      .expect("changed leaf");
    assert_eq!(leaf.kind, "changed");
    assert!(leaf.value_yaml.contains("http://127.0.0.1:11435/v1"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn apply_then_apply_is_idempotent_on_same_inputs() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-skeleton-apply");
    let path = dir.join("stub.json");
    let first = apply(&StubJson, &ctx(), Some(path.clone())).expect("first apply");
    assert!(first.written_bytes > 0);
    let second = apply(&StubJson, &ctx(), Some(path.clone())).expect("second apply");
    // No changes the second time around.
    assert!(second.diff_json.is_empty(), "idempotent");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn no_home_when_default_path_unresolvable_and_no_override() {
    let err = dry_run(&StubJson, &ctx(), None).unwrap_err();
    assert!(matches!(
      err,
      PatchError::NoHome {
        tool_id: "stub-json"
      }
    ));
  }

  /// Patcher that exposes a default + one alt path. The alt is
  /// checked for existence first; the default is the fresh-install
  /// fallback.
  struct WithAlt {
    default: PathBuf,
    alt: PathBuf,
  }

  impl ToolPatcher for WithAlt {
    fn id(&self) -> &'static str {
      "with-alt"
    }
    fn display_name(&self) -> &'static str {
      "WithAlt"
    }
    fn default_path(&self) -> Option<PathBuf> {
      Some(self.default.clone())
    }
    fn alt_paths(&self) -> Vec<PathBuf> {
      vec![self.alt.clone()]
    }
    fn format(&self) -> Format {
      Format::Json
    }
    fn build_additions(&self, _ctx: &PatchContext) -> serde_json::Value {
      serde_json::json!({ "k": "v" })
    }
  }

  #[test]
  fn resolve_path_prefers_existing_alt_over_default() {
    let dir = crate::util::test_temp::unique_temp_dir("resolve-alt");
    let patcher = WithAlt {
      default: dir.join("default.json"),
      alt: dir.join("alt.jsonc"),
    };
    std::fs::write(&patcher.alt, "{}").unwrap();
    let resolved = resolve_path(&patcher, None).unwrap();
    assert_eq!(resolved, patcher.alt);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_path_falls_back_to_default_when_no_alt_exists() {
    let dir = crate::util::test_temp::unique_temp_dir("resolve-default");
    let patcher = WithAlt {
      default: dir.join("default.json"),
      alt: dir.join("alt.jsonc"),
    };
    let resolved = resolve_path(&patcher, None).unwrap();
    assert_eq!(resolved, patcher.default);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_path_override_wins_over_alt_and_default() {
    let dir = crate::util::test_temp::unique_temp_dir("resolve-override");
    let patcher = WithAlt {
      default: dir.join("default.json"),
      alt: dir.join("alt.jsonc"),
    };
    std::fs::write(&patcher.alt, "{}").unwrap();
    let explicit = dir.join("explicit.json");
    let resolved = resolve_path(&patcher, Some(explicit.clone())).unwrap();
    assert_eq!(resolved, explicit);
    std::fs::remove_dir_all(&dir).ok();
  }
}
