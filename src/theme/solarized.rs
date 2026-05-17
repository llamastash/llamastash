use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Solarized Dark — https://ethanschoonover.com/solarized
const BASE03: Color = Color::Rgb(0x00, 0x2B, 0x36);
const BASE02: Color = Color::Rgb(0x07, 0x36, 0x42);
// Solarized's canonical "muted" tone (`BASE01 #586E75`) sits very
// close to `BASE03` (the bg here) — fine on paper, fails in
// practice once a terminal ramps the bg up a notch. Promote to
// `BASE00 #657B83` so labels/dividers stay legible on real
// hardware.
const BASE00: Color = Color::Rgb(0x65, 0x7B, 0x83);
// `BASE0 #839496` is Solarized's canonical "primary text" tone but
// reads thin against BASE03. `BASE1 #93A1A1` is the brightest
// neutral the palette ships — promote fg to it so primary text
// has more punch without leaving the Solarized family.
const BASE1: Color = Color::Rgb(0x93, 0xA1, 0xA1);
const BLUE: Color = Color::Rgb(0x26, 0x8B, 0xD2);
const GREEN: Color = Color::Rgb(0x85, 0x99, 0x00);
const YELLOW: Color = Color::Rgb(0xB5, 0x89, 0x00);
const RED: Color = Color::Rgb(0xDC, 0x32, 0x2F);
const CYAN: Color = Color::Rgb(0x2A, 0xA1, 0x98);

pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::SolarizedDark,
  is_dark: true,
  bg: BASE03,
  fg: BASE1,
  accent: BLUE,
  success: GREEN,
  warning: YELLOW,
  error: RED,
  muted: BASE00,
  selection: BASE02,
  highlight: YELLOW,
  panel_title: YELLOW,
  label: CYAN,
  on_accent: BASE03,
  status_loading: YELLOW,
  status_ready: GREEN,
  status_error: RED,
  status_stopped: BASE00,
  status_external: CYAN,
};
