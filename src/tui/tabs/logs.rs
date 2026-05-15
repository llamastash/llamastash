//! Logs tab — auto-tails the daemon's per-launch log ring buffer.
//!
//! v1 reads from `logs_tail` on the same refresher tick as the
//! status snapshots; pause/resume hotkeys land alongside Unit 8's
//! `llamatui logs --follow` work. The renderer pulls `lines` off
//! the App so the tab is purely a presentation concern.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

/// Maximum lines we keep around in the in-memory mirror so a
/// long-running model doesn't grow this buffer unboundedly.
const MAX_LINES: usize = 4096;

/// In-memory mirror of the daemon's per-launch ring buffer. The
/// renderer trims to the visible viewport so a long-running model's
/// log doesn't bloat the render path.
#[derive(Debug, Clone)]
pub struct LogsTabState {
  pub lines: Vec<String>,
  /// Auto-scroll keeps the viewport pinned to the tail; toggled by
  /// the `s` hotkey in the right pane.
  pub auto_scroll: bool,
  /// Launch id this buffer mirrors. Cleared (and `lines` reset)
  /// when the user focuses a different launch.
  pub launch_id: Option<String>,
}

impl Default for LogsTabState {
  fn default() -> Self {
    Self::new()
  }
}

impl LogsTabState {
  pub fn new() -> Self {
    Self {
      lines: Vec::new(),
      auto_scroll: true,
      launch_id: None,
    }
  }

  /// Replace the buffer with a fresh tail from the daemon. The
  /// `logs_tail` IPC method returns a snapshot so we just adopt it
  /// wholesale; the daemon's ring buffer already caps growth.
  pub fn set_tail(&mut self, launch_id: String, lines: Vec<String>) {
    self.launch_id = Some(launch_id);
    self.lines = lines;
    if self.lines.len() > MAX_LINES {
      let drop = self.lines.len() - MAX_LINES;
      self.lines.drain(..drop);
    }
  }

  /// Drop accumulated state when the user moves focus to a launch
  /// the buffer doesn't cover. Keeps the auto-scroll preference.
  pub fn clear(&mut self) {
    self.lines.clear();
    self.launch_id = None;
  }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &LogsTabState, palette: &Palette) {
  let block = Block::default()
    .title(" Logs ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.muted));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  if state.lines.is_empty() {
    let hint = Paragraph::new(Line::from(Span::styled(
      "no log lines yet — launch a model or wait for the daemon to forward stderr",
      Style::default().fg(palette.muted),
    )))
    .wrap(Wrap { trim: true });
    frame.render_widget(hint, inner);
    return;
  }

  let visible = state.lines.len().min(inner.height as usize);
  let start = state.lines.len().saturating_sub(visible);
  let body: Vec<Line<'_>> = state
    .lines
    .iter()
    .skip(start)
    .map(|l| Line::from(Span::styled(l.as_str(), Style::default().fg(palette.fg))))
    .collect();
  let mut p = Paragraph::new(body).wrap(Wrap { trim: false });
  if !state.auto_scroll {
    p = p.style(Style::default().add_modifier(Modifier::DIM));
  }
  frame.render_widget(p, inner);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn defaults_to_auto_scroll() {
    let s = LogsTabState::new();
    assert!(s.auto_scroll);
    assert!(s.lines.is_empty());
    assert!(s.launch_id.is_none());
  }

  #[test]
  fn set_tail_overwrites_lines_and_caps_to_max() {
    let mut s = LogsTabState::new();
    let lines: Vec<String> = (0..(MAX_LINES + 50)).map(|i| format!("l{i}")).collect();
    s.set_tail("L1".into(), lines);
    assert_eq!(s.launch_id.as_deref(), Some("L1"));
    assert_eq!(s.lines.len(), MAX_LINES);
    assert_eq!(s.lines[0], format!("l{}", 50));
  }
}
