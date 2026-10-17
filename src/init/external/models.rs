//! Which models the patched tool configs register.
//!
//! Two sources, in preference order: the model `init`'s download step just
//! fetched (when it ran), then every favorite in the daemon's catalog. A
//! favorite with presets registers as `<id>@<preset>`, one per preset. The ids come
//! from [`crate::launch::resolve::published_ids`], the same rule
//! `/v1/models` publishes under, so what a tool sends back as `body.model`
//! is a name the proxy already answers to — for a GGUF file, a safetensors
//! repo, an Ollama blob, or a backend-registry entry alike, and for two
//! same-named GGUFs cached in different roots.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::cli::cli_args::Cli;
use crate::config::{Config, ConfigPresetBlock};
use crate::init::external::PatchModel;
use crate::init::wizard::ModelSummary;
use crate::launch::resolve::CatalogRow;

/// Resolved model list plus anything the user should hear about how it was
/// built. `note` is a human-readable line the wizard prints on non-`--json`
/// runs — an empty list is the confusing outcome (a provider block with no
/// models), so it always says why.
pub struct ResolvedModels {
  pub models: Vec<PatchModel>,
  pub note: Option<String>,
}

/// Build the registration list. Favorites are read through the daemon, so a
/// daemon that can't be reached costs favorites but not the step: whatever
/// the download step produced is still registered, with a note.
///
/// One catalog read serves both sources. The downloaded file's id has to be
/// decided against the same catalog the favorites' are: a basename shared
/// with another model publishes qualified, so the bare stem alone would
/// register a `body.model` that always answers `400 ambiguous_model`.
pub async fn resolve(
  cli: &Cli,
  config: &Config,
  downloaded: Option<&ModelSummary>,
) -> ResolvedModels {
  let (catalog, read_error) = match load_catalog(cli, config).await {
    Ok(catalog) => (Some(catalog), None),
    Err(e) => (
      None,
      Some(format!(
        "could not read favorites ({e}) — registering without them"
      )),
    ),
  };

  let mut models: Vec<PatchModel> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  if let Some(m) = downloaded.and_then(|d| from_download(d, catalog.as_ref())) {
    seen.insert(m.id.clone());
    models.push(m);
  }

  let note = match &catalog {
    Some(catalog) => {
      let mut favs = catalog.favorites(&config.presets);
      // By model only: a model's presets keep their default-first order.
      favs.sort_by(|a, b| a.id.split('@').next().cmp(&b.id.split('@').next()));
      let count = favs.len();
      for f in favs {
        if seen.insert(f.id.clone()) {
          models.push(f);
        }
      }
      (count == 0).then(|| {
        "no favorites to register — star models in the TUI or `llamastash favorites add <model>`"
          .to_string()
      })
    }
    None => read_error,
  };

  ResolvedModels { models, note }
}

/// The daemon's catalog plus which of its rows are starred, and the id each
/// row publishes under. Held together because the publishing rule is
/// catalog-wide: no row's id can be decided on its own.
struct Catalog {
  rows: Vec<CatalogRow>,
  ids: Vec<String>,
  favorited: HashSet<String>,
}

impl Catalog {
  /// Every starred row that still resolves, under its published id.
  ///
  /// The catalog filter is the same one `favorites list` applies: a favorite
  /// whose file was deleted or moved out of a watched directory is dropped
  /// rather than written into a tool config as an unservable name.
  fn favorites(&self, presets: &BTreeMap<String, ConfigPresetBlock>) -> Vec<PatchModel> {
    self
      .rows
      .iter()
      .zip(&self.ids)
      .filter(|(r, _)| self.favorited.contains(&r.path))
      .flat_map(|(r, id)| self.patch_models(r, id, presets))
      .collect()
  }

  /// A row registered the way it launches. A model with presets registers one
  /// `<id>@<preset>` per preset, the default first so a single-slot tool picks
  /// it, each declaring that preset's context. A model without presets
  /// registers its plain id. Where no preset sets a context, it is the server
  /// entry's configured default, then the trained window.
  fn patch_models(
    &self,
    row: &CatalogRow,
    id: &str,
    presets: &BTreeMap<String, ConfigPresetBlock>,
  ) -> Vec<PatchModel> {
    let base = PatchModel::from_catalog_row(row, id.to_string());
    let path = Path::new(&row.path);
    let context = |preset_ctx: Option<u32>, server: Option<&str>| {
      preset_ctx
        .or_else(|| crate::backend::config_default_ctx(path, server))
        .map(u64::from)
        .or(base.context_window)
    };
    let eff = crate::launch::presets::effective_presets(
      &row.name(),
      &row.path,
      row.arch.as_deref(),
      presets,
      &self.rows,
    );
    if eff.presets.is_empty() {
      let mut m = base.clone();
      m.context_window = context(None, None);
      return vec![m];
    }
    let default = eff.default_preset().map(|p| p.name.as_str());
    let mut named: Vec<_> = eff.presets.iter().collect();
    named.sort_by_key(|p| Some(p.name.as_str()) != default);
    named
      .into_iter()
      .map(|p| {
        let mut m = base.clone();
        m.id = format!("{id}@{}", p.name);
        m.context_window = context(p.params.ctx, p.params.server.as_deref());
        m
      })
      .collect()
  }

  /// The id this catalog publishes `path` under, or `None` for a path it has
  /// no row for.
  fn published_id(&self, path: &str) -> Option<&str> {
    self
      .rows
      .iter()
      .zip(&self.ids)
      .find(|(r, _)| r.path == path)
      .map(|(_, id)| id.as_str())
  }
}

/// Name what the download step fetched.
///
/// A GGUF pull lands one or more `.gguf` files and the catalog will key on
/// the file, so the id is whatever that row publishes under. A safetensors
/// pull lands a whole repo whose snapshot directory is named for the revision
/// hash — the repo id is what discovery labels that row with, so it is the id
/// here too.
fn from_download(summary: &ModelSummary, catalog: Option<&Catalog>) -> Option<PatchModel> {
  let gguf = summary.files.iter().find(|f| {
    f.extension()
      .and_then(|e| e.to_str())
      .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
  });
  match gguf {
    Some(path) => {
      let (id, row) = downloaded_id(path, catalog);
      let mut m = PatchModel::from_id(id);
      let (reasoning, effort) = crate::init::external::effort::from_gguf(path);
      m.reasoning = reasoning;
      m.effort = effort;
      // Vision comes from the scan pairing a projector with the file, so
      // only a row the catalog already has can say.
      m.vision = row.and_then(|r| r.multimodal).is_some_and(|mm| mm.vision);
      Some(m)
    }
    // No GGUF but files landed: a safetensors repo, pulled whole.
    None => (!summary.files.is_empty()).then(|| PatchModel::from_id(summary.repo.clone())),
  }
}

/// The id the proxy publishes the just-downloaded file under.
///
/// Resolved through the catalog because the file's own name cannot decide it:
/// a basename shared with another model publishes qualified, and the bare
/// stem then answers `400 ambiguous_model` forever. Falls back to that stem
/// when there is no catalog (daemon unreachable) or no row for the file yet
/// (the scan has not caught up) — which is still the right answer whenever
/// the name is unique, and the best guess available otherwise. The row is
/// returned too, when there is one.
fn downloaded_id<'a>(
  path: &std::path::Path,
  catalog: Option<&'a Catalog>,
) -> (String, Option<&'a CatalogRow>) {
  let stem = || crate::util::paths::model_public_id(path, None);
  let Some(catalog) = catalog else {
    return (stem(), None);
  };
  let reference = path.to_string_lossy();
  crate::launch::resolve::resolve_model_with_candidates(&catalog.rows, &reference)
    .ok()
    .and_then(|row| {
      let id = catalog.published_id(&row.path)?.to_owned();
      let row = catalog.rows.iter().find(|r| r.path == row.path)?;
      Some((id, Some(row)))
    })
    .unwrap_or_else(|| (stem(), None))
}

/// The daemon's catalog and favorites in one read.
async fn load_catalog(cli: &Cli, config: &Config) -> Result<Catalog, String> {
  let mut client = crate::cli::client::connect_or_spawn(cli, config)
    .await
    .map_err(|e| e.message.unwrap_or_else(|| format!("exit {}", e.code)))?;
  let body = client
    .call("favorite_list", None)
    .await
    .map_err(|e| e.to_string())?;
  let favorited: HashSet<String> = body
    .get("favorites")
    .and_then(Value::as_array)
    .map(|arr| {
      arr
        .iter()
        .filter_map(crate::cli::output::row_path)
        .map(ToOwned::to_owned)
        .collect()
    })
    .unwrap_or_default();
  let rows = crate::cli::resolve::fetch_catalog(&mut client)
    .await
    .map_err(|e| e.message.unwrap_or_else(|| format!("exit {}", e.code)))?;
  // Ids come from the whole catalog, not from the favorited subset: two
  // same-named GGUFs disambiguate against each other whether or not both are
  // starred, so what lands in a tool config is the id `/v1/models` answers to.
  let ids = crate::launch::resolve::published_ids(&rows);
  Ok(Catalog {
    rows,
    ids,
    favorited,
  })
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::*;

  fn summary(repo: &str, files: &[&str]) -> ModelSummary {
    ModelSummary {
      repo: repo.to_string(),
      files: files.iter().map(PathBuf::from).collect(),
      total_bytes: 0,
    }
  }

  #[test]
  fn gguf_download_is_named_by_file_stem() {
    let m = from_download(
      &summary(
        "unsloth/Qwen3-Coder-30B-GGUF",
        &["/m/README.md", "/m/Qwen3-Coder-30B-Q4_K_M.gguf"],
      ),
      None,
    )
    .expect("model");
    assert_eq!(m.id, "Qwen3-Coder-30B-Q4_K_M");
    assert!(!m.is_embed);
  }

  #[test]
  fn safetensors_download_is_named_by_repo_id() {
    // No GGUF in the file set — the whole repo is the model, and its
    // snapshot dir is a revision hash, so the repo id is the only usable id.
    let m = from_download(
      &summary(
        "Qwen/Qwen3-0.6B",
        &["/m/model.safetensors", "/m/config.json"],
      ),
      None,
    )
    .expect("model");
    assert_eq!(m.id, "Qwen/Qwen3-0.6B");
  }

  #[test]
  fn download_with_no_files_registers_nothing() {
    assert!(from_download(&summary("owner/repo", &[]), None).is_none());
  }

  /// The download path used to write the bare stem straight into the tool
  /// config, so a pull whose basename another cached model already carries
  /// registered a `body.model` that answered `400 ambiguous_model` forever.
  #[test]
  fn gguf_download_takes_the_published_id_when_its_name_collides() {
    let cat = catalog(&[
      ("/hf/demo-model-Q4_K_M.gguf", "huggingface"),
      ("/lms/demo-model-Q4_K_M.gguf", "lm-studio"),
    ]);
    let m = from_download(
      &summary("acme/Demo-GGUF", &["/hf/demo-model-Q4_K_M.gguf"]),
      Some(&cat),
    )
    .expect("model");
    assert_eq!(m.id, "hf/demo-model-Q4_K_M");

    // A name nothing else claims still registers as the plain stem, and a
    // file the catalog has not seen yet falls back to it.
    let solo = catalog(&[("/hf/demo-model-Q4_K_M.gguf", "huggingface")]);
    assert_eq!(
      from_download(
        &summary("acme/Demo-GGUF", &["/hf/demo-model-Q4_K_M.gguf"]),
        Some(&solo),
      )
      .expect("model")
      .id,
      "demo-model-Q4_K_M"
    );
    assert_eq!(
      from_download(
        &summary("acme/Demo-GGUF", &["/elsewhere/unscanned.gguf"]),
        Some(&solo),
      )
      .expect("model")
      .id,
      "unscanned"
    );
  }

  fn catalog(rows: &[(&str, &str)]) -> Catalog {
    let rows: Vec<crate::launch::resolve::CatalogRow> = rows
      .iter()
      .map(|(path, source)| {
        let mut r =
          crate::launch::resolve::CatalogRow::for_resolution(path.to_string(), None, None);
        r.source = source.to_string();
        r
      })
      .collect();
    let ids = crate::launch::resolve::published_ids(&rows);
    Catalog {
      rows,
      ids,
      favorited: HashSet::new(),
    }
  }

  /// A favorite with presets registers one `<id>@<preset>` per preset, the
  /// default first, each with the context that preset launches at. One without
  /// presets registers its plain id. Writing the trained window for a model
  /// whose preset launches it smaller made the tool overflow it. The
  /// server-entry default is covered where [`crate::backend::config_default_ctx`]
  /// is implemented.
  #[test]
  fn favorites_register_every_preset_with_its_context() {
    let presets: BTreeMap<String, ConfigPresetBlock> = yaml_serde::from_str(
      "Integ-Model-*:\n  default: long\n  entries:\n    coder: {knobs: {ctx: 131072}}\n    long: {knobs: {ctx: 262144}}\n    plain: {extras: [--x]}\n\
       Integ-Auto-*:\n  default: auto\n  entries:\n    coder: {knobs: {ctx: 4096}}\n",
    )
    .expect("preset yaml");
    let paths = [
      "/m/Integ-Model-Q4.gguf",
      "/m/Integ-Auto-Q4.gguf",
      "/m/Integ-Bare-Q4.gguf",
      "/m/Integ-None-Q4.gguf",
    ];
    let mut cat = catalog(&paths.iter().map(|p| (*p, "local")).collect::<Vec<_>>());
    for r in &mut cat.rows {
      r.native_ctx = (!r.path.contains("None")).then_some(1_000_000);
    }
    cat.favorited = paths.iter().map(|p| p.to_string()).collect();
    let got: Vec<(String, u64)> = cat
      .favorites(&presets)
      .into_iter()
      .map(|m| (m.id.clone(), m.declared_context()))
      .collect();
    let want = |id: &str, ctx: u64| (id.to_string(), ctx);
    assert_eq!(
      got,
      vec![
        want("Integ-Model-Q4@long", 262144),
        want("Integ-Model-Q4@coder", 131072),
        want("Integ-Model-Q4@plain", 1_000_000),
        want("Integ-Auto-Q4@coder", 4096),
        want("Integ-Bare-Q4", 1_000_000),
        want("Integ-None-Q4", 32768),
      ]
    );
  }

  #[test]
  fn embedder_download_is_flagged_from_its_name() {
    let m = from_download(
      &summary(
        "nomic-ai/nomic-embed-text-v1.5-GGUF",
        &["/m/nomic-embed-text-v1.5.Q8_0.gguf"],
      ),
      None,
    )
    .expect("model");
    assert!(m.is_embed);
  }
}
