//! User-defined theme.
//!
//! Lets the user override the active palette via the YAML config. Only
//! one custom theme exists at a time (selected via
//! [`ThemeName::Custom`]); cycling with `t:theme` still walks the
//! built-ins.
//!
//! Each slot is optional: missing fields fall through to a `base`
//! palette (default macchiato), so a config can override just the
//! colours that matter and inherit the rest. Bad colour values emit
//! warnings during [`CustomThemeConfig::resolve`] and the slot keeps
//! the base value rather than collapsing the whole palette to
//! defaults.
//!
//! Accepted colour syntax (case-insensitive):
//! - Named ANSI colours: `black`, `red`, `green`, `yellow`, `blue`,
//!   `magenta`, `cyan`, `gray`/`grey`, `darkgray`, `lightred`,
//!   `lightgreen`, `lightyellow`, `lightblue`, `lightmagenta`,
//!   `lightcyan`, `white`.
//! - `reset` / `default` — falls through to the terminal default.
//! - 6-digit hex with leading `#`: `#1A1B26`, `#c0caf5`.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use super::palette::{palette_for, Palette, ThemeName};

/// YAML-shaped user theme. Every colour slot is optional; the
/// resolver fills missing slots from `base`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct CustomThemeConfig {
  /// Built-in palette to fall back to for slots the user didn't
  /// override. `None` → macchiato. `Custom` is rejected here (you
  /// can't base a custom theme on itself).
  pub base: Option<ThemeName>,
  pub is_dark: Option<bool>,
  pub bg: Option<String>,
  pub fg: Option<String>,
  pub accent: Option<String>,
  pub success: Option<String>,
  pub warning: Option<String>,
  pub error: Option<String>,
  pub muted: Option<String>,
  pub selection: Option<String>,
  pub highlight: Option<String>,
  pub panel_title: Option<String>,
  pub label: Option<String>,
  pub on_accent: Option<String>,
  pub status_loading: Option<String>,
  pub status_ready: Option<String>,
  pub status_error: Option<String>,
  pub status_stopped: Option<String>,
  pub status_external: Option<String>,
}

impl CustomThemeConfig {
  /// Build a concrete [`Palette`] from this config. Accumulates
  /// warnings for any colour value that failed to parse; the
  /// returned palette substitutes the base value for those slots so
  /// the UI still renders cleanly.
  pub fn resolve(&self) -> (Palette, Vec<String>) {
    let mut warnings = Vec::new();
    let base_name = match self.base {
      // A custom theme can't be its own base — that would either
      // loop (if the runtime palette were stored here) or no-op (we
      // don't carry the user palette in `palette_for`). Either way,
      // bounce to macchiato and warn the user.
      Some(ThemeName::Custom) => {
        warnings
          .push("custom_theme.base cannot be `custom`; falling back to macchiato".to_string());
        ThemeName::Macchiato
      }
      Some(other) => other,
      None => ThemeName::Macchiato,
    };
    let base = palette_for(base_name);

    let mut palette = Palette {
      name: ThemeName::Custom,
      is_dark: self.is_dark.unwrap_or(base.is_dark),
      bg: base.bg,
      fg: base.fg,
      accent: base.accent,
      success: base.success,
      warning: base.warning,
      error: base.error,
      muted: base.muted,
      selection: base.selection,
      highlight: base.highlight,
      panel_title: base.panel_title,
      label: base.label,
      on_accent: base.on_accent,
      status_loading: base.status_loading,
      status_ready: base.status_ready,
      status_error: base.status_error,
      status_stopped: base.status_stopped,
      status_external: base.status_external,
    };

    apply(&self.bg, "bg", &mut palette.bg, &mut warnings);
    apply(&self.fg, "fg", &mut palette.fg, &mut warnings);
    apply(&self.accent, "accent", &mut palette.accent, &mut warnings);
    apply(
      &self.success,
      "success",
      &mut palette.success,
      &mut warnings,
    );
    apply(
      &self.warning,
      "warning",
      &mut palette.warning,
      &mut warnings,
    );
    apply(&self.error, "error", &mut palette.error, &mut warnings);
    apply(&self.muted, "muted", &mut palette.muted, &mut warnings);
    apply(
      &self.selection,
      "selection",
      &mut palette.selection,
      &mut warnings,
    );
    apply(
      &self.highlight,
      "highlight",
      &mut palette.highlight,
      &mut warnings,
    );
    apply(
      &self.panel_title,
      "panel_title",
      &mut palette.panel_title,
      &mut warnings,
    );
    apply(&self.label, "label", &mut palette.label, &mut warnings);
    apply(
      &self.on_accent,
      "on_accent",
      &mut palette.on_accent,
      &mut warnings,
    );
    apply(
      &self.status_loading,
      "status_loading",
      &mut palette.status_loading,
      &mut warnings,
    );
    apply(
      &self.status_ready,
      "status_ready",
      &mut palette.status_ready,
      &mut warnings,
    );
    apply(
      &self.status_error,
      "status_error",
      &mut palette.status_error,
      &mut warnings,
    );
    apply(
      &self.status_stopped,
      "status_stopped",
      &mut palette.status_stopped,
      &mut warnings,
    );
    apply(
      &self.status_external,
      "status_external",
      &mut palette.status_external,
      &mut warnings,
    );

    (palette, warnings)
  }
}

fn apply(raw: &Option<String>, key: &str, target: &mut Color, warnings: &mut Vec<String>) {
  if let Some(value) = raw {
    match parse_color(value) {
      Ok(color) => *target = color,
      Err(error) => warnings.push(format!(
        "custom_theme.{key}: '{value}' — {error}; keeping base value"
      )),
    }
  }
}

/// Parse a colour token. Accepts the same syntax as kdash so a user
/// migrating between the two doesn't relearn the shape.
fn parse_color(value: &str) -> Result<Color, String> {
  let normalized = value.trim().to_lowercase();
  match normalized.as_str() {
    "black" => Ok(Color::Black),
    "red" => Ok(Color::Red),
    "green" => Ok(Color::Green),
    "yellow" => Ok(Color::Yellow),
    "blue" => Ok(Color::Blue),
    "magenta" => Ok(Color::Magenta),
    "cyan" => Ok(Color::Cyan),
    "gray" | "grey" => Ok(Color::Gray),
    "darkgray" | "darkgrey" | "dark_gray" | "dark_grey" => Ok(Color::DarkGray),
    "lightred" | "light_red" => Ok(Color::LightRed),
    "lightgreen" | "light_green" => Ok(Color::LightGreen),
    "lightyellow" | "light_yellow" => Ok(Color::LightYellow),
    "lightblue" | "light_blue" => Ok(Color::LightBlue),
    "lightmagenta" | "light_magenta" => Ok(Color::LightMagenta),
    "lightcyan" | "light_cyan" => Ok(Color::LightCyan),
    "white" => Ok(Color::White),
    "reset" | "default" => Ok(Color::Reset),
    _ => parse_hex_color(&normalized),
  }
}

fn parse_hex_color(value: &str) -> Result<Color, String> {
  let hex = value
    .strip_prefix('#')
    .ok_or_else(|| format!("unsupported color '{value}'"))?;
  if hex.len() != 6 {
    return Err(format!("hex color '{value}' must be 6 characters"));
  }
  let red =
    u8::from_str_radix(&hex[0..2], 16).map_err(|_| format!("invalid red channel in '{value}'"))?;
  let green = u8::from_str_radix(&hex[2..4], 16)
    .map_err(|_| format!("invalid green channel in '{value}'"))?;
  let blue =
    u8::from_str_radix(&hex[4..6], 16).map_err(|_| format!("invalid blue channel in '{value}'"))?;
  Ok(Color::Rgb(red, green, blue))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_config_returns_macchiato_clone_named_custom() {
    let (palette, warnings) = CustomThemeConfig::default().resolve();
    let base = palette_for(ThemeName::Macchiato);
    assert!(warnings.is_empty());
    assert_eq!(palette.name, ThemeName::Custom);
    // Every slot inherits the base palette when the config is empty.
    assert_eq!(palette.bg, base.bg);
    assert_eq!(palette.accent, base.accent);
    assert_eq!(palette.panel_title, base.panel_title);
    assert_eq!(palette.label, base.label);
    assert_eq!(palette.is_dark, base.is_dark);
  }

  #[test]
  fn base_field_picks_a_different_built_in() {
    let cfg = CustomThemeConfig {
      base: Some(ThemeName::Latte),
      ..Default::default()
    };
    let (palette, warnings) = cfg.resolve();
    assert!(warnings.is_empty());
    let latte = palette_for(ThemeName::Latte);
    assert_eq!(palette.bg, latte.bg);
    assert_eq!(palette.fg, latte.fg);
    assert!(!palette.is_dark);
  }

  #[test]
  fn base_custom_warns_and_falls_back_to_macchiato() {
    let cfg = CustomThemeConfig {
      base: Some(ThemeName::Custom),
      ..Default::default()
    };
    let (palette, warnings) = cfg.resolve();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("custom"));
    let mac = palette_for(ThemeName::Macchiato);
    assert_eq!(palette.bg, mac.bg);
  }

  #[test]
  fn hex_override_lands_in_target_slot() {
    let cfg = CustomThemeConfig {
      accent: Some("#FF00AA".into()),
      bg: Some("#101020".into()),
      ..Default::default()
    };
    let (palette, warnings) = cfg.resolve();
    assert!(warnings.is_empty());
    assert_eq!(palette.accent, Color::Rgb(0xFF, 0x00, 0xAA));
    assert_eq!(palette.bg, Color::Rgb(0x10, 0x10, 0x20));
  }

  #[test]
  fn named_color_resolves_via_ansi_slot() {
    let cfg = CustomThemeConfig {
      success: Some("LightGreen".into()),
      ..Default::default()
    };
    let (palette, warnings) = cfg.resolve();
    assert!(warnings.is_empty());
    assert_eq!(palette.success, Color::LightGreen);
  }

  #[test]
  fn unparseable_color_warns_and_keeps_base() {
    let cfg = CustomThemeConfig {
      accent: Some("not-a-color".into()),
      ..Default::default()
    };
    let (palette, warnings) = cfg.resolve();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("accent"));
    assert!(warnings[0].contains("not-a-color"));
    // Slot retains base macchiato accent.
    let base = palette_for(ThemeName::Macchiato);
    assert_eq!(palette.accent, base.accent);
  }

  #[test]
  fn is_dark_flag_overrides_base() {
    let cfg = CustomThemeConfig {
      is_dark: Some(false),
      ..Default::default()
    };
    let (palette, _) = cfg.resolve();
    assert!(!palette.is_dark);
  }

  #[test]
  fn yaml_round_trip_preserves_partial_overrides() {
    let yaml = "
base: latte
accent: '#FF00AA'
bg: '#101020'
";
    let cfg: CustomThemeConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    assert_eq!(cfg.base, Some(ThemeName::Latte));
    assert_eq!(cfg.accent.as_deref(), Some("#FF00AA"));
    assert_eq!(cfg.bg.as_deref(), Some("#101020"));
    assert_eq!(cfg.fg, None);
  }

  #[test]
  fn yaml_unknown_field_is_rejected() {
    let yaml = "made_up_slot: red\n";
    let result: Result<CustomThemeConfig, _> = serde_yaml::from_str(yaml);
    assert!(
      result.is_err(),
      "deny_unknown_fields should reject unrecognised keys: {result:?}"
    );
  }

  #[test]
  fn parse_color_rejects_short_hex() {
    assert!(parse_color("#FFF").is_err());
    assert!(parse_color("#FFFFFFF").is_err());
  }
}
