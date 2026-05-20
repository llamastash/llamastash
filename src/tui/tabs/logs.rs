//! Logs tab — auto-tails the daemon's per-launch log ring buffer.
//!
//! v1 reads from `logs_tail` on the same refresher tick as the
//! status snapshots; pause/resume hotkeys land alongside Unit 8's
//! `llamastash logs --follow` work. The renderer pulls `lines` off
//! the App so the tab is purely a presentation concern.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;

/// Maximum lines we keep around in the in-memory mirror so a
/// long-running model doesn't grow this buffer unboundedly.
const MAX_LINES: usize = 4096;

/// In-memory mirror of the daemon's per-launch ring buffer. The
/// renderer trims to the visible viewport so a long-running model's
/// log doesn't bloat the render path.
#[derive(Debug, Clone)]
pub struct LogsTabState {
  pub lines: Vec<String>,
  /// Auto-scroll keeps the viewport pinned to the tail; toggled by
  /// the `s` hotkey in the right pane.
  pub auto_scroll: bool,
  /// Launch id this buffer mirrors. Cleared (and `lines` reset)
  /// when the user focuses a different launch.
  pub launch_id: Option<String>,
  /// Lines scrolled back from the tail. `0` keeps the viewport
  /// pinned to the latest log line; positive values pull the view
  /// upward. Bumped by `k` / `↑` in the right pane and reset by `s`
  /// (which also re-enables `auto_scroll`).
  pub scroll_offset: usize,
}

impl Default for LogsTabState {
  fn default() -> Self {
    Self::new()
  }
}

impl LogsTabState {
  pub fn new() -> Self {
    Self {
      lines: Vec::new(),
      auto_scroll: true,
      launch_id: None,
      scroll_offset: 0,
    }
  }

  /// Scroll the viewport up one line. Disables auto-scroll so a
  /// fast-emitting log doesn't yank the viewport back to the tail
  /// the next refresh tick. The render-time clamp keeps the
  /// offset from running past the top of the buffer.
  pub fn scroll_up(&mut self) {
    self.scroll_offset = self
      .scroll_offset
      .saturating_add(1)
      .min(self.lines.len().saturating_sub(1));
    self.auto_scroll = false;
  }

  /// Scroll the viewport down one line; when it hits zero, the
  /// viewport sits on the latest line and auto-scroll resumes.
  pub fn scroll_down(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_sub(1);
    if self.scroll_offset == 0 {
      self.auto_scroll = true;
    }
  }

  /// Adopt a fresh tail from the daemon. When the daemon's tail is a
  /// strict extension of our local buffer (the steady-state shape
  /// while a model is logging), append only the new lines instead of
  /// reallocating up to MAX_LINES `String`s per 500 ms poll. On
  /// launch-id change or any non-extension overlap (rotation, etc.)
  /// fall back to a wholesale replace.
  pub fn set_tail(&mut self, launch_id: String, lines: Vec<String>) {
    if self.launch_id.as_deref() != Some(launch_id.as_str()) {
      self.launch_id = Some(launch_id);
      self.lines = lines;
    } else if let Some(suffix) = lines_extend_tail(&self.lines, &lines) {
      self.lines.extend(suffix.iter().cloned());
    } else {
      self.lines = lines;
    }
    if self.lines.len() > MAX_LINES {
      let drop = self.lines.len() - MAX_LINES;
      self.lines.drain(..drop);
    }
  }

  /// Drop accumulated state when the user moves focus to a launch
  /// the buffer doesn't cover. Keeps the auto-scroll preference.
  pub fn clear(&mut self) {
    self.lines.clear();
    self.launch_id = None;
  }
}

/// If `fresh` ends with everything in `current`'s tail (i.e. it is
/// `current` extended by some new suffix), return the suffix.
/// Otherwise `None`. The check is by-content; the daemon reports
/// the last N lines so a slow consumer + fast producer will
/// occasionally drop the prefix overlap — `None` triggers wholesale
/// replace, which is the safe fallback.
fn lines_extend_tail<'a>(current: &[String], fresh: &'a [String]) -> Option<&'a [String]> {
  if current.is_empty() {
    return Some(fresh);
  }
  if fresh.len() < current.len() {
    return None;
  }
  // Find the overlap: fresh[start..start+current.len()] must equal current,
  // where start = fresh.len() - (current.len() + suffix.len()). The cheap
  // check is: does fresh end with `current` followed by the suffix? That's
  // equivalent to `fresh[fresh.len()-current.len()-suffix.len()..fresh.len()-suffix.len()] == current`.
  // We don't know the suffix length up front, so we search by aligning
  // `current` against fresh's contiguous window starting at the earliest
  // possible position that still leaves room for current entries to align.
  //
  // Practically: try suffix lengths in 0..=fresh.len()-current.len(),
  // smallest first. The smallest match is the "tightest fit" and gives
  // the smallest suffix to append. This is O(n) lines × O(m) compares
  // worst-case; on the steady-state path the suffix length is small
  // (typically 1–5 lines per 500 ms poll) so the first iteration usually
  // wins.
  let max_suffix = fresh.len() - current.len();
  for suffix_len in 0..=max_suffix {
    let start = fresh.len() - current.len() - suffix_len;
    let end = start + current.len();
    if &fresh[start..end] == current {
      return Some(&fresh[end..]);
    }
  }
  None
}

/// Render the Logs tab body into `area`. The caller (right_pane)
/// owns the surrounding Block — this renderer paints content
/// only, so the right pane can host a model header above the logs
/// without an inner block nesting.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &LogsTabState, palette: &Palette) {
  if state.lines.is_empty() {
    let hint = Paragraph::new(Line::from(Span::styled(
      "no log lines yet — launch a model or wait for the daemon to forward stderr",
      palette.muted_style(),
    )))
    .wrap(Wrap { trim: true });
    frame.render_widget(hint, area);
    return;
  }

  let visible = state.lines.len().min(area.height as usize);
  // `scroll_offset` walks the viewport upward — clamp it to a value
  // that still leaves a full screen of lines so paging past the top
  // doesn't render a blank pane.
  let max_offset = state.lines.len().saturating_sub(visible);
  let offset = state.scroll_offset.min(max_offset);
  let end = state.lines.len().saturating_sub(offset);
  let start = end.saturating_sub(visible);
  let body: Vec<Line<'_>> = state
    .lines
    .iter()
    .skip(start)
    .take(visible)
    .map(|l| Line::from(Span::styled(l.as_str(), palette.text_style())))
    .collect();
  let mut p = Paragraph::new(body).wrap(Wrap { trim: false });
  if !state.auto_scroll {
    p = p.style(Style::default().add_modifier(Modifier::DIM));
  }
  frame.render_widget(p, area);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn defaults_to_auto_scroll() {
    let s = LogsTabState::new();
    assert!(s.auto_scroll);
    assert!(s.lines.is_empty());
    assert!(s.launch_id.is_none());
  }

  #[test]
  fn set_tail_overwrites_lines_and_caps_to_max() {
    let mut s = LogsTabState::new();
    let lines: Vec<String> = (0..(MAX_LINES + 50)).map(|i| format!("l{i}")).collect();
    s.set_tail("L1".into(), lines);
    assert_eq!(s.launch_id.as_deref(), Some("L1"));
    assert_eq!(s.lines.len(), MAX_LINES);
    assert_eq!(s.lines[0], format!("l{}", 50));
  }
}
