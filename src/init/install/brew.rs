//! `brew install --quiet llama.cpp` path. macOS arm64's default route.
//!
//! On a non-zero exit from `brew install` we abort with
//! `INIT_ABORTED = 72` and surface the captured stderr verbatim — no
//! silent downgrade to GH Releases. Reasoning lives in the plan's
//! "Key Technical Decisions" section: users who picked brew did so
//! deliberately, and a silent rewrite would surprise them.
//!
//! Doctor's binary-digest-drift finding (Unit 13) carves out brew
//! installs: routine `brew upgrade` legitimately changes the digest.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::{sha256_file, BinaryInstall, InstallError};
use crate::init::snapshot::InstallMethod;
use crate::util::process::{run_with_drain_and_timeout, RunError};

/// Upper bound on `brew install llama.cpp` wall-clock. Bottle installs
/// on a healthy macOS host complete in seconds; if the install is
/// building from source (Vulkan backend) the wait stretches but still
/// finishes in single-digit minutes. 15 minutes is generous on
/// healthy networks and prevents the wizard from hanging forever on
/// a wedged brew run.
const BREW_INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Upper bound on `brew --prefix llama.cpp`. This is a metadata lookup
/// that completes in <1 s on a working brew; 30 s covers a slow disk
/// without giving a wedged brew daemon room to hang the wizard.
const BREW_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `brew install --quiet llama.cpp`, then resolve the installed
/// `llama-server` path via `brew --prefix llama.cpp` and record its
/// digest. Caller decides whether `brew` is on PATH (we surface a
/// usable error if not).
pub fn install_via_brew() -> Result<BinaryInstall, InstallError> {
  let install = run_brew_with_timeout(&["install", "--quiet", "llama.cpp"], BREW_INSTALL_TIMEOUT)?;
  if !install.status.success() {
    let stderr = String::from_utf8_lossy(&install.stderr).into_owned();
    return Err(InstallError::Brew(format!(
      "exit status {}: {stderr}",
      install.status.code().unwrap_or(-1)
    )));
  }
  let prefix = run_brew_with_timeout(&["--prefix", "llama.cpp"], BREW_QUERY_TIMEOUT)?;
  if !prefix.status.success() {
    return Err(InstallError::Brew("brew --prefix llama.cpp failed".into()));
  }
  let prefix_str = String::from_utf8_lossy(&prefix.stdout).trim().to_string();
  let binary_path = PathBuf::from(prefix_str).join("bin").join("llama-server");
  if !binary_path.exists() {
    return Err(InstallError::Brew(format!(
      "brew install succeeded but `{}` is missing",
      binary_path.display()
    )));
  }
  // Don't trust brew's exit code alone: an interrupted previous run
  // (or a brew bug) can leave a zero-byte / non-executable file at
  // the expected path. Refuse before recording the install so doctor
  // and the smoke step don't see a half-installed binary.
  let meta = std::fs::metadata(&binary_path).map_err(|e| {
    InstallError::Brew(format!(
      "brew install: stat {} failed: {e}",
      binary_path.display()
    ))
  })?;
  if meta.len() == 0 {
    return Err(InstallError::Brew(format!(
      "brew install succeeded but `{}` is zero bytes — re-run `brew install --force-bottle llama.cpp`",
      binary_path.display()
    )));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 == 0 {
      return Err(InstallError::Brew(format!(
        "brew install succeeded but `{}` has no execute bit set",
        binary_path.display()
      )));
    }
  }
  let digest = sha256_file(&binary_path)?;
  let version = read_version(&binary_path);
  Ok(BinaryInstall {
    method: InstallMethod::Brew,
    path: binary_path,
    digest,
    version,
  })
}

/// Run `brew <args>` with a wall-clock deadline via the shared
/// process helper; surfaces typed `InstallError::Brew` on every
/// failure mode so the caller branches uniformly.
fn run_brew_with_timeout(
  args: &[&str],
  timeout: Duration,
) -> Result<std::process::Output, InstallError> {
  let mut cmd = Command::new("brew");
  cmd.args(args);
  // Don't inherit the caller's full environment — preload-style vars
  // (DYLD_INSERT_LIBRARIES, LD_PRELOAD) could survive into a brew
  // child and be picked up by a downstream tool brew invokes.
  // Re-supply only the minimum brew needs to find its own state.
  cmd.env_clear();
  for key in [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "HOMEBREW_PREFIX",
  ] {
    if let Some(v) = std::env::var_os(key) {
      cmd.env(key, v);
    }
  }
  run_with_drain_and_timeout(cmd, timeout).map_err(|e| match e {
    RunError::Spawn(e) => InstallError::Brew(format!("could not spawn brew: {e}")),
    RunError::Timeout { after } => InstallError::Brew(format!(
      "`brew {}` exceeded {}s deadline; killed",
      args.join(" "),
      after.as_secs()
    )),
    RunError::Wait(e) => InstallError::Brew(format!("waitpid on brew: {e}")),
  })
}

fn read_version(path: &std::path::Path) -> Option<String> {
  // Reuse smoke::version_probe which already drains pipes on
  // background threads and bounds the wall-clock — bottle-installed
  // `llama-server --version` returns in <100 ms but a built-from-
  // source binary on first launch can take longer.
  crate::init::smoke::version_probe(path, Duration::from_secs(30)).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn install_via_brew_returns_actionable_error_when_brew_missing() {
    if which::which("brew").is_ok() {
      // brew is on PATH; the smoke test wouldn't be deterministic.
      // Confirming the negative-path error is what we care about.
      return;
    }
    let err = install_via_brew().unwrap_err();
    assert!(
      matches!(err, InstallError::Brew(_)),
      "expected Brew error when brew is missing, got {err:?}"
    );
  }
}
