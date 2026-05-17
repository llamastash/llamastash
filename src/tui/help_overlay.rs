//! Modal `?` help overlay listing every keybinding for the current
//! focus. Centred over the dashboard with a translucent border;
//! Esc or `?` closes it.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::keybindings::{Focus, DEFAULT_BINDINGS};

/// Render the overlay. Caller is responsible for only invoking
/// this when `app.show_help` is true.
pub fn render(frame: &mut Frame<'_>, area: Rect, focus: Focus, palette: &Palette) {
  let rect = centred(area, 64, 22);
  frame.render_widget(Clear, rect);

  let block = Block::default()
    .title(Line::from(Span::styled(
      " Help · ? to close ",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    )))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  // Split inner into header + body.
  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Length(1), Constraint::Min(1)])
    .split(inner);

  let scope = focus_label(focus);
  let header = Paragraph::new(Line::from(vec![
    Span::styled("Focus: ", Style::default().fg(palette.muted)),
    Span::styled(
      scope,
      Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
    ),
  ]))
  .alignment(Alignment::Left);
  frame.render_widget(header, chunks[0]);

  let bindings = DEFAULT_BINDINGS
    .iter()
    .find(|(f, _)| *f == focus)
    .map(|(_, b)| *b)
    .unwrap_or(&[]);

  let mut lines: Vec<Line<'_>> = Vec::with_capacity(bindings.len());
  for b in bindings {
    lines.push(Line::from(vec![
      Span::styled(
        format!("  {:<10}", b.label),
        Style::default()
          .fg(palette.accent)
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(b.description, Style::default().fg(palette.fg)),
    ]));
  }
  if lines.is_empty() {
    lines.push(Line::from(Span::styled(
      "  (no bindings registered for this focus)",
      Style::default().fg(palette.muted),
    )));
  }

  frame.render_widget(Paragraph::new(lines), chunks[1]);
}

/// Short human-readable label for a focus. Mirrors the variants in
/// [`Focus`].
fn focus_label(focus: Focus) -> &'static str {
  match focus {
    Focus::List => "Models list",
    Focus::Filter => "Filter input",
    Focus::LaunchPicker => "Launch picker",
    Focus::AdvancedPanel => "Advanced flags",
    Focus::RightPane => "Right pane",
    Focus::ChatInput => "Chat prompt",
    Focus::EmbedInput => "Embed input",
    Focus::RerankInput => "Rerank input",
  }
}

/// Centre a `w × h` rect within `area`, clamping to the available
/// space so a narrow terminal still sees the overlay (just snug).
fn centred(area: Rect, w: u16, h: u16) -> Rect {
  let w = w.min(area.width.saturating_sub(2));
  let h = h.min(area.height.saturating_sub(2));
  let x = area.x + (area.width.saturating_sub(w)) / 2;
  let y = area.y + (area.height.saturating_sub(h)) / 2;
  Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn focus_label_distinct_per_variant() {
    use std::collections::HashSet;
    let labels: HashSet<&'static str> = [
      Focus::List,
      Focus::Filter,
      Focus::LaunchPicker,
      Focus::AdvancedPanel,
      Focus::RightPane,
      Focus::ChatInput,
      Focus::EmbedInput,
      Focus::RerankInput,
    ]
    .iter()
    .copied()
    .map(focus_label)
    .collect();
    assert_eq!(labels.len(), 8);
  }

  #[test]
  fn centred_clamps_to_area() {
    let area = Rect::new(0, 0, 40, 10);
    let r = centred(area, 80, 30);
    assert!(r.width <= 38);
    assert!(r.height <= 8);
  }
}
