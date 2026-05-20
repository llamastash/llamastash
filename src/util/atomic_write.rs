//! Atomic-write helper used by every llamastash component that
//! persists JSON / YAML state under `$XDG_STATE_HOME` or
//! `$XDG_CONFIG_HOME`.
//!
//! The recipe — write to a unique tempfile in the *same* directory,
//! `fsync` it, optionally chmod, atomic `rename` over the final
//! path — is shared across [`crate::daemon::state_store::save`],
//! [`crate::init::snapshot::save`], and
//! [`crate::config::writer::merge_and_write`]. Keeping the
//! mechanics in one place ensures hardening additions
//! (`O_NOFOLLOW`, fsync of the parent dir, etc.) land for every
//! consumer instead of in one.
//!
//! `tempfile::Builder` already creates files mode `0o600` on Unix;
//! the explicit `chmod` here is belt-and-braces for the case where
//! the rename target dir has a permissive umask whose mode bits a
//! future `rename` implementation might pick up.

use std::io;
use std::path::Path;

/// Write `body` to `final_path` atomically.
///
/// - `dir` must be the directory that will hold `final_path` so the
///   tempfile + rename stay on the same filesystem.
/// - `prefix` becomes the tempfile name prefix (e.g.
///   `"state.json.tmp."`). `tempfile`'s mkstemp-style suffix appends
///   the unpredictable randomness.
/// - `mode_unix`, when supplied, is `chmod`-ed onto the tempfile
///   *before* the rename so the final file lands with the intended
///   mode atomically. `None` keeps `tempfile`'s default
///   (`0o600`-equivalent on Unix; whatever the platform's umask
///   produces on non-Unix).
///
/// Creates `dir` if it doesn't exist. Returns the number of bytes
/// written on success.
pub fn write_secure(
  dir: &Path,
  prefix: &str,
  final_path: &Path,
  body: &[u8],
  mode_unix: Option<u32>,
) -> io::Result<u64> {
  use std::io::Write as _;

  std::fs::create_dir_all(dir)?;
  let mut tmp = tempfile::Builder::new().prefix(prefix).tempfile_in(dir)?;
  tmp.write_all(body)?;
  tmp.as_file().sync_all()?;
  #[cfg(unix)]
  if let Some(mode) = mode_unix {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode));
  }
  #[cfg(not(unix))]
  {
    let _ = mode_unix; // signature parity across platforms
  }
  tmp
    .persist(final_path)
    .map_err(|e| io::Error::other(e.error))?;
  Ok(body.len() as u64)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-atomic-write-{label}-{}-{nanos}",
      std::process::id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
  }

  #[test]
  fn writes_body_atomically() {
    let dir = temp_dir("happy");
    let final_path = dir.join("data.bin");
    let written = write_secure(&dir, "data.bin.tmp.", &final_path, b"hello", None).unwrap();
    assert_eq!(written, 5);
    assert_eq!(std::fs::read(&final_path).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn writes_with_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mode-0600");
    let final_path = dir.join("secret");
    write_secure(&dir, "secret.tmp.", &final_path, b"x", Some(0o600)).unwrap();
    let mode = std::fs::metadata(&final_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn creates_directory_if_missing() {
    let dir = temp_dir("missing-dir");
    let nested = dir.join("a/b/c");
    let final_path = nested.join("data");
    write_secure(&nested, "data.tmp.", &final_path, b"x", None).unwrap();
    assert!(final_path.exists());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn overwrites_existing_file() {
    let dir = temp_dir("overwrite");
    let final_path = dir.join("data");
    write_secure(&dir, "data.tmp.", &final_path, b"first", None).unwrap();
    write_secure(&dir, "data.tmp.", &final_path, b"second", None).unwrap();
    assert_eq!(std::fs::read(&final_path).unwrap(), b"second");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn tmp_sibling_not_left_after_persist() {
    let dir = temp_dir("no-tmp-sibling");
    let final_path = dir.join("data");
    write_secure(&dir, "data.tmp.", &final_path, b"x", None).unwrap();
    let lingering = std::fs::read_dir(&dir)
      .unwrap()
      .filter_map(Result::ok)
      .filter(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.starts_with("data.tmp.") && name.as_ref() != "data"
      })
      .count();
    assert_eq!(lingering, 0, "no .tmp sibling should remain after persist");
    std::fs::remove_dir_all(&dir).ok();
  }
}
