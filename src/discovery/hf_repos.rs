//! Backend-neutral HuggingFace snapshot-repo enumerator for **non-GGUF**
//! model repos (directories of `config.json` + `*.safetensors`).
//!
//! This is the shared substrate half of the two-layer discovery design: it
//! walks the same HF hub cache roots GGUF discovery scans and yields neutral
//! [`HfRepoCandidate`] rows. A future safetensors/HF-format engine adopts the
//! substrate by supplying only a small eligibility predicate + a projection
//! that stamps its own [`crate::discovery::ModelSource`] — it never re-walks
//! the cache or re-parses `config.json`.
//!
//! Most metadata parsing is generic HF-transformers shape, so it lives here in
//! [`config_to_metadata`] and is reused verbatim by every consumer; only quant
//! interpretation (affine bits/group-size) is engine-specific and stays in the
//! leaf.
//!
//! The module references no engine/backend symbols — that neutrality is the
//! deliverable, pinned by `module_references_no_backend_symbols`.

use std::path::{Path, PathBuf};

use crate::gguf::metadata::{label_for_param_count, ModeHint, ModelMetadata, Quant};

/// One non-GGUF HF repo surfaced by the enumerator. Neutral: it carries no
/// engine tag and no `ModelSource` — the consuming leaf decides eligibility
/// and stamps the source in its projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfRepoCandidate {
  /// `owner/name` reconstructed from the `models--owner--name` cache dir.
  pub repo_id: String,
  /// The resolved `snapshots/<rev>/` directory. Kept so the leaf can size
  /// weights (sum `*.safetensors` file sizes) without re-walking.
  pub snapshot_path: PathBuf,
  /// Parsed `config.json` (+ `tokenizer_config.json` overlay). `None` when
  /// `config.json` is missing or unparseable — the row is still emitted.
  pub config_summary: Option<ConfigSummary>,
  /// At least one `*.safetensors` weight file in the snapshot.
  pub has_safetensors: bool,
  /// At least one `*.gguf` file in the snapshot. Always `false` for an
  /// emitted candidate (mixed repos belong to the GGUF scanner).
  pub has_gguf: bool,
}

/// The slices of `config.json` + `tokenizer_config.json` the shared metadata
/// mapping needs. Best-effort: any field absent from the JSON stays `None` /
/// empty. All generic HF-transformers keys — no engine specifics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigSummary {
  /// `config.json: model_type` — the architecture id (`"qwen2"`, `"llama"`).
  pub model_type: Option<String>,
  /// `config.json: architectures` — class names (`["Qwen2ForCausalLM"]`).
  pub architectures: Vec<String>,
  /// `config.json: max_position_embeddings` — the native context length.
  pub max_position_embeddings: Option<u64>,
  /// Hidden dim, layer count, vocab, FFN dim — drive the param estimate.
  pub hidden_size: Option<u64>,
  pub num_hidden_layers: Option<u64>,
  pub vocab_size: Option<u64>,
  pub intermediate_size: Option<u64>,
  /// `config.json: quantization` block present. The shared mapping does not
  /// interpret it (that is the leaf's job); it only records presence.
  pub has_quantization: bool,
  /// `tokenizer_config.json: chat_template` (string form only).
  pub chat_template: Option<String>,
  /// `tokenizer_config.json: tokenizer_class`.
  pub tokenizer_class: Option<String>,
}

/// Enumerate non-GGUF safetensors repos under the given HF hub cache roots.
///
/// Best-effort and resilient like the GGUF scanner: an unreadable root, a
/// malformed repo dir, or a bad `config.json` degrades that one slice — it
/// never aborts the walk. Only repos that have `*.safetensors` and **no**
/// `*.gguf` are emitted; a mixed repo is left to the GGUF scanner.
pub fn enumerate_repos(hub_roots: &[PathBuf]) -> Vec<HfRepoCandidate> {
  let mut out = Vec::new();
  for root in hub_roots {
    enumerate_root(root, &mut out);
  }
  out
}

/// Convenience entry point: resolve the HF hub cache roots the same way GGUF
/// discovery does (honoring `HF_HOME` / `HF_HUB_CACHE`) and enumerate them.
/// `home` is the resolved home dir (production passes `dirs::home_dir`).
pub fn enumerate_hf_cache(home: Option<&Path>) -> Vec<HfRepoCandidate> {
  let roots = crate::util::model_caches::huggingface_hub_dirs(home);
  enumerate_repos(&roots)
}

fn enumerate_root(root: &Path, out: &mut Vec<HfRepoCandidate>) {
  let Ok(entries) = std::fs::read_dir(root) else {
    // Missing / inaccessible root contributes nothing (edge: empty cache).
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
      continue;
    };
    let Some(repo_id) = repo_id_from_cache_dir(name) else {
      continue;
    };
    let Some(snapshot) = resolve_snapshot_dir(&path) else {
      continue;
    };
    let class = classify_snapshot(&snapshot);
    // Safetensors present, GGUF absent: the GGUF scanner owns mixed repos.
    if class.has_safetensors && !class.has_gguf {
      out.push(HfRepoCandidate {
        repo_id,
        config_summary: parse_config_summary(&snapshot),
        snapshot_path: snapshot,
        has_safetensors: class.has_safetensors,
        has_gguf: class.has_gguf,
      });
    }
  }
}

/// Reconstruct `owner/name` from a `models--owner--name` cache dir.
///
/// HF encodes a repo id by replacing `/` with `--`; a repo id has exactly one
/// `/`, so the owner is the segment before the first `--` and the name is the
/// rest verbatim (preserving any `--` a repo name itself contains).
fn repo_id_from_cache_dir(name: &str) -> Option<String> {
  let rest = name.strip_prefix("models--")?;
  let (owner, repo) = rest.split_once("--")?;
  if owner.is_empty() || repo.is_empty() {
    return None;
  }
  Some(format!("{owner}/{repo}"))
}

/// Resolve the snapshot directory for a repo: prefer the revision `refs/main`
/// points at, else the first snapshot dir (sorted, for determinism).
fn resolve_snapshot_dir(repo_dir: &Path) -> Option<PathBuf> {
  let snapshots = repo_dir.join("snapshots");
  if let Ok(hash) = std::fs::read_to_string(repo_dir.join("refs/main")) {
    let hash = hash.trim();
    if !hash.is_empty() {
      let pinned = snapshots.join(hash);
      if pinned.is_dir() {
        return Some(pinned);
      }
    }
  }
  let mut dirs: Vec<PathBuf> = std::fs::read_dir(&snapshots)
    .ok()?
    .flatten()
    .map(|e| e.path())
    .filter(|p| p.is_dir())
    .collect();
  dirs.sort();
  dirs.into_iter().next()
}

struct Classification {
  has_safetensors: bool,
  has_gguf: bool,
}

/// Classify a snapshot's top-level files by extension. Symlink entries (the
/// HF blob layout) classify by their link name, which is correct.
fn classify_snapshot(snapshot: &Path) -> Classification {
  let mut has_safetensors = false;
  let mut has_gguf = false;
  if let Ok(rd) = std::fs::read_dir(snapshot) {
    for e in rd.flatten() {
      match e.path().extension().and_then(|s| s.to_str()) {
        Some("safetensors") => has_safetensors = true,
        Some("gguf") => has_gguf = true,
        _ => {}
      }
    }
  }
  Classification {
    has_safetensors,
    has_gguf,
  }
}

/// Parse `config.json` (+ optional `tokenizer_config.json` overlay) into a
/// [`ConfigSummary`]. Returns `None` only when `config.json` is missing or
/// unparseable — the tokenizer file is a pure overlay, so its absence just
/// leaves `chat_template` / `tokenizer_class` `None`.
fn parse_config_summary(snapshot: &Path) -> Option<ConfigSummary> {
  let config = read_json(&snapshot.join("config.json"))?;
  let mut s = ConfigSummary {
    model_type: config.get("model_type").and_then(json_str),
    architectures: config
      .get("architectures")
      .and_then(|v| v.as_array())
      .map(|a| {
        a.iter()
          .filter_map(|x| x.as_str().map(String::from))
          .collect()
      })
      .unwrap_or_default(),
    max_position_embeddings: config.get("max_position_embeddings").and_then(json_u64),
    hidden_size: config.get("hidden_size").and_then(json_u64),
    num_hidden_layers: config.get("num_hidden_layers").and_then(json_u64),
    vocab_size: config.get("vocab_size").and_then(json_u64),
    intermediate_size: config.get("intermediate_size").and_then(json_u64),
    has_quantization: config
      .get("quantization")
      .map(|v| !v.is_null())
      .unwrap_or(false),
    chat_template: None,
    tokenizer_class: None,
  };
  if let Some(tok) = read_json(&snapshot.join("tokenizer_config.json")) {
    // `chat_template` is usually a string; the list-of-named-templates form
    // is rare and left to the leaf — take only the string shape here.
    s.chat_template = tok.get("chat_template").and_then(json_str);
    s.tokenizer_class = tok.get("tokenizer_class").and_then(json_str);
  }
  Some(s)
}

/// Map a [`ConfigSummary`] to a [`ModelMetadata`], filling the generic fields
/// every safetensors/HF-format engine shares. Quant is left `Unknown(0)` /
/// `quant_label: None` — the leaf overlays engine-specific quant in its
/// projection. `weights_bytes` is left `None` (the leaf sizes the snapshot).
pub fn config_to_metadata(summary: &ConfigSummary, repo_id: &str) -> ModelMetadata {
  let mode_hint = mode_hint_from_config(&summary.architectures, summary.chat_template.as_deref());
  let reasoning_hint = summary
    .chat_template
    .as_deref()
    .map(|t| t.contains("<think>"))
    .unwrap_or(false);
  let (total_parameters, parameter_label) = match estimate_params(summary) {
    Some(n) => (Some(n), label_for_param_count(n)),
    None => (None, param_label_from_repo_name(repo_id)),
  };
  ModelMetadata {
    arch: summary.model_type.clone(),
    total_parameters,
    parameter_label,
    // GGML quant tag does not apply to a non-GGUF repo.
    quant: Quant::Unknown(0),
    // The engine leaf overlays an affine quant label where applicable.
    quant_label: None,
    native_ctx: summary.max_position_embeddings,
    chat_template: summary.chat_template.clone(),
    tokenizer_kind: summary.tokenizer_class.clone(),
    reasoning_hint,
    mode_hint,
    // The leaf sums `*.safetensors` file sizes for the SIZE column.
    weights_bytes: None,
  }
}

/// Infer the mode hint from the architectures class list, falling back to the
/// presence of a chat template. Embedding/rerank classification for
/// safetensors repos is deferred to the leaf (no consumer needs it yet).
fn mode_hint_from_config(architectures: &[String], chat_template: Option<&str>) -> ModeHint {
  let any = |needle: &str| {
    architectures
      .iter()
      .any(|a| a.to_ascii_lowercase().contains(needle))
  };
  // A decoder-only LM class, or any model shipping a chat template, is chat.
  let is_causal = any("forcausallm") || any("lmheadmodel") || any("forconditionalgeneration");
  if is_causal || chat_template.is_some() {
    ModeHint::Chat
  } else {
    ModeHint::Unknown
  }
}

/// Quant-independent parameter estimate from config dims. Rough by design
/// (exact de-packing of quantized tensors is deferred): embedding +
/// per-layer (attention `4·h²` + gated MLP `3·h·intermediate`). Requires at
/// least `hidden_size` and `num_hidden_layers`.
fn estimate_params(s: &ConfigSummary) -> Option<u64> {
  let h = s.hidden_size?;
  let layers = s.num_hidden_layers?;
  let vocab = s.vocab_size.unwrap_or(0);
  let inter = s
    .intermediate_size
    .unwrap_or_else(|| 4u64.saturating_mul(h));
  let embeddings = vocab.saturating_mul(h);
  let attn = 4u64.saturating_mul(h).saturating_mul(h);
  let mlp = 3u64.saturating_mul(h).saturating_mul(inter);
  let per_layer = attn.saturating_add(mlp);
  Some(embeddings.saturating_add(layers.saturating_mul(per_layer)))
}

/// Pull a `7B` / `0.5B`-style label out of the repo name (e.g.
/// `owner/Llama-3.2-3B-Instruct` → `3B`). Used only when config dims are
/// absent. MoE / multiplied labels (`8x7B`) don't parse → `None`.
fn param_label_from_repo_name(repo_id: &str) -> Option<String> {
  let name = repo_id.rsplit('/').next().unwrap_or(repo_id);
  name
    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
    .find_map(parse_b_token)
}

fn parse_b_token(tok: &str) -> Option<String> {
  let num = tok.to_ascii_lowercase();
  let num = num.strip_suffix('b')?;
  if num.is_empty() {
    return None;
  }
  let mut seen_dot = false;
  for c in num.chars() {
    match c {
      '.' if !seen_dot => seen_dot = true,
      '0'..='9' => {}
      _ => return None,
    }
  }
  Some(format!("{num}B"))
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
  let bytes = std::fs::read(path).ok()?;
  serde_json::from_slice(&bytes).ok()
}

fn json_str(v: &serde_json::Value) -> Option<String> {
  v.as_str().map(String::from)
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
  v.as_u64()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  /// Build a `models--owner--repo/snapshots/<rev>/` tree under `root` with
  /// the given top-level files. Returns the snapshot dir.
  fn make_repo(root: &Path, dir_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let repo = root.join(dir_name);
    let snap = repo.join("snapshots/abc123");
    fs::create_dir_all(&snap).unwrap();
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::write(repo.join("refs/main"), "abc123").unwrap();
    for (name, body) in files {
      fs::write(snap.join(name), body).unwrap();
    }
    snap
  }

  fn temp_root(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
      "llamastash-hf-repos-{label}-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
  }

  #[test]
  fn happy_path_one_safetensors_repo_yields_one_candidate() {
    let root = temp_root("happy");
    let snap = make_repo(
      &root,
      "models--mlx-community--Qwen2.5-3B",
      &[
        ("config.json", r#"{"model_type":"qwen2"}"#),
        ("model.safetensors", "weights"),
      ],
    );
    let got = enumerate_repos(std::slice::from_ref(&root));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].repo_id, "mlx-community/Qwen2.5-3B");
    assert_eq!(got[0].snapshot_path, snap);
    assert!(got[0].has_safetensors);
    assert!(!got[0].has_gguf);
    assert_eq!(
      got[0]
        .config_summary
        .as_ref()
        .unwrap()
        .model_type
        .as_deref(),
      Some("qwen2")
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn gguf_only_repo_yields_no_candidate() {
    let root = temp_root("gguf-only");
    make_repo(
      &root,
      "models--TheBloke--Foo-GGUF",
      &[("model.gguf", "gguf"), ("config.json", "{}")],
    );
    assert!(enumerate_repos(std::slice::from_ref(&root)).is_empty());
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn mixed_gguf_and_safetensors_repo_is_skipped() {
    // Intentional: the GGUF scanner owns any repo carrying a `.gguf`.
    let root = temp_root("mixed");
    make_repo(
      &root,
      "models--org--Mixed",
      &[
        ("config.json", "{}"),
        ("model.safetensors", "st"),
        ("model.gguf", "gguf"),
      ],
    );
    assert!(enumerate_repos(std::slice::from_ref(&root)).is_empty());
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn missing_config_still_emits_candidate_with_none_summary() {
    let root = temp_root("no-config");
    make_repo(
      &root,
      "models--org--NoConfig",
      &[("model.safetensors", "st")],
    );
    let got = enumerate_repos(std::slice::from_ref(&root));
    assert_eq!(got.len(), 1);
    assert!(
      got[0].config_summary.is_none(),
      "missing config.json → None summary"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn unparseable_config_still_emits_candidate_with_none_summary() {
    let root = temp_root("bad-config");
    make_repo(
      &root,
      "models--org--BadConfig",
      &[("config.json", "{not json"), ("model.safetensors", "st")],
    );
    let got = enumerate_repos(std::slice::from_ref(&root));
    assert_eq!(got.len(), 1);
    assert!(
      got[0].config_summary.is_none(),
      "unparseable config.json → None summary"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn empty_or_missing_root_yields_empty_vec() {
    // Missing root: no panic, empty result.
    assert!(enumerate_repos(&[PathBuf::from("/no/such/llamastash/root")]).is_empty());
    // Existing-but-empty root.
    let root = temp_root("empty");
    assert!(enumerate_repos(std::slice::from_ref(&root)).is_empty());
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn config_to_metadata_fills_generic_fields() {
    let root = temp_root("meta-happy");
    make_repo(
      &root,
      "models--mlx-community--Qwen2.5-3B-Instruct",
      &[
        (
          "config.json",
          r#"{"model_type":"qwen2","max_position_embeddings":32768,
              "architectures":["Qwen2ForCausalLM"]}"#,
        ),
        (
          "tokenizer_config.json",
          r#"{"chat_template":"{{ x }}","tokenizer_class":"Qwen2Tokenizer"}"#,
        ),
        ("model.safetensors", "st"),
      ],
    );
    let cand = &enumerate_repos(std::slice::from_ref(&root))[0];
    let m = config_to_metadata(cand.config_summary.as_ref().unwrap(), &cand.repo_id);
    assert_eq!(m.arch.as_deref(), Some("qwen2"));
    assert_eq!(m.native_ctx, Some(32768));
    assert_eq!(m.chat_template.as_deref(), Some("{{ x }}"));
    assert_eq!(m.tokenizer_kind.as_deref(), Some("Qwen2Tokenizer"));
    assert_eq!(m.mode_hint, ModeHint::Chat);
    // The shared helper never sets quant — that's the leaf's job.
    assert_eq!(m.quant, Quant::Unknown(0));
    assert!(m.quant_label.is_none());
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn config_to_metadata_degrades_when_tokenizer_config_absent() {
    // No tokenizer_config.json: chat_template None, config.json fields stay.
    let summary = ConfigSummary {
      model_type: Some("llama".into()),
      architectures: vec!["LlamaForCausalLM".into()],
      max_position_embeddings: Some(4096),
      ..Default::default()
    };
    let m = config_to_metadata(&summary, "meta-llama/Llama-3.2-1B");
    assert_eq!(m.arch.as_deref(), Some("llama"));
    assert_eq!(m.native_ctx, Some(4096));
    assert!(m.chat_template.is_none());
    assert_eq!(
      m.mode_hint,
      ModeHint::Chat,
      "ForCausalLM → chat without a template"
    );
  }

  #[test]
  fn param_estimate_from_config_dims_renders_familiar_label() {
    // Dims chosen to land near 3B: embeddings + 36 layers. The shared
    // `label_for_param_count` renders the precise figure (~3.3B), not a bucket.
    let summary = ConfigSummary {
      hidden_size: Some(2048),
      num_hidden_layers: Some(36),
      vocab_size: Some(151_936),
      intermediate_size: Some(11_008),
      ..Default::default()
    };
    let m = config_to_metadata(&summary, "mlx-community/Anonymous");
    assert!(m.total_parameters.is_some());
    assert_eq!(m.parameter_label.as_deref(), Some("3.3B"));
    // Helper still leaves quant unset.
    assert!(m.quant_label.is_none());
  }

  #[test]
  fn param_label_falls_back_to_repo_name_when_dims_absent() {
    let summary = ConfigSummary {
      model_type: Some("llama".into()),
      ..Default::default()
    };
    let m = config_to_metadata(&summary, "mlx-community/Llama-3.2-3B-Instruct");
    assert!(m.total_parameters.is_none(), "no dims → no exact count");
    assert_eq!(m.parameter_label.as_deref(), Some("3B"));

    // A sub-1B label round-trips its decimal.
    let half = config_to_metadata(&summary, "mlx-community/Qwen2.5-0.5B");
    assert_eq!(half.parameter_label.as_deref(), Some("0.5B"));
  }

  #[test]
  fn repo_id_reverse_preserves_double_dash_in_repo_name() {
    // A repo name that itself contains `--` keeps it (split on first `--`).
    assert_eq!(
      repo_id_from_cache_dir("models--org--foo--bar"),
      Some("org/foo--bar".to_string())
    );
    assert_eq!(repo_id_from_cache_dir("not-a-models-dir"), None);
    assert_eq!(repo_id_from_cache_dir("models--onlyowner"), None);
  }

  #[test]
  fn module_references_no_backend_symbols() {
    // Neutrality is the deliverable: the enumerator's production code must
    // name no engine / backend symbol. Scope to the pre-test portion so a
    // realistic sample org name in a test fixture (legitimate *data*) can't
    // false-trip the check; needles are split so it can't match itself.
    let full = include_str!("hf_repos.rs");
    let prod = full.split(concat!("#[cfg", "(test)]")).next().unwrap();
    let lower = prod.to_ascii_lowercase();
    let forbidden: &[&str] = &[
      concat!("crate::", "backend"),
      concat!("Back", "ends"),
      concat!("m", "lx"),
      concat!("v", "llm"),
    ];
    for n in forbidden {
      assert!(
        !lower.contains(&n.to_ascii_lowercase()),
        "neutrality violation: production code names a forbidden engine symbol (id {})",
        n.len(),
      );
    }
  }
}
