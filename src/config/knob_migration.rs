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
//!
//!    One boundary, chosen rather than stumbled into: a comment *above a key*
//!    survives; a comment *between two knobs inside a migrated entry* does
//!    not. `rewrite_entry` regenerates the entry body from the folded value,
//!    and the keys it renames (`ctx` → `ctx-size`, `flash_attn` →
//!    `flash-attn`) leave an in-body comment with nothing to anchor to.
//!    `upsert_block` already documents the analogous key-line-comment loss.
//!    The `.pre-knobs.bak` written in step 2 is what makes this recoverable
//!    rather than lost. Pinned by `a_comment_inside_a_migrated_entry_is_lost`.
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
  let flat_legacy = map.iter().any(|(k, _)| {
    let Some(k) = k.as_str() else { return false };
    k == "backend_knobs" || !RESERVED.contains(&k)
  });
  flat_legacy || holds_retired_load_flags(entry)
}

/// Whether an already-migrated entry still carries a knob that has since been
/// retired. The shape is current, so [`is_legacy_entry`]'s flat-key test says
/// no — but the entry would launch a flag the engine no longer has.
fn holds_retired_load_flags(entry: &YamlValue) -> bool {
  entry
    .get(KNOBS_KEY)
    .and_then(YamlValue::as_mapping)
    .is_some_and(|knobs| {
      knobs
        .iter()
        .any(|(k, _)| matches!(k.as_str(), Some("no-mmap") | Some("mlock")))
    })
}

/// Fold the retired `no-mmap` / `mlock` booleans into the `load-mode` enum.
///
/// llama.cpp deleted the flags they emitted in `14a9d09f7` (2026-09-09), so an
/// entry keeping them launches nothing on a current build. They also cannot
/// both survive as knobs: one engine enum driven by two booleans needs a
/// precedence rule resolved somewhere the user cannot see.
///
/// `mlock` wins when both are set — it is the stronger request, and it does not
/// mmap either (`llama-model-loader.cpp:559`). A `false` value is dropped
/// rather than translated: it only ever asserted the engine's own default.
fn retire_load_flags(knobs: &mut yaml_serde::Mapping) {
  let retired = |k: &YamlValue| matches!(k.as_str(), Some("no-mmap") | Some("mlock"));
  if !knobs.keys().any(retired) {
    return;
  }
  let truthy = |k: &str| knobs.get(YamlValue::from(k)).and_then(YamlValue::as_bool) == Some(true);
  let mode = match (truthy("mlock"), truthy("no-mmap")) {
    (true, _) => Some("mlock"),
    (false, true) => Some("none"),
    // Both present but both false: they asserted the default, so the entry ends
    // up with no load-mode knob at all, which is that same default.
    (false, false) => None,
  };
  // Rebuilt in place rather than removed-and-appended: `Mapping::remove` is
  // `swap_remove`, which drags the last knob into the hole, and appending puts
  // the replacement at the end. Both show up as spurious reordering in the diff
  // of a `config.yaml` someone keeps in a dotfiles repo.
  let mut out = yaml_serde::Mapping::new();
  let mut placed = false;
  for (k, v) in knobs.iter() {
    if retired(k) {
      if let (Some(mode), false) = (mode, placed) {
        out.insert(YamlValue::from("load-mode"), YamlValue::from(mode));
        placed = true;
      }
      continue;
    }
    out.insert(k.clone(), v.clone());
  }
  *knobs = out;
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

  // Resolve a symlinked config to its real target so the final write lands on
  // the file, not on the link (a tmp-file + rename over the link itself would
  // replace it with a regular file — the hazard `preflight` exists to prevent
  // for every other config writer). Deliberately *after* the "nothing to do"
  // return: `preflight` also refuses an insecure parent directory, and a
  // config needing no migration should not fail on a write it never makes.
  let target = crate::config::writer::preflight(config_path)?;

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
  yaml_edit::write_config(&target, &current)?;

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
  // Two sources, because this runs for two kinds of entry: a legacy one whose
  // knobs are flat keys to be folded, and an already-current one pulled in only
  // because it still holds a retired knob (`holds_retired_load_flags`). The
  // second has nothing to fold and its `knobs:` must be carried through — the
  // reserved-key loop below deliberately skips that key, so without this the
  // entry would come out empty.
  let folded = fold_legacy_entry(&flat, RESERVED);
  let mut knobs = if folded.is_empty() {
    entry
      .get(KNOBS_KEY)
      .and_then(YamlValue::as_mapping)
      .cloned()
      .unwrap_or_default()
  } else {
    match yaml_serde::to_value(&folded) {
      Ok(YamlValue::Mapping(m)) => m,
      _ => yaml_serde::Mapping::new(),
    }
  };
  retire_load_flags(&mut knobs);
  if !knobs.is_empty() {
    out.insert(YamlValue::from(KNOBS_KEY), YamlValue::Mapping(knobs));
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

  /// The flags these emitted were deleted upstream in `14a9d09f7`, so an entry
  /// keeping them launches nothing on a current build — even though its shape
  /// is already current and the flat-key test says nothing to do.
  #[test]
  fn a_retired_no_mmap_becomes_a_load_mode() {
    let p = write(
      "no-mmap",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          ctx-size: 4096\n          no-mmap: true\n",
    );
    let report = migrate(&p).unwrap().expect("should migrate");
    assert_eq!(report.entries, vec![("m.gguf".into(), "fast".into())]);
    let out = read(&p);
    assert!(out.contains("load-mode: none"), "folded:\n{out}");
    assert!(!out.contains("no-mmap"), "retired key gone:\n{out}");
    assert!(out.contains("ctx-size: 4096"), "siblings survive:\n{out}");
  }

  /// A `config.yaml` kept in a dotfiles repo is read as a diff, so the rewrite
  /// must not reorder the knobs it is not changing. `Mapping::remove` is
  /// `swap_remove`, which used to drag the last knob into the retired one's slot.
  #[test]
  fn the_surrounding_knobs_keep_their_order() {
    let p = write(
      "order",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          ctx-size: 4096\n          no-mmap: true\n          parallel: 1\n          threads: 16\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    let out = read(&p);
    let at = |needle: &str| {
      out
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} missing:\n{out}"))
    };
    assert!(at("ctx-size") < at("load-mode"), "order kept:\n{out}");
    assert!(
      at("load-mode") < at("parallel"),
      "replacement sits in the retired key's slot:\n{out}"
    );
    assert!(at("parallel") < at("threads"), "tail not swapped:\n{out}");
  }

  #[test]
  fn a_retired_mlock_becomes_a_load_mode() {
    let p = write(
      "mlock",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          mlock: true\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    assert!(read(&p).contains("load-mode: mlock"));
  }

  /// Both set is the case that forced one knob: the engine takes a single enum,
  /// so two booleans need a precedence rule. mlock is the stronger request and
  /// does not mmap either.
  #[test]
  fn mlock_wins_when_both_were_set() {
    let p = write(
      "both",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          mlock: true\n          no-mmap: true\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    let out = read(&p);
    assert!(out.contains("load-mode: mlock"), "{out}");
    assert!(!out.contains("load-mode: none"), "{out}");
  }

  /// `false` only ever asserted the engine default, so it translates to no knob
  /// at all rather than to a mode.
  #[test]
  fn a_false_value_is_dropped_rather_than_translated() {
    let p = write(
      "false",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          ctx-size: 4096\n          no-mmap: false\n",
    );
    migrate(&p).unwrap().expect("should migrate");
    let out = read(&p);
    assert!(!out.contains("load-mode"), "no mode invented:\n{out}");
    assert!(!out.contains("no-mmap"), "retired key gone:\n{out}");
    assert!(out.contains("ctx-size: 4096"), "siblings survive:\n{out}");
  }

  /// An entry already free of the retired keys must not be rewritten — a
  /// migration that fires every boot would churn the file and its backup.
  #[test]
  fn a_current_entry_without_them_is_left_alone() {
    let p = write(
      "clean",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          load-mode: mlock\n",
    );
    assert!(migrate(&p).unwrap().is_none(), "nothing to migrate");
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

  /// The documented comment boundary (module doc, safety property 1). This
  /// asserts the *loss* on purpose: the behaviour is a consequence of
  /// regenerating the entry body, and a test that pins it keeps it a choice
  /// instead of something a later reader "fixes" by accident — or worse,
  /// believes does not happen because the other comment tests only ever place
  /// comments above keys.
  #[test]
  fn a_comment_inside_a_migrated_entry_is_lost_but_the_backup_keeps_it() {
    let p = write(
      "in-body-comment",
      concat!(
        "presets:\n",
        "  # above the model key: survives\n",
        "  m.gguf:\n",
        "    entries:\n",
        "      # above the entry name: survives\n",
        "      fast:\n",
        "        ctx: 4096\n",
        "        # keep memory pinned\n",
        "        mlock: true\n",
      ),
    );
    let report = migrate(&p).unwrap().expect("should migrate");
    let after = read(&p);
    assert!(after.contains("above the model key"), "{after}");
    assert!(after.contains("above the entry name"), "{after}");
    assert!(
      !after.contains("keep memory pinned"),
      "in-body comment unexpectedly survived; update the module doc:\n{after}"
    );
    // Not silently gone: the pre-migration file still carries it.
    assert!(read(&report.backup).contains("keep memory pinned"));
  }

  /// `preflight` refuses a group- or world-writable parent directory. It
  /// guards the *write*, so a config with nothing to migrate — which makes no
  /// write — must not fail on it. Calling it up front turned every daemon
  /// start on an already-migrated config in such a directory into a logged
  /// failure the user could do nothing about.
  #[cfg(unix)]
  #[test]
  fn nothing_to_migrate_does_not_trip_the_write_path_guard() {
    use std::os::unix::fs::PermissionsExt;
    let loose = |label: &str, body: &str| {
      let p = write(label, body);
      std::fs::set_permissions(p.parent().unwrap(), std::fs::Permissions::from_mode(0o777))
        .unwrap();
      p
    };

    let empty = loose("guard-empty", "");
    assert!(matches!(migrate(&empty), Ok(None)), "empty config");

    let done = loose(
      "guard-done",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        knobs:\n          ctx-size: 4096\n",
    );
    assert!(matches!(migrate(&done), Ok(None)), "already migrated");

    // The guard still guards when there *is* a write to make, and it fires
    // before the backup so no stray `.pre-knobs.bak` is left behind.
    let legacy = loose(
      "guard-legacy",
      "presets:\n  m.gguf:\n    entries:\n      fast:\n        ctx: 4096\n",
    );
    assert!(
      matches!(migrate(&legacy), Err(WriteError::ParentDirInsecure { .. })),
      "an insecure parent must still refuse a real migration"
    );
    assert!(
      !backup_path(&legacy).exists(),
      "refused migration left a backup behind"
    );
  }

  #[cfg(unix)]
  #[test]
  fn migrate_follows_symlink_and_preserves_the_link() {
    // A `config.yaml` symlinked into (say) a dotfiles repo must migrate
    // *through* to its real target, keeping the link — not replaced by a
    // regular file the way a tmp-file + rename over the link itself would.
    use std::os::unix::fs::symlink;
    let dir = unique_temp_dir("knob-migration-symlink");
    let real = dir.join("real-config.yaml");
    std::fs::write(
      &real,
      "presets:\n  # kept on purpose\n  m.gguf:\n    entries:\n      fast:\n        ctx: 4096\n        flash_attn: true\n",
    )
    .unwrap();
    let link = dir.join("config.yaml");
    symlink(&real, &link).unwrap();

    let report = migrate(&link).unwrap().expect("should migrate");

    assert!(
      std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink(),
      "symlink preserved, not replaced by a regular file"
    );
    let real_body = read(&real);
    assert!(
      real_body.contains("knobs:"),
      "write landed on target:\n{real_body}"
    );
    assert!(real_body.contains("# kept on purpose"), "comment survives");
    assert_eq!(read(&link), real_body, "reading through the link matches");
    assert!(
      read(&report.backup).contains("flash_attn: true"),
      "backup captured the original"
    );
  }
}
