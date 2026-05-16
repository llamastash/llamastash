//! Single-frame renderer.
//!
//! Layout (top→bottom):
//! - Banner row (1 line) with the connection status pill.
//! - Body: list pane (left, 50% width) + right pane placeholder
//!   (right, 50% width). Unit 7 fills the right pane.
//! - Filter input (1 line, only when `Focus::Filter`).
//! - Help bar (1 line) — keybindings for the active focus + a
//!   transient toast slot.
//!
//! Modal overlays (launch picker, advanced panel) draw on top of
//! the body when their state is set.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::{advanced_panel, help_bar, launch_picker, list_pane, right_pane};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
  app.expire_toast();
  app.ensure_right_tab_reachable();
  let palette = app.palette();
  let area = frame.area();

  let show_filter = app.focus == Focus::Filter;
  let mut constraints: Vec<Constraint> = vec![Constraint::Length(1), Constraint::Min(1)];
  if show_filter {
    constraints.push(Constraint::Length(1));
  }
  constraints.push(Constraint::Length(1));

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints)
    .split(area);

  render_banner(frame, chunks[0], app, palette);
  render_body(frame, chunks[1], app, palette);
  let mut next = 2;
  if show_filter {
    render_filter_line(frame, chunks[next], app, palette);
    next += 1;
  }
  help_bar::render(frame, chunks[next], app.focus, app.toast_message(), palette);

  // Overlays last.
  if app.focus == Focus::LaunchPicker {
    if let Some(state) = &app.launch_picker {
      launch_picker::render(frame, area, state, palette);
    }
  }
  if app.focus == Focus::AdvancedPanel {
    if let Some(state) = &app.advanced_panel {
      advanced_panel::render(frame, area, state, palette);
    }
  }
}

fn render_banner(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let pill = if app.daemon_connected {
    Span::styled(
      " daemon: connected ",
      Style::default()
        .bg(palette.success)
        .fg(palette.bg)
        .add_modifier(Modifier::BOLD),
    )
  } else {
    Span::styled(
      " daemon: connecting… ",
      Style::default()
        .bg(palette.warning)
        .fg(palette.bg)
        .add_modifier(Modifier::BOLD),
    )
  };
  let title = Span::styled(
    "LlamaDash",
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD),
  );
  let theme = Span::styled(
    format!("  theme: {}", app.options.theme.canonical()),
    Style::default().fg(palette.muted),
  );
  let line = Line::from(vec![title, theme, Span::raw("   "), pill]);
  frame.render_widget(Paragraph::new(line), area);
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
    .split(area);
  let rows = app.rendered_rows();
  if rows.is_empty() {
    render_empty_state(frame, split[0], palette);
  } else {
    list_pane::render(frame, split[0], &rows, app.list_cursor, palette);
  }
  right_pane::render(frame, split[1], app, palette);
}

fn render_empty_state(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
  let block = Block::default()
    .title(" Models ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(area);
  frame.render_widget(block, area);
  let lines = vec![
    Line::from(Span::styled(
      "No GGUFs surfaced yet.",
      Style::default().fg(palette.fg),
    )),
    Line::from(Span::styled(
      "Drop a `.gguf` into a watched directory or run `llamadash --model-path <DIR>`.",
      Style::default().fg(palette.muted),
    )),
  ];
  frame.render_widget(Paragraph::new(lines), inner);
}

fn render_filter_line(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let line = Line::from(vec![
    Span::styled(
      "/",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    ),
    Span::raw(" "),
    Span::styled(&app.filter_buffer, Style::default().fg(palette.fg)),
    Span::styled(
      "│",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::REVERSED),
    ),
  ]);
  frame.render_widget(Paragraph::new(line), area);
}
