//! Modal "are you sure?" confirmation popup used by destructive
//! actions (stop a managed launch, kill the whole daemon).
//!
//! `y` / Enter confirms, anything else cancels — see
//! [`events::handle_key`] for the key-routing precedence.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::ConfirmAction;

/// Render the confirmation popup. Caller should only invoke this
/// when `app.confirm_dialog` is `Some(...)`.
pub fn render(frame: &mut Frame<'_>, area: Rect, action: &ConfirmAction, palette: &Palette) {
  let (title, body) = describe(action);

  let rect = centred(area, 60, 8);
  frame.render_widget(Clear, rect);

  let block = Block::default()
    .title(Line::from(Span::styled(
      format!(" {title} "),
      Style::default()
        .fg(palette.error)
        .add_modifier(Modifier::BOLD),
    )))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.error));
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Min(1), Constraint::Length(1)])
    .split(inner);

  let prompt = Paragraph::new(Line::from(Span::styled(
    body,
    Style::default().fg(palette.fg),
  )))
  .wrap(Wrap { trim: true })
  .alignment(Alignment::Center);
  frame.render_widget(prompt, chunks[0]);

  let hint = Paragraph::new(Line::from(vec![
    Span::styled(
      "y / Enter",
      Style::default()
        .fg(palette.success)
        .add_modifier(Modifier::BOLD),
    ),
    Span::styled(" confirm  ·  ", Style::default().fg(palette.muted)),
    Span::styled(
      "Esc / n",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    ),
    Span::styled(" cancel", Style::default().fg(palette.muted)),
  ]))
  .alignment(Alignment::Center);
  frame.render_widget(hint, chunks[1]);
}

/// Title + body text for a confirm action. Kept here so the action
/// definition stays a pure data carrier — the renderer owns its
/// copy of the human strings.
fn describe(action: &ConfirmAction) -> (&'static str, String) {
  match action {
    ConfirmAction::StopModel { name, .. } => (
      "Stop model",
      format!("Stop the running launch of `{name}`?"),
    ),
    ConfirmAction::KillDaemon => (
      "Kill daemon",
      "Shut down the llamadash daemon? All managed launches will be stopped.".to_string(),
    ),
  }
}

fn centred(area: Rect, w: u16, h: u16) -> Rect {
  let w = w.min(area.width.saturating_sub(4));
  let h = h.min(area.height.saturating_sub(2));
  let x = area.x + (area.width.saturating_sub(w)) / 2;
  let y = area.y + (area.height.saturating_sub(h)) / 2;
  Rect::new(x, y, w, h)
}
