//! Enumerate Ollama-managed models.
//!
//! Ollama stores models in a content-addressed layout:
//! ```text
//! ~/.ollama/models/
//!   blobs/
//!     sha256-<hex>     # the actual GGUF (and other) blobs
//!   manifests/
//!     registry.ollama.ai/library/<name>/<tag>   # JSON manifest
//! ```
//!
//! The scanner can't surface these as `.gguf` files because the
//! filenames are content hashes with no extension. This module walks
//! `manifests/` instead, parses each manifest's `layers` array, and
//! resolves the model-blob layer to its on-disk blob path. Results
//! are surfaced as [`DiscoveredModel`]s under [`ModelSource::Ollama`].

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::discovery::{DiscoveredModel, ModelSource};
use crate::gguf::{read_path, summarise_metadata, HeaderReadOptions};

/// Ollama's media-type tag for the GGUF model blob inside a manifest's
/// `layers` list. Older versions of Ollama omit the `+` suffix.
const MODEL_MEDIA_TYPE: &str = "application/vnd.ollama.image.model";

/// Top-level manifest JSON shape. We only read the fields we need to
/// resolve the model blob — extra fields are tolerated so a manifest
/// schema bump doesn't break enumeration.
#[derive(Debug, Deserialize)]
struct Manifest {
  #[serde(default)]
  layers: Vec<ManifestLayer>,
}

#[derive(Debug, Deserialize)]
struct ManifestLayer {
  #[serde(rename = "mediaType", default)]
  media_type: String,
  #[serde(default)]
  digest: String,
}

/// Stream-enumerate every Ollama-managed model under `root`
/// (the `~/.ollama/models` directory). Each [`DiscoveredModel`]
/// reports the canonical blob path so the supervisor can launch
/// `llama-server -m <blob-path>` without going through Ollama.
///
/// Errors per-manifest (malformed JSON, missing blob, unreadable
/// file) are logged and skipped; the remaining manifests still
/// surface.
pub fn enumerate(root: PathBuf) -> mpsc::Receiver<DiscoveredModel> {
  let (tx, rx) = mpsc::channel(64);
  tokio::spawn(async move {
    let manifests_dir = root.join("manifests");
    let blobs_dir = root.join("blobs");
    let manifests =
      match tokio::task::spawn_blocking(move || collect_manifests(&manifests_dir)).await {
        Ok(v) => v,
        Err(e) => {
          log::warn!("ollama manifest walker panicked: {e}");
          return;
        }
      };

    for (manifest_path, name_tag) in manifests {
      let blobs_dir = blobs_dir.clone();
      let resolved: Option<(String, PathBuf)> = tokio::task::spawn_blocking(move || {
        let raw = match std::fs::read_to_string(&manifest_path) {
          Ok(s) => s,
          Err(e) => {
            log::warn!(
              "ollama manifest unreadable {}: {e}",
              manifest_path.display()
            );
            return None;
          }
        };
        let manifest: Manifest = match serde_json::from_str(&raw) {
          Ok(m) => m,
          Err(e) => {
            log::warn!(
              "ollama manifest parse failure {}: {e}",
              manifest_path.display()
            );
            return None;
          }
        };
        let digest = manifest
          .layers
          .into_iter()
          .find(|l| l.media_type == MODEL_MEDIA_TYPE)
          .map(|l| l.digest)?;
        let blob_path = digest_to_blob_path(&blobs_dir, &digest)?;
        Some((name_tag, blob_path))
      })
      .await
      .ok()
      .flatten();
      let (resolved_name_tag, blob_path) = match resolved {
        Some(v) => v,
        None => continue,
      };

      let blob_path_for_parse = blob_path.clone();
      let header_result = tokio::task::spawn_blocking(move || {
        read_path(&blob_path_for_parse, HeaderReadOptions::default())
      })
      .await;
      let model = match header_result {
        Ok(Ok(read)) => DiscoveredModel {
          path: blob_path.clone(),
          parent: blob_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
          source: ModelSource::Ollama,
          metadata: Some(summarise_metadata(&read.header)),
          parse_error: None,
          split_siblings: Vec::new(),
        },
        Ok(Err(e)) => DiscoveredModel {
          path: blob_path.clone(),
          parent: blob_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
          source: ModelSource::Ollama,
          metadata: None,
          parse_error: Some(format!("{resolved_name_tag}: {e}")),
          split_siblings: Vec::new(),
        },
        Err(join_err) => {
          log::warn!("ollama parser task panicked: {join_err}");
          continue;
        }
      };
      if tx.send(model).await.is_err() {
        return;
      }
    }
  });
  rx
}

/// Walk the `manifests/` subtree returning `(manifest_path, "<name>:<tag>")`
/// for every regular file. The on-disk layout is
/// `registry.ollama.ai/library/<name>/<tag>` (or other registries),
/// so the model is named by the last two path components.
fn collect_manifests(manifests_dir: &Path) -> Vec<(PathBuf, String)> {
  if !manifests_dir.exists() {
    return Vec::new();
  }
  let mut out = Vec::new();
  for entry in walkdir_files(manifests_dir) {
    let name_tag = manifest_name_tag(manifests_dir, &entry);
    out.push((entry, name_tag));
  }
  out
}

/// Lightweight depth-first walker that yields regular-file paths under
/// `root`. Kept hand-rolled to avoid pulling in `walkdir` just for one
/// flat traversal that doesn't need gitignore semantics.
fn walkdir_files(root: &Path) -> Vec<PathBuf> {
  let mut stack = vec![root.to_path_buf()];
  let mut out = Vec::new();
  while let Some(dir) = stack.pop() {
    let read = match std::fs::read_dir(&dir) {
      Ok(r) => r,
      Err(e) => {
        log::warn!("ollama: cannot read {}: {e}", dir.display());
        continue;
      }
    };
    for entry in read.flatten() {
      let p = entry.path();
      match entry.file_type() {
        Ok(ft) if ft.is_dir() => stack.push(p),
        Ok(ft) if ft.is_file() => out.push(p),
        _ => {}
      }
    }
  }
  out
}

/// `name:tag` derived from the relative path under `manifests/`.
/// Ollama's canonical layout puts the model name and tag as the last
/// two segments; we use them directly so the surfaced name matches
/// what `ollama list` shows.
fn manifest_name_tag(manifests_dir: &Path, manifest_path: &Path) -> String {
  let rel = manifest_path
    .strip_prefix(manifests_dir)
    .unwrap_or(manifest_path);
  let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
  match parts.as_slice() {
    [.., name, tag] => format!("{name}:{tag}"),
    [single] => (*single).to_string(),
    _ => rel.display().to_string(),
  }
}

/// Convert a manifest digest like `sha256:<hex>` to the on-disk blob
/// path `<blobs_dir>/sha256-<hex>` Ollama uses.
fn digest_to_blob_path(blobs_dir: &Path, digest: &str) -> Option<PathBuf> {
  let (algo, hex) = digest.split_once(':')?;
  if algo.is_empty() || hex.is_empty() {
    return None;
  }
  Some(blobs_dir.join(format!("{algo}-{hex}")))
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  use crate::gguf::test_fixtures::build_minimal_gguf;

  fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-ollama-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
  }

  #[test]
  fn digest_round_trip_into_blob_path() {
    let blobs = PathBuf::from("/models/blobs");
    let p = digest_to_blob_path(&blobs, "sha256:abc123").expect("ok");
    assert_eq!(p, PathBuf::from("/models/blobs/sha256-abc123"));
    assert!(digest_to_blob_path(&blobs, "bare").is_none());
    assert!(digest_to_blob_path(&blobs, "sha256:").is_none());
  }

  #[test]
  fn manifest_name_tag_takes_last_two_segments() {
    let manifests = PathBuf::from("/m");
    let p = PathBuf::from("/m/registry.ollama.ai/library/qwen2.5-coder/7b");
    assert_eq!(manifest_name_tag(&manifests, &p), "qwen2.5-coder:7b");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn enumerate_maps_manifest_layer_to_blob_with_metadata() {
    let root = temp_root("happy");
    let manifests_dir = root.join("manifests/registry.ollama.ai/library/qwen-test");
    let blobs_dir = root.join("blobs");
    fs::create_dir_all(&manifests_dir).unwrap();
    fs::create_dir_all(&blobs_dir).unwrap();

    // Seed a blob and a manifest pointing at it.
    let blob_bytes = build_minimal_gguf("llama");
    let digest_hex = "deadbeef";
    let blob_path = blobs_dir.join(format!("sha256-{digest_hex}"));
    fs::write(&blob_path, &blob_bytes).unwrap();
    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "layers": [
        {"mediaType": MODEL_MEDIA_TYPE, "digest": format!("sha256:{digest_hex}"), "size": blob_bytes.len()},
        {"mediaType": "application/vnd.ollama.image.params", "digest": "sha256:ignored"},
      ]
    });
    fs::write(
      manifests_dir.join("7b"),
      serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let mut rx = enumerate(root.clone());
    let m = rx.recv().await.expect("one model");
    assert!(rx.recv().await.is_none(), "exactly one manifest");
    assert_eq!(m.source, ModelSource::Ollama);
    assert_eq!(m.path, blob_path);
    assert!(m.metadata.is_some(), "blob is a valid GGUF");
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn manifest_without_model_layer_is_skipped() {
    let root = temp_root("no-model-layer");
    let manifests_dir = root.join("manifests/registry.ollama.ai/library/orphan");
    fs::create_dir_all(&manifests_dir).unwrap();
    fs::create_dir_all(root.join("blobs")).unwrap();
    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "layers": [{"mediaType": "application/vnd.ollama.image.params", "digest": "sha256:nope"}]
    });
    fs::write(
      manifests_dir.join("latest"),
      serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut rx = enumerate(root.clone());
    assert!(
      rx.recv().await.is_none(),
      "no model layer → no row surfaced"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn enumerate_missing_root_emits_nothing() {
    let mut rx = enumerate(PathBuf::from("/nonexistent/ollama/root"));
    assert!(rx.recv().await.is_none());
  }
}
