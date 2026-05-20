//! Test-only helper that produces a unique temp directory per call.
//! Several modules under `src/` had their own copy of this 10-line
//! helper (`fn temp_dir(label) -> PathBuf`); consolidating reduces
//! drift and gives one obvious place to harden the naming if a real
//! filesystem ever races on `/tmp`.
//!
//! Gated `cfg(any(test, feature = "test-fixtures"))` so the helper
//! is available from internal `#[cfg(test)] mod tests` blocks AND
//! external `tests/*.rs` binaries when they're built with the
//! `test-fixtures` feature.

use std::path::PathBuf;

/// Build a unique temp directory under `std::env::temp_dir()` whose
/// name interleaves `label`, the current pid, and the nanosecond
/// clock so concurrent tests cannot collide on the same path.
///
/// On Unix the directory is `chmod 0o700` so tests that depend on
/// the parent-mode-restrictive contract of the production code (config
/// writer, custom-path install) inherit the right mode for free.
/// Caller is responsible for `remove_dir_all` after the test.
pub fn unique_temp_dir(label: &str) -> PathBuf {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!("llamastash-{label}-{}-{nanos}", std::process::id()));
  std::fs::create_dir_all(&p).expect("create unique temp dir");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
  }
  p
}
