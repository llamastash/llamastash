//! Cross-platform clipboard write helper used by the TUI's yank
//! commands (R30).
//!
//! Strategy, per the plan:
//! 1. Native first — `arboard` ships X11/XWayland by default and a
//!    Wayland-data-control backend behind a feature flag. It
//!    handles clipboard ownership, MIME negotiation, and macOS /
//!    Windows out of the box.
//! 2. Shell-out fallback — when `arboard` returns an error (e.g.
//!    headless container, exotic Wayland compositor without
//!    `wlr-data-control`, missing display server), we attempt
//!    `wl-copy` / `xclip` / `xsel` (Linux) or `pbcopy` (macOS).
//! 3. If every path fails, return [`ClipboardError::NoBackend`]
//!    with the list we tried; the TUI surfaces it as a transient
//!    toast that prints the URL inline so the user can copy it
//!    manually.

use std::io::Write;
use std::process::{Command, Stdio};

/// Outcome of a clipboard write.
#[derive(Debug)]
pub enum ClipboardError {
  /// Neither the native backend nor any shell-out helper worked.
  NoBackend { tried: Vec<String> },
  /// A backend was found but failed (non-zero exit / IO error /
  /// arboard error). `backend` names which one was attempted.
  BackendFailed { backend: String, error: String },
}

impl std::fmt::Display for ClipboardError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NoBackend { tried } => write!(
        f,
        "no clipboard backend available — tried: {}. Copy manually.",
        tried.join(", ")
      ),
      Self::BackendFailed { backend, error } => {
        write!(f, "{backend} clipboard write failed: {error}")
      }
    }
  }
}

impl std::error::Error for ClipboardError {}

/// Copy `text` into the system clipboard. Returns the name of the
/// backend that succeeded (e.g. `arboard`, `wl-copy`) so the
/// caller can mention it in a toast.
pub fn write(text: &str) -> Result<&'static str, ClipboardError> {
  let mut tried: Vec<String> = Vec::new();

  // 1. Native arboard. We construct + use the clipboard inside
  // this function; on X11 arboard documents that the clipboard
  // contents may vanish when the owning process exits, so callers
  // that need long-lived contents must keep a `Clipboard` alive
  // (TUI's yank-and-show-toast workflow doesn't need that — the
  // user pastes within seconds).
  match arboard::Clipboard::new() {
    Ok(mut cb) => match cb.set_text(text.to_string()) {
      Ok(()) => return Ok("arboard"),
      Err(e) => {
        tried.push(format!("arboard ({e})"));
        log::debug!("arboard set_text failed: {e}; trying shell-out");
      }
    },
    Err(e) => {
      tried.push(format!("arboard ({e})"));
      log::debug!("arboard init failed: {e}; trying shell-out");
    }
  }

  // 2. Shell-out fallback. This is the path that catches
  // headless / exotic-compositor setups where arboard can't
  // attach. Try in priority order; first success wins.
  for backend in shell_fallback_backends() {
    if which_on_path(backend).is_none() {
      tried.push((*backend).to_string());
      continue;
    }
    match write_via(backend, text) {
      Ok(()) => return Ok(backend),
      Err(e) => {
        return Err(ClipboardError::BackendFailed {
          backend: (*backend).to_string(),
          error: e.to_string(),
        });
      }
    }
  }

  Err(ClipboardError::NoBackend { tried })
}

/// Shell-out backends in priority order. Wayland-first because
/// modern Linux desktops are usually Wayland; X tools work in
/// XWayland too but `wl-copy` is the native path. `pbcopy` only
/// matters on macOS.
fn shell_fallback_backends() -> Vec<&'static str> {
  if cfg!(target_os = "macos") {
    vec!["pbcopy"]
  } else {
    vec!["wl-copy", "xclip", "xsel"]
  }
}

/// Canonical argv tail for each shell-out clipboard backend. Public
/// (within the crate) so tests can snapshot the table and catch
/// silent reorders / flag drops.
pub(crate) fn backend_argv(backend: &str) -> &'static [&'static str] {
  match backend {
    // `xclip` defaults to PRIMARY; `-selection clipboard` puts the
    // text where Ctrl+V pastes. `xsel`'s `--input --clipboard` does
    // the same. `wl-copy` and `pbcopy` need no flags — they target
    // the clipboard by default.
    "xclip" => &["-selection", "clipboard"],
    "xsel" => &["--input", "--clipboard"],
    "wl-copy" => &[],
    "pbcopy" => &[],
    _ => &[],
  }
}

fn write_via(backend: &str, text: &str) -> std::io::Result<()> {
  let args: &[&str] = backend_argv(backend);
  let mut child = Command::new(backend)
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()?;
  if let Some(mut stdin) = child.stdin.take() {
    stdin.write_all(text.as_bytes())?;
  }
  let output = child.wait_with_output()?;
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    return Err(std::io::Error::other(format!(
      "{backend} exited {:?}: {stderr}",
      output.status.code()
    )));
  }
  Ok(())
}

/// `which`-style check that doesn't pull in the `which` crate's
/// canonicalisation overhead. Splits `$PATH` and probes for the
/// first executable hit.
fn which_on_path(bin: &str) -> Option<std::path::PathBuf> {
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    let candidate = dir.join(bin);
    if is_executable_file(&candidate) {
      return Some(candidate);
    }
  }
  None
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
  use std::os::unix::fs::PermissionsExt;
  match std::fs::metadata(path) {
    Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
    Err(_) => false,
  }
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
  path.is_file()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn no_backend_message_is_actionable() {
    let err = ClipboardError::NoBackend {
      tried: vec!["arboard (init failed)".into(), "wl-copy".into()],
    };
    let msg = err.to_string();
    assert!(msg.contains("Copy manually"));
    assert!(msg.contains("arboard"));
    assert!(msg.contains("wl-copy"));
  }

  #[test]
  fn backend_failed_message_names_backend_and_error() {
    let err = ClipboardError::BackendFailed {
      backend: "wl-copy".into(),
      error: "exited Some(1): No display".into(),
    };
    let msg = err.to_string();
    assert!(msg.starts_with("wl-copy clipboard write failed:"));
    assert!(msg.contains("No display"));
  }

  #[test]
  fn shell_fallback_backends_pick_correct_set_per_platform() {
    let backends = shell_fallback_backends();
    if cfg!(target_os = "macos") {
      assert_eq!(backends, vec!["pbcopy"]);
    } else {
      assert!(backends.contains(&"wl-copy"));
      assert!(backends.contains(&"xclip"));
      assert!(backends.contains(&"xsel"));
    }
  }

  /// Exercise the shell-out path directly via a tool we know is on
  /// $PATH on every dev box. Bypasses the arboard probe (which
  /// returns success on headed systems and would shadow the
  /// fallback we want to test).
  #[test]
  fn write_via_succeeds_with_a_real_binary() {
    write_via("cat", "hello").expect("cat should accept stdin and exit 0");
  }

  #[test]
  fn which_on_path_finds_cat_but_not_a_made_up_name() {
    assert!(which_on_path("cat").is_some());
    assert!(which_on_path("nonexistent-tool-9f3a-llamadash").is_none());
  }

  #[test]
  fn backend_argv_snapshot_per_backend() {
    // Pins the canonical flag set for each shell backend so a
    // silent reorder (or a removed `--clipboard`) is caught at
    // build time. The `wl-copy` and `pbcopy` paths have no flags;
    // listing the empty-slice case explicitly so an accidental
    // addition becomes a test failure.
    assert_eq!(backend_argv("xclip"), &["-selection", "clipboard"]);
    assert_eq!(backend_argv("xsel"), &["--input", "--clipboard"]);
    let empty: &[&str] = &[];
    assert_eq!(backend_argv("wl-copy"), empty);
    assert_eq!(backend_argv("pbcopy"), empty);
    // Unknown backends default to an empty argv (write_via still
    // tries to spawn them; that's the existing behaviour).
    assert_eq!(backend_argv("never-heard-of"), empty);
  }
}
