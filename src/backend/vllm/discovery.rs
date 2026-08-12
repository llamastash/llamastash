//! The vLLM discovery leaf over the shared safetensors substrate.
//!
//! The substrate (`crate::discovery::hf_repos`) does the walking and the
//! `config.json` parsing; this file is only the two engine-specific pieces a
//! leaf owes it: which candidates vLLM can serve, and how to project one into
//! a catalog row.

use std::path::Path;

use crate::discovery::hf_repos::HfRepoCandidate;
use crate::discovery::{DiscoveredModel, ModelSource};
use crate::util::model_caches::repo_id_from_cache_dir;

/// Whether vLLM can serve `candidate`.
///
/// Deliberately permissive: safetensors weights present and no GGUF in the
/// repo (a mixed repo belongs to the GGUF scanner). vLLM refuses some
/// architectures, but an allowlist rots against upstream far faster than we
/// could maintain it, so an unsupported model surfaces in the catalog and
/// fails at launch with vLLM's own diagnostic rather than being hidden.
pub fn eligible(candidate: &HfRepoCandidate) -> bool {
  candidate.has_safetensors && !candidate.has_gguf
}

/// Project an eligible candidate into a catalog row.
///
/// The row's `path` is the **snapshot directory**, not a file: vLLM is handed
/// the directory and resolves weights itself. `ModelSource` stays
/// `HuggingFace` — the repo genuinely came from the hub cache, and which
/// engines can serve it is already carried by `supported_backends`, so no
/// backend-named source variant is needed.
pub fn project(candidate: &HfRepoCandidate, backend_id: &str) -> DiscoveredModel {
  let weights = weights_bytes(&candidate.snapshot_path);
  let metadata = candidate
    .config_summary
    .as_ref()
    .map(|s| crate::discovery::hf_repos::config_to_metadata(s, &candidate.repo_id))
    // A config parse failure must not also cost the size signal. The probe
    // budget scales off `weights_bytes`, so losing it here left a 140 GB load
    // racing the default budget and getting killed mid-load.
    .or_else(|| {
      weights
        .is_some()
        .then(crate::discovery::hf_repos::metadata_without_config)
    })
    .map(|mut m| {
      m.weights_bytes = weights;
      // No GGML tag on a safetensors repo, so the verbatim `quant_method`
      // is the quant. Without it the row renders `?` in `list`.
      m.quant_label = candidate
        .config_summary
        .as_ref()
        .and_then(|s| s.quant_method.clone())
        .map(|q| q.to_ascii_uppercase());
      m
    });

  DiscoveredModel {
    path: candidate.snapshot_path.clone(),
    // The repo directory, so the TUI groups every revision of a repo together
    // the way it groups GGUF quants that share a folder.
    parent: repo_dir_of(&candidate.snapshot_path)
      .unwrap_or(&candidate.snapshot_path)
      .to_path_buf(),
    source: ModelSource::HuggingFace,
    metadata,
    parse_error: candidate
      .config_summary
      .is_none()
      .then(|| "config.json missing or unparseable".to_string()),
    split_siblings: Vec::new(),
    // Required, not cosmetic: the snapshot directory's basename is an opaque
    // revision hash, so without this every vLLM row would read as a sha.
    display_label: Some(candidate.repo_id.clone()),
    multimodal: None,
    supported_backends: vec![backend_id.to_string()],
    mtp_head: None,
  }
}

/// Sum of `*.safetensors` file sizes directly in `snapshot`, following the
/// symlinks the HF cache uses so the figure is real bytes on disk.
fn weights_bytes(snapshot: &Path) -> Option<u64> {
  let mut total = 0u64;
  let mut saw_any = false;
  for entry in std::fs::read_dir(snapshot).ok()?.flatten() {
    let path = entry.path();
    if path.extension().and_then(|e| e.to_str()) != Some("safetensors") {
      continue;
    }
    if let Ok(meta) = std::fs::metadata(&path) {
      total = total.saturating_add(meta.len());
      saw_any = true;
    }
  }
  saw_any.then_some(total)
}

/// The `models--owner--name` directory a `snapshots/<rev>` path sits under.
fn repo_dir_of(snapshot: &Path) -> Option<&Path> {
  let snapshots = snapshot.parent()?;
  (snapshots.file_name()? == "snapshots").then_some(())?;
  snapshots.parent()
}

/// Whether `path` is a directory holding safetensors weights and no GGUF —
/// the on-disk shape this backend serves, checked without the cache-layout
/// assumptions [`eligible`] gets for free from the enumerator.
pub fn is_safetensors_snapshot(path: &Path) -> bool {
  if !path.is_dir() {
    return false;
  }
  // The GGUF exclusion has to reach as deep as the GGUF scanner does, or a
  // repo with `Q4_K_M/model.gguf` beside its safetensors gets claimed here and
  // emitted by the scanner both.
  if crate::discovery::hf_repos::contains_gguf_in_tree(path) {
    return false;
  }
  let Ok(entries) = std::fs::read_dir(path) else {
    return false;
  };
  entries
    .flatten()
    .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("safetensors"))
}

/// The `owner/name` repo id for a snapshot directory, recovered from the
/// enclosing `models--owner--name` cache directory.
pub fn repo_id_for_snapshot(snapshot: &Path) -> Option<String> {
  let dir_name = repo_dir_of(snapshot)?.file_name()?.to_str()?;
  repo_id_from_cache_dir(dir_name)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  fn candidate(safetensors: bool, gguf: bool) -> HfRepoCandidate {
    HfRepoCandidate {
      repo_id: "owner/name".to_string(),
      snapshot_path: PathBuf::from("/c/models--owner--name/snapshots/rev"),
      config_summary: None,
      has_safetensors: safetensors,
      has_gguf: gguf,
    }
  }

  #[test]
  fn eligible_needs_safetensors_and_no_gguf() {
    assert!(eligible(&candidate(true, false)));
    assert!(!eligible(&candidate(false, false)), "no weights");
    assert!(
      !eligible(&candidate(true, true)),
      "a mixed repo belongs to the GGUF scanner"
    );
  }

  #[test]
  fn projection_uses_the_snapshot_dir_and_repo_id() {
    let row = project(&candidate(true, false), "vllm");
    assert_eq!(
      row.path,
      PathBuf::from("/c/models--owner--name/snapshots/rev")
    );
    assert_eq!(row.parent, PathBuf::from("/c/models--owner--name"));
    assert_eq!(row.display_label.as_deref(), Some("owner/name"));
    assert_eq!(row.source, ModelSource::HuggingFace);
    assert_eq!(row.supported_backends, vec!["vllm".to_string()]);
  }

  #[test]
  fn projection_without_a_config_records_a_parse_error() {
    let row = project(&candidate(true, false), "vllm");
    assert!(row.metadata.is_none());
    assert!(row.parse_error.is_some(), "the row still surfaces, flagged");
  }

  /// The probe budget scales off `weights_bytes`. Losing it because
  /// `config.json` failed to parse left a large repo racing the default
  /// budget and getting killed mid-load, so the size must survive a config
  /// the launch never needed.
  #[test]
  fn a_config_parse_failure_still_carries_the_size() {
    let dir = crate::util::test_temp::unique_temp_dir("vllm-noconfig-size");
    std::fs::write(dir.join("model.safetensors"), vec![0u8; 4096]).unwrap();
    let mut c = candidate(true, false);
    c.snapshot_path = dir.clone();
    c.config_summary = None;

    let row = project(&c, "vllm");
    assert!(
      row.parse_error.is_some(),
      "the parse failure is still flagged"
    );
    assert_eq!(
      row.metadata.as_ref().and_then(|m| m.weights_bytes),
      Some(4096),
      "the size signal must survive an unparseable config"
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// The GGUF scanner walks recursively, so a repo shipping both formats with
  /// the GGUF one level down yields two rows unless this exclusion reaches as
  /// deep. Two rows over one repo is what made the delete destructive.
  #[test]
  fn is_safetensors_snapshot_rejects_a_nested_gguf() {
    let dir = crate::util::test_temp::unique_temp_dir("vllm-nested-gguf");
    std::fs::write(dir.join("model.safetensors"), b"w").unwrap();
    assert!(is_safetensors_snapshot(&dir), "safetensors-only is claimed");

    std::fs::create_dir_all(dir.join("Q4_K_M")).unwrap();
    std::fs::write(dir.join("Q4_K_M/model.gguf"), b"g").unwrap();
    assert!(
      !is_safetensors_snapshot(&dir),
      "a GGUF below the snapshot dir belongs to the GGUF scanner"
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn repo_id_recovers_from_the_cache_dir_name() {
    assert_eq!(
      repo_id_for_snapshot(Path::new("/c/models--Qwen--Qwen2.5-0.5B/snapshots/abc")),
      Some("Qwen/Qwen2.5-0.5B".to_string())
    );
    assert_eq!(repo_id_for_snapshot(Path::new("/c/loose/dir")), None);
  }

  #[test]
  fn weights_bytes_sums_shards_and_ignores_other_files() {
    let dir = crate::util::test_temp::unique_temp_dir("vllm-weights");
    std::fs::write(dir.join("model-00001-of-00002.safetensors"), vec![0u8; 100]).unwrap();
    std::fs::write(dir.join("model-00002-of-00002.safetensors"), vec![0u8; 50]).unwrap();
    std::fs::write(dir.join("config.json"), b"{}").unwrap();
    assert_eq!(weights_bytes(&dir), Some(150));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn weights_bytes_is_none_when_there_are_no_safetensors() {
    let dir = crate::util::test_temp::unique_temp_dir("vllm-noweights");
    std::fs::write(dir.join("config.json"), b"{}").unwrap();
    assert_eq!(weights_bytes(&dir), None);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
