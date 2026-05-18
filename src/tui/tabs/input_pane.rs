//! Shared layout for the Chat / Embed / Rerank tab bodies.
//!
//! Each of those three tabs is one or more bordered prompt fields
//! stacked at the top, a free-form body area in the middle, and a
//! single status line at the bottom. The same `render` here paints
//! the bordered prompt(s) + status frame, so the three tabs stay
//! visually identical without the per-tab modules duplicating the
//! layout math.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

/// One bordered prompt field. `active` toggles the caret + accent
/// border so the user sees an unambiguous typing target.
pub struct PromptField<'a> {
  pub title: &'a str,
  pub text: &'a str,
  pub active: bool,
}

/// Inputs to [`render`].
pub struct InputPaneOpts<'a> {
  /// Bordered prompt field(s) stacked at the top, in order. May be
  /// empty (Logs / Settings don't use the input pane).
  pub prompts: &'a [PromptField<'a>],
  /// Body content beneath the prompts. Wrapped, no border.
  pub body: Vec<Line<'a>>,
  /// Bottom status line (busy / error / idle hint chips).
  pub status: Line<'a>,
  /// Whether to render the body in BOLD (used by Chat while a
  /// stream is in flight). Has no effect on the prompts or status.
  pub bold_body: bool,
}

/// Render the input pane into `area`. Layout: `Length(3)` per
/// prompt, then `Min(1)` for the body, then `Length(1)` for the
/// status line.
pub fn render(frame: &mut Frame<'_>, area: Rect, opts: InputPaneOpts<'_>, palette: &Palette) {
  let mut constraints: Vec<Constraint> = Vec::with_capacity(opts.prompts.len() + 2);
  for _ in opts.prompts {
    constraints.push(Constraint::Length(3));
  }
  constraints.push(Constraint::Min(1));
  constraints.push(Constraint::Length(1));
  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints)
    .split(area);

  for (i, p) in opts.prompts.iter().enumerate() {
    render_prompt(frame, layout[i], p, palette);
  }
  let body_idx = opts.prompts.len();
  let status_idx = body_idx + 1;

  let mut body_widget = Paragraph::new(opts.body).wrap(Wrap { trim: false });
  if opts.bold_body {
    body_widget = body_widget.style(Style::default().add_modifier(Modifier::BOLD));
  }
  frame.render_widget(body_widget, layout[body_idx]);
  frame.render_widget(Paragraph::new(opts.status), layout[status_idx]);
}

fn render_prompt(frame: &mut Frame<'_>, area: Rect, field: &PromptField<'_>, palette: &Palette) {
  let border = if field.active {
    palette.accent
  } else {
    palette.muted
  };
  let block = Block::default()
    .title(format!(" {} ", field.title))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(border));
  let inner = block.inner(area);
  frame.render_widget(block, area);
  let mut spans = vec![
    Span::styled("▌ ", Style::default().fg(palette.accent)),
    Span::styled(field.text.to_string(), Style::default().fg(palette.fg)),
  ];
  if field.active {
    spans.push(Span::styled(
      "│",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::REVERSED),
    ));
  }
  frame.render_widget(
    Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
    inner,
  );
}

/// Build the standard idle status line for an input-pane tab: a
/// `· `-separated chip strip rendered in `palette.muted`. Empty
/// chips are dropped silently so a config rebind that removes a
/// key doesn't leave a dangling separator.
pub fn idle_status_line<'a>(chips: &[String], palette: &Palette) -> Line<'a> {
  let mut spans: Vec<Span<'a>> = Vec::with_capacity(chips.len() * 2);
  for (i, chip) in chips.iter().filter(|c| !c.is_empty()).enumerate() {
    if i > 0 {
      spans.push(Span::styled(
        " · ",
        Style::default().fg(palette.muted),
      ));
    }
    spans.push(Span::styled(
      chip.clone(),
      Style::default().fg(palette.muted),
    ));
  }
  Line::from(spans)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::theme::{palette_for, ThemeName};

  #[test]
  fn idle_status_joins_chips_with_middot_separator() {
    let palette = palette_for(ThemeName::Macchiato);
    let line = idle_status_line(
      &[
        "Shift+Enter:newline".to_string(),
        "Esc:clear".to_string(),
      ],
      palette,
    );
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "Shift+Enter:newline · Esc:clear");
  }

  #[test]
  fn idle_status_drops_empty_chips() {
    let palette = palette_for(ThemeName::Macchiato);
    let line = idle_status_line(
      &["a:b".to_string(), String::new(), "c:d".to_string()],
      palette,
    );
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "a:b · c:d");
  }
}
