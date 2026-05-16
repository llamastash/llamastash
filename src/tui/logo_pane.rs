//! Top-right info-row pane: compact `LlamaDash` glyph + active-theme
//! hint in the block title.
//!
//! No version string, no metadata text — those live elsewhere
//! (Daemon panel for build/version, theme hotkey for cycling). The
//! width-hide fallback (panel disappears when inner width <18 cols)
//! is owned by [`super::render`], not this component.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::banner::COMPACT_BANNER;
use crate::theme::Palette;
use crate::tui::app::App;

/// Render the Logo panel. The block title carries the active theme
/// name plus a `t:theme` hint so the cycle keybinding is visible
/// inline.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let title = format!(" {} · t:theme ", app.options.theme.short_name());
  let block = Block::default()
    .title(title)
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let lines: Vec<Line<'_>> = glyph_lines()
    .into_iter()
    .map(|line| {
      Line::styled(
        line,
        Style::default()
          .fg(palette.accent)
          .add_modifier(Modifier::BOLD),
      )
    })
    .collect();
  frame.render_widget(Paragraph::new(lines), inner);
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
  fn glyph_lines_are_five_rows() {
    // The Logo panel's inner content area is sized for a 5-row glyph.
    // If COMPACT_BANNER drifts to a different height, the renderer
    // would either clip or leave dead rows.
    assert_eq!(glyph_lines().len(), 5);
  }

  #[test]
  fn glyph_lines_fit_in_22_cols() {
    // Inner area on the wireframe target is ~22 cols. Keep the glyph
    // well under that so the panel reads cleanly on slightly narrower
    // terminals too.
    for (i, line) in glyph_lines().iter().enumerate() {
      assert!(
        line.chars().count() <= 22,
        "glyph row {i} = {:?} exceeds 22 cols",
        line
      );
    }
  }

  #[test]
  fn render_block_title_includes_theme_and_hint() {
    let mut app = App::new(AppOptions::default());
    app.options.theme = ThemeName::Macchiato;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(28, 7)).unwrap();
    term
      .draw(|f| {
        let area = Rect::new(0, 0, 28, 7);
        render(f, area, &app, palette);
      })
      .unwrap();

    let buf = term.backend().buffer().clone();
    let mut top = String::new();
    for x in 0..buf.area.width {
      top.push_str(buf.cell((x, 0)).unwrap().symbol());
    }
    assert!(
      top.contains("macchiato"),
      "expected theme name in block title row, got: {top:?}"
    );
    assert!(
      top.contains("t:theme"),
      "expected `t:theme` hint in block title row, got: {top:?}"
    );
  }

  #[test]
  fn render_block_title_updates_when_theme_cycles() {
    let mut app = App::new(AppOptions::default());
    app.options.theme = ThemeName::Mono;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(28, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 28, 7), &app, palette))
      .unwrap();

    let buf = term.backend().buffer().clone();
    let mut top = String::new();
    for x in 0..buf.area.width {
      top.push_str(buf.cell((x, 0)).unwrap().symbol());
    }
    assert!(
      top.contains("mono"),
      "expected `mono` in title, got: {top:?}"
    );
  }
}
