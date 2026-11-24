//! Single source of truth for every display glyph the TUI paints.
//! Two variants ship: [`GlyphSet::unicode`] (the project's default
//! house style) and [`GlyphSet::ascii`] (a `7`-bit fallback for
//! terminals or fonts that render the Unicode set as tofu — legacy
//! conhost, minimal SSH fonts, some CI ptys).
//!
//! The fallback is **opt-in**: `LLAMASTASH_ASCII=1` (env wins, per the
//! project's env-truthy convention) or the `ascii_glyphs` config key.
//! Auto-detecting glyph support is unreliable, so a documented override
//! is the pragmatic call. When neither is set the Unicode set renders
//! byte-for-byte unchanged.
//!
//! Selected once at startup via [`init`]; render sites read the active
//! set through [`active`]. Until `init` runs (unit tests, the golden
//! render harness, any non-interactive caller) [`active`] returns the
//! Unicode set.
//!
//! Severity stays **double-encoded** (shape + colour) in both sets: the
//! ASCII set keeps distinct warning (`!`) and critical (`*`) markers so
//! the tier survives a colour-stripped terminal.

use std::sync::OnceLock;

use crate::tui::status_icons::SurfaceState;

/// Named, accessor-only glyph table. Both variants implement the same
/// accessors so every render site is glyph-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphSet {
  /// Per-`SurfaceState` status dot painted in the model list and the
  /// right-pane header. Index by the `SurfaceState` discriminant via
  /// [`Self::status_icon`].
  launching: char,
  loading: char,
  ready: char,
  stopped: char,
  error: char,
  external: char,
  /// Warning-tier severity marker (yellow). Distinct shape from
  /// `severity_critical` so the tier reads without colour.
  severity_warn: &'static str,
  /// Critical-tier severity marker (red).
  severity_critical: &'static str,
  /// Animated spinner frames cycled while a launch is loading.
  spinner: &'static [&'static str],
  /// Trailing-truncation marker (`…` / `...`).
  ellipsis: &'static str,
  /// Bare separator glyph drawn between header / hint segments.
  middot: &'static str,
  /// Space-padded form of [`Self::middot`] (`" · "` / `" - "`).
  middot_sep: &'static str,
  /// Section-header marker (`▶ Running`).
  section_marker: &'static str,
  /// Left / right affordance flanking a Left/Right-cycled value.
  cycle_left: &'static str,
  cycle_right: &'static str,
  /// Two-cell marker pointing at the focused / editable form row
  /// (`"→ "` / `"> "`).
  focus_marker: &'static str,
  /// Favorite marker (also the `★ Favorites` section icon).
  star: &'static str,
  /// `↺ Recent` section icon.
  recent: &'static str,
  /// Single-cell text-input caret.
  caret: &'static str,
  /// Vision / audio multimodal capability markers.
  vision: char,
  audio: char,
  /// Temperature degree sign.
  degree: &'static str,
  /// Empty-value / "nothing here" placeholder (`—` / `-`).
  placeholder: &'static str,
  /// Horizontal divider used for full-width rules.
  hline: &'static str,
  /// Vertical separator between inline chips.
  vline: &'static str,
  /// Bar-gauge fill / trough cells (host stats pane).
  gauge_fill: char,
  gauge_trough: char,
  /// Whether box-drawing borders are available. Drives the border
  /// symbol set in [`crate::theme::Palette::panel_block`] and the
  /// open-coded `Block` sites: Unicode uses ratatui's rounded set,
  /// ASCII falls back to `+ - |`.
  rounded_borders: bool,
  /// TUI logo banner — the wide ASCII-art head only renders at
  /// ≥120 cols. The ASCII variant keeps the same line count so the
  /// width-gated render math is unchanged.
  banner: &'static str,
  /// Compact logo monogram for the narrower logo panel.
  compact_banner: &'static str,
}

const UNICODE_SPINNER: &[&str] = &["◐", "◓", "◑", "◒"];
const ASCII_SPINNER: &[&str] = &["|", "/", "-", "\\"];

/// `+ - |` border set for the ASCII fallback. Mirrors ratatui's
/// `border::PLAIN` shape with `7`-bit corners/edges.
const ASCII_BORDER_SET: ratatui::symbols::border::Set<'static> = ratatui::symbols::border::Set {
  top_left: "+",
  top_right: "+",
  bottom_left: "+",
  bottom_right: "+",
  vertical_left: "|",
  vertical_right: "|",
  horizontal_top: "-",
  horizontal_bottom: "-",
};

impl GlyphSet {
  /// The project's default house style. Geometric status dots,
  /// severity triangles, rounded box borders, the moon spinner.
  pub const fn unicode() -> Self {
    Self {
      launching: '◌',
      loading: '◐',
      ready: '●',
      stopped: '○',
      error: '▲',
      external: '⇪',
      severity_warn: "△",
      severity_critical: "▲",
      spinner: UNICODE_SPINNER,
      ellipsis: "…",
      middot: "·",
      middot_sep: " · ",
      section_marker: "▶",
      cycle_left: "◀",
      cycle_right: "▶",
      focus_marker: "→ ",
      star: "★",
      recent: "↺",
      caret: "▏",
      vision: '◉',
      audio: '♪',
      degree: "°",
      placeholder: "—",
      hline: "─",
      vline: "│",
      gauge_fill: '█',
      gauge_trough: '░',
      rounded_borders: true,
      banner: crate::banner::BANNER,
      compact_banner: crate::banner::COMPACT_BANNER,
    }
  }

  /// `7`-bit ASCII fallback. Severity stays double-encoded: `!`
  /// (warning) vs `*` (critical) read distinctly without colour.
  pub const fn ascii() -> Self {
    Self {
      launching: 'o',
      loading: 'O',
      ready: '*',
      stopped: '.',
      error: '!',
      external: '^',
      severity_warn: "!",
      severity_critical: "*",
      spinner: ASCII_SPINNER,
      ellipsis: "...",
      middot: "-",
      middot_sep: " - ",
      section_marker: ">",
      cycle_left: "<",
      cycle_right: ">",
      focus_marker: "> ",
      star: "*",
      recent: "~",
      caret: "|",
      vision: 'V',
      audio: 'A',
      // Empty: the literal `C` unit suffix follows at the call site, so
      // this yields `82C` rather than `82°C`.
      degree: "",
      placeholder: "-",
      hline: "-",
      vline: "|",
      gauge_fill: '#',
      gauge_trough: '.',
      rounded_borders: false,
      banner: crate::banner::BANNER_ASCII,
      compact_banner: crate::banner::COMPACT_BANNER_ASCII,
    }
  }

  /// Resolve which set is active from the env flag and the config
  /// flag. The env wins (project env-truthy convention): a set
  /// `LLAMASTASH_ASCII=1` forces ASCII regardless of config, and the
  /// config flag only takes effect when the env var is absent. Pure —
  /// no global state — so the precedence is unit-testable.
  pub const fn from_env_and_config(ascii_env: bool, ascii_cfg: bool) -> Self {
    if ascii_env || ascii_cfg {
      Self::ascii()
    } else {
      Self::unicode()
    }
  }

  /// Status dot for a [`SurfaceState`]. Keeps the dual encoding in
  /// lock-step with `status_icons::colour_for`.
  pub fn status_icon(&self, state: SurfaceState) -> char {
    match state {
      SurfaceState::NotLaunched => ' ',
      SurfaceState::Launching => self.launching,
      SurfaceState::Loading => self.loading,
      SurfaceState::Ready => self.ready,
      SurfaceState::Error => self.error,
      SurfaceState::Stopped => self.stopped,
      SurfaceState::External => self.external,
    }
  }

  pub fn severity_warn(&self) -> &'static str {
    self.severity_warn
  }
  pub fn severity_critical(&self) -> &'static str {
    self.severity_critical
  }
  pub fn spinner_frames(&self) -> &'static [&'static str] {
    self.spinner
  }
  pub fn ellipsis(&self) -> &'static str {
    self.ellipsis
  }
  pub fn middot(&self) -> &'static str {
    self.middot
  }
  /// The `" · "` (Unicode) / `" - "` (ASCII) hint / readout separator,
  /// space-padded both sides. The common form most hint strips use.
  pub fn middot_sep(&self) -> &'static str {
    self.middot_sep
  }
  pub fn section_marker(&self) -> &'static str {
    self.section_marker
  }
  pub fn cycle_left(&self) -> &'static str {
    self.cycle_left
  }
  pub fn cycle_right(&self) -> &'static str {
    self.cycle_right
  }
  pub fn focus_marker(&self) -> &'static str {
    self.focus_marker
  }
  pub fn star(&self) -> &'static str {
    self.star
  }
  pub fn recent(&self) -> &'static str {
    self.recent
  }
  pub fn caret(&self) -> &'static str {
    self.caret
  }
  pub fn vision(&self) -> char {
    self.vision
  }
  pub fn audio(&self) -> char {
    self.audio
  }
  pub fn degree(&self) -> &'static str {
    self.degree
  }
  pub fn placeholder(&self) -> &'static str {
    self.placeholder
  }
  pub fn hline(&self) -> &'static str {
    self.hline
  }
  pub fn vline(&self) -> &'static str {
    self.vline
  }
  pub fn gauge_fill(&self) -> char {
    self.gauge_fill
  }
  pub fn gauge_trough(&self) -> char {
    self.gauge_trough
  }
  pub fn rounded_borders(&self) -> bool {
    self.rounded_borders
  }

  /// Box-drawing set for `Borders::ALL` panels. Unicode returns
  /// ratatui's `PLAIN` set (the implicit default, so the Unicode path
  /// is byte-identical); ASCII returns a `+ - |` set.
  pub fn border_set(&self) -> ratatui::symbols::border::Set<'static> {
    if self.rounded_borders {
      ratatui::symbols::border::PLAIN
    } else {
      ASCII_BORDER_SET
    }
  }
  pub fn banner(&self) -> &'static str {
    self.banner
  }
  pub fn compact_banner(&self) -> &'static str {
    self.compact_banner
  }
}

static ACTIVE: OnceLock<GlyphSet> = OnceLock::new();

/// Select the active set once, at TUI startup. Idempotent: a second
/// call is a no-op (the first selection wins), so the interactive and
/// `--render` entry points can both call it without ordering concerns.
pub fn init(ascii_env: bool, ascii_cfg: bool) {
  let _ = ACTIVE.set(GlyphSet::from_env_and_config(ascii_env, ascii_cfg));
}

/// The active glyph set. Returns the Unicode set until [`init`] runs,
/// so every test and non-interactive render path keeps the default
/// house style with zero setup.
pub fn active() -> GlyphSet {
  *ACTIVE.get().unwrap_or(&GlyphSet::unicode())
}

/// Resolve the `LLAMASTASH_ASCII` env flag using the project's
/// env-truthy convention (`1` / `true` / `yes`). Absent / empty / any
/// other value is falsy.
pub fn ascii_env() -> bool {
  std::env::var("LLAMASTASH_ASCII")
    .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn unicode_default_keeps_house_style() {
    let g = GlyphSet::unicode();
    assert_eq!(g.status_icon(SurfaceState::Ready), '●');
    assert_eq!(g.status_icon(SurfaceState::Error), '▲');
    assert_eq!(g.ellipsis(), "…");
    assert_eq!(g.section_marker(), "▶");
    assert!(g.rounded_borders());
  }

  #[test]
  fn ascii_set_is_pure_ascii_for_every_accessor() {
    let g = GlyphSet::ascii();
    let mut blob = String::new();
    for s in [
      SurfaceState::NotLaunched,
      SurfaceState::Launching,
      SurfaceState::Loading,
      SurfaceState::Ready,
      SurfaceState::Error,
      SurfaceState::Stopped,
      SurfaceState::External,
    ] {
      blob.push(g.status_icon(s));
    }
    blob.push_str(g.severity_warn());
    blob.push_str(g.severity_critical());
    for f in g.spinner_frames() {
      blob.push_str(f);
    }
    blob.push_str(g.ellipsis());
    blob.push_str(g.middot());
    blob.push_str(g.middot_sep());
    blob.push_str(g.section_marker());
    blob.push_str(g.cycle_left());
    blob.push_str(g.cycle_right());
    blob.push_str(g.focus_marker());
    blob.push_str(g.star());
    blob.push_str(g.recent());
    blob.push_str(g.caret());
    blob.push(g.vision());
    blob.push(g.audio());
    blob.push_str(g.degree());
    blob.push_str(g.placeholder());
    blob.push_str(g.hline());
    blob.push_str(g.vline());
    blob.push(g.gauge_fill());
    blob.push(g.gauge_trough());
    blob.push_str(g.banner());
    blob.push_str(g.compact_banner());
    assert!(
      blob.is_ascii(),
      "every ASCII-set accessor must stay 7-bit, found non-ASCII in: {blob:?}"
    );
    assert!(!g.rounded_borders());
  }

  #[test]
  fn severity_markers_differ_in_both_sets() {
    for g in [GlyphSet::unicode(), GlyphSet::ascii()] {
      assert_ne!(
        g.severity_warn(),
        g.severity_critical(),
        "warning and critical must stay distinct on shape, not just colour"
      );
    }
  }

  #[test]
  fn from_env_and_config_env_wins() {
    // Env forces ASCII regardless of config.
    assert_eq!(
      GlyphSet::from_env_and_config(true, false),
      GlyphSet::ascii()
    );
    assert_eq!(GlyphSet::from_env_and_config(true, true), GlyphSet::ascii());
    // Config alone still selects ASCII when env is absent.
    assert_eq!(
      GlyphSet::from_env_and_config(false, true),
      GlyphSet::ascii()
    );
    // Neither set: Unicode default.
    assert_eq!(
      GlyphSet::from_env_and_config(false, false),
      GlyphSet::unicode()
    );
  }

  #[test]
  fn active_defaults_to_unicode_without_init() {
    // The golden render harness and every unit test rely on this: an
    // uninitialised global must paint the Unicode house style.
    assert_eq!(active(), GlyphSet::unicode());
  }

  #[test]
  fn unicode_status_icons_stay_distinct() {
    use std::collections::HashSet;
    let g = GlyphSet::unicode();
    let icons: HashSet<char> = [
      SurfaceState::Launching,
      SurfaceState::Loading,
      SurfaceState::Ready,
      SurfaceState::Error,
      SurfaceState::Stopped,
      SurfaceState::External,
    ]
    .into_iter()
    .map(|s| g.status_icon(s))
    .collect();
    assert_eq!(icons.len(), 6, "each state needs a unique dot");
  }

  #[test]
  fn ascii_status_icons_stay_distinct() {
    use std::collections::HashSet;
    let g = GlyphSet::ascii();
    let icons: HashSet<char> = [
      SurfaceState::Launching,
      SurfaceState::Loading,
      SurfaceState::Ready,
      SurfaceState::Error,
      SurfaceState::Stopped,
      SurfaceState::External,
    ]
    .into_iter()
    .map(|s| g.status_icon(s))
    .collect();
    assert_eq!(icons.len(), 6, "ASCII states must stay distinguishable");
  }
}
