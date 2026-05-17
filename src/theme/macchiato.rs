use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Catppuccin Macchiato — https://catppuccin.com/palette
const BASE: Color = Color::Rgb(0x24, 0x27, 0x3A);
const TEXT: Color = Color::Rgb(0xCA, 0xD3, 0xF5);
const MAUVE: Color = Color::Rgb(0xC6, 0xA0, 0xF6);
const GREEN: Color = Color::Rgb(0xA6, 0xDA, 0x95);
const YELLOW: Color = Color::Rgb(0xEE, 0xD4, 0x9F);
const PEACH: Color = Color::Rgb(0xF5, 0xA9, 0x7F);
const RED: Color = Color::Rgb(0xED, 0x87, 0x96);
const SUBTEXT0: Color = Color::Rgb(0xA5, 0xAD, 0xCB);
const SURFACE0: Color = Color::Rgb(0x36, 0x3A, 0x4F);
const BLUE: Color = Color::Rgb(0x8A, 0xAD, 0xF4);
const OVERLAY1: Color = Color::Rgb(0x8B, 0x90, 0xA8);

pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::Macchiato,
  is_dark: true,
  bg: BASE,
  fg: TEXT,
  accent: MAUVE,
  success: GREEN,
  warning: PEACH,
  error: RED,
  muted: SUBTEXT0,
  selection: SURFACE0,
  highlight: YELLOW,
  panel_title: YELLOW,
  label: BLUE,
  on_accent: BASE,
  status_loading: YELLOW,
  status_ready: GREEN,
  status_error: RED,
  status_stopped: OVERLAY1,
  status_external: BLUE,
};
