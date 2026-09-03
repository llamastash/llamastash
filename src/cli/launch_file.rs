//! `llamastash run <file>.yml` — a single-model launch file.
//!
//! The file is a `presets:` map in `config.yaml`'s own shape, narrowed to
//! exactly one model. Validation is stricter than the config loader's on
//! purpose: a hand-authored launch file that silently drops a misspelled knob
//! launches a different configuration than it reads as, with nothing on screen
//! to inspect afterwards.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::cli::exit_codes::{CliExit, USAGE};
use crate::config::{ConfigPresetBlock, PresetBody};
use crate::launch::presets::AUTO_DEFAULT;

#[derive(Debug, Deserialize)]
struct LaunchFileDoc {
  #[serde(default)]
  presets: BTreeMap<String, ConfigPresetBlock>,
}

/// What a launch file resolved to: the key to hand the catalog resolver, and
/// the one preset to materialize over the row it finds.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchFileSelection {
  pub model_key: String,
  pub preset_name: String,
  pub body: PresetBody,
}

/// `true` when `value` names a launch file: a `.yaml`/`.yml` extension **and**
/// an existing file. A model named `foo.yml` that is not on disk is still a
/// model reference.
pub fn is_launch_file(value: &str) -> bool {
  let path = Path::new(value);
  path
    .extension()
    .and_then(|e| e.to_str())
    .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    && path.is_file()
}

/// Read, validate, and pick the one preset this launch file runs.
///
/// `preset_flag` is `--preset` as typed, so the reserved `auto` is rejected
/// here rather than falling through to pure-fit.
pub fn load(path: &Path, preset_flag: Option<&str>) -> Result<LaunchFileSelection, CliExit> {
  let text = std::fs::read_to_string(path)
    .map_err(|e| usage(format!("cannot read launch file `{}`: {e}", path.display())))?;
  // One parse feeds both the validator and the typed doc, so the two cannot
  // disagree about what the file says. `apply_merge` has to run first: without
  // it `yaml_serde` leaves `<<` in the tree, `PresetBody` reads it as an unknown
  // key, and a merged entry launches an empty preset while reporting its name.
  let mut raw: yaml_serde::Value = yaml_serde::from_str(&text)
    .map_err(|e| usage(format!("invalid YAML in `{}`: {e}", path.display())))?;
  raw
    .apply_merge()
    .map_err(|e| usage(format!("invalid merge keys in `{}`: {e}", path.display())))?;
  let doc: LaunchFileDoc = yaml_serde::from_value(raw.clone())
    .map_err(|e| usage(format!("invalid launch file `{}`: {e}", path.display())))?;
  let sel = select_launch(doc.presets, preset_flag)?;
  validate_selection(&raw, &sel)?;
  Ok(sel)
}

/// The validation matrix, split out of [`load`] so it unit-tests without a
/// file on disk. Selection once it passes: `--preset` > `default:` > the
/// single entry.
pub fn select_launch(
  models: BTreeMap<String, ConfigPresetBlock>,
  preset_flag: Option<&str>,
) -> Result<LaunchFileSelection, CliExit> {
  if models.is_empty() {
    return Err(usage(
      "launch file has no `presets:` entries; it must name exactly one model",
    ));
  }
  if models.len() > 1 {
    return Err(usage(format!(
      "launch file names {} models ({}); it must name exactly one",
      models.len(),
      joined(models.keys())
    )));
  }
  let (key, block) = models
    .into_iter()
    .next()
    .expect("exactly one model key after the count checks");
  if block.entries.is_empty() {
    return Err(usage(format!(
      "model `{key}` has no `entries:`; a launch file must define at least one preset"
    )));
  }
  if preset_flag == Some(AUTO_DEFAULT) {
    return Err(usage(
      "a launch file always applies a preset; `--preset auto` means \"no preset\". \
       Drop the flag or drop the file.",
    ));
  }
  let names = joined(block.entries.keys());
  if block.default.as_deref() == Some(AUTO_DEFAULT) {
    return Err(usage(format!(
      "model `{key}` sets `default: auto`; a launch file's `default:` must name a preset ({names})"
    )));
  }
  if let Some(n) = preset_flag {
    if !block.entries.contains_key(n) {
      return Err(usage(format!(
        "preset `{n}` is not in the launch file; it defines: {names}"
      )));
    }
  }
  if let Some(d) = block.default.as_deref() {
    // Unlike `config.yaml`, where an absent `default:` name is ignored, a
    // launch file has no other preset to fall back to.
    if !block.entries.contains_key(d) {
      return Err(usage(format!(
        "model `{key}` sets `default: {d}`, which is not one of its presets: {names}"
      )));
    }
  }
  let preset_name = match preset_flag.or(block.default.as_deref()) {
    Some(n) => n.to_string(),
    None if block.entries.len() > 1 => {
      return Err(usage(format!(
        "launch file defines {} presets ({names}) and no `default:`; pass `--preset <name>`",
        block.entries.len()
      )))
    }
    None => block
      .entries
      .keys()
      .next()
      .expect("at least one entry after the empty check")
      .clone(),
  };
  let body = block
    .entries
    .get(&preset_name)
    .expect("selected preset is in entries")
    .clone();
  Ok(LaunchFileSelection {
    model_key: key,
    preset_name,
    body,
  })
}

/// What a `presets:` block and a preset entry may carry. Everything else in a
/// launch file is a typo.
///
/// `ConfigPresetBlock` and `PresetBody` ignore unknown keys on purpose, so
/// `config.yaml` keeps loading across versions. A hand-authored file gets no
/// such pass: it has no `presets show` to reveal what survived.
const BLOCK_FIELDS: [&str; 2] = ["default", "entries"];
const ENTRY_FIELDS: [&str; 4] = ["knobs", "extras", "backend", "server"];

/// Error on anything in the *selected* entry that would not reach the launch.
///
/// Four doors, all of which drop with a `log::warn!` the CLI never shows: an
/// unknown field on the block or the entry, a knob id no backend declares, and a
/// bad value on a good knob id. The last one is invisible to an id check, so
/// each raw knob key is asked whether it resolved *and* survived into the parsed
/// set; a key that resolved but is absent is a rejected value.
fn validate_selection(raw: &yaml_serde::Value, sel: &LaunchFileSelection) -> Result<(), CliExit> {
  let block = raw
    .get("presets")
    .and_then(|v| v.get(&sel.model_key))
    .and_then(|v| v.as_mapping())
    .ok_or_else(|| {
      usage(format!(
        "model `{}`: its `presets:` entry is not a mapping, so the launch file cannot be validated",
        sel.model_key
      ))
    })?;
  reject_unknown_fields(block, &BLOCK_FIELDS, &format!("model `{}`", sel.model_key))?;
  let entry = block
    .get("entries")
    .and_then(|v| v.get(&sel.preset_name))
    .and_then(|v| v.as_mapping())
    .ok_or_else(|| {
      usage(format!(
        "preset `{}`: its `entries:` entry is not a mapping, so the launch file cannot be validated",
        sel.preset_name
      ))
    })?;
  reject_unknown_fields(
    entry,
    &ENTRY_FIELDS,
    &format!("preset `{}`", sel.preset_name),
  )?;
  let Some(knobs) = entry.get("knobs") else {
    return Ok(());
  };
  let knobs = knobs.as_mapping().ok_or_else(|| {
    usage(format!(
      "preset `{}` sets `knobs:` to something other than a map",
      sel.preset_name
    ))
  })?;

  let mut undeclared: Vec<String> = Vec::new();
  let mut rejected: Vec<String> = Vec::new();
  for (key, _) in knobs.iter() {
    // A non-string key (a bare number) can never name a knob.
    let Some(name) = key.as_str() else {
      rejected.push(format!("{key:?}"));
      continue;
    };
    match crate::launch::knobs::resolve_id(name) {
      None => undeclared.push(name.to_string()),
      Some(id) if !sel.body.knobs.contains(id) => rejected.push(name.to_string()),
      Some(_) => {}
    }
  }
  if !undeclared.is_empty() {
    undeclared.sort_unstable();
    return Err(usage(format!(
      "preset `{}` sets knobs no backend declares: {}\nrun `llamastash knobs` for the declared list",
      sel.preset_name,
      undeclared.join(", ")
    )));
  }
  if !rejected.is_empty() {
    rejected.sort_unstable();
    return Err(usage(format!(
      "preset `{}` sets knob values the backend rejects: {}\nrun `llamastash knobs` for each knob's type and range",
      sel.preset_name,
      rejected.join(", ")
    )));
  }
  Ok(())
}

/// Error on any key of `map` outside `allowed`, naming the typo'd ones and the
/// set that is legal. A key that is itself a knob id gets an extra pointer to
/// `knobs:`, which is where `mode:` and a bare `ctx:` actually belong.
fn reject_unknown_fields(
  map: &yaml_serde::Mapping,
  allowed: &[&str],
  what: &str,
) -> Result<(), CliExit> {
  let mut unknown: Vec<String> = map
    .keys()
    .filter_map(|k| k.as_str())
    .filter(|k| !allowed.contains(k))
    .map(str::to_string)
    .collect();
  if unknown.is_empty() {
    return Ok(());
  }
  unknown.sort_unstable();
  let hint = if unknown
    .iter()
    .any(|k| crate::launch::knobs::resolve_id(k).is_some())
  {
    "\nknobs go under `knobs:`, not beside it"
  } else {
    ""
  };
  let noun = if unknown.len() > 1 {
    "fields"
  } else {
    "a field"
  };
  Err(usage(format!(
    "{what} has {noun} no preset block reads: {}\na preset takes: {}{hint}",
    unknown.join(", "),
    allowed.join(", ")
  )))
}

fn joined<'a, I: Iterator<Item = &'a String>>(keys: I) -> String {
  keys.cloned().collect::<Vec<_>>().join(", ")
}

fn usage(message: impl Into<String>) -> CliExit {
  CliExit::new(USAGE, message)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn block(default: Option<&str>, entries: &[&str]) -> ConfigPresetBlock {
    ConfigPresetBlock {
      default: default.map(str::to_string),
      entries: entries
        .iter()
        .map(|n| (n.to_string(), PresetBody::default()))
        .collect(),
    }
  }

  fn models(entries: &[(&str, ConfigPresetBlock)]) -> BTreeMap<String, ConfigPresetBlock> {
    entries
      .iter()
      .map(|(k, b)| (k.to_string(), b.clone()))
      .collect()
  }

  fn err(models: BTreeMap<String, ConfigPresetBlock>, flag: Option<&str>) -> (i32, String) {
    let e = select_launch(models, flag).unwrap_err();
    (e.code, e.message.unwrap_or_default())
  }

  #[test]
  fn no_presets_entries_is_usage() {
    let (code, msg) = err(models(&[]), None);
    assert_eq!(code, USAGE);
    assert!(msg.contains("no `presets:` entries"), "{msg}");
  }

  #[test]
  fn two_models_is_usage() {
    let (code, msg) = err(
      models(&[
        ("a.gguf", block(None, &["fast"])),
        ("b.gguf", block(None, &["fast"])),
      ]),
      None,
    );
    assert_eq!(code, USAGE);
    assert!(msg.contains("names 2 models (a.gguf, b.gguf)"), "{msg}");
  }

  #[test]
  fn a_model_with_no_entries_is_usage() {
    let (code, msg) = err(models(&[("a.gguf", block(None, &[]))]), None);
    assert_eq!(code, USAGE);
    assert!(msg.contains("has no `entries:`"), "{msg}");
  }

  #[test]
  fn the_reserved_auto_preset_flag_is_rejected() {
    let (code, msg) = err(models(&[("a.gguf", block(None, &["fast"]))]), Some("auto"));
    assert_eq!(code, USAGE);
    assert!(msg.contains("--preset auto"), "{msg}");
  }

  #[test]
  fn a_default_of_auto_is_rejected() {
    let (code, msg) = err(models(&[("a.gguf", block(Some("auto"), &["fast"]))]), None);
    assert_eq!(code, USAGE);
    assert!(msg.contains("`default: auto`"), "{msg}");
    assert!(msg.contains("(fast)"), "{msg}");
  }

  #[test]
  fn a_preset_flag_naming_no_entry_is_usage() {
    let (code, msg) = err(
      models(&[("a.gguf", block(Some("fast"), &["fast", "slow"]))]),
      Some("absent"),
    );
    assert_eq!(code, USAGE);
    assert!(
      msg.contains("preset `absent` is not in the launch file"),
      "{msg}"
    );
    assert!(msg.contains("fast, slow"), "{msg}");
  }

  #[test]
  fn a_default_naming_no_entry_is_usage() {
    // `config.yaml` ignores this; a launch file has nothing to fall back to.
    let (code, msg) = err(models(&[("a.gguf", block(Some("gone"), &["fast"]))]), None);
    assert_eq!(code, USAGE);
    assert!(msg.contains("`default: gone`"), "{msg}");
  }

  #[test]
  fn several_presets_without_a_default_need_the_flag() {
    let (code, msg) = err(models(&[("a.gguf", block(None, &["fast", "slow"]))]), None);
    assert_eq!(code, USAGE);
    assert!(msg.contains("defines 2 presets (fast, slow)"), "{msg}");
    assert!(msg.contains("--preset <name>"), "{msg}");
  }

  #[test]
  fn a_single_entry_is_selected_without_a_default() {
    let sel = select_launch(models(&[("a.gguf", block(None, &["only"]))]), None).unwrap();
    assert_eq!(sel.model_key, "a.gguf");
    assert_eq!(sel.preset_name, "only");
  }

  #[test]
  fn the_files_default_outranks_the_single_entry_order() {
    let sel = select_launch(
      models(&[("a.gguf", block(Some("slow"), &["fast", "slow"]))]),
      None,
    )
    .unwrap();
    assert_eq!(sel.preset_name, "slow");
  }

  #[test]
  fn the_preset_flag_outranks_the_files_default() {
    let sel = select_launch(
      models(&[("a.gguf", block(Some("slow"), &["fast", "slow"]))]),
      Some("fast"),
    )
    .unwrap();
    assert_eq!(sel.preset_name, "fast");
  }

  #[test]
  fn is_launch_file_needs_both_extension_and_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let yml = dir.path().join("m.yml");
    std::fs::write(&yml, "presets: {}\n").unwrap();
    let yaml = dir.path().join("M.YAML");
    std::fs::write(&yaml, "presets: {}\n").unwrap();
    let gguf = dir.path().join("m.gguf");
    std::fs::write(&gguf, b"gguf").unwrap();

    assert!(is_launch_file(yml.to_str().unwrap()));
    assert!(
      is_launch_file(yaml.to_str().unwrap()),
      "extension is case-insensitive"
    );
    assert!(
      !is_launch_file(dir.path().join("absent.yml").to_str().unwrap()),
      "a model named foo.yml that is not on disk stays a model reference"
    );
    assert!(!is_launch_file(gguf.to_str().unwrap()));
    assert!(!is_launch_file("qwen3.8"));
  }

  fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
  }

  #[test]
  fn an_undeclared_knob_id_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "l.yml",
      "presets:\n  qwen.gguf:\n    entries:\n      fast:\n        knobs:\n          n_gpu_layerz: 99\n",
    );
    let e = load(&path, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    assert!(
      e.message.unwrap().contains("n_gpu_layerz"),
      "must name the key"
    );
  }

  #[test]
  fn an_undeclared_knob_in_an_unselected_entry_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "l.yml",
      "presets:\n  qwen.gguf:\n    default: fast\n    entries:\n      fast:\n        knobs:\n          n_gpu_layers: 99\n      other:\n        knobs:\n          n_gpu_layerz: 99\n",
    );
    let sel = load(&path, None).unwrap();
    assert_eq!(sel.preset_name, "fast");
  }

  #[test]
  fn underscore_and_dash_knob_spellings_both_resolve() {
    let dir = tempfile::tempdir().unwrap();
    for key in ["n_gpu_layers", "n-gpu-layers"] {
      let path = write(
        dir.path(),
        "l.yml",
        &format!("presets:\n  qwen.gguf:\n    entries:\n      fast:\n        knobs:\n          {key}: 99\n"),
      );
      assert!(load(&path, None).is_ok(), "{key}");
    }
  }

  #[test]
  fn an_unreadable_or_malformed_file_is_usage() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gone.yml");
    let e = load(&missing, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    assert!(e.message.unwrap().contains("cannot read launch file"));

    let bad = write(dir.path(), "bad.yml", "presets: [oops\n  - : :");
    let yaml_err = load(&bad, None).unwrap_err();
    assert_eq!(yaml_err.code, USAGE);
  }

  /// The deserializer drops a bad value on a good id as silently as a bad id,
  /// so a key that resolved but did not survive into the parsed set is an error.
  #[test]
  fn a_bad_value_on_a_good_knob_id_is_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "v.yml",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          n_gpu_layers: 99\n          ctx_size: 8k\n",
    );
    let e = load(&path, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    let msg = e.message.unwrap();
    assert!(msg.contains("ctx_size"), "{msg}");
    assert!(msg.contains("rejects"), "{msg}");
  }

  #[test]
  fn merge_keys_reach_the_selected_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "m.yml",
      "presets:\n  m.gguf:\n    default: slow\n    entries:\n      fast: &base\n        knobs:\n          n_gpu_layers: 99\n        backend: engine-x\n      slow:\n        <<: *base\n",
    );
    let sel = load(&path, None).expect("merged entry loads");
    assert_eq!(sel.preset_name, "slow");
    assert_eq!(sel.body.backend.as_deref(), Some("engine-x"));
    assert_eq!(sel.body.knobs.len(), 1, "the anchor's knob must survive");
  }

  /// The stacked case: a typo'd knob riding in through an anchor. Applying
  /// merges before validating is what makes it visible.
  #[test]
  fn a_bad_knob_inside_a_merged_anchor_is_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "m.yml",
      "presets:\n  m.gguf:\n    default: slow\n    entries:\n      fast: &base\n        knobs:\n          n_gpu_layers: 99\n          n_gpu_layerz: 1\n      slow:\n        <<: *base\n",
    );
    let e = load(&path, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    assert!(e.message.unwrap().contains("n_gpu_layerz"));
  }

  /// `backend:` and `server:` are launch identity. A typo there picks a
  /// different engine and still reports success, which is worse than a typo'd
  /// knob, not better.
  #[test]
  fn an_unknown_entry_field_is_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "f.yml",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          n_gpu_layers: 99\n        backendd: engine-x\n        extra: [--flash-attn]\n",
    );
    let e = load(&path, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    let msg = e.message.unwrap();
    assert!(msg.contains("backendd") && msg.contains("extra"), "{msg}");
    assert!(msg.contains("has fields"), "{msg}");
    assert!(msg.contains("knobs, extras, backend, server"), "{msg}");
  }

  /// `mode:` is a knob now, so beside `knobs:` it is an unknown field. The
  /// message says where it belongs, since `docs/architecture.md` said otherwise
  /// until this fix.
  #[test]
  fn a_knob_written_beside_knobs_is_usage_with_a_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "f.yml",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          n_gpu_layers: 99\n        mode: embedding\n",
    );
    let e = load(&path, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    let msg = e.message.unwrap();
    assert!(msg.contains("mode"), "{msg}");
    assert!(msg.contains("has a field"), "{msg}");
    assert!(msg.contains("knobs go under `knobs:`"), "{msg}");
  }

  #[test]
  fn an_unknown_block_field_is_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "b.yml",
      "presets:\n  m.gguf:\n    defualt: fast\n    entries:\n      fast:\n        knobs:\n          n_gpu_layers: 99\n",
    );
    let e = load(&path, None).unwrap_err();
    assert_eq!(e.code, USAGE);
    let msg = e.message.unwrap();
    assert!(msg.contains("defualt"), "{msg}");
    assert!(msg.contains("default, entries"), "{msg}");
  }

  /// Only the selected entry is checked, so a typo in the entry you did not
  /// launch stays silent. The block-level fields are shared, so they are not.
  #[test]
  fn an_unselected_entry_keeps_its_own_typo_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
      dir.path(),
      "u.yml",
      "presets:\n  m.gguf:\n    default: fast\n    entries:\n      fast:\n        knobs:\n          n_gpu_layers: 99\n      slow:\n        backendd: engine-y\n",
    );
    assert_eq!(load(&path, None).unwrap().preset_name, "fast");
  }
}
