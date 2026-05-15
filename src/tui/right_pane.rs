//! Right-pane tab dispatcher.
//!
//! Renders the tab strip at the top of the pane and delegates the
//! body area to the active tab's renderer. The renderer pulls per-
//! tab state straight off `App` so the dispatch is purely a
//! switching concern.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::status_icons::{glyph_for, label_for};
use crate::tui::tabs::{chat, embed, logs, rerank, RightTab};

/// Render the full right-pane area: tab strip + the active tab.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let outer = Block::default()
    .title(right_pane_title(app))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = outer.inner(area);
  frame.render_widget(outer, area);

  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Length(1), Constraint::Min(1)])
    .split(inner);

  render_tab_strip(frame, layout[0], app, palette);

  match app.right_tab {
    RightTab::Logs => {
      let state = build_logs_state(app);
      logs::render(frame, layout[1], &state, palette);
    }
    RightTab::Chat => chat::render(frame, layout[1], &app.chat, palette),
    RightTab::Embed => embed::render(frame, layout[1], &app.embed, palette),
    RightTab::Rerank => rerank::render(frame, layout[1], &app.rerank, palette),
  }
}

fn right_pane_title(app: &App) -> String {
  match app.focused_managed() {
    Some(m) => format!(
      " {} · port {} · {} {} ",
      m.path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("model"),
      m.port,
      glyph_for(m.state),
      label_for(m.state)
    ),
    None => match app.focused_path() {
      Some(p) => format!(
        " {} · not launched ",
        p.file_stem().and_then(|s| s.to_str()).unwrap_or("model")
      ),
      None => " — ".into(),
    },
  }
}

fn render_tab_strip(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let tabs = app.available_right_tabs();
  let mut spans: Vec<Span<'_>> = Vec::with_capacity(tabs.len() * 3);
  for (i, tab) in tabs.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" │ ", Style::default().fg(palette.muted)));
    }
    let style = if *tab == app.right_tab {
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
      Style::default().fg(palette.muted)
    };
    spans.push(Span::styled(tab.label(), style));
  }
  spans.push(Span::styled(
    "   Tab cycles · Esc returns to list",
    Style::default().fg(palette.muted),
  ));
  frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn build_logs_state(_app: &App) -> crate::tui::tabs::logs::LogsTabState {
  // v1 surfaces an empty Logs tab — the `logs_tail` refresher hook
  // lands alongside Unit 8's CLI `logs --follow`. The plumbing is
  // intentionally stubbed here so the tab is reachable; filling it
  // with live lines is a paired follow-up.
  crate::tui::tabs::logs::LogsTabState::default()
}
