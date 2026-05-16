//! Title-row global hint strip.
//!
//! Pre-relayout this module owned a focus-aware bottom help bar. After
//! the kdash-style relayout, the bottom bar is gone and panel-specific
//! hints live inside each panel's block title (`list_pane`,
//! `right_pane`, etc.). What's left is the small static strip of
//! **global** keybindings — `?`, `t`, `/`, `q` — that the title row
//! right-aligns over the accent background.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;

/// Canonical list of global hints. Single source of truth: both
/// [`global_hint_text`] (used to compute the right-slot width in
/// `render`) and [`render_global`] iterate this slice. Adding or
/// reordering an entry updates both call sites automatically.
const GLOBAL_HINTS: &[(&str, &str)] = &[
  ("?", "help"),
  ("t", "theme"),
  ("/", "filter"),
  ("q", "quit"),
];

const HINT_SEP: &str = "  ";

/// Width in columns the title row should reserve for the global hint
/// strip, including the leading space inside each `key:label` pair and
/// a single trailing pad column.
pub fn global_hint_slot_width() -> u16 {
  let mut w: usize = 0;
  for (i, (key, label)) in GLOBAL_HINTS.iter().enumerate() {
    if i > 0 {
      w += HINT_SEP.chars().count();
    }
    w += key.chars().count() + 1 + label.chars().count();
  }
  // One-cell trailing pad so the rightmost hint isn't flush against
  // the terminal edge.
  w += 1;
  u16::try_from(w).unwrap_or(u16::MAX)
}

/// Format the global hint string. Stable order; no focus awareness.
/// Built from [`GLOBAL_HINTS`] so it can never drift from the renderer.
pub fn global_hint_text() -> String {
  let mut out = String::new();
  for (i, (key, label)) in GLOBAL_HINTS.iter().enumerate() {
    if i > 0 {
      out.push_str(HINT_SEP);
    }
    out.push_str(key);
    out.push(':');
    out.push_str(label);
  }
  out
}

/// Render the global hint strip into `area`, right-aligned with text
/// in `palette.bg` on the accent background already painted by the
/// title-row renderer.
pub fn render_global(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
  let mut spans: Vec<Span<'_>> = Vec::with_capacity(GLOBAL_HINTS.len() * 2);
  for (i, (key, label)) in GLOBAL_HINTS.iter().enumerate() {
    if i > 0 {
      spans.push(Span::raw(HINT_SEP));
    }
    spans.push(hint_span(key, label));
  }
  spans.push(Span::raw(" "));
  let para = Paragraph::new(Line::from(spans))
    .alignment(Alignment::Right)
    .style(Style::default().bg(palette.accent).fg(palette.bg));
  frame.render_widget(para, area);
}

/// Build a `key:label` span. The key+colon+label are bolded together;
/// both inherit the accent-bg/bg-fg style from the parent Paragraph.
/// All four sub-strings are `&'static str` so no per-frame allocation.
fn hint_span(key: &'static str, label: &'static str) -> Span<'static> {
  Span::styled(
    [key, ":", label].concat(),
    Style::default().add_modifier(Modifier::BOLD),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn global_hint_text_lists_required_keys() {
    let text = global_hint_text();
    assert!(text.contains("?:help"));
    assert!(text.contains("t:theme"));
    assert!(text.contains("/:filter"));
    assert!(text.contains("q:quit"));
  }

  #[test]
  fn global_hint_text_fits_typical_terminal_widths() {
    assert!(global_hint_text().chars().count() < 45);
  }

  #[test]
  fn slot_width_matches_rendered_text_plus_pad() {
    // Slot width should equal the visible text width plus the one
    // trailing pad column. If the constants drift, the title row
    // would either clip the rightmost hint or leave a gap.
    let text_w = global_hint_text().chars().count() as u16;
    assert_eq!(global_hint_slot_width(), text_w + 1);
  }
}
