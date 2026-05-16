//! Single-frame renderer (kdash-style dashboard layout).
//!
//! Vertical:
//! 1. **Title row** (1 line) — `LlamaDash v0.1.0 · ● daemon` left,
//!    global hint strip (`?:help  t:theme  /:filter  q:quit`) right.
//!    Both styled with `palette.accent` background and `palette.bg`
//!    foreground.
//! 2. **Info row** (7 lines) — `Host` (fixed 32 cols), `Daemon` (flex
//!    middle), `Logo` (fixed ~25 cols when there's room). Skipped
//!    entirely when `area.height < 18`.
//! 3. **Body** — Models pane (60%) + right pane with tab strip (40%).
//! 4. **Filter input** (1 line) — only rendered when
//!    `Focus::Filter`. Sits above the body's last row.
//!
//! No bottom help bar — panel-specific hints live in each panel's
//! block title. The global hint strip is on row 1.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::list_pane::TitleInputs;
use crate::tui::{
  advanced_panel, help_bar, host_stats_pane, info_pane, launch_picker, list_pane, logo_pane,
  right_pane,
};

const INFO_ROW_HEIGHT: u16 = 7;
const MIN_HEIGHT_FOR_INFO_ROW: u16 = 18;
const HOST_PANEL_WIDTH: u16 = 32;
const LOGO_PANEL_WIDTH: u16 = 25;
const MIN_LOGO_INNER_WIDTH: u16 = 18;

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
  app.expire_toast();
  app.ensure_right_tab_reachable();
  let palette = app.palette();
  let area = frame.area();

  let show_filter = app.focus == Focus::Filter;
  let show_info_row = area.height >= MIN_HEIGHT_FOR_INFO_ROW;

  // Vertical layout: title, [info,] body, [filter].
  let mut constraints: Vec<Constraint> = Vec::with_capacity(4);
  constraints.push(Constraint::Length(1));
  if show_info_row {
    constraints.push(Constraint::Length(INFO_ROW_HEIGHT));
  }
  constraints.push(Constraint::Min(1));
  if show_filter {
    constraints.push(Constraint::Length(1));
  }
  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints)
    .split(area);

  let mut idx = 0;
  render_title_row(frame, chunks[idx], app, palette);
  idx += 1;
  if show_info_row {
    render_info_row(frame, chunks[idx], app, palette);
    idx += 1;
  }
  render_body(frame, chunks[idx], app, palette);
  idx += 1;
  if show_filter {
    render_filter_line(frame, chunks[idx], app, palette);
  }

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

/// Render the accent-bg title row: brand + daemon dot on the left,
/// global hints on the right.
fn render_title_row(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // Paint the whole row with the accent background first; the
  // sub-renderers below overlay text but inherit the bg via the
  // Paragraph's base style.
  let bg = Paragraph::new("").style(Style::default().bg(palette.accent));
  frame.render_widget(bg, area);

  // Reserve the right slot for the global hint strip; the left slot
  // (brand + daemon dot) flexes into the rest. `global_hint_slot_width`
  // is derived from the same `GLOBAL_HINTS` list the renderer iterates,
  // so the slot can't drift from the rendered text.
  let hint_slot = help_bar::global_hint_slot_width();
  let split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Min(1), Constraint::Length(hint_slot)])
    .split(area);

  render_title_left(frame, split[0], app, palette);
  help_bar::render_global(frame, split[1], palette);
}

fn render_title_left(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let version = env!("CARGO_PKG_VERSION");
  let (dot_color, daemon_label) = if app.daemon_connected {
    (palette.success, "daemon")
  } else {
    (palette.warning, "daemon connecting…")
  };
  let line = Line::from(vec![
    Span::raw(" "),
    Span::styled(
      "LlamaDash",
      Style::default().fg(palette.bg).add_modifier(Modifier::BOLD),
    ),
    Span::styled(format!(" v{version} · "), Style::default().fg(palette.bg)),
    Span::styled("●", Style::default().fg(dot_color)),
    Span::raw(" "),
    Span::styled(daemon_label, Style::default().fg(palette.bg)),
  ]);
  let para = Paragraph::new(line).style(Style::default().bg(palette.accent).fg(palette.bg));
  frame.render_widget(para, area);
}

/// Render the three-panel info row.
fn render_info_row(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // Reserve the Logo panel only when there's enough width for its
  // inner area to be readable. Otherwise the Daemon panel claims the
  // freed space.
  let show_logo = area
    .width
    .saturating_sub(HOST_PANEL_WIDTH)
    .saturating_sub(LOGO_PANEL_WIDTH)
    >= MIN_LOGO_INNER_WIDTH + 2;
  let constraints = if show_logo {
    vec![
      Constraint::Length(HOST_PANEL_WIDTH),
      Constraint::Min(1),
      Constraint::Length(LOGO_PANEL_WIDTH),
    ]
  } else {
    vec![Constraint::Length(HOST_PANEL_WIDTH), Constraint::Min(1)]
  };
  let split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints(constraints)
    .split(area);
  host_stats_pane::render(frame, split[0], &app.host_metrics, palette);
  info_pane::render(frame, split[1], app, palette);
  if show_logo {
    logo_pane::render(frame, split[2], app, palette);
  }
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
    .split(area);
  let rows = app.rendered_rows();
  if rows.is_empty() {
    render_empty_state(
      frame,
      split[0],
      palette,
      app.models.len(),
      app.filter_buffer.as_str(),
    );
  } else {
    let filter = if app.filter_buffer.is_empty() {
      None
    } else {
      Some(app.filter_buffer.as_str())
    };
    list_pane::render(
      frame,
      split[0],
      &rows,
      app.list_cursor,
      TitleInputs {
        total: app.models.len(),
        filter,
      },
      palette,
    );
  }
  right_pane::render(frame, split[1], app, palette);
}

fn render_empty_state(
  frame: &mut Frame<'_>,
  area: Rect,
  palette: &Palette,
  total: usize,
  filter: &str,
) {
  use ratatui::widgets::{Block, Borders};
  let filter_chip = if filter.is_empty() {
    None
  } else {
    Some(filter)
  };
  let title = list_pane::build_block_title(
    &TitleInputs {
      total,
      filter: filter_chip,
    },
    area.width as usize,
  );
  let block = Block::default()
    .title(title)
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  fn render_into(width: u16, height: u16, mut app: App) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| render(f, &mut app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
      let mut row = String::new();
      for x in 0..buf.area.width {
        row.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(row.trim_end().to_string());
    }
    rows
  }

  #[test]
  fn full_size_renders_title_info_and_body() {
    let app = App::new(AppOptions::default());
    let rows = render_into(100, 30, app);
    let body = rows.join("\n");
    assert!(
      body.contains("LlamaDash"),
      "title row missing brand: {body}"
    );
    assert!(body.contains("?:help"), "title row missing global hints");
    assert!(body.contains("Host"), "info row missing Host block");
    assert!(body.contains("Daemon"), "info row missing Daemon block");
    assert!(body.contains("Models"), "body missing Models block");
  }

  #[test]
  fn narrow_height_collapses_info_row() {
    // A 16-row terminal is below MIN_HEIGHT_FOR_INFO_ROW; the info
    // row drops and only title + body render.
    let app = App::new(AppOptions::default());
    let rows = render_into(80, 16, app);
    let body = rows.join("\n");
    assert!(body.contains("LlamaDash"), "title still renders");
    assert!(body.contains("Models"), "body still renders");
    // Host panel block title shouldn't appear when info row is hidden.
    assert!(
      !body.contains("─ Host ─"),
      "info row should be hidden: {body}"
    );
  }

  #[test]
  fn narrow_width_hides_logo_panel() {
    // 70-col terminal: 32 (host) + Logo's 25 + min 18 inner > 70.
    // Logo should drop; Daemon flexes.
    let app = App::new(AppOptions::default());
    let rows = render_into(70, 30, app);
    let body = rows.join("\n");
    assert!(body.contains("Host"));
    assert!(body.contains("Daemon"));
    // The theme name (default `macchiato`) only appears in the Logo
    // block title, so its absence is a clean signal that the Logo
    // panel did not render. (The global hint strip on the title row
    // contains `t:theme` regardless of the Logo panel's visibility,
    // which is why we test the theme *name* rather than the hint.)
    assert!(
      !body.contains("macchiato"),
      "logo panel should be hidden at width 70: {body}"
    );
  }

  #[test]
  fn wider_width_shows_logo_panel() {
    let app = App::new(AppOptions::default());
    let rows = render_into(100, 30, app);
    let body = rows.join("\n");
    // At 100 cols, Host(32) + Daemon(min 1) + Logo(25) easily fit.
    assert!(
      body.contains("macchiato"),
      "logo panel should render at width 100: {body}"
    );
  }

  #[test]
  fn filter_input_appears_only_when_focused() {
    let mut app = App::new(AppOptions::default());
    app.focus = Focus::Filter;
    app.filter_buffer = "qwen".into();
    let rows = render_into(100, 30, app);
    // The filter line appears below the body — find a row starting
    // with `/`.
    assert!(
      rows.iter().any(|r| r.starts_with("/")),
      "expected filter input line: {rows:#?}"
    );
  }
}
