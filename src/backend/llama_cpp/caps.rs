//! What spellings *this* `llama-server` build accepts, asked of the build.
//!
//! Upstream removed `--mmap` / `--no-mmap` / `--mlock` / `-dio` in `14a9d09f7`
//! (2026-09-09) in favour of `-lm, --load-mode MODE`. Both spellings are in the
//! field at once: a host commonly has a current stock build beside older fork
//! builds pinned for a specific model, and a launch has to work on whichever it
//! resolves to.
//!
//! Version numbers cannot decide it — forks report their own strings
//! (`v1.5.2`, `b10715-mix`) and a wrapper script reports whatever it wraps — so
//! the probe reads `--help` and looks for the flag itself. Same shape as
//! [`super::list_devices::probe`]: one bounded subprocess per binary at boot,
//! and a binary that will not answer degrades to the older dialect rather than
//! failing the launch path.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// `--help` is a local, non-loading call; the budget only guards a binary that
/// hangs rather than prints. Matches the `--list-devices` probe.
const HELP_TIMEOUT: Duration = Duration::from_secs(10);

/// How a build spells "how should the model be loaded".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LoadModeDialect {
  /// `-lm, --load-mode {auto,none,mmap,mlock,mmap+mlock,dio}` — upstream from
  /// `e6dd0e29a` on.
  Enum,
  /// `--mmap` / `--no-mmap` / `--mlock` / `-dio` as separate flags. The default
  /// because it is what an unreachable or unparseable build most likely is: the
  /// flags were accepted for years and only removed on 2026-09-09.
  #[default]
  Flags,
}

impl LoadModeDialect {
  /// Read one binary's `--help` and decide which spelling it takes.
  ///
  /// Keyed on the literal `--load-mode`, which is what the new dialect is; a
  /// build advertising neither is left on [`Self::Flags`], where the engine
  /// itself will reject the argv with a message naming the flag. Guessing
  /// `Enum` there would turn a clear rejection into a confusing one.
  pub fn probe(binary: &Path) -> Self {
    let mut cmd = Command::new(binary);
    // The unified app takes server flags behind `serve`; `llama --help` prints
    // the dispatcher's own help, which lists no server flags at all.
    cmd.args(super::serve_prefix(binary));
    cmd.arg("--help");
    match crate::util::process::run_with_drain_and_timeout(cmd, HELP_TIMEOUT) {
      Ok(out) => {
        // Some builds print help on stderr, some on stdout; read both rather
        // than depending on which.
        let text = format!(
          "{}{}",
          String::from_utf8_lossy(&out.stdout),
          String::from_utf8_lossy(&out.stderr)
        );
        Self::from_help(&text)
      }
      Err(e) => {
        log::warn!(
          "`{} --help` failed: {e:?}; assuming the pre-`--load-mode` flags",
          binary.display()
        );
        Self::Flags
      }
    }
  }

  /// The parse, split out so tests drive it with captured help text instead of
  /// a binary.
  pub fn from_help(help: &str) -> Self {
    if help.contains("--load-mode") {
      Self::Enum
    } else {
      Self::Flags
    }
  }

  /// The `launch_config` value `compose` reads back.
  pub fn label(self) -> &'static str {
    match self {
      Self::Enum => "enum",
      Self::Flags => "flags",
    }
  }

  /// Inverse of [`Self::label`]. An unknown or absent value reads as
  /// [`Self::Flags`], so a launch whose params predate this key still emits
  /// something the older builds accept.
  pub fn from_label(label: Option<&str>) -> Self {
    match label {
      Some("enum") => Self::Enum,
      _ => Self::Flags,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Captured from `llama-server --help` on build 10892 (commit `e5a8d439c`).
  const NEW_HELP: &str = "\
-dt,   --defrag-thold N                 KV cache defragmentation threshold (DEPRECATED)
-lm,   --load-mode MODE                 model loading mode (default: auto)
                                        - auto: mmap, unless a device does not support it
                                        - none: no special loading mode
";

  /// Captured from the `q38rocm` fork build, which predates the removal.
  const OLD_HELP: &str = "\
       --mmap, --no-mmap                whether to memory-map model. (if mmap disabled, slower load but may
                                        (env: LLAMA_ARG_MMAP)
       --mlock                          force system to keep model in RAM
";

  #[test]
  fn a_build_advertising_load_mode_takes_the_enum() {
    assert_eq!(LoadModeDialect::from_help(NEW_HELP), LoadModeDialect::Enum);
  }

  #[test]
  fn a_build_advertising_the_old_flags_keeps_them() {
    assert_eq!(LoadModeDialect::from_help(OLD_HELP), LoadModeDialect::Flags);
  }

  /// A build that answers with nothing useful must not be guessed into the new
  /// dialect: the old flags were valid for years, so they are the safer default
  /// and the engine's own rejection stays legible if the guess is wrong.
  #[test]
  fn an_unreadable_help_falls_back_to_the_flags() {
    assert_eq!(LoadModeDialect::from_help(""), LoadModeDialect::Flags);
    assert_eq!(LoadModeDialect::default(), LoadModeDialect::Flags);
  }

  #[test]
  fn label_round_trips() {
    for d in [LoadModeDialect::Enum, LoadModeDialect::Flags] {
      assert_eq!(LoadModeDialect::from_label(Some(d.label())), d);
    }
    assert_eq!(
      LoadModeDialect::from_label(None),
      LoadModeDialect::Flags,
      "params written before this key existed must still emit a spelling the \
       older builds accept"
    );
  }
}
