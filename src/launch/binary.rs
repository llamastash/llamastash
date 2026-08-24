//! Locate the llama.cpp server binary.
//!
//! Priority order, per the plan:
//! 1. CLI flag `--llama-server <path>`
//! 2. `LLAMASTASH_LLAMA_SERVER` environment variable
//! 3. `$PATH` lookup via the `which` crate
//!
//! The `$PATH` step tries each name the default backend declares in
//! `process_markers`, in order — the standalone server first, then the
//! unified app that ships under a different basename. When `$PATH` has
//! multiple candidates for one name (e.g. `llama-server-cuda`,
//! `llama-server`), we take the first and log the full list so the user
//! knows which one was picked and how to pin a different one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::backend::Backend;

/// Inputs to [`locate`]. Each source is optional; the function
/// applies the priority order described in the module docs.
#[derive(Debug, Clone, Default)]
pub struct LocateInputs {
  pub cli_flag: Option<PathBuf>,
  pub env_var: Option<OsString>,
  pub config_path: Option<PathBuf>,
}

/// What went wrong when [`locate`] couldn't find `llama-server`.
#[derive(Debug, thiserror::Error)]
pub enum LocateError {
  /// None of the supplied sources pointed at a real, executable file
  /// and `which` found nothing on `$PATH`.
  #[error("could not find `llama-server` (or the unified `llama` binary) — set `--llama-server <path>` or `LLAMASTASH_LLAMA_SERVER`, or add it to your $PATH")]
  NotFound,
  /// A specific path was supplied (flag/env/config) but it doesn't
  /// exist or isn't a regular file. Distinct from `NotFound` so the
  /// UI can surface the right error.
  #[error("configured `llama-server` path does not exist: {}", .0.display())]
  ExplicitPathMissing(PathBuf),
  /// A specific path was supplied (flag/env/config) and exists, but
  /// it is not executable by the current user. Caught here so the
  /// supervisor doesn't need to translate a generic spawn failure into
  /// a user-actionable message later.
  #[error("configured `llama-server` path is not executable: {p} — run `chmod +x {p}` or point `--llama-server` / `LLAMASTASH_LLAMA_SERVER` at the real binary", p = .0.display())]
  ExplicitPathNotExecutable(PathBuf),
}

/// Resolve `llama-server`'s on-disk path. Returns the canonicalised
/// path on success.
pub fn locate(inputs: LocateInputs) -> Result<PathBuf, LocateError> {
  if let Some(p) = inputs.cli_flag {
    return canonicalise_or_err(p);
  }
  if let Some(raw) = inputs.env_var {
    if !raw.is_empty() {
      return canonicalise_or_err(PathBuf::from(raw));
    }
  }
  if let Some(p) = inputs.config_path {
    return canonicalise_or_err(p);
  }
  // Fall back to `$PATH`. The names come from the default backend's own
  // marker list, so this resolver names no backend; they are searched in the
  // backend's declared order (the standalone server first, then the alternate
  // distributions), and only the first name that matches anything is used.
  // `which::which_all` returns *every* match in path order; we take the first
  // and log the rest so the user can pin a specific one via flag/env if the
  // first is wrong.
  for marker in crate::backend::default_backend().process_markers() {
    let Ok(iter) = which::which_all(marker) else {
      continue;
    };
    let candidates: Vec<PathBuf> = iter.collect();
    let Some(first) = candidates.first() else {
      continue;
    };
    if candidates.len() > 1 {
      log::info!(
        "multiple `{marker}` candidates on $PATH (using {}): {}",
        first.display(),
        candidates
          .iter()
          .map(|p| p.display().to_string())
          .collect::<Vec<_>>()
          .join(", ")
      );
    }
    return Ok(first.clone());
  }
  Err(LocateError::NotFound)
}

fn canonicalise_or_err(p: PathBuf) -> Result<PathBuf, LocateError> {
  match crate::util::paths::canonicalize(&p) {
    Ok(c) if c.is_file() => {
      if !is_executable(&c) {
        return Err(LocateError::ExplicitPathNotExecutable(p));
      }
      Ok(c)
    }
    Ok(_) => Err(LocateError::ExplicitPathMissing(p)),
    Err(_) => Err(LocateError::ExplicitPathMissing(p)),
  }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
  use std::os::unix::fs::PermissionsExt;
  match std::fs::metadata(path) {
    Ok(meta) => meta.permissions().mode() & 0o111 != 0,
    // If we can't stat, defer to the caller's spawn attempt — better
    // a slightly worse error than a false-negative permission check.
    Err(_) => true,
  }
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
  // Non-unix targets don't gate on the +x bit. The supervisor's
  // spawn attempt will surface "not executable" appropriately.
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("binary-locate-{label}"))
  }

  /// Create an empty file at `path` with mode `0755` so [`locate`]
  /// treats it as a real binary. Used by the happy-path tests.
  fn touch_exec(path: &Path) {
    fs::write(path, b"#!/bin/sh\n").expect("write fake binary");
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod 755");
    }
  }

  #[test]
  fn cli_flag_wins_over_env_and_config() {
    let dir = temp_dir("cli-wins");
    let cli_target = dir.join("cli-target");
    touch_exec(&cli_target);
    let env_target = dir.join("env-target");
    touch_exec(&env_target);

    let out = locate(LocateInputs {
      cli_flag: Some(cli_target.clone()),
      env_var: Some(env_target.into_os_string()),
      config_path: None,
    })
    .expect("locate");
    assert_eq!(out, crate::util::paths::canonicalize(&cli_target).unwrap());
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn missing_explicit_path_returns_actionable_error() {
    let err = locate(LocateInputs {
      cli_flag: Some(PathBuf::from("/nonexistent/llama-server")),
      env_var: None,
      config_path: None,
    })
    .unwrap_err();
    match err {
      LocateError::ExplicitPathMissing(p) => {
        assert_eq!(p, PathBuf::from("/nonexistent/llama-server"));
      }
      other => panic!("expected ExplicitPathMissing, got {other:?}"),
    }
  }

  #[test]
  fn empty_env_var_falls_through_to_next_source() {
    let dir = temp_dir("empty-env");
    let cfg = dir.join("cfg-target");
    touch_exec(&cfg);
    let out = locate(LocateInputs {
      cli_flag: None,
      env_var: Some(OsString::from("")),
      config_path: Some(cfg.clone()),
    })
    .expect("locate");
    assert_eq!(out, crate::util::paths::canonicalize(&cfg).unwrap());
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn cli_flag_non_executable_returns_actionable_error() {
    // Default `fs::write` creates a 0644 regular file. Without the
    // executable check, `locate` would pass back a path that
    // `Command::spawn` later rejects with a generic ENOEXEC. The
    // fix is to surface the not-executable case here so the user
    // sees a chmod hint up-front.
    let dir = temp_dir("cli-not-exec");
    let target = dir.join("not-exec");
    fs::write(&target, "not actually a binary").unwrap();

    let err = locate(LocateInputs {
      cli_flag: Some(target.clone()),
      env_var: None,
      config_path: None,
    })
    .expect_err("non-executable path must error");
    match err {
      LocateError::ExplicitPathNotExecutable(p) => assert_eq!(p, target),
      other => panic!("expected ExplicitPathNotExecutable, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn env_path_non_executable_returns_actionable_error() {
    let dir = temp_dir("env-not-exec");
    let target = dir.join("not-exec");
    fs::write(&target, "not actually a binary").unwrap();
    let err = locate(LocateInputs {
      cli_flag: None,
      env_var: Some(target.clone().into_os_string()),
      config_path: None,
    })
    .expect_err("non-executable env path must error");
    assert!(matches!(err, LocateError::ExplicitPathNotExecutable(_)));
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn config_path_non_executable_returns_actionable_error() {
    let dir = temp_dir("cfg-not-exec");
    let target = dir.join("not-exec");
    fs::write(&target, "not actually a binary").unwrap();
    let err = locate(LocateInputs {
      cli_flag: None,
      env_var: None,
      config_path: Some(target.clone()),
    })
    .expect_err("non-executable config path must error");
    assert!(matches!(err, LocateError::ExplicitPathNotExecutable(_)));
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn not_executable_error_message_names_chmod_remedy() {
    let err = LocateError::ExplicitPathNotExecutable(PathBuf::from("/opt/llama-server"));
    let msg = format!("{err}");
    assert!(
      msg.contains("chmod +x") && msg.contains("/opt/llama-server"),
      "error message should suggest a fix and name the path: {msg}"
    );
  }

  #[test]
  fn explicit_path_pointing_at_a_directory_is_missing_not_executable() {
    // A path that canonicalizes successfully but resolves to a directory
    // (not a regular file) takes the `Ok(_)` non-file arm → reported as
    // ExplicitPathMissing rather than misclassified as not-executable.
    let dir = temp_dir("path-is-dir");
    let err = locate(LocateInputs {
      cli_flag: Some(dir.clone()),
      env_var: None,
      config_path: None,
    })
    .expect_err("a directory is not a launchable binary");
    match err {
      LocateError::ExplicitPathMissing(p) => assert_eq!(p, dir),
      other => panic!("expected ExplicitPathMissing for a directory, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn no_sources_returns_not_found_when_path_lacks_binary() {
    // We don't manipulate $PATH (would affect other parallel tests),
    // so this only fails-soft: if `llama-server` happens to be on
    // the test machine's $PATH, the locate succeeds and we still
    // pass — what matters is the function doesn't panic or hang.
    let result = locate(LocateInputs::default());
    match result {
      Ok(p) => assert!(
        p.exists(),
        "if locate succeeded, the path must be real: {}",
        p.display()
      ),
      Err(LocateError::NotFound) => {}
      Err(other) => panic!("unexpected error: {other:?}"),
    }
  }
}
