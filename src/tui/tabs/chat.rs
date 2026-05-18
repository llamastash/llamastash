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

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::oai_client::{collapse_think_blocks, ChatStreamMsg};
use crate::tui::tabs::input_pane::{self, InputPaneOpts, PromptField};

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
  /// Top-of-viewport offset into the rendered response. 0 pins
  /// to the top; ↑/↓ in `Focus::RightPane` walk this (round-8).
  pub scroll_offset: u16,
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
    self.scroll_offset = 0;
  }

  /// Scroll the output viewport up by one line. Saturating add so
  /// repeated presses past the top of the response don't overflow.
  pub fn scroll_up(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_add(1);
  }

  /// Scroll the output viewport down by one line, clamping at 0.
  pub fn scroll_down(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_sub(1);
  }
}

/// Render the Chat tab body into `area`. The caller (right_pane)
/// owns the surrounding Block — this renderer paints content only,
/// no outer wrapper.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let state = &app.chat;
  let active = app.focus == Focus::ChatInput;

  let body_text = if state.collapse_thinks {
    collapse_think_blocks(&state.response)
  } else {
    state.response.clone()
  };
  let body: Vec<Line<'_>> = if body_text.is_empty() {
    vec![Line::from(Span::styled(
      "Send a prompt with Enter. Responses stream here.",
      palette.muted_style(),
    ))]
  } else {
    body_text
      .lines()
      .map(|l| Line::from(Span::styled(l.to_string(), palette.text_style())))
      .collect()
  };

  // The status row carries only live-state messages (streaming /
  // error / finished). Idle key-hint chips moved to the right
  // pane's bottom border — keeps the in-body row from doubling up
  // with the same information.
  let status = match (state.streaming, &state.last_error, &state.finish_reason) {
    (true, _, _) => Line::from(Span::styled(
      "streaming…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err), _) => Line::from(Span::styled(format!("error: {err}"), palette.error_style())),
    (_, _, Some(reason)) => Line::from(Span::styled(
      format!("finished: {reason}"),
      palette.muted_style(),
    )),
    _ => Line::from(""),
  };

  let prompt = PromptField {
    title: "Prompt",
    text: &state.prompt,
    active,
  };
  input_pane::render(
    frame,
    area,
    InputPaneOpts {
      prompts: &[prompt],
      body,
      status,
      bold_body: state.streaming,
      scroll_offset: state.scroll_offset,
    },
    palette,
  );
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
