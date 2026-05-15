//! Rerank tab — call `/v1/rerank` with a query + candidate list
//! and render ranked scores top-to-bottom.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

#[derive(Debug, Clone, Default)]
pub struct RerankTabState {
  pub query: String,
  pub candidates: Vec<String>,
  pub ranked: Vec<(usize, f64)>,
  pub last_error: Option<String>,
  pub busy: bool,
}

impl RerankTabState {
  pub fn record(&mut self, ranked: Vec<(usize, f64)>) {
    self.ranked = ranked;
    self.last_error = None;
    self.busy = false;
  }

  pub fn record_error(&mut self, msg: String) {
    self.last_error = Some(msg);
    self.busy = false;
  }

  pub fn add_candidate(&mut self, s: String) {
    if !s.trim().is_empty() {
      self.candidates.push(s);
    }
  }

  pub fn clear(&mut self) {
    self.query.clear();
    self.candidates.clear();
    self.ranked.clear();
    self.last_error = None;
  }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &RerankTabState, palette: &Palette) {
  let block = Block::default()
    .title(" Rerank ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(3),
      Constraint::Min(1),
      Constraint::Length(1),
    ])
    .split(inner);

  let prompt_block = Block::default()
    .title(" Query ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.muted));
  let prompt_inner = prompt_block.inner(layout[0]);
  frame.render_widget(prompt_block, layout[0]);
  frame.render_widget(
    Paragraph::new(Line::from(vec![
      Span::styled("▌ ", Style::default().fg(palette.accent)),
      Span::styled(&state.query, Style::default().fg(palette.fg)),
    ]))
    .wrap(Wrap { trim: false }),
    prompt_inner,
  );

  let mut body: Vec<Line<'_>> = Vec::new();
  if state.ranked.is_empty() {
    body.push(Line::from(Span::styled(
      format!(
        "{} candidate(s) staged. Press Enter to rank.",
        state.candidates.len()
      ),
      Style::default().fg(palette.muted),
    )));
    for (i, c) in state.candidates.iter().enumerate() {
      body.push(Line::from(Span::styled(
        format!("  [{i}] {c}"),
        Style::default().fg(palette.fg),
      )));
    }
  } else {
    for (rank, (idx, score)) in state.ranked.iter().enumerate() {
      let text = state.candidates.get(*idx).cloned().unwrap_or_default();
      body.push(Line::from(vec![
        Span::styled(
          format!("#{} ", rank + 1),
          Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{score:.3}  "), Style::default().fg(palette.muted)),
        Span::styled(text, Style::default().fg(palette.fg)),
      ]));
    }
  }
  frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), layout[1]);

  let status = match (state.busy, &state.last_error) {
    (true, _) => Line::from(Span::styled(
      "calling /v1/rerank…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err)) => Line::from(Span::styled(
      format!("error: {err}"),
      Style::default().fg(palette.error),
    )),
    _ => Line::from(Span::styled(
      "Enter to rank · type candidate text then Tab to stage",
      Style::default().fg(palette.muted),
    )),
  };
  frame.render_widget(Paragraph::new(status), layout[2]);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn add_candidate_skips_empty() {
    let mut s = RerankTabState::default();
    s.add_candidate("   ".into());
    s.add_candidate("doc1".into());
    assert_eq!(s.candidates, vec!["doc1".to_string()]);
  }

  #[test]
  fn clear_drops_state() {
    let mut s = RerankTabState {
      query: "q".into(),
      candidates: vec!["c".into()],
      ranked: vec![(0, 1.0)],
      ..Default::default()
    };
    s.clear();
    assert!(s.query.is_empty());
    assert!(s.candidates.is_empty());
    assert!(s.ranked.is_empty());
  }
}
