//! Modal `?` help overlay listing every keybinding grouped by
//! panel. Rendered in two columns so a single screen can carry the
//! full keymap. Centred over the dashboard with a translucent
//! border; Esc or `?` closes it.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Binding, Focus};

/// Focus order in the help dialog. The currently-focused panel is
/// rendered first so the most-relevant bindings sit at the top of
/// column 1.
const FOCUS_ORDER: &[Focus] = &[
  Focus::List,
  Focus::Filter,
  Focus::RightPane,
  Focus::ChatInput,
  Focus::EmbedInput,
  Focus::RerankInput,
  Focus::LaunchPicker,
  Focus::AdvancedPanel,
];

/// Render the overlay. Caller is responsible for only invoking
/// this when `app.show_help` is true. Reads bindings from
/// `app.bindings_for(...)` so config-driven keybinding overrides
/// surface in the help screen.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let focus = app.focus;
  let rect = centred(
    area,
    area.width.saturating_sub(6).min(110),
    area.height.saturating_sub(4).max(20),
  );
  frame.render_widget(Clear, rect);

  let block = Block::default()
    .title(Line::from(Span::styled(
      " Help · Esc or ? to close ",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    )))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  // Split inner into header + body, then body into two columns.
  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Min(1),
    ])
    .split(inner);

  let header = Paragraph::new(Line::from(vec![
    Span::styled("Active focus: ", Style::default().fg(palette.muted)),
    Span::styled(
      focus_label(focus),
      Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
    ),
  ]))
  .alignment(Alignment::Left);
  frame.render_widget(header, chunks[0]);

  let cols = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
    .split(chunks[2]);

  let groups = group_lines(focus, app, palette);
  let (left, right) = split_groups(groups);
  frame.render_widget(Paragraph::new(left).wrap(Wrap { trim: false }), cols[0]);
  frame.render_widget(Paragraph::new(right).wrap(Wrap { trim: false }), cols[1]);
}

/// Build per-focus blocks of `Line`s. Each block leads with a
/// heading (focus label) followed by one line per binding
/// (`key:label`) and a trailing blank for vertical separation.
fn group_lines(active: Focus, app: &App, palette: &Palette) -> Vec<Vec<Line<'static>>> {
  let mut groups: Vec<Vec<Line<'static>>> = Vec::with_capacity(FOCUS_ORDER.len());
  // Walk in priority order: currently-active focus first, then the
  // rest in the canonical order. Keeps the most-relevant chunk at
  // the top of the left column.
  let mut ordered: Vec<Focus> = vec![active];
  for f in FOCUS_ORDER {
    if *f != active {
      ordered.push(*f);
    }
  }
  for focus in ordered {
    let bindings: &[Binding] = app.bindings_for(focus);
    if bindings.is_empty() {
      continue;
    }
    let mut block: Vec<Line<'static>> = Vec::with_capacity(bindings.len() + 2);
    block.push(Line::from(Span::styled(
      focus_label(focus).to_string(),
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    )));
    for b in bindings {
      block.push(Line::from(vec![
        Span::styled(
          format!("  {:<10}", b.label),
          Style::default()
            .fg(palette.label)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(b.description.to_string(), Style::default().fg(palette.fg)),
      ]));
    }
    block.push(Line::default());
    groups.push(block);
  }
  groups
}

/// Split the per-focus blocks across two columns. We pack greedily
/// from the top by total line height — biased so column 1 ≥
/// column 2 (the active focus, always first, lives in column 1).
fn split_groups(groups: Vec<Vec<Line<'static>>>) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
  let total: usize = groups.iter().map(|g| g.len()).sum();
  let target = total.div_ceil(2);
  let mut left: Vec<Line<'static>> = Vec::with_capacity(target + 4);
  let mut right: Vec<Line<'static>> = Vec::with_capacity(target + 4);
  let mut left_height = 0usize;
  for g in groups {
    if left_height + g.len() <= target || left.is_empty() {
      left_height += g.len();
      left.extend(g);
    } else {
      right.extend(g);
    }
  }
  (left, right)
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
