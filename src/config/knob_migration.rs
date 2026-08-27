//! One-time `config.yaml` migration to the unified knob shape (plan D10).
//!
//! Users cannot hand-edit their way out of a shape change, and a compatibility
//! reader that is deleted later is a delayed break rather than a migration. So
//! the daemon rewrites the config once, on load, and says what it did.
//!
//! What moves, per preset entry:
//!
//! ```yaml
//! # before                          # after
//! entries:                          entries:
//!   long-ctx:                         long-ctx:
//!     ctx: 65536                        knobs:
//!     flash_attn: true                    ctx-size: 65536
//!     mode: embedding                     flash-attn: true
//!     mtp: off                            mode: embedding
//!     backend_knobs:                      mtp: false
//!       ssd_streaming: "false"            ssd-streaming: false
//!     extras: [--rope-freq-base]        extras: [--rope-freq-base]
//! ```
//!
//! `arch_defaults` needs no migration: it was already a bare knob map and
//! still is. Only the key spelling changed, and both spellings load.
//!
//! Four safety properties, in priority order:
//!
//! 1. **Comments survive** — the rewrite goes through
//!    [`crate::config::yaml_edit`], which exists so app-driven writes preserve
//!    hand-authored prose. Real configs carry measured tuning results and
//!    "why this is pinned off" notes; losing those is worse than the break.
//! 2. **The original is kept** — a `.pre-knobs.bak` sibling is written first,
//!    and the migration aborts if it cannot be.
//! 3. **It is announced** — the report names the backup and the entry count.
//! 4. **It is idempotent** — a config already in the new shape is untouched,
//!    so downgrading and upgrading again does not double-migrate.

use std::path::{Path, PathBuf};

use yaml_serde::Value as YamlValue;

use crate::config::writer::WriteError;
use crate::config::yaml_edit;
use crate::launch::knobs::serde_impl::fold_legacy_entry;

const PRESETS_KEY: &str = "presets";
const ENTRIES_KEY: &str = "entries";
const KNOBS_KEY: &str = "knobs";

/// Keys inside a preset entry that are *not* knobs and stay where they are.
/// `port` is absent by design (D7): presets have never carried one.
const RESERVED: &[&str] = &["extras", "backend", "server", KNOBS_KEY];

/// What a migration did, for the log line the daemon prints.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationReport {
  pub config: PathBuf,
  pub backup: PathBuf,
  /// `(model key, entry name)` for every entry rewritten.
  pub entries: Vec<(String, String)>,
}

impl std::fmt::Display for MigrationReport {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "migrated {} preset entr{} in {} to the unified knob shape (original saved as {})",
      self.entries.len(),
      if self.entries.len() == 1 { "y" } else { "ies" },
      self.config.display(),
      self.backup.display()
    )
  }
}

/// Whether `entry` still uses the pre-registry shape — any key that is neither
/// reserved nor the new `knobs:` wrapper, or a `backend_knobs:` sub-map.
fn is_legacy_entry(entry: &YamlValue) -> bool {
  let Some(map) = entry.as_mapping() else {
    return false;
  };
  map.iter().any(|(k, _)| {
    let Some(k) = k.as_str() else { return false };
    k == "backend_knobs" || !RESERVED.contains(&k)
  })
}

/// Rewrite `config_path` into the new shape, or `Ok(None)` when it is already
/// there (or has no presets at all).
///
/// Never partial: the backup is written before the first edit, and any failure
/// leaves the original in place because each `upsert_block` rewrites the whole
/// source and only the final `write_config` touches disk.
pub fn migrate(config_path: &Path) -> Result<Option<MigrationReport>, WriteError> {
  let source = yaml_edit::read_source(config_path)?;
  if source.trim().is_empty() {
    return Ok(None);
  }
  let doc: YamlValue = yaml_serde::from_str(&source).map_err(|e| WriteError::ParseCurrent {
    path: config_path.to_path_buf(),
    error: e.to_string(),
  })?;

  // Collect the work first, so an already-migrated config costs one parse and
  // no writes at all.
  let mut todo: Vec<(String, String, YamlValue)> = Vec::new();
  if let Some(presets) = doc.get(PRESETS_KEY).and_then(YamlValue::as_mapping) {
    for (model, block) in presets {
      let (Some(model), Some(entries)) = (
        model.as_str(),
        block.get(ENTRIES_KEY).and_then(YamlValue::as_mapping),
      ) else {
        continue;
      };
      for (name, entry) in entries {
        let Some(name) = name.as_str() else { continue };
        if is_legacy_entry(entry) {
          todo.push((model.to_string(), name.to_string(), entry.clone()));
        }
      }
    }
  }
  if todo.is_empty() {
    return Ok(None);
  }

  // Backup before touching anything. A migration we cannot undo is one we
  // should not run.
  let backup = backup_path(config_path);
  std::fs::copy(config_path, &backup).map_err(|e| WriteError::Io {
    path: backup.clone(),
    error: format!("could not back up before migrating: {e}"),
  })?;

  let mut current = source;
  let mut migrated = Vec::new();
  for (model, name, entry) in todo {
    let rewritten = rewrite_entry(&entry);
    current = yaml_edit::upsert_block(
      &current,
      &[PRESETS_KEY, &model, ENTRIES_KEY, &name],
      &rewritten,
    )?;
    migrated.push((model, name));
  }
  yaml_edit::write_config(config_path, &current)?;

  Ok(Some(MigrationReport {
    config: config_path.to_path_buf(),
    backup,
    entries: migrated,
  }))
}

/// `config.yaml` → `config.yaml.pre-knobs.bak`, keeping the original extension
/// visible so the file is obviously a config and obviously not the live one.
fn backup_path(config_path: &Path) -> PathBuf {
  let mut name = config_path.file_name().unwrap_or_default().to_os_string();
  name.push(".pre-knobs.bak");
  config_path.with_file_name(name)
}

/// Build the new entry body: every knob folded into `knobs:`, reserved keys
/// carried through untouched.
fn rewrite_entry(entry: &YamlValue) -> YamlValue {
  let mut out = yaml_serde::Mapping::new();

  let flat: std::collections::BTreeMap<String, YamlValue> =
    yaml_serde::from_value(entry.clone()).unwrap_or_default();
  let knobs = fold_legacy_entry(&flat, RESERVED);
  if !knobs.is_empty() {
    if let Ok(v) = yaml_serde::to_value(&knobs) {
      out.insert(YamlValue::from(KNOBS_KEY), v);
    }
  }

  // Reserved keys keep their place and their value verbatim.
  if let Some(map) = entry.as_mapping() {
    for (k, v) in map {
      if let Some(k) = k.as_str() {
        if RESERVED.contains(&k) && k != KNOBS_KEY {
          out.insert(YamlValue::from(k), v.clone());
        }
      }
    }
  }
  YamlValue::Mapping(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::util::test_temp::unique_temp_dir;

  fn write(label: &str, body: &str) -> PathBuf {
    let dir = unique_temp_dir(&format!("knob-migration-{label}"));
    let p = dir.join("config.yaml");
    std::fs::write(&p, body).unwrap();
    p
  }

  fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
  }

  #[test]
  fn a_legacy_entry_is_rewritten_and_the_original_backed_up() {
    let p = write(
      "basic",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        ctx: 4096\n        flash_attn: true\n",
    );
    let report = migrate(&p).unwrap().expect("should migrate");
    assert_eq!(report.entries, vec![("m.gguf".into(), "fast".into())]);

    let out = read(&p);
    assert!(out.contains("knobs:"), "knobs wrapper added:\n{out}");
    assert!(out.contains("ctx-size: 4096"), "ctx renamed:\n{out}");
    assert!(out.contains("flash-attn: true"), "kebab keys:\n{out}");

    let backup = read(&report.backup);
    assert!(backup.contains("flash_attn: true"), "original preserved");
  }

  #[test]
  fn comments_survive_the_rewrite() {
    // The property that matters most: real configs carry load-bearing prose.
    let p = write(
      "comments",
      "# top of file\npresets:\n  # why this model is pinned\n  m.gguf:\n    entries:\n      fast:\n        ctx: 4096\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    let out = read(&p);
    assert!(out.contains("# top of file"), "file comment kept:\n{out}");
    assert!(
      out.contains("# why this model is pinned"),
      "entry comment kept:\n{out}"
    );
  }

  #[test]
  fn backend_knobs_and_siblings_fold_into_the_same_map() {
    let p = write(
      "fold",
      "presets:\n  m.gguf:\n    entries:\n      e:\n        mode: embedding\n        mtp: off\n        backend_knobs:\n          ssd_streaming: \"false\"\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    let out = read(&p);
    assert!(!out.contains("backend_knobs"), "sub-map is gone:\n{out}");
    assert!(out.contains("ssd-streaming: false"), "folded up:\n{out}");
    assert!(out.contains("mode: embedding"), "mode is a knob:\n{out}");
    assert!(out.contains("mtp: false"), "mtp is a knob:\n{out}");
  }

  #[test]
  fn extras_are_carried_through_untouched() {
    let p = write(
      "extras",
      "presets:\n  m.gguf:\n    entries:\n      e:\n        ctx: 8192\n        extras:\n          - --rope-freq-base\n          - \"10000\"\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    let out = read(&p);
    assert!(out.contains("extras:"), "extras kept:\n{out}");
    assert!(out.contains("--rope-freq-base"), "{out}");
    // Quoted either way, so long as it did not become a bare integer.
    assert!(
      out.contains("'10000'") || out.contains("\"10000\""),
      "a numeric-looking token stays a string:\n{out}"
    );
  }

  #[test]
  fn migration_is_idempotent() {
    let p = write(
      "idempotent",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        ctx: 4096\n",
    );
    migrate(&p).unwrap().expect("first run migrates");
    let after_first = read(&p);
    assert!(
      migrate(&p).unwrap().is_none(),
      "a migrated config is left alone"
    );
    assert_eq!(read(&p), after_first, "and byte-identical");
  }

  #[test]
  fn a_config_with_no_presets_is_untouched() {
    let p = write("nopresets", "theme: latte\ndisable_scan: false\n");
    assert!(migrate(&p).unwrap().is_none());
    assert_eq!(read(&p), "theme: latte\ndisable_scan: false\n");
  }

  #[test]
  fn arch_defaults_are_not_touched() {
    // Already a bare knob map; only the key spelling changed, and both load.
    let p = write(
      "arch",
      "arch_defaults:\n  qwen2:\n    ctx: 8192\n    n_gpu_layers: 99\n",
    );
    assert!(migrate(&p).unwrap().is_none());
    assert!(read(&p).contains("n_gpu_layers: 99"));
  }

  /// The shape a real config actually has: several model keys, each introduced
  /// by its own comment block. The first synthetic test passed while this one
  /// would have failed — its comments all sat above the first entry, so the
  /// span bug never showed. Verified against a live 8-model config: 24 of 24
  /// comments preserved (18 were lost before the `yaml_edit` span fix).
  #[test]
  fn comments_between_entries_survive() {
    let p = write(
      "interleaved",
      "presets:\n         # first model, tuned for long context\n  a.gguf:\n    entries:\n      e:\n        ctx: 1\n         # second model: streaming pinned off on purpose\n  b.gguf:\n    entries:\n      e:\n        backend_knobs:\n          ssd_streaming: \"false\"\n         # third model, measured 6.15 -> 18.5 t/s\n  c.gguf:\n    entries:\n      e:\n        mtp: on\n",
    );
    let before = read(&p);
    migrate(&p).unwrap().expect("should migrate");
    let after = read(&p);
    for comment in before.lines().map(str::trim).filter(|l| l.starts_with('#')) {
      assert!(
        after.contains(comment),
        "lost comment {comment:?}:\n{after}"
      );
    }
    assert!(after.contains("ssd-streaming: false"), "{after}");
    assert!(after.contains("mtp: true"), "{after}");
  }
}
