use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Solarized Dark — https://ethanschoonover.com/solarized
const BASE03: Color = Color::Rgb(0x00, 0x2B, 0x36);
const BASE02: Color = Color::Rgb(0x07, 0x36, 0x42);
const BASE01: Color = Color::Rgb(0x58, 0x6E, 0x75);
const BASE0: Color = Color::Rgb(0x83, 0x94, 0x96);
const BLUE: Color = Color::Rgb(0x26, 0x8B, 0xD2);
const GREEN: Color = Color::Rgb(0x85, 0x99, 0x00);
const YELLOW: Color = Color::Rgb(0xB5, 0x89, 0x00);
const RED: Color = Color::Rgb(0xDC, 0x32, 0x2F);
const CYAN: Color = Color::Rgb(0x2A, 0xA1, 0x98);

pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::SolarizedDark,
  is_dark: true,
  bg: BASE03,
  fg: BASE0,
  accent: BLUE,
  success: GREEN,
  warning: YELLOW,
  error: RED,
  muted: BASE01,
  selection: BASE02,
  highlight: YELLOW,
  status_loading: YELLOW,
  status_ready: GREEN,
  status_error: RED,
  status_stopped: BASE01,
  status_external: CYAN,
};
