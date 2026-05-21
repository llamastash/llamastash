//! Free-form `llama-server` flag editor (R14).
//!
//! v1 ships a plain text input pre-populated with the current
//! launch params' `advanced` slot. Users edit a space-separated
//! flag list; submit appends it to the launch and (for new
//! launches) flushes through the picker. Tab-completion hints
//! over common flags are deferred to a follow-up.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

/// State of the advanced panel. Wraps the modal
/// [`crate::tui::input_field::InputField`] so the
/// `e:edit / Esc:stop / 2nd-Esc:clear` contract matches every other
/// text input in the TUI. The panel auto-enters edit mode on open
/// (see `App::open_advanced_panel`) so the user can type immediately.
#[derive(Debug, Clone, Default)]
pub struct AdvancedPanelState {
  /// Modal text-input field carrying the free-form flag list.
  pub buffer: crate::tui::input_field::InputField,
}

impl AdvancedPanelState {
  pub fn from_advanced(advanced: &[std::ffi::OsString]) -> Self {
    let parts: Vec<String> = advanced
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    let mut buffer = crate::tui::input_field::InputField::with_text(parts.join(" "));
    buffer.enter_edit();
    Self { buffer }
  }

  /// Split the current buffer into `OsString` argv tokens. Empty
  /// runs collapse — extra whitespace is forgiven so users can
  /// reformat their flag string for readability.
  pub fn argv(&self) -> Vec<std::ffi::OsString> {
    self
      .buffer
      .buffer()
      .split_whitespace()
      .map(std::ffi::OsString::from)
      .collect()
  }
}

/// Render the panel centred over `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AdvancedPanelState, palette: &Palette) {
  let modal = centered_rect(80, 50, area);
  frame.render_widget(Clear, modal);
  // Paint the theme surface back over the cleared area so the
  // dialog body honours `palette.bg` (e.g. Latte's light surface)
  // instead of falling through to the terminal default.
  crate::tui::render::paint_theme_bg(frame, modal, palette);
  let block = palette.panel_block(" Advanced flags ", true);
  frame.render_widget(block.clone(), modal);
  let inner = block.inner(modal);

  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Length(3), Constraint::Min(0)])
    .split(inner);

  let intro = Paragraph::new(Line::from(vec![Span::styled(
    "Edit `llama-server` flags. They append AFTER bundled flags so they trump the picker.",
    palette.muted_style(),
  )]))
  .wrap(Wrap { trim: true });
  frame.render_widget(intro, layout[0]);

  // Round-8: caret style mirrors the filter input (no leading
  // `▌ ` block, `▏` cursor) so every text-input in the TUI reads
  // identically.
  let mut spans = vec![Span::styled(
    state.buffer.buffer().to_string(),
    palette.text_style(),
  )];
  if state.buffer.is_editing() {
    spans.push(crate::tui::fmt::caret(palette));
  }
  let body = Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false });
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
  fn from_advanced_joins_with_spaces_and_auto_edits() {
    let advanced = vec![OsString::from("--threads"), OsString::from("8")];
    let s = AdvancedPanelState::from_advanced(&advanced);
    assert_eq!(s.buffer.buffer(), "--threads 8");
    assert!(
      s.buffer.is_editing(),
      "panel must auto-enter edit so the user can type immediately"
    );
  }

  #[test]
  fn argv_splits_on_whitespace_and_collapses_runs() {
    let buffer = crate::tui::input_field::InputField::with_text("  --threads   8  --flash-attn  ");
    let s = AdvancedPanelState { buffer };
    let v: Vec<String> = s
      .argv()
      .iter()
      .map(|o| o.to_string_lossy().into())
      .collect();
    assert_eq!(v, vec!["--threads", "8", "--flash-attn"]);
  }
}
