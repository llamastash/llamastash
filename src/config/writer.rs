//! Secure-write helper for `config.yaml`.
//!
//! Owns the `tempfile`-based atomic rename, mode 0600 on Unix, parent-
//! dir-mode pre-flight check, and symlink refusal that mirror the
//! `state.json` hardening pattern from
//! [`crate::daemon::state_store::save`]. Unit 11 (the init wizard's
//! diff preview + redaction) layers on top of this primitive.
//!
//! Merge semantics (R72):
//!   - YAML-aware recursive merge: leaf-level user edits inside a
//!     managed block (e.g. `arch_defaults.qwen2`) are preserved when
//!     the wizard regenerates the block.
//!   - User-authored keys that the wizard never wrote are left
//!     untouched. The wizard never edits keys outside its
//!     `managed_keys` allowlist.
//!   - Managed keys whose on-disk value still matches the recorded
//!     `ManagedKey::value_digest` are treated as wizard-owned →
//!     regenerable. A digest mismatch means the user hand-edited the
//!     value → preserve as-is.
//!
//! The digest comparison lives in Unit 11's wrapper; this primitive
//! takes a finalised `Value` for the merged config and writes it.

use std::path::{Path, PathBuf};

use serde_yaml::Value;

/// Outcome of a successful merge-and-write. `diff` is the set of keys
/// whose serialised value changed (added or modified); `written_bytes`
/// is the size of the new file. Unit 11 renders `diff` to the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOutcome {
  pub diff: Vec<DiffEntry>,
  pub written_bytes: u64,
}

/// One row of the merge's structural diff. `kind` distinguishes
/// "added the key entirely" from "value changed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
  pub path: String,
  pub kind: DiffKind,
  /// YAML serialisation of the value as it now appears in the merged
  /// file. Unit 11 redacts this before rendering when the path matches
  /// the secret-key allowlist.
  pub value_yaml: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
  Added,
  Changed,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
  #[error("config target {} is a symlink; refusing to follow (init never writes through symlinks)", path.display())]
  TargetIsSymlink { path: PathBuf },
  #[error("parent dir {} is group/world-writable (mode {mode:#o}); refuse to write a 0600 config there", path.display())]
  ParentDirInsecure { path: PathBuf, mode: u32 },
  #[error("config write I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("config serialise: {0}")]
  Serialise(String),
  #[error("config parse current contents at {}: {error}", path.display())]
  ParseCurrent { path: PathBuf, error: String },
}

/// Recursive merge of `additions` into `current`. Mapping keys present
/// in both are recursed; sequences and scalars in `additions` replace
/// their counterparts in `current`. Pure function — exposed for unit
/// tests; the wizard calls it indirectly via `merge_and_write`.
pub fn merge(current: Value, additions: Value) -> Value {
  match (current, additions) {
    (Value::Mapping(mut cur), Value::Mapping(add)) => {
      for (k, v) in add {
        let merged = match cur.remove(&k) {
          Some(existing) => merge(existing, v),
          None => v,
        };
        cur.insert(k, merged);
      }
      Value::Mapping(cur)
    }
    (_, other) => other,
  }
}

/// Compute a structural diff between `before` and `after`. Returns one
/// entry per leaf that was added or modified. Pure; used by Unit 11
/// to render the diff preview.
pub fn diff(before: &Value, after: &Value) -> Vec<DiffEntry> {
  let mut out = Vec::new();
  walk_diff("", before, after, &mut out);
  out
}

fn walk_diff(prefix: &str, before: &Value, after: &Value, out: &mut Vec<DiffEntry>) {
  match (before, after) {
    (Value::Mapping(b), Value::Mapping(a)) => {
      for (k, v_after) in a {
        let key = k.as_str().unwrap_or("?").to_string();
        let path = if prefix.is_empty() {
          key
        } else {
          format!("{prefix}.{key}")
        };
        match b.get(k) {
          Some(v_before) if v_before == v_after => {}
          Some(v_before) => walk_diff(&path, v_before, v_after, out),
          None => out.push(DiffEntry {
            path,
            kind: DiffKind::Added,
            value_yaml: serialise_inline(v_after),
          }),
        }
      }
    }
    (b, a) if b == a => {}
    (_, a) => out.push(DiffEntry {
      path: prefix.to_string(),
      kind: DiffKind::Changed,
      value_yaml: serialise_inline(a),
    }),
  }
}

fn serialise_inline(v: &Value) -> String {
  // `serde_yaml`'s flow style produces a one-liner suitable for inline
  // diff rendering. Falls back to the default block style on error.
  serde_yaml::to_string(v)
    .unwrap_or_default()
    .trim()
    .to_string()
}

/// Pre-flight checks that the write target is safe to touch.
/// Refuses if:
/// - `path` exists and is a symlink (no through-symlink writes).
/// - `path.parent()` exists and is group/world-writable on Unix
///   (mode bits & 0o022 != 0); writing 0600 into such a dir is
///   pointless because the dir mode dominates effective access.
pub fn preflight(path: &Path) -> Result<(), WriteError> {
  if let Ok(meta) = std::fs::symlink_metadata(path) {
    if meta.file_type().is_symlink() {
      return Err(WriteError::TargetIsSymlink {
        path: path.to_path_buf(),
      });
    }
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
      if let Ok(meta) = std::fs::metadata(parent) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o022 != 0 {
          return Err(WriteError::ParentDirInsecure {
            path: parent.to_path_buf(),
            mode,
          });
        }
      }
    }
  }
  let _ = path; // keep `path` referenced on non-unix targets
  Ok(())
}

/// Merge `additions` into the YAML at `path`, write the result
/// atomically. Returns the structural diff between the old and new
/// contents. Creates the parent dir if missing. Mode 0600 on Unix.
pub fn merge_and_write(path: &Path, additions: Value) -> Result<WriteOutcome, WriteError> {
  preflight(path)?;
  let current = read_or_default(path)?;
  let merged = merge(current.clone(), additions);
  let diff_rows = diff(&current, &merged);
  let body = serde_yaml::to_string(&merged).map_err(|e| WriteError::Serialise(e.to_string()))?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| WriteError::Io {
      path: parent.to_path_buf(),
      error: e.to_string(),
    })?;
  }
  // Atomic write — `tempfile + fsync + 0o600 + rename` lives in the
  // shared `util::atomic_write` helper so this site, the daemon
  // state store, and the init snapshot writer stay in lockstep.
  let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
  let written = crate::util::atomic_write::write_secure(
    &dir,
    "config.yaml.tmp.",
    path,
    body.as_bytes(),
    Some(0o600),
  )
  .map_err(|e| WriteError::Io {
    path: path.to_path_buf(),
    error: e.to_string(),
  })?;
  Ok(WriteOutcome {
    diff: diff_rows,
    written_bytes: written,
  })
}

/// Read the YAML at `path`, returning an empty mapping when the file
/// is missing or empty. Exposed so init's dry-run diff path can build
/// the same `(current, merged)` pair `merge_and_write` does without
/// committing to disk.
pub fn read_or_default(path: &Path) -> Result<Value, WriteError> {
  match std::fs::read_to_string(path) {
    Ok(s) if s.trim().is_empty() => Ok(Value::Mapping(serde_yaml::Mapping::new())),
    Ok(s) => serde_yaml::from_str(&s).map_err(|e| WriteError::ParseCurrent {
      path: path.to_path_buf(),
      error: e.to_string(),
    }),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
      Ok(Value::Mapping(serde_yaml::Mapping::new()))
    }
    Err(e) => Err(WriteError::Io {
      path: path.to_path_buf(),
      error: e.to_string(),
    }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("config-writer-{label}"))
  }

  fn yaml(s: &str) -> Value {
    serde_yaml::from_str(s).expect("yaml fixture")
  }

  #[test]
  fn merge_replaces_scalars_and_recurses_into_mappings() {
    let cur = yaml(
      r"
theme: latte
port_range:
  start: 41100
  end: 41300
",
    );
    let add = yaml(
      r"
port_range:
  start: 50000
arch_defaults:
  qwen2:
    n_gpu_layers: 99
",
    );
    let out = merge(cur, add);
    let s = serde_yaml::to_string(&out).unwrap();
    assert!(s.contains("theme: latte"), "untouched key survives");
    assert!(s.contains("start: 50000"), "leaf value replaced");
    assert!(
      s.contains("end: 41300"),
      "sibling under recursed key survives"
    );
    assert!(s.contains("n_gpu_layers: 99"), "added subtree present");
  }

  #[test]
  fn merge_preserves_user_added_keys_inside_managed_block() {
    // The wizard regenerates `arch_defaults.qwen2` with `n_gpu_layers`.
    // The user added `parallel: 8` to the same block. Recursive merge
    // must keep `parallel: 8`.
    let cur = yaml(
      r"
arch_defaults:
  qwen2:
    parallel: 8
    threads: 4
",
    );
    let add = yaml(
      r"
arch_defaults:
  qwen2:
    n_gpu_layers: 99
    threads: 8
",
    );
    let out = merge(cur, add);
    let s = serde_yaml::to_string(&out).unwrap();
    assert!(
      s.contains("parallel: 8"),
      "user key under managed block survives"
    );
    assert!(s.contains("n_gpu_layers: 99"), "wizard key added");
    assert!(
      s.contains("threads: 8"),
      "wizard key replaces user value (intentional regen)"
    );
  }

  #[test]
  fn diff_flags_added_and_changed_leaves() {
    let before = yaml("theme: latte\nport_range:\n  start: 41100\n  end: 41300\n");
    let after = yaml(
      "theme: latte\nport_range:\n  start: 50000\n  end: 41300\narch_defaults:\n  qwen2:\n    n_gpu_layers: 99\n",
    );
    let rows = diff(&before, &after);
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"port_range.start"));
    assert!(paths.contains(&"arch_defaults"));
    let row = rows.iter().find(|r| r.path == "port_range.start").unwrap();
    assert_eq!(row.kind, DiffKind::Changed);
  }

  #[cfg(unix)]
  #[test]
  fn preflight_refuses_symlink_target() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("symlink");
    let target = dir.join("config.yaml");
    let victim = dir.join("victim.dat");
    fs::write(&victim, b"important").unwrap();
    symlink(&victim, &target).unwrap();
    let err = preflight(&target).unwrap_err();
    assert!(matches!(err, WriteError::TargetIsSymlink { .. }));
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn preflight_refuses_group_world_writable_parent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("perm");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let target = dir.join("config.yaml");
    let err = preflight(&target).unwrap_err();
    assert!(matches!(err, WriteError::ParentDirInsecure { .. }));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn merge_and_write_creates_file_when_missing() {
    let dir = temp_dir("create");
    let target = dir.join("config.yaml");
    let outcome = merge_and_write(&target, yaml("theme: latte\n")).expect("write");
    assert!(target.exists());
    assert!(outcome.written_bytes > 0);
    assert!(outcome.diff.iter().any(|r| r.path == "theme"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn merge_and_write_is_atomic_no_tmp_lingers() {
    let dir = temp_dir("atomic");
    let target = dir.join("config.yaml");
    merge_and_write(&target, yaml("theme: latte\n")).expect("write");
    for entry in fs::read_dir(&dir).expect("readdir") {
      let entry = entry.expect("dirent");
      let name = entry.file_name();
      let name = name.to_string_lossy();
      assert!(
        !name.starts_with("config.yaml.tmp"),
        ".tmp sibling lingered: {name}"
      );
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn merge_and_write_sets_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mode");
    let target = dir.join("config.yaml");
    merge_and_write(&target, yaml("theme: latte\n")).expect("write");
    let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn merge_and_write_preserves_user_keys_under_recursive_merge() {
    let dir = temp_dir("recursive");
    let target = dir.join("config.yaml");
    fs::write(
      &target,
      "theme: latte\narch_defaults:\n  qwen2:\n    parallel: 8\n",
    )
    .unwrap();
    let additions = yaml(
      r"
arch_defaults:
  qwen2:
    n_gpu_layers: 99
",
    );
    merge_and_write(&target, additions).expect("write");
    let body = fs::read_to_string(&target).unwrap();
    assert!(body.contains("theme: latte"));
    assert!(body.contains("parallel: 8"));
    assert!(body.contains("n_gpu_layers: 99"));
    fs::remove_dir_all(&dir).ok();
  }
}
