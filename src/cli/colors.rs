//! Process-global color policy for the CLI.
//!
//! `init` is called exactly once from `cli::dispatch` before any
//! subcommand handler runs. The three OR-ed off-conditions are:
//!
//! 1. `--no-colors` global flag,
//! 2. `NO_COLOR` env var present and non-empty (per https://no-color.org),
//! 3. stdout is not attached to a terminal (piped / redirected).
//!
//! Any one of these silences ANSI escapes for every helper here and
//! every direct `console::style(...)` site downstream. Helpers return
//! `String`; when colors are disabled the `console` crate transparently
//! emits the plain text form, so callers don't branch.
//!
//! JSON output paths must never wrap their bytes in these helpers —
//! `--json` is a machine contract and stays byte-for-byte stable.
//!
//! Module is `pub(crate)`; nothing outside the binary consumes it.
//!
//! # Why this lives in one place
//!
//! The policy has to be initialised before any output happens, and a
//! later fourth condition (e.g. `LLAMASTASH_FORCE_COLOR`) would land
//! here without revisiting every print site.

use std::io::IsTerminal;

/// Initialise the process-wide color policy. Call exactly once from
/// `cli::dispatch` before any subcommand handler runs.
///
/// `console::set_colors_enabled` is process-global; tests that need a
/// specific state set it explicitly within their own scope rather than
/// relying on call ordering.
pub(crate) fn init(no_colors_flag: bool) {
  let off = no_colors_flag || no_color_env_disables() || !std::io::stdout().is_terminal();
  console::set_colors_enabled(!off);
}

/// `NO_COLOR` triggers when the variable is present AND non-empty.
/// An empty value (`NO_COLOR=`) does NOT disable colors, matching the
/// official spec at https://no-color.org/.
fn no_color_env_disables() -> bool {
  std::env::var_os("NO_COLOR")
    .map(|v| !v.is_empty())
    .unwrap_or(false)
}

pub(crate) fn success(msg: &str) -> String {
  format!(
    "{} {}",
    console::style("✓").green().bold(),
    console::style(msg).green().bold()
  )
}

pub(crate) fn error(msg: &str) -> String {
  format!(
    "{} {}",
    console::style("✗").red().bold(),
    console::style(msg).red().bold()
  )
}

pub(crate) fn warning(msg: &str) -> String {
  format!(
    "{} {}",
    console::style("!").yellow(),
    console::style(msg).yellow()
  )
}

pub(crate) fn dim(msg: &str) -> String {
  console::style(msg).dim().to_string()
}

/// Bold without underline — for table column headers where underline
/// would visually overlap row borders. Use `header` for section titles.
pub(crate) fn bold(msg: &str) -> String {
  console::style(msg).bold().to_string()
}

pub(crate) fn header(msg: &str) -> String {
  console::style(msg).bold().underlined().to_string()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Mutex, MutexGuard, OnceLock};

  /// Process-global color state and the `NO_COLOR` env var are both
  /// shared across the cargo-test thread pool. The mutex serialises
  /// every test in this module so neither raceably leaks between runs.
  /// Poisoned guards are unwrapped — a panic in one test should not
  /// silently disable the next.
  fn serialize() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .unwrap_or_else(|poison| poison.into_inner())
  }

  /// RAII guard that snapshots the color-enabled flag plus the
  /// `NO_COLOR` env var on construction and restores both on drop.
  struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    prior_colors: bool,
    prior_no_color: Option<std::ffi::OsString>,
  }

  impl EnvGuard {
    fn capture() -> Self {
      Self {
        _lock: serialize(),
        prior_colors: console::colors_enabled(),
        prior_no_color: std::env::var_os("NO_COLOR"),
      }
    }
  }

  impl Drop for EnvGuard {
    fn drop(&mut self) {
      console::set_colors_enabled(self.prior_colors);
      match &self.prior_no_color {
        Some(v) => std::env::set_var("NO_COLOR", v),
        None => std::env::remove_var("NO_COLOR"),
      }
    }
  }

  #[test]
  fn init_true_always_disables_colors() {
    let _g = EnvGuard::capture();
    std::env::remove_var("NO_COLOR");
    init(true);
    assert!(!console::colors_enabled());
  }

  #[test]
  fn no_color_set_to_nonempty_value_disables() {
    let _g = EnvGuard::capture();
    std::env::set_var("NO_COLOR", "1");
    assert!(no_color_env_disables());
  }

  #[test]
  fn no_color_empty_value_does_not_disable() {
    // Spec: empty value is treated as unset.
    let _g = EnvGuard::capture();
    std::env::set_var("NO_COLOR", "");
    assert!(!no_color_env_disables());
  }

  #[test]
  fn no_color_unset_does_not_disable() {
    let _g = EnvGuard::capture();
    std::env::remove_var("NO_COLOR");
    assert!(!no_color_env_disables());
  }

  #[test]
  fn success_helper_carries_glyph_and_text_in_both_modes() {
    let _g = EnvGuard::capture();
    for enabled in [true, false] {
      console::set_colors_enabled(enabled);
      let rendered = success("ok");
      let plain = console::strip_ansi_codes(&rendered);
      assert!(plain.contains('✓'), "expected ✓ in plain form `{plain}`");
      assert!(
        plain.contains("ok"),
        "expected `ok` in plain form `{plain}`"
      );
    }
  }

  #[test]
  fn error_helper_carries_glyph_and_text_in_both_modes() {
    let _g = EnvGuard::capture();
    for enabled in [true, false] {
      console::set_colors_enabled(enabled);
      let rendered = error("bad");
      let plain = console::strip_ansi_codes(&rendered);
      assert!(plain.contains('✗'), "expected ✗ in plain form `{plain}`");
      assert!(
        plain.contains("bad"),
        "expected `bad` in plain form `{plain}`"
      );
    }
  }

  #[test]
  fn warning_helper_renders_text_with_glyph() {
    let _g = EnvGuard::capture();
    console::set_colors_enabled(false);
    let rendered = warning("watch out");
    assert!(rendered.contains('!'));
    assert!(rendered.contains("watch out"));
  }

  #[test]
  fn dim_and_bold_and_header_emit_content() {
    let _g = EnvGuard::capture();
    console::set_colors_enabled(false);
    assert_eq!(dim("note"), "note");
    assert_eq!(bold("Header"), "Header");
    assert_eq!(header("Title"), "Title");
  }

  /// T-03 (testing review): exercise the OR composition in `init()`,
  /// not just `no_color_env_disables` in isolation. With `NO_COLOR=1`
  /// set, `init(false)` must still disable colors.
  #[test]
  fn init_false_with_no_color_env_disables() {
    let _g = EnvGuard::capture();
    std::env::set_var("NO_COLOR", "1");
    init(false);
    assert!(!console::colors_enabled());
  }
}
