//! Model-catalog row type and the fuzzy reference matcher.
//!
//! `clap`/IPC-free so both the CLI (`crate::cli::resolve`, which adds the
//! IPC fetch + `CliExit` mapping on top) and the HTTP proxy depend *down*
//! on one matcher instead of the proxy importing "up" into `cli`.
//!
//! Model references accept an absolute path (matched verbatim against the
//! canonical path), an exact file name, or a case-insensitive substring
//! of the file name or parent directory.
//!
//! `CatalogRow` is also the **single source of truth for the `list_models`
//! wire shape**: its hand-written `Serialize`/`Deserialize` (the private
//! `to_wire_value` / `from_wire_value` pair) is the one place that nested JSON
//! shape is defined. The daemon builds a row from its `DiscoveredModel` and
//! serialises it; the CLI and TUI deserialise it. No second serializer.

use serde::de::{Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};
use serde_json::{json, Value};

use crate::discovery::Multimodal;

/// MTP (multi-token prediction) speculative-decoding capability of a catalog
/// row. `embedded_layers` is the in-file draft-head count
/// (`{arch}.nextn_predict_layers`); `separate_head` is true when a
/// `mtp-*.gguf` drafter sibling was found. `None` on the row ⇒ not capable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MtpCapability {
  pub embedded_layers: Option<u32>,
  pub separate_head: bool,
}

/// One row from `list_models`. Lean wrapper kept independent of the
/// catalog's internal `DiscoveredModel` shape so the resolver stays
/// transport-agnostic. Flat fields for ergonomic access; the nested wire
/// shape lives in the serde impls below.
#[derive(Debug, Clone)]
pub struct CatalogRow {
  /// Canonical absolute path to the launchable file (or shard 1).
  pub path: String,
  /// Short BLAKE3-derived canonical id (8 hex chars). Optional
  /// because the daemon's catalog computes it lazily — pre-launch
  /// rows omit it.
  pub model_id: Option<String>,
  pub parent: String,
  pub source: String,
  pub arch: Option<String>,
  pub quant: Option<String>,
  pub native_ctx: Option<u64>,
  pub mode_hint: Option<String>,
  pub parameter_label: Option<String>,
  /// GGUF weights footprint (sum of per-tensor storage bytes). `None`
  /// when the file is metadata-only or the header parse failed. Used
  /// by `list_human` for the SIZE column.
  pub weights_bytes: Option<u64>,
  /// Source-supplied human label preferred over the path's basename
  /// when set. Currently populated only for Ollama rows, where the
  /// content-addressed blob filename (`sha256-<hex>`) is hostile to
  /// scanning by eye.
  pub display_label: Option<String>,
  pub parse_error: Option<String>,
  /// Sibling shard paths for split GGUFs. Empty for single-file
  /// models. `path` is always shard 1; this carries shards 2..N so
  /// callers (`show`, future size aggregators) can compute the
  /// on-disk total without re-scanning the parent dir.
  pub split_siblings: Vec<String>,
  /// `true` when the GGUF header carried a `tokenizer.chat_template`
  /// string. Surfacing the boolean (not the full template) keeps the
  /// `list_models` wire shape lean; the template body is large.
  pub has_chat_template: bool,
  /// `true` when the GGUF carried a reasoning hint. Mirrors the
  /// `metadata.has_reasoning_hint` field on `list_models`.
  pub has_reasoning_hint: bool,
  /// `tokenizer.ggml.model` from the GGUF header (`"llama"`, `"qwen2"`).
  pub tokenizer_kind: Option<String>,
  /// `general.parameter_count` — the raw count behind
  /// `parameter_label` (`"7B"` is derived from `7e9`).
  pub total_parameters: Option<u64>,
  /// Backend that serves this row, as the daemon resolved it (`list_models`
  /// `backend` field): `"llamacpp"` / `"lemonade"` / `"ds4"`. The honest R14
  /// badge — `"ds4"` only when the file is ds4-compatible *and* ds4 is
  /// available. `None` on rows the daemon didn't tag (falls back to a
  /// source-derived badge in `list_json`).
  pub backend: Option<String>,
  /// Every backend that can serve this model, priority-ordered (first =
  /// default). Drives the `list` "backend" column + right-pane badges, which
  /// render all of them (clipping the column if it overflows). Empty on rows the
  /// daemon didn't tag (registry sources, parse failures).
  pub supported_backends: Vec<String>,
  /// Multimodal projector capability (vision / audio), or `None` when the model
  /// has no mmproj companion. Drives the TUI title glyph.
  pub multimodal: Option<Multimodal>,
  /// MTP speculative-decoding capability, or `None` when not capable. Drives
  /// the TUI `↯` badge + the launch picker's MTP row.
  pub mtp: Option<MtpCapability>,
}

impl CatalogRow {
  /// Build the nested `list_models` wire `Value` from the flat fields. The
  /// single definition of the JSON shape — `Serialize` and the daemon's
  /// `list_models` response both go through here.
  fn to_wire_value(&self) -> Value {
    // The daemon sets `mode_hint` (and every other block field) exactly when a
    // header parsed, so its presence tracks "has metadata block" faithfully —
    // no separate presence flag needed.
    let has_metadata = self.mode_hint.is_some()
      || self.arch.is_some()
      || self.quant.is_some()
      || self.native_ctx.is_some()
      || self.parameter_label.is_some()
      || self.weights_bytes.is_some()
      || self.total_parameters.is_some()
      || self.tokenizer_kind.is_some()
      || self.has_chat_template
      || self.has_reasoning_hint;
    let metadata = has_metadata.then(|| {
      json!({
        "arch": self.arch,
        "total_parameters": self.total_parameters,
        "parameter_label": self.parameter_label,
        "quant": self.quant,
        "native_ctx": self.native_ctx,
        "tokenizer_kind": self.tokenizer_kind,
        "mode_hint": self.mode_hint,
        "has_reasoning_hint": self.has_reasoning_hint,
        "has_chat_template": self.has_chat_template,
        "weights_bytes": self.weights_bytes,
      })
    });
    let mut row = json!({
      // Friendly label (display_label or path basename). Additive to the raw
      // IPC shape so CLI `--json` consumers keep a `name` field.
      "name": self.name(),
      // Repo-shaped location (`unsloth/Qwen3.8-27B-GGUF`), derived like
      // `name` rather than stored. Empty string, not null, when the row has
      // none — the `list` REPO column renders it verbatim.
      "repo": self.group_label(),
      "path": self.path,
      "parent": self.parent,
      "source": self.source,
      "backend": self.backend,
      "supported_backends": self.supported_backends,
      "split_siblings": self.split_siblings,
      "metadata": metadata,
      "parse_error": self.parse_error,
      "display_label": self.display_label,
      "multimodal": self.multimodal.map(|mm| json!({
        "vision": mm.vision,
        "audio": mm.audio,
      })),
      "mtp": self.mtp.map(|m| json!({
        "embedded_layers": m.embedded_layers,
        "separate_head": m.separate_head,
      })),
    });
    // `model_id` is emitted only when populated — a `null` would mislead
    // agents into thinking a stable handle exists.
    if let Some(id) = &self.model_id {
      row["model_id"] = Value::String(id.clone());
    }
    row
  }

  /// Parse a `list_models` wire `Value` back into the flat fields. Lenient by
  /// design (missing keys → `None`/empty) so a shape drift degrades a field
  /// rather than dropping the row. Mirror of [`Self::to_wire_value`].
  fn from_wire_value(row: &Value) -> Self {
    let s = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_string);
    let md = row.get("metadata").filter(|m| !m.is_null());
    let ms = |k: &str| {
      md.and_then(|m| m.get(k))
        .and_then(Value::as_str)
        .map(str::to_string)
    };
    let mu = |k: &str| md.and_then(|m| m.get(k)).and_then(Value::as_u64);
    let mb = |k: &str| {
      md.and_then(|m| m.get(k))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    };
    let str_vec = |k: &str| {
      row
        .get(k)
        .and_then(Value::as_array)
        .map(|a| {
          a.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
        })
        .unwrap_or_default()
    };
    let multimodal = row
      .get("multimodal")
      .filter(|v| !v.is_null())
      .map(|mm| Multimodal {
        vision: mm.get("vision").and_then(Value::as_bool).unwrap_or(false),
        audio: mm.get("audio").and_then(Value::as_bool).unwrap_or(false),
      });
    let mtp = row
      .get("mtp")
      .filter(|v| !v.is_null())
      .map(|m| MtpCapability {
        embedded_layers: m
          .get("embedded_layers")
          .and_then(Value::as_u64)
          .and_then(|n| u32::try_from(n).ok()),
        separate_head: m
          .get("separate_head")
          .and_then(Value::as_bool)
          .unwrap_or(false),
      });
    Self {
      path: s("path").unwrap_or_default(),
      model_id: s("model_id"),
      parent: s("parent").unwrap_or_default(),
      source: s("source").unwrap_or_default(),
      arch: ms("arch"),
      quant: ms("quant"),
      native_ctx: mu("native_ctx"),
      mode_hint: ms("mode_hint"),
      parameter_label: ms("parameter_label"),
      weights_bytes: mu("weights_bytes"),
      display_label: s("display_label"),
      parse_error: s("parse_error"),
      split_siblings: str_vec("split_siblings"),
      has_chat_template: mb("has_chat_template"),
      has_reasoning_hint: mb("has_reasoning_hint"),
      tokenizer_kind: ms("tokenizer_kind"),
      total_parameters: mu("total_parameters"),
      backend: s("backend"),
      supported_backends: str_vec("supported_backends"),
      multimodal,
      mtp,
    }
  }
}

impl Serialize for CatalogRow {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    self.to_wire_value().serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for CatalogRow {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(Self::from_wire_value(&value))
  }
}

impl CatalogRow {
  /// Build a row carrying only the fields preset-key classification and
  /// the fuzzy matcher read: `path`, `display_label` (→ [`Self::name`]),
  /// and `arch`. The daemon projects its `DiscoveredModel` catalog into
  /// these for `effective_presets` without rebuilding the full
  /// `list_models` shape; every other field is left empty/`None`.
  pub fn for_resolution(path: String, display_label: Option<String>, arch: Option<String>) -> Self {
    Self {
      path,
      model_id: None,
      parent: String::new(),
      source: String::new(),
      arch,
      quant: None,
      native_ctx: None,
      mode_hint: None,
      parameter_label: None,
      weights_bytes: None,
      display_label,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  /// Friendly label for human matching and table rendering.
  /// `display_label` (Ollama's `<name>:<tag>`) wins when set; falls
  /// back to the path basename.
  pub fn name(&self) -> String {
    if let Some(label) = &self.display_label {
      return label.clone();
    }
    crate::util::paths::model_file_label(std::path::Path::new(&self.path))
  }

  /// The identifier this row answers to on the wire — what `/v1/models`
  /// publishes and what a client sends back as `body.model`. Same rule as
  /// the proxy's projection ([`crate::util::paths::model_public_id`]):
  /// the repo id / `<name>:<tag>` label where the source has one, the file
  /// stem for a plain GGUF. Distinct from [`Self::name`], which keeps the
  /// extension for table rendering.
  pub fn public_id(&self) -> String {
    crate::util::paths::model_public_id(
      std::path::Path::new(&self.path),
      self.display_label.as_deref(),
    )
  }

  /// The repo-qualified form of [`Self::public_id`]
  /// (`unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M`). What the proxy
  /// publishes for a row whose bare id collides with another's, and what
  /// this resolver accepts for every row so that id routes back.
  pub fn qualified_id(&self) -> String {
    crate::util::paths::model_qualified_id(
      std::path::Path::new(&self.path),
      self.display_label.as_deref(),
    )
  }

  /// The source-qualified form of [`Self::qualified_id`]
  /// (`huggingface/unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M`) — the rung
  /// the proxy publishes when one repo is cached by two different tools and
  /// the repo qualifier reads identically for both.
  pub fn source_qualified_id(&self) -> String {
    crate::util::paths::model_source_qualified_id(
      std::path::Path::new(&self.path),
      self.display_label.as_deref(),
      &self.source,
    )
  }

  /// Where the model lives, repo-shaped (`unsloth/Qwen3.8-27B-GGUF`) — the
  /// `list` REPO column and the prefix [`Self::qualified_id`] carries.
  ///
  /// Empty for a source that supplies its own `display_label`: that label
  /// already names the origin, and the synthetic path behind it has no
  /// directory worth showing. Also empty when the path has no parent.
  pub fn group_label(&self) -> String {
    if self.display_label.is_some() {
      return String::new();
    }
    crate::util::paths::model_group_label(std::path::Path::new(&self.path)).unwrap_or_default()
  }

  /// Every string this row answers to as an exact reference, beyond its path
  /// and file name: the two qualified rungs the proxy can publish, each in
  /// both the extensionless spelling `/v1/models` uses and the `.gguf`
  /// spelling `list` shows.
  fn qualified_aliases(&self) -> Vec<String> {
    let mut out = vec![self.qualified_id(), self.source_qualified_id()];
    let group = self.group_label();
    if !group.is_empty() {
      let name = self.name();
      out.push(format!("{group}/{name}"));
      if !self.source.is_empty() {
        out.push(format!("{source}/{group}/{name}", source = self.source));
      }
    }
    out
  }
}

/// Published ids for a whole catalog, aligned with `rows`: unique by
/// construction, so every row is reachable by the id it is listed under.
/// See [`crate::util::paths::published_id_index`] for the escalation rule.
pub fn published_ids(rows: &[CatalogRow]) -> Vec<String> {
  published_ids_for(rows, rows)
}

/// The ids `catalog` publishes `subset` under, aligned with `subset`.
///
/// What an `ambiguous_model` body must list: a candidate's own
/// [`CatalogRow::qualified_id`] can still collide (two quant subdirectories
/// of one repo), and the client is being told to send one of these back, so
/// they have to be resolved against the whole catalog.
pub fn published_ids_for(catalog: &[CatalogRow], subset: &[CatalogRow]) -> Vec<String> {
  let index = crate::util::paths::published_id_index(catalog.iter().map(|r| {
    (
      std::path::Path::new(&r.path),
      r.display_label.as_deref(),
      r.source.as_str(),
    )
  }));
  subset
    .iter()
    .map(|r| {
      index
        .get(&r.path)
        .cloned()
        .unwrap_or_else(|| r.qualified_id())
    })
    .collect()
}

/// Distinguishes the three resolver failure modes the HTTP proxy needs
/// to surface as distinct HTTP responses (and which the CLI folds
/// together into a single `MODEL_NOT_FOUND` exit).
#[derive(Debug, Clone)]
pub enum ResolveError {
  /// Reference was empty after trimming.
  Empty,
  /// Zero candidates matched the reference. Proxy emits 404
  /// `model_not_found`.
  None,
  /// More than one candidate matched. Proxy emits 400
  /// `ambiguous_model` with the candidate list in `matches`.
  Many(Vec<CatalogRow>),
}
/// Collapse a user-supplied model reference that names shard 1 of a split set,
/// preserving whether the caller wrote the extension. `None` when the
/// reference is not a shard name, so callers skip the alias entirely.
pub(crate) fn collapse_shard_reference(needle: &str) -> Option<String> {
  if needle.to_lowercase().ends_with(".gguf") {
    let c = crate::util::paths::model_file_label(std::path::Path::new(needle));
    return (c != needle).then_some(c);
  }
  let c = crate::util::paths::model_file_label(std::path::Path::new(&format!("{needle}.gguf")));
  let stem = c.strip_suffix(".gguf").unwrap_or(&c).to_string();
  (stem != needle).then_some(stem)
}

/// Resolve a model reference, preserving the distinction between "zero
/// candidates" and "many candidates" so callers (the HTTP proxy emits
/// 404 vs 400 with `matches: [...]`) can branch without re-running the
/// substring matcher themselves. The CLI's `resolve_model` wraps this,
/// folding every failure into a single `MODEL_NOT_FOUND` exit.
///
/// Precedence: exact path → exact name → exact published id → repo- or
/// source-qualified id → a split model's pre-rename shard-1 name →
/// case-insensitive substring of name, parent, or qualified id.
pub fn resolve_model_with_candidates(
  rows: &[CatalogRow],
  reference: &str,
) -> Result<CatalogRow, ResolveError> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(ResolveError::Empty);
  }

  // Tier 1: exact path / exact name. A full canonical path is
  // unambiguous by construction.
  let exact_path: Vec<&CatalogRow> = rows.iter().filter(|r| r.path == needle).collect();
  if exact_path.len() == 1 {
    return Ok(exact_path[0].clone());
  }
  let exact_name: Vec<&CatalogRow> = rows.iter().filter(|r| r.name() == needle).collect();
  if exact_name.len() == 1 {
    return Ok(exact_name[0].clone());
  }
  // The plain id `/v1/models` publishes: the file stem, which `name()` above
  // never matches because it keeps the extension. Without this tier a
  // published *unique* id still fell through to the substring tier and fanned
  // out against any longer name containing it — `demo-model` matching
  // `demo-model-Q4_K_M` — so the proxy 400'd `ambiguous_model` on an id it
  // had just published, and listed that same dead id in `matches`.
  let exact_public: Vec<&CatalogRow> = rows
    .iter()
    .filter(|r| r.public_id().eq_ignore_ascii_case(needle))
    .collect();
  if exact_public.len() == 1 {
    return Ok(exact_public[0].clone());
  }
  // The proxy publishes a qualified id for a model whose bare name collides
  // with another row's, so that id has to route back here. Accepted for every
  // row, collision or not, at both qualification rungs and in both the
  // extensionless form `/v1/models` publishes and the filename form `list`
  // shows.
  let qualified: Vec<&CatalogRow> = rows
    .iter()
    .filter(|r| {
      r.qualified_aliases()
        .iter()
        .any(|a| a.eq_ignore_ascii_case(needle))
    })
    .collect();
  match qualified.len() {
    1 => return Ok(qualified[0].clone()),
    // Two rows answering to the same qualified string is the collision the
    // source rung exists for, and the caller has to be told which. Falling
    // through instead reached the substring tier, where the `.gguf` spelling
    // matches nothing — no published id carries the extension — so
    // `unsloth/Demo-GGUF/demo-Q4_K_M.gguf`, the form assembled from `list`'s
    // REPO and NAME columns, came back as "no such model" for a string
    // naming two real ones.
    n if n > 1 => return Err(ResolveError::Many(qualified.into_iter().cloned().collect())),
    _ => {}
  }
  // Split models used to be named after shard 1, and that string is still out
  // there: in 0.2.0 tool configs, saved `body.model` values, and preset keys.
  // Accept it as an alias for the collapsed name so an upgrade does not 404.
  // Both spellings matter — `list` shows the filename, `/v1/models` publishes
  // the stem — and the shard parser only recognises the `.gguf` form.
  if let Some(collapsed) = collapse_shard_reference(needle) {
    let aliased: Vec<&CatalogRow> = rows
      .iter()
      .filter(|r| {
        let name = r.name();
        let stem = std::path::Path::new(&name)
          .file_stem()
          .and_then(|s| s.to_str())
          .unwrap_or(&name)
          .to_string();
        name == collapsed || stem == collapsed
      })
      .collect();
    if aliased.len() == 1 {
      return Ok(aliased[0].clone());
    }
  }

  // Tier 2: case-insensitive substring of name OR parent.
  let lower = needle.to_lowercase();
  let candidates: Vec<&CatalogRow> = rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower)
        || r.parent.to_lowercase().contains(&lower)
        // The source-qualified id, which carries the repo-qualified one as a
        // suffix, so `unsloth/Qwen3.8` matches: the raw parent spells an HF
        // repo `models--unsloth--Qwen3.8-…`, which no reference a user reads
        // off `list` or `/v1/models` looks like.
        || r.source_qualified_id().to_lowercase().contains(&lower)
    })
    .collect();
  match candidates.len() {
    0 => Err(ResolveError::None),
    1 => Ok(candidates[0].clone()),
    _ => Err(ResolveError::Many(
      candidates.into_iter().cloned().collect(),
    )),
  }
}

#[cfg(test)]
mod tests {

  use super::*;

  #[test]
  fn a_split_models_old_shard_name_still_resolves() {
    // 0.2.0 published `…-00001-of-00004` as the model id and wrote preset keys
    // under it. Those strings live in tool configs, so they must keep working
    // after the rename.
    let row = CatalogRow::for_resolution(
      "/m/UD-Q4_K_XL/foo-UD-Q4_K_XL-00001-of-00004.gguf".to_string(),
      None,
      Some("llama".to_string()),
    );
    let rows = vec![row];
    assert_eq!(rows[0].name(), "foo-UD-Q4_K_XL.gguf", "collapsed name");
    for needle in [
      "foo-UD-Q4_K_XL-00001-of-00004.gguf",
      "foo-UD-Q4_K_XL-00001-of-00004",
      "foo-UD-Q4_K_XL.gguf",
    ] {
      assert!(
        resolve_model_with_candidates(&rows, needle).is_ok(),
        "`{needle}` must resolve"
      );
    }
  }

  fn row(path: &str, parent: &str) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: parent.to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: Some("Q4_K".to_string()),
      native_ctx: Some(8192),
      mode_hint: Some("chat".to_string()),
      parameter_label: Some("7B".to_string()),
      weights_bytes: Some(4_200_000_000),
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  #[test]
  fn a_shared_basename_is_reachable_by_its_repo_qualified_id() {
    // The bare name is genuinely ambiguous and stays a `Many`; the qualified
    // id `/v1/models` publishes for each row resolves to exactly that row.
    let rows = vec![
      row("/hf/gemma-4-E2B-it-Q4_K_M.gguf", "/hf"),
      row("/lms/gemma-4-E2B-it-Q4_K_M.gguf", "/lms"),
    ];
    assert!(
      matches!(
        resolve_model_with_candidates(&rows, "gemma-4-E2B-it-Q4_K_M"),
        Err(ResolveError::Many(_))
      ),
      "the bare name still names two models"
    );
    for (needle, want) in [
      ("hf/gemma-4-E2B-it-Q4_K_M", "/hf/gemma-4-E2B-it-Q4_K_M.gguf"),
      (
        "lms/gemma-4-E2B-it-Q4_K_M",
        "/lms/gemma-4-E2B-it-Q4_K_M.gguf",
      ),
      // The filename spelling `list` shows, not just the published stem.
      (
        "lms/gemma-4-E2B-it-Q4_K_M.gguf",
        "/lms/gemma-4-E2B-it-Q4_K_M.gguf",
      ),
    ] {
      let got = resolve_model_with_candidates(&rows, needle)
        .unwrap_or_else(|e| panic!("`{needle}` must resolve; got {e:?}"));
      assert_eq!(got.path, want, "`{needle}`");
    }
  }

  #[test]
  fn a_partial_repo_reference_matches_through_the_qualified_id() {
    // The raw parent spells an HF repo `models--unsloth--…`, which is not
    // what a user reads off `list` or `/v1/models`.
    let rows = vec![row(
      "/c/hub/models--unsloth--Qwen3.8-27B-GGUF/snapshots/1/Qwen3.8-27B-Q4_K_M.gguf",
      "/c/hub/models--unsloth--Qwen3.8-27B-GGUF/snapshots/1",
    )];
    assert!(resolve_model_with_candidates(&rows, "unsloth/Qwen3.8").is_ok());
  }

  #[test]
  fn group_label_is_empty_for_a_source_that_labels_itself() {
    let mut ollama = row("/o/blobs/sha256-abc", "/o/blobs");
    ollama.display_label = Some("llama3:8b".to_string());
    assert_eq!(ollama.group_label(), "");
    assert_eq!(ollama.qualified_id(), "llama3:8b");
    assert_eq!(row("/m/x.gguf", "/m").group_label(), "m");
  }

  #[test]
  fn published_ids_are_unique_across_the_catalog() {
    let rows = vec![
      row("/hf/x.gguf", "/hf"),
      row("/lms/x.gguf", "/lms"),
      row("/m/y.gguf", "/m"),
    ];
    assert_eq!(published_ids(&rows), vec!["hf/x", "lms/x", "y"]);
  }

  /// The invariant the whole published-id scheme exists for, asserted over
  /// one catalog that exercises every rung of the ladder at once. It did not
  /// hold: `demo-model` published as a unique plain id but 400'd
  /// `ambiguous_model` because the substring tier fanned it out against
  /// `demo-model-Q4_K_M`, and the two mirrored copies of one repo published
  /// absolute paths because their repo qualifiers read identically.
  #[test]
  fn every_published_id_resolves_back_to_its_own_row() {
    let mut hf = row(
      "/w/huggingface/hub/models--lms-community--Demo-GGUF/snapshots/1/demo-model-Q4_K_M.gguf",
      "/w/huggingface/hub/models--lms-community--Demo-GGUF/snapshots/1",
    );
    hf.source = "huggingface".to_string();
    let mut lms = row(
      "/w/lmstudio-models/lms-community/Demo-GGUF/demo-model-Q4_K_M.gguf",
      "/w/lmstudio-models/lms-community/Demo-GGUF",
    );
    lms.source = "lm-studio".to_string();
    // Plain-unique, but a substring of both names above.
    let plain = row("/m/demo-model.gguf", "/m");
    // Every rung identical to another row's: only the path is left.
    let mut sub_a = row(
      "/w/huggingface/hub/models--o--r/snapshots/1/UD-Q4_K_XL/w.gguf",
      "/w/huggingface/hub/models--o--r/snapshots/1/UD-Q4_K_XL",
    );
    sub_a.source = "huggingface".to_string();
    let mut sub_b = row(
      "/w/huggingface/hub/models--o--r/snapshots/1/Q8_0/w.gguf",
      "/w/huggingface/hub/models--o--r/snapshots/1/Q8_0",
    );
    sub_b.source = "huggingface".to_string();
    let mut ollama = row("/o/blobs/sha256-abc", "/o/blobs");
    ollama.display_label = Some("llama3:8b".to_string());
    ollama.source = "ollama".to_string();

    let rows = vec![hf, lms, plain, sub_a, sub_b, ollama];
    let ids = published_ids(&rows);
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "published ids collide: {ids:?}");
    for (row, id) in rows.iter().zip(&ids) {
      let got = resolve_model_with_candidates(&rows, id)
        .unwrap_or_else(|e| panic!("published id `{id}` does not resolve: {e:?}"));
      assert_eq!(got.path, row.path, "`{id}` routed to the wrong row");
    }
    // The two mirrored copies read as their source, not as absolute paths.
    assert_eq!(
      ids[0],
      "huggingface/lms-community/Demo-GGUF/demo-model-Q4_K_M"
    );
    assert_eq!(
      ids[1],
      "lm-studio/lms-community/Demo-GGUF/demo-model-Q4_K_M"
    );
    assert_eq!(ids[2], "demo-model");
  }

  /// Both qualification rungs, in both spellings, for a pair no rung below
  /// the source can separate. The `.gguf` spelling of a shared rung used to
  /// come back `None` — a 404 for a string naming two real models, with no
  /// candidates to retry from — because it fell through to the substring
  /// tier and no published id carries the extension.
  #[test]
  fn a_qualified_form_two_rows_share_is_ambiguous_not_missing() {
    let mut hf = row(
      "/w/huggingface/hub/models--lms-community--Demo-GGUF/snapshots/1/demo-model-Q4_K_M.gguf",
      "/w/huggingface/hub/models--lms-community--Demo-GGUF/snapshots/1",
    );
    hf.source = "huggingface".to_string();
    let mut lms = row(
      "/w/lmstudio-models/lms-community/Demo-GGUF/demo-model-Q4_K_M.gguf",
      "/w/lmstudio-models/lms-community/Demo-GGUF",
    );
    lms.source = "lm-studio".to_string();
    let rows = vec![hf, lms];

    // Shared: the repo rung, in the form `/v1/models` would publish and the
    // form assembled from `list`'s REPO and NAME columns.
    for shared in [
      "lms-community/Demo-GGUF/demo-model-Q4_K_M",
      "lms-community/Demo-GGUF/demo-model-Q4_K_M.gguf",
    ] {
      match resolve_model_with_candidates(&rows, shared) {
        Err(ResolveError::Many(c)) => {
          let ids = published_ids_for(&rows, &c);
          assert_eq!(ids.len(), 2, "`{shared}` listed {ids:?}");
          for id in ids {
            resolve_model_with_candidates(&rows, &id)
              .unwrap_or_else(|e| panic!("`{shared}` offered `{id}`, which fails: {e:?}"));
          }
        }
        other => panic!("`{shared}` should be ambiguous, got {other:?}"),
      }
    }

    // Distinct: the source rung, same two spellings.
    for (needle, want) in [
      ("huggingface/lms-community/Demo-GGUF/demo-model-Q4_K_M", 0),
      (
        "lm-studio/lms-community/Demo-GGUF/demo-model-Q4_K_M.gguf",
        1,
      ),
    ] {
      let got = resolve_model_with_candidates(&rows, needle)
        .unwrap_or_else(|e| panic!("`{needle}` does not resolve: {e:?}"));
      assert_eq!(
        got.path, rows[want].path,
        "`{needle}` routed to the wrong row"
      );
    }
  }

  #[test]
  fn with_candidates_returns_many_for_ambiguous() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    match resolve_model_with_candidates(&rows, "qwen-coder") {
      Err(ResolveError::Many(cands)) => assert_eq!(cands.len(), 2),
      other => panic!("expected Many(2); got {other:?}"),
    }
  }

  #[test]
  fn with_candidates_returns_none_for_unmatched() {
    let rows = vec![row("/m/llama.gguf", "/m")];
    match resolve_model_with_candidates(&rows, "phi") {
      Err(ResolveError::None) => {}
      other => panic!("expected None; got {other:?}"),
    }
  }

  #[test]
  fn with_candidates_returns_empty_for_blank_reference() {
    match resolve_model_with_candidates(&[], "   ") {
      Err(ResolveError::Empty) => {}
      other => panic!("expected Empty; got {other:?}"),
    }
  }

  #[test]
  fn exact_path_wins_over_substring_overlap() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let pick = resolve_model_with_candidates(&rows, "/m/qwen-coder-7b.gguf").unwrap();
    assert_eq!(pick.path, "/m/qwen-coder-7b.gguf");
  }

  #[test]
  fn wire_round_trips_nested_shape_and_capability_blocks() {
    // The single source of truth for the `list_models` wire shape: serialize →
    // nested `metadata` + root-level capability blocks; deserialize is the exact
    // mirror (the same parser the daemon, CLI, and TUI all go through).
    let mut r = row("/m/qwen35.gguf", "/m");
    r.mtp = Some(MtpCapability {
      embedded_layers: Some(1),
      separate_head: false,
    });
    r.multimodal = Some(Multimodal {
      vision: true,
      audio: false,
    });
    let v = serde_json::to_value(&r).unwrap();
    // GGUF-derived fields nest under `metadata`; nothing flat at the root.
    assert_eq!(v["metadata"]["arch"], serde_json::json!("llama"));
    assert_eq!(v["metadata"]["native_ctx"], serde_json::json!(8192));
    assert!(v.get("arch").is_none(), "arch is nested, not top-level");
    // Capability blocks live at the root.
    assert_eq!(v["mtp"]["embedded_layers"], serde_json::json!(1));
    assert_eq!(v["mtp"]["separate_head"], serde_json::json!(false));
    assert_eq!(v["multimodal"]["vision"], serde_json::json!(true));
    assert_eq!(v["name"], serde_json::json!("qwen35.gguf"));

    let back: CatalogRow = serde_json::from_value(v).unwrap();
    assert_eq!(back.arch.as_deref(), Some("llama"));
    assert_eq!(back.native_ctx, Some(8192));
    assert_eq!(back.mtp.and_then(|m| m.embedded_layers), Some(1));
    assert_eq!(
      back.multimodal,
      Some(Multimodal {
        vision: true,
        audio: false
      })
    );
  }

  #[test]
  fn wire_omits_metadata_block_and_model_id_when_absent() {
    let mut r = row("/m/x.gguf", "/m");
    // A parse failure carries no metadata signals.
    r.arch = None;
    r.quant = None;
    r.native_ctx = None;
    r.mode_hint = None;
    r.parameter_label = None;
    r.weights_bytes = None;
    r.total_parameters = None;
    r.tokenizer_kind = None;
    r.has_chat_template = false;
    r.has_reasoning_hint = false;
    r.model_id = None;
    let v = serde_json::to_value(&r).unwrap();
    assert!(
      v["metadata"].is_null(),
      "no metadata block when nothing parsed"
    );
    assert!(
      v.get("model_id").is_none(),
      "model_id omitted (not null) when None"
    );
    assert!(v["mtp"].is_null());
    assert!(v["multimodal"].is_null());
  }
}
