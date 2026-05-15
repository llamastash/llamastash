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

/// In-memory mirror of the daemon's per-launch ring buffer. The
/// renderer trims to the visible viewport so a long-running model's
/// log doesn't bloat the render path.
#[derive(Debug, Clone, Default)]
pub struct LogsTabState {
  pub lines: Vec<String>,
  /// User scrolled up — auto-scroll stays paused until they press
  /// `End` or `g` to resume. v1 ships without scroll keys; the
  /// flag lives here so Unit 8's CLI logs --follow shares the
  /// same field.
  pub auto_scroll: bool,
}

impl LogsTabState {
  pub fn new() -> Self {
    Self {
      lines: Vec::new(),
      auto_scroll: true,
    }
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
  }
}
