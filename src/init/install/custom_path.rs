//! User-supplied existing `llama-server` path. Runs the same integrity
//! gates the GH Releases / brew paths emit — parent-dir mode check,
//! no cross-UID symlink, +x bit, SHA-256 digest record.

use std::path::{Path, PathBuf};

use super::{sha256_file, BinaryInstall, InstallError};
use crate::init::snapshot::InstallMethod;

/// Accept `path` after the integrity gates pass. The caller is
/// responsible for confirming the user actually picked this path
/// (e.g. via a dialoguer prompt).
pub fn install_from_custom_path(path: &Path) -> Result<BinaryInstall, InstallError> {
  preflight_integrity(path)?;
  let canonical = std::fs::canonicalize(path).map_err(|e| {
    InstallError::Integrity(format!("could not canonicalise `{}`: {e}", path.display()))
  })?;
  let digest = sha256_file(&canonical)?;
  Ok(BinaryInstall {
    method: InstallMethod::CustomPath,
    path: canonical,
    digest,
    version: None, // Caller may probe `--version` separately.
  })
}

/// Adversarial pre-flight checks. Mirrors the rules the install path
/// applies to its own extracted output so a custom-path adoption
/// doesn't bypass them.
pub fn preflight_integrity(path: &Path) -> Result<(), InstallError> {
  let meta = std::fs::symlink_metadata(path)
    .map_err(|e| InstallError::Integrity(format!("could not stat `{}`: {e}", path.display())))?;
  if meta.file_type().is_symlink() {
    return refuse_cross_uid_symlink(path);
  }
  if !meta.file_type().is_file() {
    return Err(InstallError::Integrity(format!(
      "`{}` is not a regular file",
      path.display()
    )));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o111 == 0 {
      return Err(InstallError::Integrity(format!(
        "`{}` lacks the +x bit (mode {mode:#o})",
        path.display()
      )));
    }
    if let Some(parent) = path.parent() {
      if let Ok(parent_meta) = std::fs::metadata(parent) {
        let pmode = parent_meta.permissions().mode() & 0o777;
        if pmode & 0o022 != 0 {
          return Err(InstallError::Integrity(format!(
            "parent dir `{}` is group/world-writable (mode {pmode:#o})",
            parent.display()
          )));
        }
      }
    }
  }
  Ok(())
}

#[cfg(unix)]
fn refuse_cross_uid_symlink(path: &Path) -> Result<(), InstallError> {
  use std::os::unix::fs::MetadataExt;
  let our_uid = unsafe { libc::geteuid() };
  // Stat the target through the symlink — this returns the underlying
  // file's owner. A symlink whose target's UID differs from ours is the
  // adversarial case the v2 contract refuses.
  let target_meta = std::fs::metadata(path)
    .map_err(|e| InstallError::Integrity(format!("could not stat symlink target: {e}")))?;
  if target_meta.uid() != our_uid {
    return Err(InstallError::Integrity(format!(
      "symlink `{}` points at a file owned by UID {} (we are UID {our_uid}); refusing",
      path.display(),
      target_meta.uid()
    )));
  }
  // Even when UIDs match, we refuse symlinks outright per the plan —
  // they're an avoidable bypass surface.
  Err(InstallError::Integrity(format!(
    "`{}` is a symlink; init never adopts a symlink as the canonical \
     binary path (point --llama-server at the real file instead)",
    path.display()
  )))
}

#[cfg(not(unix))]
fn refuse_cross_uid_symlink(path: &Path) -> Result<(), InstallError> {
  Err(InstallError::Integrity(format!(
    "`{}` is a symlink; refused",
    path.display()
  )))
}

/// Accessor used by callers that just need to confirm a path is safe
/// before pre-selecting it in the install picker (R54).
pub fn is_safe_to_adopt(path: &Path) -> bool {
  preflight_integrity(path).is_ok()
}

/// Take a user input and resolve it to an absolute path that
/// `install_from_custom_path` can consume.
pub fn resolve_input(raw: &str) -> PathBuf {
  let p = PathBuf::from(raw);
  if p.is_absolute() {
    p
  } else {
    std::env::current_dir().unwrap_or_default().join(p)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("custom-path-{label}"))
  }

  fn write_exec(path: &Path, body: &[u8]) {
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
  }

  #[test]
  fn accepts_a_well_formed_binary() {
    let dir = temp_dir("happy");
    let bin = dir.join("llama-server");
    write_exec(&bin, b"#!/bin/sh\necho ok\n");
    let install = install_from_custom_path(&bin).expect("accept");
    assert_eq!(install.method, InstallMethod::CustomPath);
    assert_eq!(install.digest.len(), 64);
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn refuses_a_symlink() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("symlink");
    let real = dir.join("llama-server-real");
    write_exec(&real, b"#!/bin/sh\n");
    let link = dir.join("llama-server");
    symlink(&real, &link).unwrap();
    let err = install_from_custom_path(&link).unwrap_err();
    assert!(matches!(err, InstallError::Integrity(_)));
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn refuses_a_world_writable_parent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("perm");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let bin = dir.join("llama-server");
    write_exec(&bin, b"#!/bin/sh\n");
    let err = install_from_custom_path(&bin).unwrap_err();
    assert!(
      matches!(err, InstallError::Integrity(ref msg) if msg.contains("group/world-writable")),
      "expected world-writable-parent refusal, got {err:?}"
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn refuses_non_executable_file() {
    let dir = temp_dir("noexec");
    let bin = dir.join("llama-server");
    fs::write(&bin, b"non-exec").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
    let err = install_from_custom_path(&bin).unwrap_err();
    assert!(
      matches!(err, InstallError::Integrity(ref msg) if msg.contains("+x")),
      "expected +x missing refusal, got {err:?}"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_input_makes_relative_absolute() {
    let resolved = resolve_input("./relative");
    assert!(resolved.is_absolute());
  }
}
