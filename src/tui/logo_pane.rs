//! Top-right info-row pane: compact monogram glyph only.
//!
//! Theme tag lives in the top header bar (next to the daemon
//! label), not in this panel. The width-hide fallback (panel
//! disappears when inner width is too small) is owned by
//! [`super::render`].

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::banner::COMPACT_BANNER;
use crate::theme::Palette;
use crate::tui::app::App;

/// Render the Logo panel.
pub fn render(frame: &mut Frame<'_>, area: Rect, _app: &App, palette: &Palette) {
  let block = Block::default()
    .borders(Borders::ALL)
    .border_style(palette.accent_style());
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let style = Style::default()
    .fg(palette.accent)
    .add_modifier(Modifier::BOLD);
  let lines: Vec<Line<'_>> = glyph_lines()
    .into_iter()
    .map(|line| Line::styled(line, style))
    .collect();

  let para = Paragraph::new(lines).alignment(Alignment::Center);
  frame.render_widget(para, inner);
}

/// Split [`COMPACT_BANNER`] into its rendered lines, dropping the
/// leading newline the raw string literal keeps for readability.
fn glyph_lines() -> Vec<&'static str> {
  COMPACT_BANNER
    .strip_prefix('\n')
    .unwrap_or(COMPACT_BANNER)
    .lines()
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::theme::ThemeName;
  use crate::tui::app::{App, AppOptions};
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  #[test]
  fn glyph_lines_fit_in_panel_inner_area() {
    // INFO_ROW_HEIGHT is 7 (5 inner rows). Glyph must fit so the
    // logo panel doesn't clip.
    assert!(
      glyph_lines().len() <= 5,
      "glyph height {} exceeds 5-row inner area",
      glyph_lines().len()
    );
  }

  fn render_lines(app: &App, w: u16, h: u16) -> Vec<String> {
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, w, h), app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area.height {
      let mut row = String::new();
      for x in 0..buf.area.width {
        row.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(row);
    }
    rows
  }

  #[test]
  fn logo_panel_has_no_theme_tag_inside() {
    // Theme tag now lives in the top header bar, not inside the
    // logo panel. Assert the panel body contains no theme name.
    let mut app = App::new(AppOptions::default());
    app.options.theme = ThemeName::Macchiato;
    let rows = render_lines(&app, 14, 7);
    let body = rows.join("\n");
    assert!(
      !body.contains("macchiato"),
      "theme tag must not render in logo panel: {body}"
    );
  }
}
