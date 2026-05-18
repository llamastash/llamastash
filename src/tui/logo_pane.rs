//! Top-right info-row pane: compact monogram glyph only.
//!
//! Theme tag lives in the top header bar (next to the daemon
//! label), not in this panel. The width-hide fallback (panel
//! disappears when inner width is too small) is owned by
//! [`super::render`].

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::banner::COMPACT_BANNER;
use crate::theme::Palette;
use crate::tui::app::App;

/// Llama mascot perched on top of the "L-" dash. 2-cell wide emoji
/// that replaces the same number of cells in the row immediately
/// above the dash.
const LLAMA_GLYPH: &str = "🦙";

/// Render the Logo panel.
pub fn render(frame: &mut Frame<'_>, area: Rect, _app: &App, palette: &Palette) {
  let block = Block::default()
    .borders(Borders::ALL)
    .border_style(palette.accent_style());
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let glyphs = glyph_lines();
  let dash_row = locate_dash_row(&glyphs);
  // Resolve the dash column from the actual dash row so the emoji
  // row above it knows where to inject — the L vertical rows
  // themselves don't carry a dash to detect.
  let dash_col = dash_row.and_then(|r| dash_column(glyphs[r]));
  let lines: Vec<Line<'_>> = glyphs
    .iter()
    .enumerate()
    .map(|(i, &line)| build_row(i, line, dash_row, dash_col, palette))
    .collect();

  let para = Paragraph::new(lines).alignment(Alignment::Center);
  frame.render_widget(para, inner);
}

/// Compose one banner row. The row directly above the dash gets the
/// llama emoji overlaid at the dash's horizontal slot — the rest
/// stay as plain accent-coloured banner glyphs.
fn build_row<'a>(
  idx: usize,
  raw: &'a str,
  dash_row: Option<usize>,
  dash_col: Option<usize>,
  palette: &Palette,
) -> Line<'a> {
  let base_style = Style::default()
    .fg(palette.accent)
    .add_modifier(Modifier::BOLD);
  if Some(idx + 1) == dash_row {
    // Row above the dash: split the row at the same byte offset the
    // dash uses on the next row so the emoji lands directly above.
    if let Some(col) = dash_col {
      if col <= raw.len() {
        let left = &raw[..col];
        // Emoji is 2 cells; trail with a single space so the row's
        // total cell width matches the 3-cell `███` beneath it.
        return Line::from(vec![
          Span::styled(left, base_style),
          Span::raw(LLAMA_GLYPH.to_string()),
          Span::styled(" ", base_style),
        ]);
      }
    }
  }
  Line::styled(raw, base_style)
}

/// Find the index of the row containing the dash. The dash is the
/// only banner row where `██` reappears past the leading column —
/// e.g. `██  ███`. Returns `None` when the banner has no such row
/// (single-letter monograms, etc.) so the caller renders untouched.
fn locate_dash_row(rows: &[&str]) -> Option<usize> {
  rows.iter().position(|line| dash_column(line).is_some())
}

/// Byte offset where the dash segment starts — suitable for
/// `str::split_at`. Detected as the second `██` run in the row;
/// the first run is the L's vertical stroke. `██` is 3 UTF-8 bytes
/// per glyph (2 display cells), so this is a byte index, not a
/// cell index.
fn dash_column(line: &str) -> Option<usize> {
  let bytes = line.as_bytes();
  // Find the first `██` (L vertical), skip past it, then look for
  // another `██` after at least one space.
  let pat = "██".as_bytes();
  if !bytes.starts_with(pat) {
    return None;
  }
  let mut i = pat.len();
  // Require at least one space between the L stroke and the dash.
  let mut saw_space = false;
  while i < bytes.len() {
    if bytes[i] == b' ' {
      saw_space = true;
      i += 1;
    } else {
      break;
    }
  }
  if !saw_space || i >= bytes.len() {
    return None;
  }
  if line[i..].starts_with("██") {
    Some(i)
  } else {
    None
  }
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
  fn dash_column_locates_the_l_dash_segment() {
    // Plain "L" rows have no second `██` run.
    assert_eq!(dash_column("██     "), None);
    assert_eq!(dash_column("███████"), None);
    // The dash row has a second `██` after whitespace. `██` is 3
    // UTF-8 bytes per glyph (2 display cells), so `██  ` is 8 bytes.
    assert_eq!(dash_column("██  ███"), Some(8));
  }

  #[test]
  fn locate_dash_row_finds_the_row_with_a_dash() {
    let rows = ["██     ", "██     ", "██  ███", "██     ", "███████"];
    assert_eq!(locate_dash_row(&rows), Some(2));
  }

  #[test]
  fn llama_glyph_renders_one_row_above_the_dash() {
    // Render at a width that prevents the centred Paragraph from
    // shifting cells around, so we can match cell positions
    // directly.
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app, 14, 7);
    let body = rows.join("\n");
    assert!(
      body.contains('🦙'),
      "expected llama glyph somewhere in panel: {body}"
    );
    // Find the dash row; the llama must land on the row above it.
    let dash_row_idx = rows
      .iter()
      .position(|r| r.contains("███") && r.contains("██  "))
      .expect("dash row present");
    assert!(dash_row_idx > 0, "dash row cannot be at the top");
    let above = &rows[dash_row_idx - 1];
    assert!(
      above.contains('🦙'),
      "llama must sit directly above the dash: above={above:?}"
    );
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
