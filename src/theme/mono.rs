use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Monochrome — relies on glyph cues plus bold/reverse modifiers (applied at
// render sites) since colour cannot carry meaning here. Status colours fall
// back to terminal defaults so the layer above can use Modifier::BOLD /
// Modifier::REVERSED for emphasis.
//
// Contrast bump: `Color::DarkGray` for `muted` and `status_stopped`
// rendered close to invisible on dark terminals where the dark-gray
// ANSI slot maps to a dim shade. Bump both to `Color::Gray` so
// labels and dividers stay legible. The "muted vs primary text"
// distinction in mono is now carried by bold/regular modifiers at
// the render site rather than by hue.
pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::Mono,
  is_dark: true,
  bg: Color::Reset,
  fg: Color::White,
  accent: Color::White,
  success: Color::White,
  warning: Color::Gray,
  error: Color::White,
  muted: Color::Gray,
  selection: Color::DarkGray,
  // Reset → list_pane falls back to Modifier::REVERSED so mono
  // keeps its glyph + invert idiom rather than gaining a colour
  // it doesn't have anywhere else.
  highlight: Color::Reset,
  panel_title: Color::White,
  label: Color::Gray,
  // White accent bar (the title row) needs a concrete dark text
  // colour. `bg` is `Color::Reset` here, which falls through to the
  // terminal default — usually light on a dark terminal — so the
  // shared `palette.bg` fallback used by colour themes would render
  // as white-on-white. Pin to Black so the title bar always reads.
  on_accent: Color::Black,
  status_loading: Color::Gray,
  status_ready: Color::White,
  status_error: Color::White,
  status_stopped: Color::Gray,
  status_external: Color::Gray,
};
