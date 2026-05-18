//! Free-form `llama-server` flag editor (R14).
//!
//! v1 ships a plain text input pre-populated with the current
//! launch params' `advanced` slot. Users edit a space-separated
//! flag list; submit appends it to the launch and (for new
//! launches) flushes through the picker. Tab-completion hints
//! over common flags are deferred to a follow-up.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

/// State of the advanced panel.
#[derive(Debug, Clone, Default)]
pub struct AdvancedPanelState {
  /// Current text buffer. Caller is responsible for splitting on
  /// whitespace when assembling the launch params.
  pub buffer: String,
  /// Caret position into `buffer` (in bytes — the renderer accepts
  /// it as the column to highlight).
  pub cursor: usize,
}

impl AdvancedPanelState {
  pub fn from_advanced(advanced: &[std::ffi::OsString]) -> Self {
    let parts: Vec<String> = advanced
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    let buffer = parts.join(" ");
    let cursor = buffer.len();
    Self { buffer, cursor }
  }

  /// Insert a single character at the cursor.
  pub fn insert(&mut self, ch: char) {
    self.buffer.insert(self.cursor, ch);
    self.cursor += ch.len_utf8();
  }

  /// Delete the char to the left of the cursor (Backspace).
  pub fn backspace(&mut self) {
    if self.cursor == 0 {
      return;
    }
    let mut new_cursor = self.cursor - 1;
    while !self.buffer.is_char_boundary(new_cursor) {
      new_cursor -= 1;
    }
    self.buffer.replace_range(new_cursor..self.cursor, "");
    self.cursor = new_cursor;
  }

  /// Split the current buffer into `OsString` argv tokens. Empty
  /// runs collapse — extra whitespace is forgiven so users can
  /// reformat their flag string for readability.
  pub fn argv(&self) -> Vec<std::ffi::OsString> {
    self
      .buffer
      .split_whitespace()
      .map(std::ffi::OsString::from)
      .collect()
  }
}

/// Render the panel centred over `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AdvancedPanelState, palette: &Palette) {
  let modal = centered_rect(80, 50, area);
  frame.render_widget(Clear, modal);
  let block = Block::default()
    .title(" Advanced flags ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  frame.render_widget(block.clone(), modal);
  let inner = block.inner(modal);

  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Length(3), Constraint::Min(0)])
    .split(inner);

  let intro = Paragraph::new(Line::from(vec![Span::styled(
    "Edit `llama-server` flags. They append AFTER bundled flags so they trump the picker.",
    Style::default().fg(palette.muted),
  )]))
  .wrap(Wrap { trim: true });
  frame.render_widget(intro, layout[0]);

  // Round-8: caret style mirrors the filter input (no leading
  // `▌ ` block, `▏` cursor) so every text-input in the TUI reads
  // identically.
  let body = Paragraph::new(Line::from(vec![
    Span::styled(&state.buffer, Style::default().fg(palette.fg)),
    Span::styled(
      "▏",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::REVERSED),
    ),
  ]))
  .wrap(Wrap { trim: false });
  frame.render_widget(body, layout[1]);
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
  let v = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Percentage((100 - pct_y) / 2),
      Constraint::Percentage(pct_y),
      Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
  Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
      Constraint::Percentage((100 - pct_x) / 2),
      Constraint::Percentage(pct_x),
      Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::ffi::OsString;

  #[test]
  fn from_advanced_joins_with_spaces() {
    let advanced = vec![OsString::from("--threads"), OsString::from("8")];
    let s = AdvancedPanelState::from_advanced(&advanced);
    assert_eq!(s.buffer, "--threads 8");
    assert_eq!(s.cursor, "--threads 8".len());
  }

  #[test]
  fn argv_splits_on_whitespace_and_collapses_runs() {
    let s = AdvancedPanelState {
      buffer: "  --threads   8  --flash-attn  ".into(),
      ..Default::default()
    };
    let v: Vec<String> = s
      .argv()
      .iter()
      .map(|o| o.to_string_lossy().into())
      .collect();
    assert_eq!(v, vec!["--threads", "8", "--flash-attn"]);
  }

  #[test]
  fn insert_then_backspace_round_trips() {
    let mut s = AdvancedPanelState::default();
    for ch in "--threads 8".chars() {
      s.insert(ch);
    }
    assert_eq!(s.buffer, "--threads 8");
    s.backspace();
    assert_eq!(s.buffer, "--threads ");
  }

  #[test]
  fn backspace_at_zero_is_noop() {
    let mut s = AdvancedPanelState::default();
    s.backspace();
    assert_eq!(s.buffer, "");
    assert_eq!(s.cursor, 0);
  }
}
