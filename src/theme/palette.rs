use std::str::FromStr;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString};

/// Named themes shipped with llamadash v1.
///
/// String forms accept both the short name and the conventional long name
/// where one exists (e.g. `macchiato` and `catppuccin-macchiato`).
#[derive(
  Clone,
  Copy,
  Debug,
  Default,
  PartialEq,
  Eq,
  Hash,
  EnumIter,
  EnumString,
  Display,
  Serialize,
  Deserialize,
)]
#[strum(ascii_case_insensitive)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
  #[strum(serialize = "macchiato", serialize = "catppuccin-macchiato")]
  #[serde(alias = "catppuccin-macchiato")]
  #[default]
  Macchiato,
  #[strum(serialize = "latte", serialize = "catppuccin-latte")]
  #[serde(alias = "catppuccin-latte")]
  Latte,
  #[strum(serialize = "gruvbox-dark", serialize = "gruvbox")]
  #[serde(alias = "gruvbox")]
  GruvboxDark,
  #[strum(serialize = "solarized-dark", serialize = "solarized")]
  #[serde(alias = "solarized")]
  SolarizedDark,
  #[strum(serialize = "mono", serialize = "monochrome")]
  #[serde(alias = "monochrome")]
  Mono,
  /// User-defined theme loaded from `config.yaml`'s `custom_theme:`
  /// block. The actual palette is built at startup (see
  /// `crate::theme::custom::CustomThemeConfig::resolve`) and lives on
  /// `App.options.custom_palette`; `palette_for(Custom)` returns the
  /// macchiato palette as a benign fallback for code paths that don't
  /// have an App in scope.
  Custom,
}

impl ThemeName {
  /// Canonical kebab-case identifier (used in config files and CLI args).
  /// Mirrors the first `#[strum(serialize = ...)]` attribute on each variant,
  /// which is what `to_string()` already produces via the derived `Display`.
  pub fn canonical(self) -> String {
    self.to_string()
  }

  /// Short display name — what the Logo panel and any chip-style UI
  /// surface uses when the canonical form ("catppuccin-macchiato")
  /// would overflow. Mapped explicitly so changes to the strum
  /// serializer order don't silently lengthen the chip.
  pub fn short_name(self) -> &'static str {
    match self {
      ThemeName::Macchiato => "macchiato",
      ThemeName::Latte => "latte",
      ThemeName::GruvboxDark => "gruvbox",
      ThemeName::SolarizedDark => "solarized",
      ThemeName::Mono => "mono",
      ThemeName::Custom => "custom",
    }
  }

  /// Parse a theme name from a user-supplied string, returning a structured
  /// error when the value is unknown. Useful for surfacing actionable
  /// validation errors from the config loader.
  pub fn parse(input: &str) -> Result<Self, UnknownThemeError> {
    Self::from_str(input).map_err(|_| UnknownThemeError {
      value: input.to_string(),
    })
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownThemeError {
  pub value: String,
}

impl std::fmt::Display for UnknownThemeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let valid: Vec<String> = <ThemeName as strum::IntoEnumIterator>::iter()
      .map(|t| t.to_string())
      .collect();
    write!(
      f,
      "unknown theme '{}' (valid: {})",
      self.value,
      valid.join(", ")
    )
  }
}

impl std::error::Error for UnknownThemeError {}

/// A self-contained colour palette used by the TUI.
///
/// Slots are *semantic*, not visual: `accent` is whatever the theme uses for
/// "primary action" highlighting, `muted` is for secondary text like
/// directory group labels, and so on. Renderers pick a slot by meaning, not
/// by colour name, so theme swaps don't require call-site changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
  pub name: ThemeName,
  pub is_dark: bool,
  pub bg: Color,
  pub fg: Color,
  pub accent: Color,
  pub success: Color,
  pub warning: Color,
  pub error: Color,
  pub muted: Color,
  pub selection: Color,
  /// Background colour used to highlight the focused row in the
  /// Models pane. KDash-style: a saturated warm tone (gold / amber)
  /// so the active row pops against the panel border. Text on the
  /// highlighted row is painted in `bg` for high contrast. Themes
  /// that prefer the legacy REVERSED behaviour (e.g. mono) can set
  /// this to `Color::Reset` and the list renderer falls back to
  /// `Modifier::REVERSED`.
  pub highlight: Color,
  /// Foreground for block titles (` Host `, ` Daemon `, ` Models `).
  /// Distinct from `accent` so titles can pop in a different hue
  /// from the panel borders (e.g. yellow titles on mauve borders).
  /// Renderers also bold this slot.
  pub panel_title: Color,
  /// Foreground for label prefixes inside a panel (`CPU  `,
  /// `socket  `, list group headers like `★ Favorites` and folder
  /// paths). Distinct from `muted` because `muted` doubles as the
  /// "secondary text / hint divider" tone — separating the two lets
  /// labels read as legible, scannable category markers without
  /// also brightening every `·` separator.
  pub label: Color,
  /// Foreground for content painted on top of `accent`-backed
  /// surfaces (the top title bar). Most themes can reuse `bg` here
  /// because the accent is saturated enough that the panel-bg tone
  /// reads as dark text on a coloured bar. Themes whose `bg` is
  /// `Color::Reset` (mono) must override with a concrete colour
  /// (Black) because `Reset` falls through to the terminal default,
  /// which on a dark terminal renders as light-on-light over the
  /// White accent bar.
  pub on_accent: Color,
  pub status_loading: Color,
  pub status_ready: Color,
  pub status_error: Color,
  pub status_stopped: Color,
  pub status_external: Color,
}

impl Palette {
  // ─── Single-slot style helpers ────────────────────────────────
  //
  // Each helper returns `Style::default().fg(palette.<slot>)`. They
  // exist so render callers don't repeat the 6-token incantation
  // 100+ times across the TUI tree, and so a slot rename here
  // (e.g. dropping `panel_title` for `title`) is a one-line change.
  //
  // BOLD-bearing helpers (`title_style`, `accent_bold_style`) keep
  // the modifier together with the slot so emphasized labels stay
  // visually consistent even when a theme leaves bold off the
  // panel-title slot.

  /// `palette.fg` foreground for primary body text.
  pub fn text_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.fg)
  }

  /// `palette.muted` foreground for secondary text (dividers,
  /// timestamps, dimmed metadata).
  pub fn muted_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.muted)
  }

  /// `palette.accent` foreground — primary-action highlight, panel
  /// borders, focus indicators.
  pub fn accent_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.accent)
  }

  /// `palette.label` foreground for label prefixes (`CPU  `,
  /// `socket  `, group headers).
  pub fn label_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.label)
  }

  /// `palette.error` foreground for error chrome / destructive
  /// confirmations.
  pub fn error_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.error)
  }

  /// `palette.warning` foreground.
  pub fn warning_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.warning)
  }

  /// `palette.success` foreground.
  pub fn success_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(self.success)
  }

  /// `palette.panel_title` foreground + BOLD modifier. The
  /// canonical block-title style — used by `panel_block` and by
  /// the right-pane title strip / list-pane filter chip.
  pub fn title_style(&self) -> ratatui::style::Style {
    ratatui::style::Style::default()
      .fg(self.panel_title)
      .add_modifier(ratatui::style::Modifier::BOLD)
  }

  /// Build a standard panel block with consistent border + title
  /// styling. `focused` switches the border to `palette.accent`;
  /// otherwise it uses `palette.muted`. The title is painted
  /// through `fmt::panel_title` for the same `BOLD + panel_title`
  /// shape every panel uses.
  pub fn panel_block(&self, title: &str, focused: bool) -> ratatui::widgets::Block<'static> {
    let border = if focused { self.accent } else { self.muted };
    ratatui::widgets::Block::default()
      .borders(ratatui::widgets::Borders::ALL)
      .border_style(ratatui::style::Style::default().fg(border))
      .title(ratatui::text::Line::from(ratatui::text::Span::styled(
        title.to_string(),
        self.title_style(),
      )))
  }

  /// Border colour for focus-aware panes. Focused panes adopt
  /// `palette.highlight` so the active pane reads with the same
  /// warm "this row is live" tone the list selection uses; the
  /// fallback to `palette.accent` covers themes whose `highlight`
  /// is `Color::Reset` (Mono opts out so the focus border doesn't
  /// disappear into the terminal default). Centralised here so
  /// every pane-border render goes through the palette — no
  /// hard-coded `Color::Yellow` left in the renderers.
  pub fn focus_border(&self, focused: bool) -> ratatui::style::Color {
    if focused && self.highlight != ratatui::style::Color::Reset {
      self.highlight
    } else {
      self.accent
    }
  }

  /// `Style` to paint under a popup or modal so the dialog's
  /// interior adopts `palette.bg` rather than the terminal default
  /// that `Clear` leaves behind. Subsequent foreground-only spans
  /// inherit the bg from the cells this style paints. Returns
  /// `None` for themes whose `bg` is `Color::Reset` (Mono opts out
  /// so the terminal's own surface tone still shows through).
  pub fn popup_bg_style(&self) -> Option<ratatui::style::Style> {
    if self.bg == ratatui::style::Color::Reset {
      None
    } else {
      Some(ratatui::style::Style::default().bg(self.bg).fg(self.fg))
    }
  }
}

/// Resolve a `ThemeName` to its concrete `Palette`. Returns a `'static`
/// reference because all built-in palettes are compile-time constants.
/// `Custom` is a runtime-loaded palette that cannot be `'static`; this
/// resolver returns macchiato for it as a benign fallback. Code paths
/// that need to honour a user-loaded custom palette go through
/// `App::palette()` instead, which overlays the loaded palette when
/// `options.theme == Custom`.
pub fn palette_for(theme: ThemeName) -> &'static Palette {
  match theme {
    ThemeName::Macchiato | ThemeName::Custom => &super::macchiato::PALETTE,
    ThemeName::Latte => &super::latte::PALETTE,
    ThemeName::GruvboxDark => &super::gruvbox::PALETTE,
    ThemeName::SolarizedDark => &super::solarized::PALETTE,
    ThemeName::Mono => &super::mono::PALETTE,
  }
}

#[cfg(test)]
mod tests {
  use strum::IntoEnumIterator;

  use super::*;

  #[test]
  fn every_theme_has_a_palette() {
    for theme in ThemeName::iter() {
      let palette = palette_for(theme);
      // `Custom` resolves to the macchiato fallback at the
      // `palette_for` layer — the user-loaded custom palette is
      // overlaid by `App::palette()`, not by this resolver — so the
      // name match only holds for built-in themes here.
      if theme != ThemeName::Custom {
        assert_eq!(palette.name, theme, "palette.name must match its key");
      } else {
        assert_eq!(palette.name, ThemeName::Macchiato);
      }
    }
  }

  #[test]
  fn parse_accepts_canonical_and_aliases() {
    assert_eq!(ThemeName::parse("macchiato"), Ok(ThemeName::Macchiato));
    assert_eq!(
      ThemeName::parse("catppuccin-macchiato"),
      Ok(ThemeName::Macchiato)
    );
    assert_eq!(ThemeName::parse("Latte"), Ok(ThemeName::Latte));
    assert_eq!(ThemeName::parse("gruvbox"), Ok(ThemeName::GruvboxDark));
    assert_eq!(
      ThemeName::parse("solarized-dark"),
      Ok(ThemeName::SolarizedDark)
    );
    assert_eq!(ThemeName::parse("monochrome"), Ok(ThemeName::Mono));
  }

  #[test]
  fn parse_rejects_unknown_values_with_actionable_error() {
    let err = ThemeName::parse("dracula").unwrap_err();
    assert_eq!(err.value, "dracula");
    let rendered = err.to_string();
    assert!(rendered.contains("dracula"));
    assert!(rendered.contains("macchiato"));
  }

  #[test]
  fn default_is_macchiato() {
    assert_eq!(ThemeName::default(), ThemeName::Macchiato);
  }

  #[test]
  fn canonical_strings_roundtrip_through_parse() {
    for theme in ThemeName::iter() {
      assert_eq!(ThemeName::parse(&theme.to_string()), Ok(theme));
    }
  }

  #[test]
  fn yaml_roundtrip_uses_kebab_case_and_aliases() {
    let cases: &[(&str, ThemeName)] = &[
      ("macchiato", ThemeName::Macchiato),
      ("catppuccin-macchiato", ThemeName::Macchiato),
      ("latte", ThemeName::Latte),
      ("catppuccin-latte", ThemeName::Latte),
      ("gruvbox-dark", ThemeName::GruvboxDark),
      ("gruvbox", ThemeName::GruvboxDark),
      ("solarized-dark", ThemeName::SolarizedDark),
      ("solarized", ThemeName::SolarizedDark),
      ("mono", ThemeName::Mono),
      ("monochrome", ThemeName::Mono),
      ("custom", ThemeName::Custom),
    ];
    for (input, expected) in cases {
      let parsed: ThemeName =
        serde_yaml::from_str(input).unwrap_or_else(|e| panic!("`{input}` failed to parse: {e}"));
      assert_eq!(parsed, *expected, "input: {input}");
    }
  }

  #[test]
  fn palettes_carry_the_expected_brand_colors() {
    use ratatui::style::Color;

    // Pin the four most user-visible slots per theme so a swapped match arm
    // in `palette_for` shows up immediately instead of silently rendering the
    // wrong palette.
    assert!(palette_for(ThemeName::Macchiato).is_dark);
    assert_eq!(
      palette_for(ThemeName::Macchiato).bg,
      Color::Rgb(0x24, 0x27, 0x3A)
    );

    assert!(!palette_for(ThemeName::Latte).is_dark);
    assert_eq!(
      palette_for(ThemeName::Latte).bg,
      Color::Rgb(0xEF, 0xF1, 0xF5)
    );

    assert!(palette_for(ThemeName::GruvboxDark).is_dark);
    assert_eq!(
      palette_for(ThemeName::GruvboxDark).bg,
      Color::Rgb(0x1D, 0x20, 0x21)
    );

    assert!(palette_for(ThemeName::SolarizedDark).is_dark);
    assert_eq!(
      palette_for(ThemeName::SolarizedDark).bg,
      Color::Rgb(0x00, 0x2B, 0x36)
    );

    assert!(palette_for(ThemeName::Mono).is_dark);
    assert_eq!(palette_for(ThemeName::Mono).bg, Color::Reset);
  }

  #[test]
  fn helpers_paint_correct_slot() {
    use ratatui::style::Modifier;
    let p = palette_for(ThemeName::Macchiato);
    assert_eq!(p.text_style().fg, Some(p.fg));
    assert_eq!(p.muted_style().fg, Some(p.muted));
    assert_eq!(p.accent_style().fg, Some(p.accent));
    assert_eq!(p.label_style().fg, Some(p.label));
    assert_eq!(p.error_style().fg, Some(p.error));
    assert_eq!(p.warning_style().fg, Some(p.warning));
    assert_eq!(p.success_style().fg, Some(p.success));
    let t = p.title_style();
    assert_eq!(t.fg, Some(p.panel_title));
    assert!(t.add_modifier.contains(Modifier::BOLD));
  }

  #[test]
  fn focus_border_routes_through_highlight_then_falls_back_to_accent() {
    // Themes with a real highlight slot (every built-in except Mono)
    // must use it for focus so the focused border matches the
    // list-row highlight tone.
    for theme in [
      ThemeName::Macchiato,
      ThemeName::Latte,
      ThemeName::GruvboxDark,
      ThemeName::SolarizedDark,
    ] {
      let p = palette_for(theme);
      assert_eq!(p.focus_border(true), p.highlight, "{theme:?} focused");
      assert_eq!(p.focus_border(false), p.accent, "{theme:?} unfocused");
    }
    // Mono opts out (`highlight == Color::Reset`); falling back to
    // accent keeps the focused border visible against the terminal.
    let mono = palette_for(ThemeName::Mono);
    assert_eq!(mono.highlight, Color::Reset);
    assert_eq!(mono.focus_border(true), mono.accent);
    assert_eq!(mono.focus_border(false), mono.accent);
  }

  #[test]
  fn popup_bg_style_emits_palette_bg_for_concrete_themes_and_none_for_mono() {
    // Concrete-bg themes must hand back a Style that paints both bg
    // and fg so overlays inherit the theme surface after `Clear`.
    for theme in [
      ThemeName::Macchiato,
      ThemeName::Latte,
      ThemeName::GruvboxDark,
      ThemeName::SolarizedDark,
    ] {
      let p = palette_for(theme);
      let style = p
        .popup_bg_style()
        .unwrap_or_else(|| panic!("{theme:?} should produce a popup bg style"));
      assert_eq!(style.bg, Some(p.bg));
      assert_eq!(style.fg, Some(p.fg));
    }
    // Mono's `bg == Color::Reset` is the explicit opt-out signal.
    assert!(palette_for(ThemeName::Mono).popup_bg_style().is_none());
  }
}
