use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Gruvbox Dark (hard) — https://github.com/morhetz/gruvbox
const BG: Color = Color::Rgb(0x1D, 0x20, 0x21);
const BG2: Color = Color::Rgb(0x50, 0x49, 0x45);
const FG: Color = Color::Rgb(0xEB, 0xDB, 0xB2);
const ORANGE: Color = Color::Rgb(0xFE, 0x80, 0x19);
const GREEN: Color = Color::Rgb(0xB8, 0xBB, 0x26);
const YELLOW: Color = Color::Rgb(0xFA, 0xBD, 0x2F);
const RED: Color = Color::Rgb(0xFB, 0x49, 0x34);
// `#928374` (gruvbox `gray`) on `#1D2021` (hard-dark BG) tested too
// dim for label / divider text — labels almost vanished on a dim
// monitor. Bumped to `#A89984` (gruvbox FG4) so labels and dividers
// stay legible without crossing into "primary text" brightness.
const GRAY: Color = Color::Rgb(0xA8, 0x99, 0x84);
const BLUE: Color = Color::Rgb(0x83, 0xA5, 0x98);

pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::GruvboxDark,
  is_dark: true,
  bg: BG,
  fg: FG,
  accent: ORANGE,
  success: GREEN,
  warning: YELLOW,
  error: RED,
  muted: GRAY,
  selection: BG2,
  highlight: YELLOW,
  panel_title: YELLOW,
  label: BLUE,
  on_accent: BG,
  status_loading: YELLOW,
  status_ready: GREEN,
  status_error: RED,
  status_stopped: GRAY,
  status_external: BLUE,
};
