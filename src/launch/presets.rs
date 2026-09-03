//! Named launch presets per model (`R21`).
//!
//! The `config.yaml` `presets:` blocks materialize into [`NamedPreset`] values.
//! A preset's `params` is a full [`LaunchParams`] snapshot so applying one is
//! just "clone these params, then layer per-invocation overrides on top".

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{ConfigPresetBlock, PresetBody};
use crate::launch::knobs::{Concept, KnobValue, Scalar};
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;
use crate::launch::resolve::CatalogRow;

/// One saved preset. `name` is unique within a single model's preset
/// list and is the handle users type at the CLI / TUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedPreset {
  pub name: String,
  pub params: LaunchParams,
}

/// Per-model preset list. Wrapper around `Vec<NamedPreset>` so the
/// state_store API stays clear about insertion / removal semantics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Presets {
  entries: Vec<NamedPreset>,
}

impl Presets {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn iter(&self) -> std::slice::Iter<'_, NamedPreset> {
    self.entries.iter()
  }

  /// Insert or replace a preset by name. Returns the previous entry
  /// (if any) so callers can surface "renamed" / "overwrote" messages.
  pub fn upsert(&mut self, preset: NamedPreset) -> Option<NamedPreset> {
    if let Some(existing) = self.entries.iter_mut().find(|p| p.name == preset.name) {
      let old = std::mem::replace(existing, preset);
      Some(old)
    } else {
      self.entries.push(preset);
      None
    }
  }

  /// Remove a preset by name. Returns the entry if it existed.
  pub fn remove(&mut self, name: &str) -> Option<NamedPreset> {
    let idx = self.entries.iter().position(|p| p.name == name)?;
    Some(self.entries.remove(idx))
  }

  pub fn get(&self, name: &str) -> Option<&NamedPreset> {
    self.entries.iter().find(|p| p.name == name)
  }
}

/// Fold a launch's settings into the config-layer [`PresetBody`]. `ctx`
/// and `reasoning` live inside [`crate::launch::knobs::KnobSet`] at the config
/// layer (the flat `ctx:` / `reasoning:` keys), so a pinned ctx becomes
/// `Set` and an `--ctx auto` keeps its `Auto`; `mode` is stored only when
/// it differs from the default `Chat`, and `extras` only when non-empty.
/// The inverse of [`materialize_preset`].
pub fn preset_body_from_launch_params(params: &LaunchParams) -> PresetBody {
  let mut knobs = params.knobs.clone();
  // `ctx` / `reasoning` / `mode` are still `LaunchParams` siblings on the
  // wire; fold them into the knob map so a preset stores one shape.
  let backend_id = params
    .backend
    .explicit_id()
    .unwrap_or(crate::backend::DEFAULT_BACKEND_ID);
  if let Some(c) = params.ctx {
    knobs.set_by_concept(
      backend_id,
      Concept::ContextLength,
      KnobValue::Set(Scalar::U32(c)),
    );
  }
  if params.reasoning {
    knobs.set_by_name("reasoning", "true");
  }
  if params.mode != LaunchMode::Chat {
    knobs.set_by_concept(
      backend_id,
      Concept::Mode,
      KnobValue::Set(Scalar::Str(params.mode.label().to_string())),
    );
  }
  if !matches!(params.mtp, crate::launch::params::MtpEnable::Auto) {
    knobs.set_by_name("mtp", params.mtp.label());
  }
  if let Some(n) = params.mtp_draft_n {
    knobs.set_by_name("mtp-draft-n", n.to_string());
  }
  let extras: Vec<String> = params
    .extras
    .iter()
    .map(|s| s.to_string_lossy().into_owned())
    .collect();
  PresetBody {
    knobs,
    extras: (!extras.is_empty()).then_some(extras),
    backend: params.backend.explicit_id().map(str::to_string),
    server: params.server.clone(),
  }
}

/// Materialise a stored [`PresetBody`] back into a [`NamedPreset`] over
/// `model_path`.
///
/// `ctx` / `reasoning` / `mode` / `mtp` are projected back out of the knob map
/// onto their [`LaunchParams`] siblings so the IPC and CLI wire shapes are
/// unchanged. An `Auto` ctx stays in the knob, so `--fit` still governs it.
/// The inverse of [`preset_body_from_launch_params`].
pub fn materialize_preset(name: &str, body: &PresetBody, model_path: PathBuf) -> NamedPreset {
  let mut knobs = body.knobs.clone();
  let backend = body
    .backend
    .as_deref()
    .map(crate::launch::params::BackendChoice::from_id)
    .unwrap_or_default();
  let backend_id = backend
    .explicit_id()
    .unwrap_or(crate::backend::DEFAULT_BACKEND_ID);

  let mode = knobs
    .str_by_concept(backend_id, Concept::Mode)
    .and_then(LaunchMode::from_label)
    .unwrap_or(LaunchMode::Chat);
  if let Some(def) = crate::launch::knobs::def_for_backend_concept(backend_id, Concept::Mode) {
    knobs.clear(def.knob_id());
  }

  // Only a pinned ctx moves out; an Auto ctx belongs to the fitter.
  let ctx = knobs.u32_by_concept(backend_id, Concept::ContextLength);
  if ctx.is_some() {
    if let Some(def) =
      crate::launch::knobs::def_for_backend_concept(backend_id, Concept::ContextLength)
    {
      knobs.clear(def.knob_id());
    }
  }
  let reasoning = knobs
    .get_by_name("reasoning")
    .and_then(|v| v.set_value())
    .and_then(|s| s.as_bool())
    .unwrap_or(false);
  knobs.remove_by_name("reasoning");

  let mtp = knobs
    .text_by_name("mtp")
    .and_then(|v| crate::launch::params::MtpEnable::from_token(&v))
    .unwrap_or_default();
  let mtp_draft_n = knobs
    .get_by_name("mtp-draft-n")
    .and_then(|v| v.set_value())
    .and_then(|s| s.as_u32());

  let mut params = LaunchParams::new(model_path, mode);
  params.ctx = ctx;
  params.reasoning = reasoning;
  params.knobs = knobs;
  params.extras = body
    .extras
    .clone()
    .unwrap_or_default()
    .into_iter()
    .map(OsString::from)
    .collect();
  params.backend = backend;
  params.server = body.server.clone();
  params.mtp = mtp;
  params.mtp_draft_n = mtp_draft_n;
  NamedPreset {
    name: name.to_string(),
    params,
  }
}

/// How a config `presets:` key resolves against the live model catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyClass {
  /// Names exactly one discovered model — a per-model key (carries the
  /// matched model's canonical path).
  Model { path: String },
  /// Matches no discovered model — read as a GGUF `general.architecture`
  /// id that applies to every model of that arch.
  Arch,
  /// Names more than one discovered model (a shared basename). Read as a
  /// per-model key applying to all of them — never as an arch id. Only used
  /// to keep such a key out of the arch layer.
  Ambiguous,
}

/// Classify a config `presets:` key against the live catalog. Matching is
/// **exact** (case-insensitive basename, or exact canonical path), never a
/// fuzzy substring, so an arch id like `qwen2` is not accidentally
/// captured by a model file named `qwen2.5-7b.gguf`: a key naming exactly
/// one model is per-model, one naming none is an arch id, one naming
/// several is ambiguous.
pub fn classify_preset_key(key: &str, catalog: &[CatalogRow]) -> KeyClass {
  if let Some(row) = catalog.iter().find(|r| r.path == key) {
    return KeyClass::Model {
      path: row.path.clone(),
    };
  }
  // A key written before split models were named after their shard set still
  // reads `…-00001-of-00004.gguf`; collapse it so those presets keep applying.
  // Same helper the reference resolver uses, so the two cannot drift.
  let key_collapsed =
    crate::launch::resolve::collapse_shard_reference(key).unwrap_or_else(|| key.to_string());
  let mut named = catalog.iter().filter(|r| {
    r.name().eq_ignore_ascii_case(key) || r.name().eq_ignore_ascii_case(&key_collapsed)
  });
  match (named.next(), named.next()) {
    (Some(row), None) => KeyClass::Model {
      path: row.path.clone(),
    },
    (Some(_), Some(_)) => KeyClass::Ambiguous,
    (None, _) => KeyClass::Arch,
  }
}

/// The reserved `default:` value meaning "launch pure-fit by default"
/// (skip the default-preset + last_params layers). Mirrors the reserved
/// `auto` knob value. Any other `default:` value names a preset.
pub const AUTO_DEFAULT: &str = "auto";

/// A model's resolved preset set plus its default selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectivePresets {
  pub presets: Presets,
  /// The model's `default:` selection — per-model `default` else arch
  /// `default`. Either the reserved [`AUTO_DEFAULT`] sentinel, or a name
  /// kept only when it matches a present entry (an absent name resolves to
  /// `None`). Drives both the TUI cycle's opening stop and the server-side
  /// `PresetDefault` resolver layer.
  pub default: Option<String>,
}

impl EffectivePresets {
  /// `true` when `default: auto` — the model launches pure-fit by default.
  pub fn default_is_auto(&self) -> bool {
    self.default.as_deref() == Some(AUTO_DEFAULT)
  }

  /// The default preset's body, when `default:` names a present entry
  /// (not the `auto` sentinel). This is the `PresetDefault` resolver layer
  /// source for a no-selection launch.
  pub fn default_preset(&self) -> Option<&NamedPreset> {
    match self.default.as_deref() {
      Some(d) if d != AUTO_DEFAULT => self.presets.get(d),
      _ => None,
    }
  }
}

/// Resolve a model's effective preset set from the config store: the union
/// of its per-model entries and its arch entries, with per-model winning
/// on a name collision. `model_name` is the model's display name (basename
/// for a local GGUF — the key CLI/TUI saves write under); `model_path` the
/// canonical path; `model_arch` the GGUF `general.architecture` (compared
/// case-insensitively).
///
/// Per-model keys match this model's own name/path **directly**, so a model
/// that isn't currently in the catalog (e.g. started by raw path) still
/// resolves its own presets. The catalog is consulted only for the arch
/// layer — to keep a key that is actually some discovered model's name from
/// being read as a shared arch id.
pub fn effective_presets(
  model_name: &str,
  model_path: &str,
  model_arch: Option<&str>,
  store: &BTreeMap<String, ConfigPresetBlock>,
  catalog: &[CatalogRow],
) -> EffectivePresets {
  // BTreeMap keeps the merged set name-sorted (config entries are an
  // unordered map, so a deterministic order is the right surface).
  let mut merged: BTreeMap<String, NamedPreset> = BTreeMap::new();
  let mut arch_default = None;
  let mut model_default = None;

  // Arch layer first (lower precedence): a key equal to this model's arch
  // that doesn't name any discovered model.
  if let Some(arch) = model_arch {
    for (key, block) in store {
      if key.eq_ignore_ascii_case(arch) && classify_preset_key(key, catalog) == KeyClass::Arch {
        merge_block(&mut merged, block, model_path);
        arch_default = arch_default.or_else(|| block.default.clone());
      }
    }
  }
  // Per-model layer (higher precedence) — a key naming this model, by full
  // path or by basename. A basename key applies to *every* discovered model
  // with that basename: a filename collision in practice means the same GGUF
  // cached in two roots (e.g. the HF cache and the LM Studio cache), so they
  // intentionally share one preset set rather than being disambiguated by
  // path.
  for (key, block) in store {
    if key == model_path || key.eq_ignore_ascii_case(model_name) {
      merge_block(&mut merged, block, model_path);
      model_default = model_default.or_else(|| block.default.clone());
    }
  }

  let mut presets = Presets::new();
  for np in merged.into_values() {
    presets.upsert(np);
  }
  // `auto` is the reserved "pure-fit default" sentinel and is kept verbatim;
  // any other name is kept only when it matches a present entry.
  let default = model_default
    .or(arch_default)
    .filter(|d| d.eq_ignore_ascii_case(AUTO_DEFAULT) || presets.get(d).is_some());
  EffectivePresets { presets, default }
}

fn merge_block(
  merged: &mut BTreeMap<String, NamedPreset>,
  block: &ConfigPresetBlock,
  model_path: &str,
) {
  for (name, body) in &block.entries {
    merged.insert(
      name.clone(),
      materialize_preset(name, body, PathBuf::from(model_path)),
    );
  }
}

#[cfg(test)]
mod tests {
  /// A preset on a backend whose context knob is not the one a backend-blind
  /// lookup matches first still gets its context window.
  ///
  /// `ctx:` in config.yaml resolves before the serving backend is known, so it
  /// lands under whichever knob declares that alias — not necessarily the one
  /// this preset's backend reads. `materialize_preset` then looked for its own
  /// backend's spelling, found nothing, and dropped the window with no warning:
  /// the preset launched at the engine default instead of the size asked for.
  #[test]
  fn a_preset_context_window_survives_whichever_key_it_lands_under() {
    for (backend_id, yaml) in [
      ("ds4", "knobs:\n  ctx: 8192\nbackend: ds4\n"),
      ("llamacpp", "knobs:\n  ctx: 8192\nbackend: llamacpp\n"),
    ] {
      let body: crate::config::PresetBody = yaml_serde::from_str(yaml).expect("parse");
      let np = materialize_preset("p", &body, PathBuf::from("/m/x.gguf"));
      assert_eq!(
        np.params.ctx,
        Some(8192),
        "{backend_id} preset lost its context window; stored under {:?}",
        body
          .knobs
          .iter()
          .map(|(id, _)| id.to_string())
          .collect::<Vec<_>>()
      );
    }
  }

  use super::*;

  use std::path::PathBuf;

  use crate::launch::mode::LaunchMode;

  fn p(name: &str) -> NamedPreset {
    NamedPreset {
      name: name.to_string(),
      params: LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat),
    }
  }

  #[test]
  fn upsert_inserts_then_replaces() {
    let mut store = Presets::new();
    assert!(store.upsert(p("coding")).is_none());
    let mut alt = p("coding");
    alt.params.ctx = Some(32768);
    let prev = store.upsert(alt.clone()).expect("returns previous entry");
    assert!(prev.params.ctx.is_none());
    assert_eq!(store.len(), 1, "no duplicate");
    assert_eq!(store.get("coding"), Some(&alt));
  }

  #[test]
  fn remove_returns_entry_and_none_for_unknown() {
    let mut store = Presets::new();
    store.upsert(p("coding"));
    let removed = store.remove("coding");
    assert!(removed.is_some());
    assert!(store.is_empty());
    assert!(store.remove("nope").is_none());
  }

  #[test]
  fn json_round_trip_preserves_order() {
    let mut store = Presets::new();
    store.upsert(p("a"));
    store.upsert(p("b"));
    let json = serde_json::to_string(&store).unwrap();
    let back: Presets = serde_json::from_str(&json).unwrap();
    assert_eq!(back, store);
    let names: Vec<_> = back.iter().map(|p| p.name.clone()).collect();
    assert_eq!(names, vec!["a", "b"]);
  }

  // ---- materialization / capture / resolution ----

  fn catalog_row(path: &str, arch: &str) -> CatalogRow {
    CatalogRow {
      mtp: None,
      multimodal: None,
      path: path.to_string(),
      model_id: None,
      parent: String::new(),
      source: "user".into(),
      arch: Some(arch.to_string()),
      quant: None,
      native_ctx: None,
      mode_hint: Some("chat".into()),
      parameter_label: None,
      weights_bytes: None,
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
    }
  }

  fn block(entries: &[(&str, PresetBody)], default: Option<&str>) -> ConfigPresetBlock {
    ConfigPresetBlock {
      default: default.map(str::to_string),
      entries: entries
        .iter()
        .map(|(n, b)| (n.to_string(), b.clone()))
        .collect(),
    }
  }

  fn body_ctx(ctx: u32) -> PresetBody {
    PresetBody {
      knobs: crate::knobset! {
        ctx: ctx
      },
      ..PresetBody::default()
    }
  }

  #[test]
  fn materialize_then_capture_round_trips_ctx_reasoning_extras() {
    let body = PresetBody {
      knobs: crate::knobset! {
        ctx: 65536,
        reasoning: true,
        flash_attn: true,
        mode: "embedding"
      },
      extras: Some(vec!["--rope-freq-base".into(), "10000".into()]),
      ..PresetBody::default()
    };
    let np = materialize_preset("p", &body, PathBuf::from("/m/a.gguf"));
    // ctx/reasoning landed on the LaunchParams siblings, out of the knobs.
    assert_eq!(np.params.ctx, Some(65536));
    assert!(np.params.reasoning);
    assert_eq!(np.params.mode, LaunchMode::Embedding);
    assert_eq!(
      np.params.knobs.u32(crate::launch::knobs::kid("ctx-size")),
      None
    );
    assert_eq!(
      np.params.knobs.bool(crate::launch::knobs::kid("reasoning")),
      None
    );
    assert_eq!(
      np.params
        .knobs
        .bool(crate::launch::knobs::kid("flash-attn")),
      Some(true)
    );
    assert_eq!(np.params.extras.len(), 2);
    // Capture is the inverse — back to a flat body with ctx/reasoning in knobs.
    let back = preset_body_from_launch_params(&np.params);
    assert_eq!(back, body);
  }

  #[test]
  fn reasoning_and_mode_normalize_to_off_on_round_trip() {
    // `reasoning: false` and `mode: chat` are the inert defaults: they
    // round-trip to "absent" (no reasoning, chat mode) rather than being
    // re-emitted. This pins the normalization so it's intentional, not a
    // silent surprise. Behaviorally identical — both collapse to "off".
    let body = PresetBody {
      knobs: crate::knobset! {
        reasoning: false
      },
      ..PresetBody::default()
    };
    let np = materialize_preset("p", &body, PathBuf::from("/m/a.gguf"));
    assert!(!np.params.reasoning);
    assert_eq!(np.params.mode, LaunchMode::Chat);
    // Capture drops the inert defaults (reasoning false / chat mode).
    let back = preset_body_from_launch_params(&np.params);
    assert_eq!(back.knobs.str(crate::launch::knobs::kid("mode")), None);
    assert_eq!(
      back.knobs.bool(crate::launch::knobs::kid("reasoning")),
      None
    );
  }

  #[test]
  fn mtp_round_trips_through_a_preset_body() {
    // KD2 scopes MTP to "launch / TUI / preset". The preset half was missing:
    // PresetBody had no mtp field, so `mtp: off` in config.yaml was parsed,
    // stored, materialised and then silently dropped at launch.
    let mut lp = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    lp.mtp = crate::launch::params::MtpEnable::Off;
    lp.mtp_draft_n = Some(4);
    let body = preset_body_from_launch_params(&lp);
    assert_eq!(
      body.knobs.bool(crate::launch::knobs::kid("mtp")),
      Some(false)
    );
    assert_eq!(
      body.knobs.u32(crate::launch::knobs::kid("mtp-draft-n")),
      Some(4)
    );
    let np = materialize_preset("p", &body, PathBuf::from("/m/a.gguf"));
    assert_eq!(np.params.mtp, crate::launch::params::MtpEnable::Off);
    assert_eq!(np.params.mtp_draft_n, Some(4));
  }

  #[test]
  fn an_untouched_mtp_preset_stays_byte_stable() {
    // Auto is the default, so a preset that never mentioned MTP must not grow
    // an `mtp:` key in config.yaml on rewrite.
    let lp = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    let body = preset_body_from_launch_params(&lp);
    let yaml = serde_json::to_string(&body).expect("preset body serialises");
    assert!(
      !yaml.contains("mtp"),
      "unset MTP must not serialise, got {yaml}"
    );
  }

  #[test]
  fn a_preset_key_written_under_the_old_shard_name_still_classifies() {
    // 0.2.0 wrote per-model preset keys as shard 1's filename. After split
    // models started reading by their base name, those keys matched nothing
    // and the preset silently stopped applying.
    let catalog = vec![catalog_row("/m/foo-Q4_K_M-00001-of-00004.gguf", "llama")];
    let expected = KeyClass::Model {
      path: "/m/foo-Q4_K_M-00001-of-00004.gguf".into(),
    };
    assert_eq!(
      classify_preset_key("foo-Q4_K_M-00001-of-00004.gguf", &catalog),
      expected,
      "the old shard-suffixed key must still resolve"
    );
    assert_eq!(
      classify_preset_key("foo-Q4_K_M.gguf", &catalog),
      expected,
      "and so must the collapsed key it is written under now"
    );
  }

  #[test]
  fn classify_exact_name_is_per_model_arch_id_is_arch() {
    let catalog = vec![catalog_row("/m/qwen2.5-7b.gguf", "qwen2")];
    // An arch id must NOT be captured by a substring of a model file name.
    assert_eq!(classify_preset_key("qwen2", &catalog), KeyClass::Arch);
    assert_eq!(
      classify_preset_key("qwen2.5-7b.gguf", &catalog),
      KeyClass::Model {
        path: "/m/qwen2.5-7b.gguf".into()
      }
    );
    assert_eq!(
      classify_preset_key("/m/qwen2.5-7b.gguf", &catalog),
      KeyClass::Model {
        path: "/m/qwen2.5-7b.gguf".into()
      }
    );
  }

  #[test]
  fn classify_duplicate_basename_is_ambiguous() {
    let catalog = vec![
      catalog_row("/a/model.gguf", "llama"),
      catalog_row("/b/model.gguf", "llama"),
    ];
    assert_eq!(
      classify_preset_key("model.gguf", &catalog),
      KeyClass::Ambiguous
    );
  }

  #[test]
  fn effective_presets_unions_model_and_arch_model_wins() {
    let catalog = vec![catalog_row("/m/coder.gguf", "qwen2")];
    let mut store = BTreeMap::new();
    store.insert(
      "coder.gguf".to_string(),
      block(
        &[("shared", body_ctx(100)), ("only-model", body_ctx(1))],
        Some("only-model"),
      ),
    );
    store.insert(
      "qwen2".to_string(),
      block(
        &[("shared", body_ctx(999)), ("only-arch", body_ctx(2))],
        Some("only-arch"),
      ),
    );
    let eff = effective_presets(
      "coder.gguf",
      "/m/coder.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    let names: Vec<_> = eff.presets.iter().map(|p| p.name.clone()).collect();
    assert_eq!(
      names,
      vec!["only-arch", "only-model", "shared"],
      "union, name-sorted"
    );
    // Model wins the `shared` name collision.
    assert_eq!(eff.presets.get("shared").unwrap().params.ctx, Some(100));
    // Per-model default beats arch default.
    assert_eq!(eff.default.as_deref(), Some("only-model"));
  }

  #[test]
  fn effective_presets_falls_back_to_arch_default() {
    let catalog = vec![catalog_row("/m/coder.gguf", "qwen2")];
    let mut store = BTreeMap::new();
    store.insert("coder.gguf".into(), block(&[("m1", body_ctx(1))], None));
    store.insert("qwen2".into(), block(&[("a1", body_ctx(2))], Some("a1")));
    let eff = effective_presets(
      "coder.gguf",
      "/m/coder.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    assert_eq!(eff.default.as_deref(), Some("a1"));
  }

  #[test]
  fn effective_presets_drops_default_naming_absent_entry() {
    let catalog = vec![catalog_row("/m/coder.gguf", "qwen2")];
    let mut store = BTreeMap::new();
    store.insert(
      "coder.gguf".into(),
      block(&[("m1", body_ctx(1))], Some("ghost")),
    );
    let eff = effective_presets(
      "coder.gguf",
      "/m/coder.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    assert_eq!(
      eff.default, None,
      "a default naming a missing entry is ignored"
    );
  }

  #[test]
  fn effective_presets_keeps_auto_default_sentinel() {
    let catalog = vec![catalog_row("/m/coder.gguf", "qwen2")];
    let mut store = BTreeMap::new();
    store.insert(
      "coder.gguf".into(),
      block(&[("m1", body_ctx(1))], Some("auto")),
    );
    let eff = effective_presets(
      "coder.gguf",
      "/m/coder.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    assert_eq!(
      eff.default.as_deref(),
      Some("auto"),
      "auto sentinel survives"
    );
    assert!(eff.default_is_auto(), "recognized as the auto default");
    assert!(eff.default_preset().is_none(), "auto names no preset entry");
  }

  #[test]
  fn effective_presets_default_preset_resolves_named_entry() {
    let catalog = vec![catalog_row("/m/coder.gguf", "qwen2")];
    let mut store = BTreeMap::new();
    store.insert(
      "coder.gguf".into(),
      block(&[("m1", body_ctx(42))], Some("m1")),
    );
    let eff = effective_presets(
      "coder.gguf",
      "/m/coder.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    assert!(!eff.default_is_auto());
    assert_eq!(
      eff.default_preset().map(|np| np.name.as_str()),
      Some("m1"),
      "named default resolves to its entry"
    );
  }

  #[test]
  fn effective_presets_resolves_per_model_key_with_empty_catalog() {
    // A model not in the catalog (e.g. started by raw path, before
    // discovery) must still resolve its own per-model presets — the
    // per-model match is on the model's own name/path, not the catalog.
    let mut store = BTreeMap::new();
    store.insert(
      "coder.gguf".to_string(),
      block(&[("p", body_ctx(42))], Some("p")),
    );
    let eff = effective_presets("coder.gguf", "/m/coder.gguf", Some("qwen2"), &store, &[]);
    assert_eq!(eff.presets.len(), 1);
    assert_eq!(eff.presets.get("p").unwrap().params.ctx, Some(42));
    assert_eq!(eff.default.as_deref(), Some("p"));
  }

  #[test]
  fn shared_basename_key_applies_to_every_model_with_that_name() {
    // Two models share a basename (the same GGUF cached in two roots). A
    // basename key applies to BOTH — they intentionally share one preset
    // set. A path key still pins to its specific model on top.
    let catalog = vec![
      catalog_row("/a/model.gguf", "qwen2"),
      catalog_row("/b/model.gguf", "qwen2"),
    ];
    let mut store = BTreeMap::new();
    store.insert(
      "model.gguf".to_string(),
      block(&[("shared-name", body_ctx(1))], None),
    );
    store.insert(
      "/a/model.gguf".to_string(),
      block(&[("a-only", body_ctx(2))], None),
    );
    // Model A: the basename key applies, plus its own path key.
    let eff_a = effective_presets(
      "model.gguf",
      "/a/model.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    let names_a: Vec<_> = eff_a.presets.iter().map(|p| p.name.clone()).collect();
    assert_eq!(
      names_a,
      vec!["a-only", "shared-name"],
      "basename key + A's own path key both apply"
    );
    // Model B: the basename key applies (shared); it has no path key.
    let eff_b = effective_presets(
      "model.gguf",
      "/b/model.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    let names_b: Vec<_> = eff_b.presets.iter().map(|p| p.name.clone()).collect();
    assert_eq!(
      names_b,
      vec!["shared-name"],
      "basename key shared with B too"
    );
  }

  #[test]
  fn effective_presets_matches_per_model_key_by_full_path() {
    let catalog = vec![catalog_row("/m/coder.gguf", "qwen2")];
    let mut store = BTreeMap::new();
    store.insert(
      "/m/coder.gguf".to_string(),
      block(&[("byp", body_ctx(7))], None),
    );
    let eff = effective_presets(
      "coder.gguf",
      "/m/coder.gguf",
      Some("qwen2"),
      &store,
      &catalog,
    );
    assert_eq!(eff.presets.get("byp").unwrap().params.ctx, Some(7));
  }

  #[test]
  fn effective_presets_ignores_other_models_and_other_archs() {
    let catalog = vec![
      catalog_row("/m/a.gguf", "qwen2"),
      catalog_row("/m/b.gguf", "llama"),
    ];
    let mut store = BTreeMap::new();
    store.insert(
      "b.gguf".into(),
      block(&[("other-model", body_ctx(1))], None),
    );
    store.insert("llama".into(), block(&[("other-arch", body_ctx(2))], None));
    let eff = effective_presets("a.gguf", "/m/a.gguf", Some("qwen2"), &store, &catalog);
    assert!(
      eff.presets.is_empty(),
      "no presets apply to this model/arch"
    );
  }
}
