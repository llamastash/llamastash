//! Comment-preserving writer for the `presets:` block of `config.yaml`.
//!
//! Presets are keyed by name under `presets: <model>: entries: <name>`. This
//! module serialises a preset body to a `yaml_serde::Value`, prunes its null
//! fields, and delegates the actual comment-safe splice to
//! [`crate::config::yaml_edit`]'s `upsert_block` — which writes the entry in
//! readable block style (multi-line, unquoted keys) and shares its mutation
//! primitive with the init / cli config writer, so every other comment and bit
//! of formatting (including a hand-authored arch preset) survives byte-for-byte.

use std::path::Path;

use serde::Serialize;
use yaml_serde::Value as YamlValue;

use crate::config::writer::{preflight, WriteError};
use crate::config::yaml_edit;

const PRESETS_KEY: &str = "presets";
const ENTRIES_KEY: &str = "entries";

/// Create or update the named preset `name` under `model_key` in
/// `config_path`, preserving every unrelated comment and bit of formatting.
/// `body` serialises to a flat YAML mapping (the preset's `ctx` / `reasoning`
/// / `extras` plus the flattened typed knobs); `null` fields are dropped so
/// the written entry only carries set values. The entry is written in
/// block style (multi-line, unquoted keys) to match a hand-authored config.
pub fn upsert_preset(
  config_path: &Path,
  model_key: &str,
  name: &str,
  body: &impl Serialize,
) -> Result<(), WriteError> {
  // Resolve a symlinked config to its real target (the link is preserved).
  let target = preflight(config_path)?;
  let source = yaml_edit::read_source(config_path)?;
  let pruned = prune_nulls(serialise(body)?);
  let new_source = yaml_edit::upsert_block(
    &source,
    &[PRESETS_KEY, model_key, ENTRIES_KEY, name],
    &pruned,
  )?;
  yaml_edit::write_config(&target, &new_source)
}

/// Remove the named preset `name` under `model_key` from `config_path`,
/// preserving unrelated comments / formatting. A now-empty `entries:` (or the
/// whole model key, or the whole `presets:` key) left behind is pruned.
/// Returns `false` when the preset wasn't present, so nothing was written.
pub fn remove_preset(config_path: &Path, model_key: &str, name: &str) -> Result<bool, WriteError> {
  let target = preflight(config_path)?;
  let source = yaml_edit::read_source(config_path)?;
  match yaml_edit::remove(&source, &[PRESETS_KEY, model_key, ENTRIES_KEY, name])? {
    Some(new_source) => {
      yaml_edit::write_config(&target, &new_source)?;
      Ok(true)
    }
    None => Ok(false),
  }
}

fn serialise(body: &impl Serialize) -> Result<YamlValue, WriteError> {
  yaml_serde::to_value(body).map_err(|e| WriteError::Serialise(e.to_string()))
}

/// Drop top-level `null` values so an entry only carries set fields (a
/// default [`crate::launch::knobs::KnobSet`] serialises all knobs, most as
/// `null`). The body is flat, so a single-level prune suffices; an `Auto`
/// knob serialises as the non-null `auto` token (and a literal-`auto` `Set`
/// as the `{ value: … }` escape), so both survive the prune.
fn prune_nulls(value: YamlValue) -> YamlValue {
  match value {
    YamlValue::Mapping(map) => {
      YamlValue::Mapping(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
    }
    other => other,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeMap;
  use std::fs;
  use std::path::PathBuf;
  use yaml_serde::Value as YamlValue;

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("presets-writer-{label}"))
  }

  /// A minimal flat preset body for tests — mirrors the real `PresetBody`
  /// shape (scalars + the `auto` token + an extras list) without depending on
  /// its definition.
  fn body(pairs: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
      map.insert((*k).to_string(), v.clone());
    }
    serde_json::Value::Object(map)
  }

  fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
  }

  fn entries_of<'a>(yaml: &'a YamlValue, model: &str) -> &'a yaml_serde::Mapping {
    yaml
      .get("presets")
      .and_then(|p| p.get(model))
      .and_then(|m| m.get("entries"))
      .and_then(YamlValue::as_mapping)
      .expect("entries map")
  }

  fn ctx_of(yaml: &YamlValue, model: &str, name: &str) -> Option<u64> {
    entries_of(yaml, model)
      .get(name)
      .and_then(|e| e.get("ctx"))
      .and_then(YamlValue::as_u64)
  }

  #[test]
  fn create_under_new_model_key_in_empty_file() {
    let dir = temp_dir("empty");
    let path = dir.join("config.yaml");
    upsert_preset(
      &path,
      "qwen-coder",
      "long-ctx",
      &body(&[("ctx", 65536.into())]),
    )
    .unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    assert_eq!(ctx_of(&yaml, "qwen-coder", "long-ctx"), Some(65536));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn create_appends_under_existing_presets_block() {
    let dir = temp_dir("append-model");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "theme: latte\npresets:\n  other-model:\n    entries:\n      fast: { ctx: 4096 }\n",
    )
    .unwrap();
    upsert_preset(
      &path,
      "qwen-coder",
      "long-ctx",
      &body(&[("ctx", 65536.into())]),
    )
    .unwrap();
    let text = read(&path);
    assert!(text.contains("theme: latte"), "unrelated key survives");
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(
      ctx_of(&yaml, "other-model", "fast"),
      Some(4096),
      "sibling model untouched"
    );
    assert_eq!(ctx_of(&yaml, "qwen-coder", "long-ctx"), Some(65536));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn update_existing_entry_preserves_all_comments() {
    let dir = temp_dir("update-comments");
    let path = dir.join("config.yaml");
    let original = "\
# top of file comment
theme: latte  # inline kept

presets:
  qwen-coder:
    entries:
      short-ctx: { ctx: 8192 }   # the small one
      long-ctx: { ctx: 32768 }   # bump this
";
    fs::write(&path, original).unwrap();
    upsert_preset(
      &path,
      "qwen-coder",
      "long-ctx",
      &body(&[("ctx", 65536.into())]),
    )
    .unwrap();
    let text = read(&path);
    assert!(text.contains("# top of file comment"));
    assert!(text.contains("theme: latte  # inline kept"));
    assert!(text.contains("short-ctx: { ctx: 8192 }   # the small one"));
    assert!(
      text.contains("# bump this"),
      "the edited line's trailing comment survives"
    );
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(
      ctx_of(&yaml, "qwen-coder", "long-ctx"),
      Some(65536),
      "value actually changed"
    );
    assert_eq!(
      ctx_of(&yaml, "qwen-coder", "short-ctx"),
      Some(8192),
      "sibling unchanged"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn app_write_to_one_model_leaves_hand_authored_arch_preset_untouched() {
    let dir = temp_dir("arch-untouched");
    let path = dir.join("config.yaml");
    let original = "\
presets:
  # hand-authored arch-level preset — must never be rewritten by an app save
  qwen2:
    default: balanced
    entries:
      balanced: { ctx: 16384, flash_attn: true }  # tuned by hand
";
    fs::write(&path, original).unwrap();
    upsert_preset(
      &path,
      "some-model.gguf",
      "coding",
      &body(&[("ctx", 8192.into())]),
    )
    .unwrap();
    let text = read(&path);
    assert!(text.contains("# hand-authored arch-level preset"));
    assert!(text.contains("default: balanced"));
    assert!(text.contains("balanced: { ctx: 16384, flash_attn: true }  # tuned by hand"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "some-model.gguf", "coding"), Some(8192));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn round_trips_block_scalar_bool_auto_and_numeric_string_extras() {
    let dir = temp_dir("round-trip");
    let path = dir.join("config.yaml");
    let b = body(&[
      ("ctx", 32768.into()),
      ("flash_attn", true.into()),
      ("n_gpu_layers", serde_json::json!("auto")), // the bare auto token
      ("extras", serde_json::json!(["--rope-freq-base", "10000"])),
      ("dropped", serde_json::Value::Null),
    ]);
    upsert_preset(&path, "m", "p", &b).unwrap();
    let text = read(&path);
    // Block style on disk: multiline, unquoted keys, `auto` bare, block list —
    // no JSON flow braces / quoted keys.
    assert!(
      text.contains("n_gpu_layers: auto"),
      "Auto is the bare token in block style:\n{text}"
    );
    assert!(
      text.contains("extras:") && text.contains("- --rope-freq-base"),
      "extras render as a block list:\n{text}"
    );
    assert!(
      !text.contains("{\""),
      "no JSON flow braces / quoted keys:\n{text}"
    );
    // Values round-trip.
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    let entry = entries_of(&yaml, "m").get("p").unwrap();
    assert_eq!(entry.get("ctx").unwrap().as_u64(), Some(32768));
    assert_eq!(entry.get("flash_attn").unwrap().as_bool(), Some(true));
    assert_eq!(
      entry.get("n_gpu_layers").unwrap().as_str(),
      Some("auto"),
      "Auto round-trips as the bare token"
    );
    let extras = entry.get("extras").unwrap().as_sequence().unwrap();
    assert_eq!(extras.len(), 2, "extras list round-trips");
    assert_eq!(
      extras[1].as_str(),
      Some("10000"),
      "a numeric-looking extras token stays a string, not an int"
    );
    assert!(entry.get("dropped").is_none(), "null fields are pruned out");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_entry_keeps_siblings_and_their_comments() {
    let dir = temp_dir("delete-entry");
    let path = dir.join("config.yaml");
    let original = "\
presets:
  qwen-coder:
    entries:
      short-ctx: { ctx: 8192 }   # keep me
      long-ctx: { ctx: 32768 }
";
    fs::write(&path, original).unwrap();
    assert!(remove_preset(&path, "qwen-coder", "long-ctx").unwrap());
    let text = read(&path);
    assert!(text.contains("short-ctx: { ctx: 8192 }   # keep me"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    let entries = entries_of(&yaml, "qwen-coder");
    assert!(entries.contains_key("short-ctx"));
    assert!(!entries.contains_key("long-ctx"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_json_flow_entry_written_by_upsert() {
    // A round-trip: upsert (writes compact JSON flow) then delete must
    // find and remove that entry via the yamlpatch Remove path.
    let dir = temp_dir("delete-json");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "m", "a", &body(&[("ctx", 1.into())])).unwrap();
    upsert_preset(&path, "m", "b", &body(&[("ctx", 2.into())])).unwrap();
    assert!(remove_preset(&path, "m", "a").unwrap());
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    let entries = entries_of(&yaml, "m");
    assert!(!entries.contains_key("a"));
    assert!(entries.contains_key("b"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_last_entry_prunes_model_key() {
    let dir = temp_dir("delete-prune");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "theme: latte\npresets:\n  m:\n    entries:\n      only: { ctx: 8192 }\n",
    )
    .unwrap();
    assert!(remove_preset(&path, "m", "only").unwrap());
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    assert!(
      yaml.get("presets").and_then(|p| p.get("m")).is_none(),
      "now-empty model key is pruned"
    );
    assert!(
      read(&path).contains("theme: latte"),
      "unrelated key survives the prune"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_last_entry_keeps_model_key_when_default_present() {
    let dir = temp_dir("delete-keep-default");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  m:\n    default: only\n    entries:\n      only: { ctx: 8192 }\n",
    )
    .unwrap();
    assert!(remove_preset(&path, "m", "only").unwrap());
    let text = read(&path);
    assert!(
      text.contains("default: only"),
      "hand-authored default survives"
    );
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert!(
      yaml
        .get("presets")
        .and_then(|p| p.get("m"))
        .and_then(|m| m.get("entries"))
        .is_none(),
      "only the empty entries map is pruned"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_absent_preset_is_a_noop_returning_false() {
    let dir = temp_dir("delete-absent");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  m:\n    entries:\n      only: { ctx: 8192 }\n",
    )
    .unwrap();
    let before = read(&path);
    assert!(!remove_preset(&path, "m", "nope").unwrap());
    assert!(!remove_preset(&path, "other", "only").unwrap());
    assert_eq!(read(&path), before, "no write on a no-op delete");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn create_into_block_that_has_default_but_no_entries() {
    let dir = temp_dir("default-no-entries");
    let path = dir.join("config.yaml");
    fs::write(&path, "presets:\n  m:\n    default: coding\n").unwrap();
    upsert_preset(&path, "m", "coding", &body(&[("ctx", 8192.into())])).unwrap();
    let text = read(&path);
    assert!(text.contains("default: coding"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "m", "coding"), Some(8192));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn create_when_entries_is_empty_flow_map() {
    // Degenerate hand-authored `entries: {}` — replaced with a populated
    // block; the model key and any siblings survive.
    let dir = temp_dir("empty-entries");
    let path = dir.join("config.yaml");
    fs::write(&path, "presets:\n  m:\n    entries: {}\n").unwrap();
    upsert_preset(&path, "m", "coding", &body(&[("ctx", 8192.into())])).unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    assert_eq!(ctx_of(&yaml, "m", "coding"), Some(8192));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn upsert_is_atomic_no_tmp_lingers() {
    let dir = temp_dir("atomic");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "m", "p", &body(&[("ctx", 8192.into())])).unwrap();
    for entry in fs::read_dir(&dir).unwrap() {
      let name = entry.unwrap().file_name();
      assert!(
        !name.to_string_lossy().starts_with("config.yaml.tmp"),
        ".tmp sibling lingered"
      );
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn upsert_sets_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mode");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "m", "p", &body(&[("ctx", 8192.into())])).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn follows_symlink_to_target_and_preserves_the_link() {
    // A preset save on a symlinked `config.yaml` writes through to the real
    // file (keeping comments) and leaves the link intact.
    use std::os::unix::fs::symlink;
    let dir = temp_dir("symlink");
    let real = dir.join("real-config.yaml");
    fs::write(&real, "# my hand-written config\ntheme: latte\n").unwrap();
    let path = dir.join("config.yaml");
    symlink(&real, &path).unwrap();

    upsert_preset(&path, "m", "p", &body(&[("ctx", 8192.into())])).unwrap();

    assert!(
      fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink(),
      "symlink preserved"
    );
    let real_body = fs::read_to_string(&real).unwrap();
    assert!(
      real_body.contains("# my hand-written config"),
      "target comment survives"
    );
    assert!(real_body.contains("theme: latte"), "target key survives");
    let yaml: YamlValue = yaml_serde::from_str(&real_body).unwrap();
    assert_eq!(
      ctx_of(&yaml, "m", "p"),
      Some(8192),
      "write landed on target"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn three_writes_across_two_models_keep_every_entry() {
    let dir = temp_dir("two-models");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "a", "p1", &body(&[("ctx", 1024.into())])).unwrap();
    upsert_preset(&path, "b", "p2", &body(&[("ctx", 2048.into())])).unwrap();
    upsert_preset(&path, "a", "p3", &body(&[("ctx", 4096.into())])).unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    let a: BTreeMap<String, YamlValue> = yaml_serde::from_value(
      yaml
        .get("presets")
        .unwrap()
        .get("a")
        .unwrap()
        .get("entries")
        .unwrap()
        .clone(),
    )
    .unwrap();
    assert_eq!(a.len(), 2, "model a has both p1 and p3");
    assert_eq!(ctx_of(&yaml, "a", "p1"), Some(1024));
    assert_eq!(ctx_of(&yaml, "a", "p3"), Some(4096));
    assert_eq!(ctx_of(&yaml, "b", "p2"), Some(2048));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn numeric_and_keyword_preset_names_are_quoted_and_update_in_place() {
    // A name that bare-parses as a non-string scalar (digits, `true`)
    // must be quoted so a second save UPDATES it rather than appending a
    // duplicate key (which would make the file unparseable).
    let dir = temp_dir("tautology");
    let path = dir.join("config.yaml");
    for name in ["12345", "true", "null"] {
      upsert_preset(&path, "m", name, &body(&[("ctx", 1.into())])).unwrap();
      upsert_preset(&path, "m", name, &body(&[("ctx", 2.into())])).unwrap();
      // Re-reads cleanly (no duplicate key) and reflects the update.
      let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
      let entries = entries_of(&yaml, "m");
      let got = entries
        .get(name)
        .unwrap_or_else(|| panic!("{name} present"));
      assert_eq!(
        got.get("ctx").and_then(YamlValue::as_u64),
        Some(2),
        "{name} updated in place"
      );
      assert!(
        read(&path).contains(&format!("\"{name}\":")),
        "{name} written quoted"
      );
      remove_preset(&path, "m", name).unwrap();
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn refuses_to_add_into_a_flow_style_entries_block() {
    // A hand-authored flow container must not be block-spliced (that
    // produces invalid YAML). Refuse cleanly; leave the file untouched.
    let dir = temp_dir("flow-add");
    let path = dir.join("config.yaml");
    let original = "presets:\n  qwen2: { entries: { balanced: { ctx: 16384 } } }\n";
    fs::write(&path, original).unwrap();
    let err = upsert_preset(&path, "qwen2", "fast", &body(&[("ctx", 4096.into())])).unwrap_err();
    assert!(matches!(err, WriteError::Patch(_)));
    assert_eq!(read(&path), original, "file untouched on refusal");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn refuses_to_delete_from_a_flow_style_entries_block() {
    // Op::Remove on a flow mapping wipes siblings — refuse instead.
    let dir = temp_dir("flow-del");
    let path = dir.join("config.yaml");
    let original =
      "presets:\n  qwen2: { entries: { balanced: { ctx: 16384 }, fast: { ctx: 4096 } } }\n";
    fs::write(&path, original).unwrap();
    let err = remove_preset(&path, "qwen2", "fast").unwrap_err();
    assert!(matches!(err, WriteError::Patch(_)));
    assert_eq!(read(&path), original, "siblings untouched on refusal");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn refuses_to_add_entries_under_a_scalar_model_value() {
    // A model key whose value is a scalar can't carry a block `entries:`.
    let dir = temp_dir("scalar-model");
    let path = dir.join("config.yaml");
    let original = "presets:\n  m: somescalar\n";
    fs::write(&path, original).unwrap();
    let err = upsert_preset(&path, "m", "p", &body(&[("ctx", 1.into())])).unwrap_err();
    assert!(matches!(err, WriteError::Patch(_)));
    assert_eq!(read(&path), original, "file untouched on refusal");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn append_entry_lands_before_a_trailing_dedented_comment() {
    // A comment at a shallower indent trails the entries block. The new
    // entry must land adjacent to its sibling, not after the comment
    // (which tree-sitter folds into the block's span).
    let dir = temp_dir("trailing-comment");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  m:\n    entries:\n      a: { ctx: 1 }\n  # arch presets below\n  qwen2:\n    entries:\n      b: { ctx: 2 }\n",
    )
    .unwrap();
    upsert_preset(&path, "m", "c", &body(&[("ctx", 3.into())])).unwrap();
    let text = read(&path);
    // `c` sits next to `a`, above the comment.
    let a_line = text.find("a: { ctx: 1 }").unwrap();
    let c_line = text.find("c:").unwrap();
    let comment = text.find("# arch presets below").unwrap();
    assert!(
      a_line < c_line && c_line < comment,
      "new entry stays with its siblings:\n{text}"
    );
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "m", "c"), Some(3));
    assert_eq!(ctx_of(&yaml, "qwen2", "b"), Some(2), "arch block intact");
    assert!(text.contains("# arch presets below"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn append_model_before_trailing_top_level_key() {
    // presets is not the last block in the file — a new model must land
    // inside presets, not after the trailing key.
    let dir = temp_dir("trailing-key");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  a:\n    entries:\n      p: { ctx: 1 }\ntheme: latte\n",
    )
    .unwrap();
    upsert_preset(&path, "b", "q", &body(&[("ctx", 2.into())])).unwrap();
    let text = read(&path);
    assert!(text.contains("theme: latte"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "a", "p"), Some(1));
    assert_eq!(ctx_of(&yaml, "b", "q"), Some(2));
    assert_eq!(yaml.get("theme").and_then(YamlValue::as_str), Some("latte"));
    fs::remove_dir_all(&dir).ok();
  }
}
