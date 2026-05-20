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

// Bold / underline-bold helpers were removed once `cli::format` took
// over header rendering for tables (`format::table` does its own bold
// directly via `console::style`) and section titles
// (`format::section_header`). If a future call site needs a one-off
// bold span, prefer `console::style(s).bold().to_string()` inline; if
// it shows up in two places, lift it back into this module.

// ─────────────────────────────────────────────────────────────────────
// Semantic value helpers
//
// These exist so the table / kv_block / single-line renderers stay
// "shape only" — *they* decide layout, *these helpers* decide which
// color a given kind of value gets. Future tweaks ("ready should be
// teal, not green") land here without touching any call site.
// ─────────────────────────────────────────────────────────────────────

/// Color a model-launch / supervisor state cell.
///
/// Mapping mirrors the supervisor lifecycle (`Launching` → `Loading`
/// → `Ready` → `Stopping` → `Stopped`, plus `Error`). Unknown states
/// pass through plain so a new supervisor variant never crashes the
/// renderer; future variants can be added here without revisiting
/// every call site.
pub(crate) fn state(s: &str) -> String {
  // Trim leading/trailing whitespace so callers don't have to pre-clean
  // the daemon's response strings.
  let trimmed = s.trim();
  let styled = match trimmed.to_ascii_lowercase().as_str() {
    "ready" => console::style(trimmed).green().bold(),
    "launching" | "loading" | "stopping" => console::style(trimmed).yellow(),
    "stopped" | "ext" | "external" => console::style(trimmed).dim(),
    "error" => console::style(trimmed).red().bold(),
    _ => console::style(trimmed),
  };
  styled.to_string()
}

/// Color a port number. Cyan matches the init diff preview's path
/// styling so the two surfaces feel like one identity.
pub(crate) fn port(n: u16) -> String {
  console::style(n.to_string()).cyan().to_string()
}

/// Color a launch id (`L3`, `ext-1234`). Bold magenta gives high
/// contrast against state-green and path-dim cells in the same row.
pub(crate) fn launch_id(id: &str) -> String {
  console::style(id).magenta().bold().to_string()
}

/// Render a filesystem path with the user's `$HOME` collapsed to `~`
/// when colors are enabled. Non-TTY / colors-disabled paths return
/// verbatim so byte-for-byte TSV snapshots stay stable.
///
/// The collapse is only cosmetic — it never changes the bytes piped
/// through scripts, only what humans see on a terminal.
pub(crate) fn path(p: &str) -> String {
  if !console::colors_enabled() {
    return p.to_string();
  }
  collapse_home(p)
}

fn collapse_home(p: &str) -> String {
  let Some(home) = crate::util::paths::home_dir() else {
    return p.to_string();
  };
  let home_str = home.to_string_lossy();
  // Refuse to collapse `/` or an empty home to avoid mangling root
  // paths like `/etc/...`.
  if home_str.is_empty() || home_str == "/" {
    return p.to_string();
  }
  // Match either exactly `$HOME` or `$HOME/...`. A `/foo` path whose
  // prefix happens to share bytes with `$HOMEx` must not collapse.
  if p == home_str {
    return "~".to_string();
  }
  let with_sep = format!("{home_str}/");
  if let Some(rest) = p.strip_prefix(&with_sep) {
    return format!("~/{rest}");
  }
  p.to_string()
}

/// Dim "(N noun)" suffix used by `format::section_header` and
/// trailing footers on empty / non-empty list output.
pub(crate) fn count(n: usize, noun: &str) -> String {
  console::style(format!("({n} {noun})")).dim().to_string()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::test_lock::serialize;
  use std::sync::MutexGuard;

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
  fn dim_emits_content_unchanged_when_colors_off() {
    let _g = EnvGuard::capture();
    console::set_colors_enabled(false);
    assert_eq!(dim("note"), "note");
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

  #[test]
  fn state_helper_passes_through_text_in_both_modes() {
    let _g = EnvGuard::capture();
    for enabled in [true, false] {
      console::set_colors_enabled(enabled);
      let ready = state("ready");
      assert_eq!(console::strip_ansi_codes(&ready), "ready");
      let err = state("error");
      assert_eq!(console::strip_ansi_codes(&err), "error");
      let unknown = state("brand-new");
      assert_eq!(console::strip_ansi_codes(&unknown), "brand-new");
    }
  }

  #[test]
  fn state_helper_trims_whitespace() {
    let _g = EnvGuard::capture();
    console::set_colors_enabled(false);
    assert_eq!(state(" ready "), "ready");
  }

  #[test]
  fn port_helper_renders_number_in_both_modes() {
    let _g = EnvGuard::capture();
    for enabled in [true, false] {
      console::set_colors_enabled(enabled);
      let s = port(41100);
      assert_eq!(console::strip_ansi_codes(&s), "41100");
    }
  }

  #[test]
  fn launch_id_helper_renders_id_in_both_modes() {
    let _g = EnvGuard::capture();
    for enabled in [true, false] {
      console::set_colors_enabled(enabled);
      let s = launch_id("L3");
      assert_eq!(console::strip_ansi_codes(&s), "L3");
    }
  }

  #[test]
  fn path_helper_collapses_home_prefix_on_tty_only() {
    let _g = EnvGuard::capture();
    // The collapse is best-effort: we don't override `$HOME` in tests
    // (would race other tests), so just assert the helper at least
    // returns the input unchanged when colors are off, and a string
    // that round-trips through strip_ansi_codes back to a non-empty
    // path when on. Stronger collapse coverage lives in the unit test
    // for `collapse_home` below.
    console::set_colors_enabled(false);
    assert_eq!(path("/etc/foo"), "/etc/foo");
    console::set_colors_enabled(true);
    let rendered = path("/etc/foo");
    let collapsed = console::strip_ansi_codes(&rendered);
    assert!(collapsed.contains("foo"));
  }

  #[test]
  fn collapse_home_substitutes_tilde_and_leaves_other_paths_alone() {
    let prior = std::env::var_os("HOME");
    std::env::set_var("HOME", "/home/alice");
    // `directories::BaseDirs` is what `paths::home_dir()` reads, and
    // it caches via `BaseDirs::new()`. On Linux it consults `$HOME`
    // directly, so the override above takes effect.
    assert_eq!(collapse_home("/home/alice"), "~");
    assert_eq!(collapse_home("/home/alice/work/x"), "~/work/x");
    // A path that shares a prefix substring but isn't actually under
    // $HOME must not collapse.
    assert_eq!(collapse_home("/home/alicex/y"), "/home/alicex/y");
    assert_eq!(collapse_home("/etc/passwd"), "/etc/passwd");
    match prior {
      Some(v) => std::env::set_var("HOME", v),
      None => std::env::remove_var("HOME"),
    }
  }

  #[test]
  fn count_helper_renders_suffix_in_both_modes() {
    let _g = EnvGuard::capture();
    for enabled in [true, false] {
      console::set_colors_enabled(enabled);
      let s = count(3, "models");
      assert_eq!(console::strip_ansi_codes(&s), "(3 models)");
    }
  }
}
