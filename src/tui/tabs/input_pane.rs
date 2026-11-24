//! Shared layout for the Chat / Embed / Rerank tab bodies.
//!
//! Each of those three tabs is one or more bordered prompt fields
//! stacked at the top, a free-form body area in the middle, and a
//! single status line at the bottom. The same `render` here paints
//! the bordered prompt(s) + status frame, so the three tabs stay
//! visually identical without the per-tab modules duplicating the
//! layout math.

use std::cell::Cell;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
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
  /// Top-of-viewport offset into `body`. 0 = pinned to the top;
  /// larger values reveal content further down (scroll toward the
  /// end). Passed as a cell so the render can clamp it to the wrapped
  /// content height and *write the clamp back*: without that, holding
  /// `↓` past the end (common while following a stream) inflates the
  /// stored offset far beyond the content, and a later `↑` does
  /// nothing until it drains back below the max — the pane reads as
  /// frozen once the response stops growing.
  pub scroll_offset: &'a Cell<u16>,
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

  let body_area = layout[body_idx];
  let mut body_widget = Paragraph::new(opts.body).wrap(Wrap { trim: false });
  if opts.bold_body {
    body_widget = body_widget.style(Style::default().add_modifier(Modifier::BOLD));
  }
  // Clamp the offset to the wrapped content height so scrolling past
  // the last line shows the tail pinned to the bottom, not a blank
  // pane. `line_count` accounts for wrapping at the body width. Write
  // the clamped value back so an over-scroll can't inflate the stored
  // offset (which would leave a later `↑` looking dead).
  let wrapped = body_widget.line_count(body_area.width) as u16;
  let max_offset = wrapped.saturating_sub(body_area.height);
  let offset = opts.scroll_offset.get().min(max_offset);
  opts.scroll_offset.set(offset);
  let body_widget = body_widget.scroll((offset, 0));
  frame.render_widget(body_widget, body_area);
  frame.render_widget(Paragraph::new(opts.status), layout[status_idx]);
}

fn render_prompt(frame: &mut Frame<'_>, area: Rect, field: &PromptField<'_>, palette: &Palette) {
  let block = palette.panel_block(&format!(" {} ", field.title), field.active);
  let inner = block.inner(area);
  frame.render_widget(block, area);
  // Caret style matches the Models pane filter (`▏` + REVERSED) so
  // all single-line text inputs read the same.
  let mut spans = vec![Span::styled(field.text.to_string(), palette.text_style())];
  if field.active {
    spans.push(crate::tui::fmt::caret(palette));
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
      spans.push(Span::styled(" · ", palette.muted_style()));
    }
    spans.push(Span::styled(chip.clone(), palette.muted_style()));
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
      &["⇧+Enter:newline".to_string(), "Esc:clear".to_string()],
      palette,
    );
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "⇧+Enter:newline · Esc:clear");
  }

  fn render_body_frame(body: Vec<Line<'static>>, scroll_offset: &Cell<u16>) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let palette = palette_for(ThemeName::Macchiato);
    let mut term = Terminal::new(TestBackend::new(40, 10)).unwrap();
    term
      .draw(|f| {
        super::render(
          f,
          Rect::new(0, 0, 40, 10),
          InputPaneOpts {
            prompts: &[],
            body,
            status: Line::from(""),
            bold_body: false,
            scroll_offset,
          },
          palette,
        );
      })
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area.height {
      let mut r = String::new();
      for x in 0..buf.area.width {
        r.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(r.trim_end().to_string());
    }
    rows.join("\n")
  }

  #[test]
  fn input_pane_render_applies_scroll_offset() {
    // With content taller than the pane, a non-zero offset skips
    // earlier lines without panicking or blanking the buffer.
    let body: Vec<Line<'static>> = (0..20)
      .map(|i| Line::from(Span::raw(format!("line {i}"))))
      .collect();
    let off = Cell::new(2);
    let frame = render_body_frame(body, &off);
    assert!(
      frame.contains("line 2"),
      "scroll_offset must skip earlier lines: {frame}"
    );
    assert!(
      !frame.contains("line 0"),
      "scroll_offset must hide skipped lines: {frame}"
    );
  }

  #[test]
  fn input_pane_clamps_scroll_offset_to_content_height() {
    // Over-scrolling past the last line must pin the tail to the
    // bottom, not blank the pane — and the clamp must be written back
    // so the stored offset can't inflate (which would leave a later
    // `↑` looking dead until it drained off the excess).
    let body: Vec<Line<'static>> = (0..3)
      .map(|i| Line::from(Span::raw(format!("line {i}"))))
      .collect();
    let off = Cell::new(999);
    let frame = render_body_frame(body, &off);
    assert!(
      frame.contains("line 0") && frame.contains("line 2"),
      "huge offset on a short body must clamp, keeping content visible: {frame}"
    );
    assert_eq!(
      off.get(),
      0,
      "the clamp must be written back so the offset can't stay inflated"
    );
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
