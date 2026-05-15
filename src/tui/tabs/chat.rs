//! Chat tab — single-shot smoke-test prompt against the focused
//! model's `/v1/chat/completions` endpoint.
//!
//! v1 keeps the surface narrow:
//! - one prompt buffer the user types into;
//! - one output viewport the streamer appends to;
//! - no conversation history (the plan calls v1 a single-shot
//!   smoke test).
//!
//! When the model reports `reasoning` is on, `<think>...</think>`
//! blocks collapse to a `⏵ reasoning (N tokens)` glyph in the
//! viewport so the user can still see the final answer without
//! scrolling past chain-of-thought spam (R32).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::theme::Palette;
use crate::tui::oai_client::{collapse_think_blocks, ChatStreamMsg};

/// Working state for the chat tab. Owned by [`crate::tui::app::App`]
/// so the streamer and the renderer share one buffer.
#[derive(Debug, Default)]
pub struct ChatTabState {
  /// The user's current prompt input.
  pub prompt: String,
  /// Accumulated response from the most recent send.
  pub response: String,
  /// Whether a stream is currently in flight.
  pub streaming: bool,
  /// Last `finish_reason` reported by the server, if any.
  pub finish_reason: Option<String>,
  /// Last error message — empty when the previous send succeeded.
  pub last_error: Option<String>,
  /// Collapse `<think>` blocks. Drives the same toggle the plan
  /// calls out for reasoning-aware models.
  pub collapse_thinks: bool,
  /// Receiver for the most recent `spawn_chat_stream` invocation.
  /// The render loop drains it via `try_recv` on every tick — that
  /// way SSE deltas land in [`response`] without the input thread
  /// having to await anything. `None` once the stream signals
  /// `Finished` or `Error`.
  pub stream_rx: Option<mpsc::Receiver<ChatStreamMsg>>,
}

impl ChatTabState {
  pub fn append_delta(&mut self, s: &str) {
    self.response.push_str(s);
  }

  pub fn mark_finished(&mut self, reason: Option<String>) {
    self.streaming = false;
    self.finish_reason = reason;
  }

  pub fn mark_error(&mut self, msg: String) {
    self.streaming = false;
    self.last_error = Some(msg);
  }

  pub fn reset_for_send(&mut self) {
    self.response.clear();
    self.last_error = None;
    self.finish_reason = None;
    self.streaming = true;
  }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &ChatTabState, palette: &Palette) {
  let block = Block::default()
    .title(" Chat ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Min(1),
      Constraint::Length(3),
      Constraint::Length(1),
    ])
    .split(inner);

  let body_text = if state.collapse_thinks {
    collapse_think_blocks(&state.response)
  } else {
    state.response.clone()
  };
  let body_lines: Vec<Line<'_>> = if body_text.is_empty() {
    vec![Line::from(Span::styled(
      "Send a prompt with Ctrl+Enter. Responses stream here.",
      Style::default().fg(palette.muted),
    ))]
  } else {
    body_text
      .lines()
      .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(palette.fg))))
      .collect()
  };
  let mut viewport = Paragraph::new(body_lines).wrap(Wrap { trim: false });
  if state.streaming {
    viewport = viewport.style(Style::default().add_modifier(Modifier::BOLD));
  }
  frame.render_widget(viewport, layout[0]);

  let prompt_block = Block::default()
    .title(" Prompt ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.muted));
  let prompt_inner = prompt_block.inner(layout[1]);
  frame.render_widget(prompt_block, layout[1]);
  frame.render_widget(
    Paragraph::new(Line::from(vec![
      Span::styled("▌ ", Style::default().fg(palette.accent)),
      Span::styled(&state.prompt, Style::default().fg(palette.fg)),
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

  let status = match (state.streaming, &state.last_error, &state.finish_reason) {
    (true, _, _) => Line::from(Span::styled(
      "streaming…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err), _) => Line::from(Span::styled(
      format!("error: {err}"),
      Style::default().fg(palette.error),
    )),
    (_, _, Some(reason)) => Line::from(Span::styled(
      format!("finished: {reason}"),
      Style::default().fg(palette.muted),
    )),
    _ => Line::from(Span::styled(
      "ready — Ctrl+Enter to send, r toggles reasoning collapse",
      Style::default().fg(palette.muted),
    )),
  };
  frame.render_widget(Paragraph::new(status), layout[2]);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn append_delta_concatenates() {
    let mut s = ChatTabState::default();
    s.append_delta("he");
    s.append_delta("llo");
    assert_eq!(s.response, "hello");
  }

  #[test]
  fn reset_clears_response_and_marks_streaming() {
    let mut s = ChatTabState {
      response: "stale".into(),
      last_error: Some("nope".into()),
      ..Default::default()
    };
    s.reset_for_send();
    assert!(s.response.is_empty());
    assert!(s.last_error.is_none());
    assert!(s.streaming);
  }

  #[test]
  fn collapse_think_off_passes_through() {
    let s = ChatTabState {
      response: "hi <think>plan</think> done".into(),
      ..Default::default()
    };
    assert!(!s.collapse_thinks);
  }
}
