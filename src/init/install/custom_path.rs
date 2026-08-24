//! User-supplied existing `llama-server` path. Runs the same integrity
//! gates the GH Releases / brew paths emit — resolves a symlink to its
//! real target, then requires that target to be a regular file owned by
//! us or root, carry the +x bit, and sit in directories no other user
//! can write. Records a SHA-256 digest of the resolved file.

use std::path::{Path, PathBuf};

use super::{sha256_file, BinaryInstall, InstallError};
use crate::init::snapshot::InstallMethod;

/// Accept `path` after the integrity gates pass. The caller is
/// responsible for confirming the user actually picked this path
/// (e.g. via a dialoguer prompt).
pub fn install_from_custom_path(path: &Path) -> Result<BinaryInstall, InstallError> {
  preflight_integrity(path)?;
  let canonical = crate::util::paths::canonicalize(path).map_err(|e| {
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
///
/// A symlink is resolved to the file it points at and that real file is
/// validated: a symlink to a root-owned binary under `/usr/bin` is the
/// canonical system-package-manager install, not an attack. What matters
/// is the target's owner and mode plus whether any directory on the way
/// in is writable by another user, all checked below regardless of how
/// we reached the file.
pub fn preflight_integrity(path: &Path) -> Result<(), InstallError> {
  let link_meta = std::fs::symlink_metadata(path)
    .map_err(|e| InstallError::Integrity(format!("could not stat `{}`: {e}", path.display())))?;
  let is_symlink = link_meta.file_type().is_symlink();
  let target = if is_symlink {
    crate::util::paths::canonicalize(path).map_err(|e| {
      InstallError::Integrity(format!(
        "could not resolve symlink `{}`: {e}",
        path.display()
      ))
    })?
  } else {
    path.to_path_buf()
  };
  let meta = std::fs::metadata(&target)
    .map_err(|e| InstallError::Integrity(format!("could not stat `{}`: {e}", target.display())))?;
  if !meta.file_type().is_file() {
    return Err(InstallError::Integrity(format!(
      "`{}` is not a regular file",
      target.display()
    )));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    // Owner must be ourselves or root. Root-owned binaries are the
    // trusted system case (package manager → /usr/bin); a binary owned
    // by some other non-root user is the bypass surface we refuse.
    let our_uid = unsafe { libc::geteuid() };
    let owner = meta.uid();
    if owner != our_uid && owner != 0 {
      return Err(InstallError::Integrity(format!(
        "`{}` is owned by UID {owner} (neither you, UID {our_uid}, nor root); refusing",
        target.display()
      )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o111 == 0 {
      return Err(InstallError::Integrity(format!(
        "`{}` lacks the +x bit (mode {mode:#o})",
        target.display()
      )));
    }
    // Both the symlink's own directory and the target's directory are
    // swap surfaces: a group/world-writable parent lets another user
    // repoint the link or replace the file. Check each that applies.
    let mut dirs = vec![path.parent()];
    if is_symlink {
      dirs.push(target.parent());
    }
    for parent in dirs.into_iter().flatten() {
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

/// Warn when the adopted path's file name doesn't look like the server
/// binary. llama.cpp ships several tools (`llama-cli`, `llama-bench`, …)
/// in the same directory; only `llama-server` — or the unified app, which
/// serves behind its own subcommand — speaks the HTTP API the daemon drives.
/// Non-fatal — a custom build may use any name that still contains "server" —
/// so this only nudges on an obvious mismatch.
pub fn server_name_hint(path: &Path) -> Option<String> {
  let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
  if name.contains("server") || !crate::backend::llama_cpp::serve_prefix(path).is_empty() {
    return None;
  }
  Some(format!(
    "`{}` doesn't look like a llama-server binary. llamastash drives \
     llama-server's HTTP API, not llama-cli or other llama.cpp tools; \
     adopting it anyway, but model launches will fail if it can't serve.",
    path.display()
  ))
}

/// Accessor used by callers that just need to confirm a path is safe
/// before pre-selecting it in the install picker.
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
  fn accepts_symlink_to_well_formed_target() {
    // A symlink to a same-user binary in a non-writable dir is the
    // /usr/bin-style system install; adopt it and record the resolved
    // target, not the link.
    use std::os::unix::fs::symlink;
    let dir = temp_dir("symlink-ok");
    let real = dir.join("llama-server-real");
    write_exec(&real, b"#!/bin/sh\n");
    let link = dir.join("llama-server");
    symlink(&real, &link).unwrap();
    let install = install_from_custom_path(&link).expect("adopt symlink to same-user binary");
    assert!(
      install.path.ends_with("llama-server-real"),
      "recorded path should be the resolved target, got {}",
      install.path.display()
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn refuses_symlink_into_world_writable_target_dir() {
    // The link's own dir is safe but the file it points at sits in a
    // world-writable dir — another user could swap the target.
    use std::os::unix::fs::{symlink, PermissionsExt};
    let link_dir = temp_dir("symlink-safe-linkdir");
    let target_dir = temp_dir("symlink-unsafe-targetdir");
    let real = target_dir.join("llama-server");
    write_exec(&real, b"#!/bin/sh\n");
    let link = link_dir.join("llama-server");
    symlink(&real, &link).unwrap();
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o777)).unwrap();
    let err = install_from_custom_path(&link).unwrap_err();
    assert!(
      matches!(err, InstallError::Integrity(ref msg) if msg.contains("group/world-writable")),
      "expected world-writable target-dir refusal, got {err:?}"
    );
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&link_dir).ok();
    fs::remove_dir_all(&target_dir).ok();
  }

  #[test]
  fn server_name_hint_flags_non_server_binaries() {
    assert!(server_name_hint(Path::new("/usr/bin/llama-cli")).is_some());
    // The unified app serves behind `serve` — not a mismatch.
    assert!(server_name_hint(Path::new("/home/u/.local/bin/llama")).is_none());
    assert!(server_name_hint(Path::new("/usr/bin/llama-bench")).is_some());
    assert!(server_name_hint(Path::new("/usr/bin/llama-server")).is_none());
    assert!(server_name_hint(Path::new("/opt/x/llama-server.exe")).is_none());
    assert!(server_name_hint(Path::new("/opt/x/my-server-build")).is_none());
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
