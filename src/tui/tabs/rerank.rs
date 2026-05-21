//! Rerank tab — call `/v1/rerank` with a query + candidate list
//! and render ranked scores top-to-bottom.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::tabs::input_pane::{InputPaneOpts, PromptField};

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
  /// Modal text-input for the rerank query. Same `e:edit / Esc:stop /
  /// 2nd-Esc:clear` contract as every other text input in the TUI.
  pub query: crate::tui::input_field::InputField,
  pub candidates: Vec<String>,
  pub ranked: Vec<(usize, f64)>,
  pub last_error: Option<String>,
  pub busy: bool,
  /// In-progress candidate text — staged onto `candidates` when
  /// the user presses `Tab` in the candidate sub-field. Same modal
  /// contract as `query`.
  pub candidate_buffer: crate::tui::input_field::InputField,
  pub field: RerankField,
  /// Top-of-viewport offset into the rendered output. Round-8: ↑/↓
  /// in `Focus::RightPane` walk this — same shape as Chat / Embed.
  pub scroll_offset: u16,
  /// Receiver for the in-flight `/v1/rerank` call. The render loop
  /// drains it via `try_recv` once per tick.
  pub pending: Option<tokio::sync::mpsc::UnboundedReceiver<crate::tui::tabs::TabEvent>>,
}

impl RerankTabState {
  pub fn record(&mut self, ranked: Vec<(usize, f64)>) {
    self.ranked = ranked;
    self.last_error = None;
    self.busy = false;
    self.scroll_offset = 0;
  }

  pub fn scroll_up(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_add(1);
  }

  pub fn scroll_down(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_sub(1);
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
    self.query.exit_edit();
    self.candidates.clear();
    self.ranked.clear();
    self.last_error = None;
    self.candidate_buffer.clear();
    self.candidate_buffer.exit_edit();
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
    let trimmed = self.candidate_buffer.buffer().trim().to_string();
    if trimmed.is_empty() {
      return false;
    }
    self.candidates.push(trimmed);
    self.candidate_buffer.clear();
    true
  }
}

/// Render the Rerank tab body into `area`. Block borders are owned
/// by the right pane caller.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let state = &app.rerank;
  let input_active = app.focus == Focus::RerankInput;
  let query_active = input_active && state.field == RerankField::Query;
  let cand_active = input_active && state.field == RerankField::Candidate;

  let mut body: Vec<Line<'_>> = Vec::new();
  if state.ranked.is_empty() {
    body.push(Line::from(Span::styled(
      format!("{} candidate(s) staged.", state.candidates.len()),
      palette.muted_style(),
    )));
    for (i, c) in state.candidates.iter().enumerate() {
      body.push(Line::from(Span::styled(
        format!("  [{i}] {c}"),
        palette.text_style(),
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
        Span::styled(format!("{score:.3}  "), palette.muted_style()),
        Span::styled(text, palette.text_style()),
      ]));
    }
  }

  // Status row carries only the live busy / error state; idle key
  // hint chips moved to the right pane's bottom border.
  let status = match (state.busy, &state.last_error) {
    (true, _) => Line::from(Span::styled(
      "calling /v1/rerank…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err)) => Line::from(Span::styled(format!("error: {err}"), palette.error_style())),
    _ => Line::from(""),
  };

  let prompts = [
    PromptField {
      title: "Query",
      text: state.query.buffer(),
      active: query_active,
    },
    PromptField {
      title: "Candidate",
      text: state.candidate_buffer.buffer(),
      active: cand_active,
    },
  ];
  crate::tui::tabs::input_pane::render(
    frame,
    area,
    InputPaneOpts {
      prompts: &prompts,
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

  #[test]
  fn add_candidate_skips_empty() {
    let mut s = RerankTabState::default();
    s.add_candidate("   ".into());
    s.add_candidate("doc1".into());
    assert_eq!(s.candidates, vec!["doc1".to_string()]);
  }

  #[test]
  fn clear_drops_state() {
    use crate::tui::input_field::InputField;
    let mut s = RerankTabState {
      query: InputField::with_text("q"),
      candidates: vec!["c".into()],
      ranked: vec![(0, 1.0)],
      candidate_buffer: InputField::with_text("buf"),
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
    use crate::tui::input_field::InputField;
    let mut s = RerankTabState {
      candidate_buffer: InputField::with_text("doc one"),
      ..Default::default()
    };
    assert!(s.stage_candidate());
    assert_eq!(s.candidates, vec!["doc one".to_string()]);
    assert!(s.candidate_buffer.is_empty());
  }

  #[test]
  fn stage_candidate_returns_false_when_buffer_empty() {
    use crate::tui::input_field::InputField;
    let mut s = RerankTabState {
      candidate_buffer: InputField::with_text("   "),
      ..Default::default()
    };
    assert!(!s.stage_candidate());
    assert!(s.candidates.is_empty());
  }
}
