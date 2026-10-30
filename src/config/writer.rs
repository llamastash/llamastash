//! Secure-write helper for `config.yaml`.
//!
//! Owns the `tempfile`-based atomic rename, mode 0600 on Unix, and the
//! parent-dir-mode pre-flight check that mirror the `state.json` hardening
//! from [`crate::daemon::state_store::save`]. Unlike `state.json`, a
//! **symlinked** `config.yaml` is followed to its target (the link is
//! user-authored, e.g. a dotfiles repo) rather than refused — see
//! [`preflight`]. The init wizard's diff preview + redaction layer on top.
//!
//! Merge semantics:
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
//! The digest comparison lives in the wizard's wrapper; this primitive
//! takes a finalised `Value` for the merged config and writes it.

use std::path::{Path, PathBuf};

use yaml_serde::Value;

use super::yaml_edit;

// Reserve edits and preset edits share a file; serialize their read/modify/write.
pub(crate) static CONFIG_MUTATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub use crate::util::config_patch::{DiffEntry, DiffKind};

/// Outcome of a successful merge-and-write. `diff` is the set of keys
/// whose serialised value changed (added or modified); `written_bytes`
/// is the size of the new file. The wizard renders `diff` to the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOutcome {
  pub diff: Vec<DiffEntry>,
  pub written_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
  #[error("parent dir {} {reason}; refusing to write a 0600 config there", path.display())]
  ParentDirInsecure { path: PathBuf, reason: String },
  #[error("config write I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("config serialise: {0}")]
  Serialise(String),
  #[error("config parse current contents at {}: {error}", path.display())]
  ParseCurrent { path: PathBuf, error: String },
  #[error("config presets patch: {0}")]
  Patch(String),
}

/// Recursive merge of `additions` into `current`. Mapping keys present
/// in both are recursed; sequences and scalars in `additions` replace
/// their counterparts in `current`. Pure function — exposed for unit
/// tests; the wizard calls it indirectly via `merge_and_write`.
pub fn merge(current: Value, additions: Value) -> Value {
  match (current, additions) {
    (Value::Mapping(mut cur), Value::Mapping(add)) => {
      // In place, so existing keys keep their order: `remove` swaps the last
      // key into the gap.
      for (k, v) in add {
        match cur.get_mut(&k) {
          Some(existing) => *existing = merge(std::mem::replace(existing, Value::Null), v),
          None => {
            cur.insert(k, v);
          }
        }
      }
      Value::Mapping(cur)
    }
    (_, other) => other,
  }
}

/// Compute a structural diff between `before` and `after`. Returns one
/// entry per leaf that was added or modified. Pure; used by the wizard
/// to render the diff preview.
pub fn diff(before: &Value, after: &Value) -> Vec<DiffEntry> {
  let mut out = Vec::new();
  walk_diff("", before, after, &mut out);
  out
}

// Sibling of [`walk_writes`]: both recurse the same `(before, after)` pair,
// but this one produces human-facing `DiffEntry` rows (add/change classified,
// values rendered to strings) for the wizard's preview/redaction, while
// `walk_writes` produces the changed leaves to splice. Kept separate because
// their outputs differ; keep their traversal shape in sync if either changes.
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
  // `yaml_serde`'s flow style produces a one-liner suitable for inline
  // diff rendering. Falls back to the default block style on error.
  yaml_serde::to_string(v)
    .unwrap_or_default()
    .trim()
    .to_string()
}

/// Pre-flight a config write and return the path to actually write to.
///
/// `config.yaml` may legitimately be a **symlink** (e.g. into a dotfiles
/// repo). Unlike `state.json` — machine-managed runtime state that nobody
/// symlinks — we *follow* the link and write to its canonical target, so the
/// link survives the save (a tmp-file + rename over the link itself would
/// replace the link with a regular file). A non-symlink path is returned
/// unchanged. The swap-surface parent-dir check (which runs on the
/// *resolved* target's parent, where the rename lands) is the only
/// refusal: writing a 0600 config into a dir another user can write is
/// pointless because that user could rename the file away, and the dir
/// mode dominates effective access.
pub fn preflight(path: &Path) -> Result<PathBuf, WriteError> {
  let target = resolve_write_target(path);
  #[cfg(unix)]
  {
    use crate::util::file_security::dir_swap_surface;
    if let Some(parent) = target.parent() {
      let our_uid = unsafe { libc::geteuid() };
      if let Some(surface) = dir_swap_surface(parent, our_uid) {
        return Err(WriteError::ParentDirInsecure {
          path: parent.to_path_buf(),
          reason: surface.describe(our_uid),
        });
      }
    }
  }
  Ok(target)
}

/// Resolve `path` to the real file a write should land on. A non-symlink is
/// returned unchanged. A symlink is followed (chain) to its final target —
/// even one that doesn't exist yet — and the target's parent is canonicalized
/// (collapsing `..` / directory symlinks) while the filename is kept, so the
/// caller writes the real file and the link is preserved. Following stays in
/// the config write path only; `state.json` keeps its non-following behavior.
fn resolve_write_target(path: &Path) -> PathBuf {
  let is_symlink = std::fs::symlink_metadata(path)
    .map(|m| m.file_type().is_symlink())
    .unwrap_or(false);
  if !is_symlink {
    return path.to_path_buf();
  }
  let mut cur = path.to_path_buf();
  for _ in 0..40 {
    let is_link = std::fs::symlink_metadata(&cur)
      .map(|m| m.file_type().is_symlink())
      .unwrap_or(false);
    if !is_link {
      break;
    }
    let Ok(target) = std::fs::read_link(&cur) else {
      break;
    };
    cur = if target.is_absolute() {
      target
    } else {
      cur.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };
  }
  // Canonicalize the parent (must exist) so a not-yet-created target still
  // resolves to a real directory; fall back to the lexical path otherwise.
  match (cur.parent(), cur.file_name()) {
    (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
      Ok(real_parent) => real_parent.join(name),
      Err(_) => cur,
    },
    _ => cur,
  }
}

/// Merge `additions` into the YAML at `path`, write the result
/// atomically. Returns the structural diff between the old and new
/// contents. Creates the parent dir if missing. Mode 0600 on Unix.
///
/// The write is **comment-safe**: each changed leaf is spliced into the
/// original file text via the shared [`yaml_edit`] primitive, so the user's
/// hand-written comments and formatting survive (the old whole-file
/// re-serialise stripped them on every run).
pub fn merge_and_write(path: &Path, additions: Value) -> Result<WriteOutcome, WriteError> {
  let _mutation = CONFIG_MUTATION
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  // Resolve a symlinked config to its real target (the link is preserved).
  let target = preflight(path)?;
  // Read the source text once — comments live here and must survive. The
  // parsed `current` drives merge/diff; the original text is what we splice.
  let source = yaml_edit::read_source(path)?;
  let current = if source.trim().is_empty() {
    Value::Mapping(yaml_serde::Mapping::new())
  } else {
    yaml_serde::from_str(&source).map_err(|e| WriteError::ParseCurrent {
      path: path.to_path_buf(),
      error: e.to_string(),
    })?
  };
  let merged = merge(current.clone(), additions);
  let diff_rows = diff(&current, &merged);

  // Splice each changed leaf into the original text rather than
  // re-serialising `merged`. Untouched keys stay byte-for-byte.
  let mut new_source = source;
  for (segments, value) in collect_leaf_writes(&current, &merged) {
    let token = yaml_edit::render_value(&value)?;
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    new_source = yaml_edit::upsert(&new_source, &segs, &token)?;
  }
  yaml_edit::write_config(&target, &new_source)?;
  Ok(WriteOutcome {
    diff: diff_rows,
    written_bytes: new_source.len() as u64,
  })
}

/// Collect the scalar / sequence leaves of `after` that differ from
/// `before`, each with its full key path. Recurses into mappings so a fresh
/// block is written leaf-by-leaf in block style (not one inline-flow blob
/// that a later write couldn't append to); a sequence — or the `{auto: true}`
/// knob sentinel, which round-trips identically as a one-key block — is a
/// leaf value. Drives the comment-safe splice in [`merge_and_write`].
///
/// Sibling of [`walk_diff`], which walks the same `(before, after)` pair for
/// the human-facing preview; this one yields the leaves to write. If you change
/// one walker's traversal, mirror it in the other.
fn collect_leaf_writes(before: &Value, after: &Value) -> Vec<(Vec<String>, Value)> {
  let mut out = Vec::new();
  walk_writes(&mut Vec::new(), Some(before), after, &mut out);
  out
}

fn walk_writes(
  prefix: &mut Vec<String>,
  before: Option<&Value>,
  after: &Value,
  out: &mut Vec<(Vec<String>, Value)>,
) {
  let Value::Mapping(a) = after else { return };
  let before_map = before.and_then(Value::as_mapping);
  for (k, v_after) in a {
    let v_before = before_map.and_then(|m| m.get(k));
    if v_before == Some(v_after) {
      continue; // unchanged leaf / subtree
    }
    prefix.push(k.as_str().unwrap_or("?").to_string());
    if matches!(v_after, Value::Mapping(_)) {
      walk_writes(prefix, v_before, v_after, out);
    } else {
      out.push((prefix.clone(), v_after.clone()));
    }
    prefix.pop();
  }
}

/// Read the YAML at `path`, returning an empty mapping when the file
/// is missing or empty. Exposed so init's dry-run diff path can build
/// the same `(current, merged)` pair `merge_and_write` does without
/// committing to disk.
pub fn read_or_default(path: &Path) -> Result<Value, WriteError> {
  // Share the file-read (missing → empty, I/O errors bubble) with the writers.
  let source = yaml_edit::read_source(path)?;
  if source.trim().is_empty() {
    Ok(Value::Mapping(yaml_serde::Mapping::new()))
  } else {
    yaml_serde::from_str(&source).map_err(|e| WriteError::ParseCurrent {
      path: path.to_path_buf(),
      error: e.to_string(),
    })
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
    yaml_serde::from_str(s).expect("yaml fixture")
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
    let s = yaml_serde::to_string(&out).unwrap();
    assert!(s.contains("theme: latte"), "untouched key survives");
    assert!(s.contains("start: 50000"), "leaf value replaced");
    assert!(
      s.contains("end: 41300"),
      "sibling under recursed key survives"
    );
    assert!(s.contains("n_gpu_layers: 99"), "added subtree present");
  }

  /// Existing keys keep their place and new ones go at the end.
  #[test]
  fn merge_keeps_existing_key_order() {
    let cur = yaml("zeta: 1\nalpha: {b: 1, a: 2}\nmid: 3\n");
    let add = yaml("alpha: {a: 9, c: 4}\nnew: 5\nzeta: 0\n");
    let out = merge(cur, add);
    let keys = |v: &Value| {
      v.as_mapping()
        .unwrap()
        .keys()
        .map(|k| k.as_str().unwrap().to_string())
        .collect::<Vec<_>>()
    };
    assert_eq!(keys(&out), ["zeta", "alpha", "mid", "new"]);
    assert_eq!(keys(&out["alpha"]), ["b", "a", "c"]);
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
    let s = yaml_serde::to_string(&out).unwrap();
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
  fn merge_and_write_follows_symlink_and_preserves_the_link() {
    // A `config.yaml` symlinked into (say) a dotfiles repo must be written
    // *through* to its real target, keeping both the link and the target's
    // comments — not refused, and not clobbered into a regular file.
    use std::os::unix::fs::symlink;
    let dir = temp_dir("symlink-follow");
    let real = dir.join("real-config.yaml");
    fs::write(&real, "theme: latte  # mine\n").unwrap();
    let link = dir.join("config.yaml");
    symlink(&real, &link).unwrap();

    merge_and_write(&link, yaml("llama_server_path: /opt/ls\n")).expect("write");

    // The link is still a symlink (not replaced by a regular file).
    assert!(
      fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink(),
      "symlink preserved"
    );
    // The real target got the update and kept its comment.
    let real_body = fs::read_to_string(&real).unwrap();
    assert!(real_body.contains("# mine"), "target comment survives");
    assert!(
      real_body.contains("llama_server_path: /opt/ls"),
      "write landed on target"
    );
    // Reading through the link sees the same content.
    assert_eq!(fs::read_to_string(&link).unwrap(), real_body);
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

  #[cfg(unix)]
  #[test]
  fn preflight_self_owned_group_writable_parent_follows_the_private_group_rule() {
    // A group-writable config dir *we* own is a swap surface only when its
    // group is shared: with a user-private `user:user` group (the reported
    // `mise` / 002-umask case) no other user can rename a 0600 file out of
    // it. A shared group — macOS `staff`, a Linux `users` — refuses.
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("perm-own-gw");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
    let target = dir.join("config.yaml");
    let our_uid = unsafe { libc::geteuid() };
    let gid = fs::metadata(&dir).unwrap().gid();
    if crate::util::file_security::is_user_private_group(gid, our_uid) {
      assert!(
        preflight(&target).is_ok(),
        "a self-owned 0775 dir on a user-private group must not refuse the config write"
      );
    } else {
      let err = preflight(&target).unwrap_err();
      assert!(
        matches!(err, WriteError::ParentDirInsecure { .. })
          && err.to_string().contains("group-writable"),
        "a self-owned 0775 dir on a shared group must refuse: {err}"
      );
    }
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

  #[test]
  fn merge_and_write_preserves_comments() {
    // The whole point of the yaml_edit rewrite: a wizard / cli write must
    // not strip the user's hand-written comments.
    let dir = temp_dir("comments");
    let target = dir.join("config.yaml");
    fs::write(
      &target,
      "# my hand-written config\ntheme: latte  # I like this one\n",
    )
    .unwrap();
    merge_and_write(&target, yaml("llama_server_path: /opt/llama-server\n")).expect("write");
    let body = fs::read_to_string(&target).unwrap();
    assert!(
      body.contains("# my hand-written config"),
      "header comment survives"
    );
    assert!(
      body.contains("theme: latte  # I like this one"),
      "inline comment survives"
    );
    let y: Value = yaml_serde::from_str(&body).unwrap();
    assert_eq!(
      y.get("llama_server_path").and_then(Value::as_str),
      Some("/opt/llama-server")
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn merge_and_write_adds_nested_key_into_commented_block() {
    // `proxy.api_key` (the cli write) lands under an existing, commented
    // `proxy:` block without disturbing its other keys / comments.
    let dir = temp_dir("nested-comments");
    let target = dir.join("config.yaml");
    fs::write(
      &target,
      "proxy:\n  port: 11500  # pinned away from ollama\n",
    )
    .unwrap();
    let additions = yaml("proxy:\n  api_key: sekret\n");
    merge_and_write(&target, additions).expect("write");
    let body = fs::read_to_string(&target).unwrap();
    assert!(
      body.contains("port: 11500  # pinned away from ollama"),
      "sibling + comment kept"
    );
    let y: Value = yaml_serde::from_str(&body).unwrap();
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("api_key"))
        .and_then(Value::as_str),
      Some("sekret")
    );
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("port"))
        .and_then(Value::as_u64),
      Some(11500)
    );
    fs::remove_dir_all(&dir).ok();
  }
}
