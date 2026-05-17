use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Catppuccin Latte — https://catppuccin.com/palette
const BASE: Color = Color::Rgb(0xEF, 0xF1, 0xF5);
const TEXT: Color = Color::Rgb(0x4C, 0x4F, 0x69);
const MAUVE: Color = Color::Rgb(0x88, 0x39, 0xEF);
const GREEN: Color = Color::Rgb(0x40, 0xA0, 0x2B);
const YELLOW: Color = Color::Rgb(0xDF, 0x8E, 0x1D);
const PEACH: Color = Color::Rgb(0xFE, 0x64, 0x0B);
const RED: Color = Color::Rgb(0xD2, 0x0F, 0x39);
const SUBTEXT0: Color = Color::Rgb(0x6C, 0x6F, 0x85);
const SURFACE0: Color = Color::Rgb(0xCC, 0xD0, 0xDA);
const BLUE: Color = Color::Rgb(0x1E, 0x66, 0xF5);
const OVERLAY1: Color = Color::Rgb(0x8C, 0x8F, 0xA1);

pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::Latte,
  is_dark: false,
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
