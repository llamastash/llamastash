//! Rerank tab — call `/v1/rerank` with a query + candidate list
//! and render ranked scores top-to-bottom.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

/// Which sub-field of the Rerank tab the user is typing into.
/// `Tab` cycles between the query and the candidate buffer; the
/// staged candidates render below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RerankField {
  #[default]
  Query,
  Candidate,
}

#[derive(Debug, Default)]
pub struct RerankTabState {
  pub query: String,
  pub candidates: Vec<String>,
  pub ranked: Vec<(usize, f64)>,
  pub last_error: Option<String>,
  pub busy: bool,
  /// In-progress candidate text — staged onto `candidates` when
  /// the user presses `Tab` in the candidate sub-field.
  pub candidate_buffer: String,
  pub field: RerankField,
  /// Receiver for the in-flight `/v1/rerank` call. The render loop
  /// drains it via `try_recv` once per tick.
  pub pending: Option<tokio::sync::mpsc::UnboundedReceiver<crate::tui::tabs::TabEvent>>,
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
    self.candidate_buffer.clear();
    self.field = RerankField::Query;
  }

  /// Move the type-cursor between the query and candidate sub-
  /// fields. The candidate buffer is preserved across cycles.
  pub fn cycle_field(&mut self) {
    self.field = match self.field {
      RerankField::Query => RerankField::Candidate,
      RerankField::Candidate => RerankField::Query,
    };
  }

  /// Stage the in-progress candidate buffer onto the candidate list.
  /// Returns true if a candidate was added.
  pub fn stage_candidate(&mut self) -> bool {
    let trimmed = self.candidate_buffer.trim().to_string();
    if trimmed.is_empty() {
      return false;
    }
    self.candidates.push(trimmed);
    self.candidate_buffer.clear();
    true
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
      Constraint::Length(3),
      Constraint::Min(1),
      Constraint::Length(1),
    ])
    .split(inner);

  // Query field — caret visible only when this is the active
  // sub-field, so the user has an unambiguous typing target.
  let query_active = state.field == RerankField::Query;
  let query_block = Block::default()
    .title(" Query ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(if query_active {
      palette.accent
    } else {
      palette.muted
    }));
  let query_inner = query_block.inner(layout[0]);
  frame.render_widget(query_block, layout[0]);
  let mut query_spans = vec![
    Span::styled("▌ ", Style::default().fg(palette.accent)),
    Span::styled(&state.query, Style::default().fg(palette.fg)),
  ];
  if query_active {
    query_spans.push(Span::styled(
      "│",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::REVERSED),
    ));
  }
  frame.render_widget(
    Paragraph::new(Line::from(query_spans)).wrap(Wrap { trim: false }),
    query_inner,
  );

  // Candidate buffer field — accepts text input when active, and
  // shows the staged list (with the size hint) below.
  let cand_active = state.field == RerankField::Candidate;
  let cand_block = Block::default()
    .title(" Candidate (Tab stages) ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(if cand_active {
      palette.accent
    } else {
      palette.muted
    }));
  let cand_inner = cand_block.inner(layout[1]);
  frame.render_widget(cand_block, layout[1]);
  let mut cand_spans = vec![
    Span::styled("▌ ", Style::default().fg(palette.accent)),
    Span::styled(&state.candidate_buffer, Style::default().fg(palette.fg)),
  ];
  if cand_active {
    cand_spans.push(Span::styled(
      "│",
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::REVERSED),
    ));
  }
  frame.render_widget(
    Paragraph::new(Line::from(cand_spans)).wrap(Wrap { trim: false }),
    cand_inner,
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
  frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), layout[2]);

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
      "Tab cycles field · Tab stages candidate · Enter ranks",
      Style::default().fg(palette.muted),
    )),
  };
  frame.render_widget(Paragraph::new(status), layout[3]);
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
      candidate_buffer: "buf".into(),
      field: RerankField::Candidate,
      ..Default::default()
    };
    s.clear();
    assert!(s.query.is_empty());
    assert!(s.candidates.is_empty());
    assert!(s.ranked.is_empty());
    assert!(s.candidate_buffer.is_empty());
    assert_eq!(s.field, RerankField::Query);
  }

  #[test]
  fn cycle_field_swaps_query_and_candidate() {
    let mut s = RerankTabState::default();
    assert_eq!(s.field, RerankField::Query);
    s.cycle_field();
    assert_eq!(s.field, RerankField::Candidate);
    s.cycle_field();
    assert_eq!(s.field, RerankField::Query);
  }

  #[test]
  fn stage_candidate_moves_buffer_to_candidates_when_non_empty() {
    let mut s = RerankTabState {
      candidate_buffer: "doc one".into(),
      ..Default::default()
    };
    assert!(s.stage_candidate());
    assert_eq!(s.candidates, vec!["doc one".to_string()]);
    assert!(s.candidate_buffer.is_empty());
  }

  #[test]
  fn stage_candidate_returns_false_when_buffer_empty() {
    let mut s = RerankTabState {
      candidate_buffer: "   ".into(),
      ..Default::default()
    };
    assert!(!s.stage_candidate());
    assert!(s.candidates.is_empty());
  }
}
