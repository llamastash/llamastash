//! Rerank tab — call `/v1/rerank` with a query + candidate list
//! and render ranked scores top-to-bottom.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus};
use crate::tui::tabs::input_pane::{self, InputPaneOpts, PromptField};

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
    _ => input_pane::idle_status_line(&idle_status_chips(app, input_active), palette),
  };

  let prompts = [
    PromptField {
      title: "Query",
      text: &state.query,
      active: query_active,
    },
    PromptField {
      title: "Candidate",
      text: &state.candidate_buffer,
      active: cand_active,
    },
  ];
  input_pane::render(
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

/// Chip strip for the idle status line. Round-8 trimmed this to
/// just the stage-candidate chip and the trailing `clear`/`edit`
/// — newline keys live in the `?` help overlay now so the status
/// row stays scannable.
pub(crate) fn idle_status_chips(app: &App, input_active: bool) -> Vec<String> {
  let mut chips: Vec<String> = Vec::with_capacity(2);
  if let Some(c) = app.hint(Focus::RerankInput, Action::StageRerankCandidate) {
    chips.push(c);
  }
  let trailing = if input_active {
    app.hint_with(Focus::RerankInput, Action::ExitEdit, "clear")
  } else {
    app.hint(Focus::RightPane, Action::EnterEdit)
  };
  if let Some(c) = trailing {
    chips.push(c);
  }
  chips
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crate::tui::keybindings::KeyMap;
  use std::collections::BTreeMap;

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

  #[test]
  fn idle_chips_when_input_active_use_keymap_labels() {
    // Round-7: staging migrated from Tab → `+` / `=` (same
    // physical key on US keyboards). The chip picks up the first
    // bound key, which is `+`.
    // Round-8: dropped the `󰘶+Enter:newline` chip from the idle
    // row — only stage + clear/edit survive.
    let app = App::new(AppOptions::default());
    let chips = idle_status_chips(&app, true);
    assert_eq!(
      chips,
      vec!["+:stage candidate".to_string(), "Esc:clear".to_string(),]
    );
  }

  #[test]
  fn idle_chips_drop_newline_chip_in_round_8() {
    let app = App::new(AppOptions::default());
    let chips = idle_status_chips(&app, true);
    for stale in ["newline", "Shift+", "󰘶+Enter:newline"] {
      assert!(
        !chips.iter().any(|c| c.contains(stale)),
        "stale chip text `{stale}` resurfaced: {chips:?}"
      );
    }
  }

  #[test]
  fn idle_chips_when_input_inactive_swap_clear_for_edit() {
    let app = App::new(AppOptions::default());
    let chips = idle_status_chips(&app, false);
    assert_eq!(chips.last().map(String::as_str), Some("e:edit"));
  }

  #[test]
  fn idle_chips_pick_up_config_keybinding_overrides() {
    // Rebind stage_rerank_candidate to F2 — the chip label flips.
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> =
      [(String::from("stage_rerank_candidate"), String::from("f2"))]
        .into_iter()
        .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let chips = idle_status_chips(&app, true);
    assert!(
      chips.iter().any(|c| c == "F2:stage candidate"),
      "F2 binding missing: {chips:?}"
    );
  }
}
