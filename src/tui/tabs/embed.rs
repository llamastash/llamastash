//! Embed tab — call `/v1/embeddings` on the focused model and
//! show the result's dimensionality + first eight values + L2 norm.

use std::cell::Cell;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::tabs::input_pane::{InputPaneOpts, PromptField};

#[derive(Debug, Default)]
pub struct EmbedTabState {
  /// Modal text-input field. Same `e:edit / Esc:stop / 2nd-Esc:clear`
  /// contract as every other text input in the TUI.
  pub input: crate::tui::input_field::InputField,
  pub dim: Option<usize>,
  pub preview: Vec<f64>,
  pub norm: Option<f64>,
  pub last_error: Option<String>,
  pub busy: bool,
  /// Top-of-viewport offset into the rendered output. ↑/↓ walk
  /// this — same shape as Chat / Rerank. A cell so the renderer
  /// can clamp it to the wrapped content height and write the clamp
  /// back (see `input_pane::InputPaneOpts::scroll_offset`).
  pub scroll_offset: Cell<u16>,
}

impl EmbedTabState {
  pub fn record(&mut self, result: crate::tui::oai_client::EmbedResult) {
    self.dim = Some(result.dim);
    self.preview = result.preview;
    self.norm = Some(result.norm);
    self.last_error = None;
    self.busy = false;
    self.scroll_offset.set(0);
  }

  pub fn record_error(&mut self, msg: String) {
    self.last_error = Some(msg);
    self.busy = false;
  }

  /// Scroll the output up one line (toward the top). `scroll_offset`
  /// is the top-of-viewport index, so up *decreases* it; clamps at 0.
  pub fn scroll_up(&mut self) {
    self
      .scroll_offset
      .set(self.scroll_offset.get().saturating_sub(1));
  }

  /// Scroll the output down one line (toward the end). Increases the
  /// offset; the render clamps it to the wrapped content height (and
  /// writes the clamp back).
  pub fn scroll_down(&mut self) {
    self
      .scroll_offset
      .set(self.scroll_offset.get().saturating_add(1));
  }
}

/// Render the Embed tab body into `area`. Block borders are owned
/// by the right pane caller.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let state = &app.embed;
  let active = app.focus == Focus::EmbedInput;

  let mut body: Vec<Line<'_>> = Vec::new();
  if let Some(dim) = state.dim {
    body.push(Line::from(Span::styled(
      format!("dim = {dim}"),
      palette.text_style(),
    )));
    if !state.preview.is_empty() {
      let preview = state
        .preview
        .iter()
        .map(|v| format!("{v:+.4}"))
        .collect::<Vec<_>>()
        .join(", ");
      body.push(Line::from(Span::styled(
        format!("first8 = [{preview}]"),
        palette.muted_style(),
      )));
    }
    if let Some(n) = state.norm {
      body.push(Line::from(Span::styled(
        format!("L2 norm = {n:.4}"),
        palette.muted_style(),
      )));
    }
  } else {
    body.push(Line::from(Span::styled(
      "Embed the input with Enter.",
      palette.muted_style(),
    )));
  }

  // The status row carries only the live busy / error state. Idle
  // key-hint chips live on the right pane's bottom border.
  let status = match (state.busy, &state.last_error) {
    (true, _) => Line::from(Span::styled(
      "calling /v1/embeddings…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err)) => Line::from(Span::styled(format!("error: {err}"), palette.error_style())),
    _ => Line::from(""),
  };

  let prompt = PromptField {
    title: "Input",
    text: state.input.buffer(),
    active,
  };
  crate::tui::tabs::input_pane::render(
    frame,
    area,
    InputPaneOpts {
      prompts: &[prompt],
      body,
      status,
      bold_body: false,
      scroll_offset: &state.scroll_offset,
    },
    palette,
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::oai_client::EmbedResult;

  #[test]
  fn record_overrides_previous_error() {
    let mut s = EmbedTabState {
      last_error: Some("stale".into()),
      ..Default::default()
    };
    s.record(EmbedResult {
      dim: 1024,
      preview: vec![0.0; 8],
      norm: 1.0,
    });
    assert_eq!(s.dim, Some(1024));
    assert!(s.last_error.is_none());
  }
}
