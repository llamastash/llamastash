//! Which models the patched tool configs register.
//!
//! Two sources, in preference order: the model `init`'s download step just
//! fetched (when it ran), then every favorite in the daemon's catalog. The
//! ids come from [`crate::launch::resolve::published_ids`], the same rule
//! `/v1/models` publishes under, so what a tool sends back as `body.model`
//! is a name the proxy already answers to — for a GGUF file, a safetensors
//! repo, an Ollama blob, or a Lemonade registry entry alike, and for two
//! same-named GGUFs cached in different roots.

use std::collections::HashSet;

use serde_json::Value;

use crate::cli::cli_args::Cli;
use crate::config::Config;
use crate::init::external::PatchModel;
use crate::init::wizard::ModelSummary;

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
pub async fn resolve(
  cli: &Cli,
  config: &Config,
  downloaded: Option<&ModelSummary>,
) -> ResolvedModels {
  let mut models: Vec<PatchModel> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  if let Some(m) = downloaded.and_then(from_download) {
    seen.insert(m.id.clone());
    models.push(m);
  }

  let note = match favorites(cli, config).await {
    Ok(favs) => {
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
    Err(e) => Some(format!(
      "could not read favorites ({e}) — registering without them"
    )),
  };

  ResolvedModels { models, note }
}

/// Name what the download step fetched.
///
/// A GGUF pull lands one or more `.gguf` files and the catalog will key on
/// the file, so the id is the file's stem. A safetensors pull lands a whole
/// repo whose snapshot directory is named for the revision hash — the repo
/// id is what discovery labels that row with, so it is the id here too.
fn from_download(summary: &ModelSummary) -> Option<PatchModel> {
  let gguf = summary.files.iter().find(|f| {
    f.extension()
      .and_then(|e| e.to_str())
      .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
  });
  match gguf {
    Some(path) => Some(PatchModel::from_id(crate::util::paths::model_public_id(
      path, None,
    ))),
    // No GGUF but files landed: a safetensors repo, pulled whole.
    None => (!summary.files.is_empty()).then(|| PatchModel::from_id(summary.repo.clone())),
  }
}

/// Every favorite that still resolves to a catalog row, sorted by id.
///
/// The catalog filter is the same one `favorites list` applies: a favorite
/// whose file was deleted or moved out of a watched directory is dropped
/// rather than written into a tool config as an unservable name.
async fn favorites(cli: &Cli, config: &Config) -> Result<Vec<PatchModel>, String> {
  let mut client = crate::cli::client::connect_or_spawn(cli, config)
    .await
    .map_err(|e| e.message.unwrap_or_else(|| format!("exit {}", e.code)))?;
  let body = client
    .call("favorite_list", None)
    .await
    .map_err(|e| e.to_string())?;
  let favorited: HashSet<&str> = body
    .get("favorites")
    .and_then(Value::as_array)
    .map(|arr| {
      arr
        .iter()
        .filter_map(crate::cli::output::row_path)
        .collect()
    })
    .unwrap_or_default();
  if favorited.is_empty() {
    return Ok(Vec::new());
  }
  let rows = crate::cli::resolve::fetch_catalog(&mut client)
    .await
    .map_err(|e| e.message.unwrap_or_else(|| format!("exit {}", e.code)))?;
  // Ids come from the whole catalog, not from the favorited subset: two
  // same-named GGUFs disambiguate against each other whether or not both
  // are starred, so what lands in a tool config is the id `/v1/models`
  // answers to.
  let ids = crate::launch::resolve::published_ids(&rows);
  let mut models: Vec<PatchModel> = rows
    .iter()
    .zip(ids)
    .filter(|(r, _)| favorited.contains(r.path.as_str()))
    .map(|(r, id)| PatchModel::from_catalog_row(r, id))
    .collect();
  models.sort_by(|a, b| a.id.cmp(&b.id));
  Ok(models)
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
    let m = from_download(&summary(
      "unsloth/Qwen3-Coder-30B-GGUF",
      &["/m/README.md", "/m/Qwen3-Coder-30B-Q4_K_M.gguf"],
    ))
    .expect("model");
    assert_eq!(m.id, "Qwen3-Coder-30B-Q4_K_M");
    assert!(!m.is_embed);
  }

  #[test]
  fn safetensors_download_is_named_by_repo_id() {
    // No GGUF in the file set — the whole repo is the model, and its
    // snapshot dir is a revision hash, so the repo id is the only usable id.
    let m = from_download(&summary(
      "Qwen/Qwen3-0.6B",
      &["/m/model.safetensors", "/m/config.json"],
    ))
    .expect("model");
    assert_eq!(m.id, "Qwen/Qwen3-0.6B");
  }

  #[test]
  fn download_with_no_files_registers_nothing() {
    assert!(from_download(&summary("owner/repo", &[])).is_none());
  }

  #[test]
  fn embedder_download_is_flagged_from_its_name() {
    let m = from_download(&summary(
      "nomic-ai/nomic-embed-text-v1.5-GGUF",
      &["/m/nomic-embed-text-v1.5.Q8_0.gguf"],
    ))
    .expect("model");
    assert!(m.is_embed);
  }
}
