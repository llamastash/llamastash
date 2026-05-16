//! Title-row global hint strip.
//!
//! Pre-Unit-6 this module owned a focus-aware bottom help bar. After
//! the kdash-style relayout, the bottom bar is gone and panel-specific
//! hints live inside each panel's block title (`list_pane`,
//! `right_pane`, etc.). What's left here is the small static strip of
//! **global** keybindings — `?`, `t`, `/`, `q` — that the title row
//! right-aligns over the accent background.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;

/// Format the global hint string. Stable order; no focus awareness.
pub fn global_hint_text() -> &'static str {
  "?:help  t:theme  /:filter  q:quit"
}

/// Render the global hint strip into `area`, right-aligned with text
/// in `palette.bg` on the accent background already painted by the
/// title-row renderer. Adds a one-cell trailing pad so the rightmost
/// hint isn't flush against the terminal edge.
pub fn render_global(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
  let line = Line::from(vec![
    hint_span("?", "help", palette),
    Span::raw("  "),
    hint_span("t", "theme", palette),
    Span::raw("  "),
    hint_span("/", "filter", palette),
    Span::raw("  "),
    hint_span("q", "quit", palette),
    Span::raw(" "),
  ]);
  let para = Paragraph::new(line)
    .alignment(Alignment::Right)
    .style(Style::default().bg(palette.accent).fg(palette.bg));
  frame.render_widget(para, area);
}

fn hint_span<'a>(key: &'a str, label: &'a str, palette: &Palette) -> Span<'a> {
  // Key gets BOLD; label stays regular. Both inherit the accent-bg/bg-fg
  // style from the Paragraph below.
  let _ = palette;
  Span::styled(
    format!("{key}:{label}"),
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
    // The title row is 1 row; right slot is bounded by total width
    // minus the left title chunk (~30 cols). On an 80-col terminal
    // that leaves ~50 cols for hints — the static string is well
    // under that.
    assert!(global_hint_text().chars().count() < 45);
  }
}
