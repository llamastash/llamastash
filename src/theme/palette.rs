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
}
