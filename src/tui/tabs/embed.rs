//! Embed tab — call `/v1/embeddings` on the focused model and
//! show the result's dimensionality + first eight values + L2 norm.

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
  pub input: String,
  pub dim: Option<usize>,
  pub preview: Vec<f64>,
  pub norm: Option<f64>,
  pub last_error: Option<String>,
  pub busy: bool,
  /// Top-of-viewport offset into the rendered output. Round-8: ↑/↓
  /// in `Focus::RightPane` walk this — same shape as Chat / Rerank.
  pub scroll_offset: u16,
  /// Receiver fed by the background `oai_client::embed` task. The
  /// render loop drains it via `try_recv` so a slow `/v1/embeddings`
  /// call never blocks input.
  pub pending: Option<tokio::sync::mpsc::UnboundedReceiver<crate::tui::tabs::TabEvent>>,
}

impl EmbedTabState {
  pub fn record(&mut self, result: crate::tui::oai_client::EmbedResult) {
    self.dim = Some(result.dim);
    self.preview = result.preview;
    self.norm = Some(result.norm);
    self.last_error = None;
    self.busy = false;
    self.scroll_offset = 0;
  }

  pub fn record_error(&mut self, msg: String) {
    self.last_error = Some(msg);
    self.busy = false;
  }

  pub fn scroll_up(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_add(1);
  }

  pub fn scroll_down(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_sub(1);
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

  // Idle key-hint chips moved to the right pane's bottom border.
  // Status row carries only the live busy / error state.
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
    text: &state.input,
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
      scroll_offset: state.scroll_offset,
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
