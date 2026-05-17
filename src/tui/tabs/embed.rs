//! Embed tab — call `/v1/embeddings` on the focused model and
//! show the result's dimensionality + first eight values + L2 norm.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

#[derive(Debug, Default)]
pub struct EmbedTabState {
  pub input: String,
  pub dim: Option<usize>,
  pub preview: Vec<f64>,
  pub norm: Option<f64>,
  pub last_error: Option<String>,
  pub busy: bool,
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
  }

  pub fn record_error(&mut self, msg: String) {
    self.last_error = Some(msg);
    self.busy = false;
  }
}

/// Render the Embed tab body into `area`. Block borders are owned
/// by the right pane caller.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &EmbedTabState, palette: &Palette) {
  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(3),
      Constraint::Min(1),
      Constraint::Length(1),
    ])
    .split(area);

  let prompt_block = Block::default()
    .title(" Input ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.muted));
  let prompt_inner = prompt_block.inner(layout[0]);
  frame.render_widget(prompt_block, layout[0]);
  frame.render_widget(
    Paragraph::new(Line::from(vec![
      Span::styled("▌ ", Style::default().fg(palette.accent)),
      Span::styled(&state.input, Style::default().fg(palette.fg)),
      Span::styled(
        "│",
        Style::default()
          .fg(palette.accent)
          .add_modifier(Modifier::REVERSED),
      ),
    ]))
    .wrap(Wrap { trim: false }),
    prompt_inner,
  );

  let mut body: Vec<Line<'_>> = Vec::new();
  if let Some(dim) = state.dim {
    body.push(Line::from(Span::styled(
      format!("dim = {dim}"),
      Style::default().fg(palette.fg),
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
        Style::default().fg(palette.muted),
      )));
    }
    if let Some(n) = state.norm {
      body.push(Line::from(Span::styled(
        format!("L2 norm = {n:.4}"),
        Style::default().fg(palette.muted),
      )));
    }
  } else {
    body.push(Line::from(Span::styled(
      "Press Enter to embed the input above.",
      Style::default().fg(palette.muted),
    )));
  }
  frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), layout[1]);

  let status = match (state.busy, &state.last_error) {
    (true, _) => Line::from(Span::styled(
      "calling /v1/embeddings…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err)) => Line::from(Span::styled(
      format!("error: {err}"),
      Style::default().fg(palette.error),
    )),
    _ => Line::from(Span::styled(
      "Enter to embed · Esc to clear",
      Style::default().fg(palette.muted),
    )),
  };
  frame.render_widget(Paragraph::new(status), layout[2]);
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
