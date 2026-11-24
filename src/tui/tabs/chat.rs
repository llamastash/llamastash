//! Chat tab — single-shot smoke-test prompt against the focused
//! model's `/v1/chat/completions` endpoint.
//!
//! The surface stays narrow on purpose: one prompt buffer, one output
//! viewport the streamer appends to, no conversation history.
//!
//! When the model reports `reasoning` is on, `<think>...</think>`
//! blocks collapse to a `⏵ reasoning (N tokens)` glyph in the
//! viewport so the user can still see the final answer without
//! scrolling past chain-of-thought spam.

use std::cell::Cell;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::tabs::input_pane::{self, InputPaneOpts, PromptField};

/// Working state for the chat tab. Owned by [`crate::tui::app::App`]
/// so the streamer and the renderer share one buffer.
#[derive(Debug, Default)]
pub struct ChatTabState {
  /// Modal input, so the `e:edit / Esc:stop / 2nd-Esc:clear`
  /// walk-back contract matches every other text input in the TUI.
  pub prompt: crate::tui::input_field::InputField,
  /// Accumulated response from the most recent send.
  pub response: String,
  /// Whether a stream is currently in flight.
  pub streaming: bool,
  /// Last `finish_reason` reported by the server, if any.
  pub finish_reason: Option<String>,
  /// Last error message — empty when the previous send succeeded.
  pub last_error: Option<String>,
  /// Collapse `<think>` blocks in the rendered output.
  pub collapse_thinks: bool,
  /// Top-of-viewport line index into the rendered response; 0 pins
  /// to the top. A cell so the renderer can clamp it to the wrapped
  /// content height and write the clamp back (see
  /// `input_pane::InputPaneOpts::scroll_offset`).
  pub scroll_offset: Cell<u16>,
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
    self.scroll_offset.set(0);
  }

  /// Scroll one line toward the top of the response; clamps at 0.
  pub fn scroll_up(&mut self) {
    self
      .scroll_offset
      .set(self.scroll_offset.get().saturating_sub(1));
  }

  /// Scroll one line toward the end of the response; the render
  /// clamps to the wrapped content height.
  pub fn scroll_down(&mut self) {
    self
      .scroll_offset
      .set(self.scroll_offset.get().saturating_add(1));
  }
}

/// Render the Chat tab body into `area`. The caller (right_pane)
/// owns the surrounding Block — this renderer paints content only,
/// no outer wrapper.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let state = &app.chat;
  let active = app.focus == Focus::ChatInput;

  let body: Vec<Line<'_>> = if state.response.is_empty() {
    vec![Line::from(Span::styled(
      "Send a prompt with Enter. Responses stream here.",
      palette.muted_style(),
    ))]
  } else {
    render_response_lines(&state.response, state.collapse_thinks, palette)
  };

  // The status row carries only live-state messages (streaming /
  // error / finished); idle key-hint chips live on the right pane's
  // bottom border.
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
    text: state.prompt.buffer(),
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
      scroll_offset: &state.scroll_offset,
    },
    palette,
  );
}

/// Walk `text` and emit ratatui [`Line`]s with `<think>...</think>`
/// content styled muted so the reasoning trace reads as secondary
/// text. A blank line is inserted after each `</think>` so the
/// answer that follows always starts on a fresh row — without it,
/// streamed models that emit the close marker immediately followed
/// by content render as one run-on block.
///
/// When `collapse` is `true`, each terminated `<think>` block is
/// replaced by a single `⏵ reasoning (N tokens)` badge (also muted).
/// Unterminated blocks (still streaming) pass through as raw text in
/// muted style so the user sees the live trace instead of stuck-at
/// "(0 tokens)".
fn render_response_lines(text: &str, collapse: bool, palette: &Palette) -> Vec<Line<'static>> {
  let normal = palette.text_style();
  let muted = palette.muted_style();
  let mut lines: Vec<Line<'static>> = Vec::new();
  let mut current: Vec<Span<'static>> = Vec::new();
  let mut rest = text;
  let mut in_think = false;
  loop {
    let needle = if in_think { "</think>" } else { "<think>" };
    match rest.find(needle) {
      Some(idx) => {
        let segment = &rest[..idx];
        if in_think && collapse {
          let toks = segment.split_whitespace().count();
          current.push(Span::styled(format!("⏵ reasoning ({toks} tokens)"), muted));
        } else {
          let style = if in_think { muted } else { normal };
          push_segment(&mut current, &mut lines, segment, style);
        }
        rest = &rest[idx + needle.len()..];
        if in_think {
          // Flush the reasoning span, then emit the blank separator
          // line before the answer.
          if !current.is_empty() {
            lines.push(Line::from(std::mem::take(&mut current)));
          }
          lines.push(Line::default());
        }
        in_think = !in_think;
      }
      None => {
        // No more tags. The remaining text is either tail content
        // (in_think = false) or an unterminated reasoning trace
        // (in_think = true, still streaming).
        let style = if in_think { muted } else { normal };
        push_segment(&mut current, &mut lines, rest, style);
        break;
      }
    }
  }
  if !current.is_empty() {
    lines.push(Line::from(current));
  }
  lines
}

/// Push `text` into the in-progress line buffer, splitting on `\n` so
/// each terminal line lands as its own [`Line`]. Empty leading or
/// trailing fragments are skipped so a segment that starts or ends
/// with a newline doesn't emit a redundant blank span.
fn push_segment(
  current: &mut Vec<Span<'static>>,
  lines: &mut Vec<Line<'static>>,
  text: &str,
  style: Style,
) {
  let mut iter = text.split('\n');
  if let Some(first) = iter.next() {
    if !first.is_empty() {
      current.push(Span::styled(first.to_string(), style));
    }
  }
  for chunk in iter {
    lines.push(Line::from(std::mem::take(current)));
    if !chunk.is_empty() {
      current.push(Span::styled(chunk.to_string(), style));
    }
  }
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

  /// Helper that drops styling and converts the line layout to a
  /// `Vec<String>` so the assertion focuses on structure (line breaks
  /// + blank-line separator) without coupling to ratatui's `Style`.
  fn render_to_strings(text: &str, collapse: bool) -> Vec<String> {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    render_response_lines(text, collapse, palette)
      .into_iter()
      .map(|l| {
        l.spans
          .iter()
          .map(|s| s.content.as_ref())
          .collect::<String>()
      })
      .collect()
  }

  #[test]
  fn expanded_think_block_emits_blank_line_separator_before_answer() {
    let lines = render_to_strings("<think>step one\nstep two</think>final answer", false);
    assert_eq!(
      lines,
      vec![
        "step one".to_string(),
        "step two".to_string(),
        // Blank line — the visual gap between the reasoning trace
        // and the answer the user actually cares about.
        String::new(),
        "final answer".to_string(),
      ]
    );
  }

  #[test]
  fn expanded_think_content_uses_muted_style_and_answer_uses_text_style() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    let lines = render_response_lines("<think>reason</think>answer", false, palette);
    // 3 lines: reasoning, blank separator, answer.
    assert_eq!(lines.len(), 3);
    // First line span styled muted.
    let first = lines[0].spans.first().expect("reason span");
    assert_eq!(first.content, "reason");
    assert_eq!(first.style, palette.muted_style());
    // Third line styled with the normal text style.
    let third = lines[2].spans.first().expect("answer span");
    assert_eq!(third.content, "answer");
    assert_eq!(third.style, palette.text_style());
  }

  #[test]
  fn collapsed_think_block_renders_muted_reasoning_badge() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    let lines = render_response_lines("<think>one two three</think>answer", true, palette);
    assert_eq!(lines.len(), 3);
    let badge = lines[0].spans.first().expect("badge span");
    assert_eq!(badge.content, "⏵ reasoning (3 tokens)");
    assert_eq!(badge.style, palette.muted_style());
    assert!(lines[1].spans.is_empty(), "blank separator line");
    assert_eq!(lines[2].spans.first().expect("answer").content, "answer");
  }

  #[test]
  fn unterminated_think_block_passes_through_as_muted_stream() {
    // Mid-stream — `</think>` hasn't arrived yet. The trace should
    // still render (muted) so the user sees live reasoning instead
    // of an empty pane.
    let lines = render_to_strings("<think>still thinking", false);
    assert_eq!(lines, vec!["still thinking".to_string()]);
  }

  #[test]
  fn response_without_think_blocks_renders_each_line_in_text_style() {
    let lines = render_to_strings("hello\nworld", false);
    assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
  }
}
