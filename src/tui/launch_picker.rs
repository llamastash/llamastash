//! Launch picker overlay.
//!
//! Modal-style three-control overlay: context length, reasoning
//! toggle, and an "Advanced…" entry that opens the free-form flag
//! editor (see [`super::advanced_panel`]). `Enter` dispatches
//! `start_model` against the daemon; `Esc` cancels.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;

/// Pre-canned context-length presets surfaced as quick picks.
/// Plan reference R12. Custom values flow through the same field
/// when the user types digits.
pub const CTX_PRESETS: &[u32] = &[2048, 4096, 8192, 16384, 32768, 65536, 131072];

/// State of the launch picker. Cheap to clone — the App owns one
/// and rebuilds it whenever the focus opens onto a new model.
#[derive(Debug, Clone)]
pub struct LaunchPickerState {
  /// Display name of the focused model (rendered in the title).
  pub model_name: String,
  /// Selected ctx length. `None` lets the supervisor honour the
  /// GGUF's native `context_length` (no `-c` flag).
  pub ctx: Option<u32>,
  /// Reasoning bundle on/off.
  pub reasoning: bool,
  /// Index into CTX_PRESETS for cycling via Tab. `None` means
  /// custom (free-form input or `native`).
  pub preset_idx: Option<usize>,
  /// Currently focused field (cycles via Tab).
  pub field: PickerField,
  /// Count of active `ManagedRow`s for the focused model. v1 does
  /// not block duplicate launches — submitting just spins up a new
  /// instance on a fresh port — but the picker surfaces a heads-up
  /// so the user isn't surprised.
  pub active_instances: usize,
  /// Soft port preference seeded from the daemon's `last_params`
  /// snapshot. Submitted as `prefer_port` so the daemon honours it
  /// when free and falls back to range allocation otherwise.
  pub prefer_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerField {
  Ctx,
  Reasoning,
  Advanced,
}

impl LaunchPickerState {
  pub fn for_model(model_name: impl Into<String>) -> Self {
    Self {
      model_name: model_name.into(),
      ctx: None,
      reasoning: false,
      preset_idx: None,
      field: PickerField::Ctx,
      active_instances: 0,
      prefer_port: None,
    }
  }

  /// Cycle to the next ctx preset, wrapping around. Pressing the
  /// cycle key with `ctx = None` jumps to the first preset.
  pub fn cycle_ctx_preset(&mut self) {
    let next = match self.preset_idx {
      Some(i) if i + 1 < CTX_PRESETS.len() => Some(i + 1),
      Some(_) => None,
      None => Some(0),
    };
    self.preset_idx = next;
    self.ctx = next.map(|i| CTX_PRESETS[i]);
  }

  pub fn toggle_reasoning(&mut self) {
    self.reasoning = !self.reasoning;
  }

  pub fn next_field(&mut self) {
    self.field = match self.field {
      PickerField::Ctx => PickerField::Reasoning,
      PickerField::Reasoning => PickerField::Advanced,
      PickerField::Advanced => PickerField::Ctx,
    };
  }

  /// Cycle backward through the field set. Symmetric inverse of
  /// [`Self::next_field`] so `Shift+Tab` walks the form in the
  /// opposite direction.
  pub fn prev_field(&mut self) {
    self.field = match self.field {
      PickerField::Ctx => PickerField::Advanced,
      PickerField::Reasoning => PickerField::Ctx,
      PickerField::Advanced => PickerField::Reasoning,
    };
  }
}

/// Render the picker centred over `area`. Clears the underlying
/// region first so the picker reads as a true modal even though
/// the caller draws everything to one frame.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &LaunchPickerState, palette: &Palette) {
  let modal = centered_rect(60, 30, area);
  frame.render_widget(Clear, modal);

  let block = Block::default()
    .title(format!(" Launch · {} ", state.model_name))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  frame.render_widget(block.clone(), modal);
  let inner = block.inner(modal);

  let warning_lines = if state.active_instances > 0 { 1 } else { 0 };
  let rows = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(2),
      Constraint::Length(2),
      Constraint::Length(2),
      Constraint::Length(warning_lines as u16),
      Constraint::Min(0),
    ])
    .split(inner);

  let ctx_text = match state.ctx {
    Some(n) => format!("{n} tokens"),
    None => "native (GGUF default)".into(),
  };
  frame.render_widget(
    field_line(
      "Context",
      &ctx_text,
      state.field == PickerField::Ctx,
      palette,
    ),
    rows[0],
  );
  let reasoning_text = if state.reasoning { "on" } else { "off" };
  frame.render_widget(
    field_line(
      "Reasoning",
      reasoning_text,
      state.field == PickerField::Reasoning,
      palette,
    ),
    rows[1],
  );
  frame.render_widget(
    field_line(
      "Advanced",
      "open editor (a)",
      state.field == PickerField::Advanced,
      palette,
    ),
    rows[2],
  );
  if warning_lines > 0 {
    let warn = format!(
      "⚠ {} instance(s) already running — submit launches a new one on a fresh port",
      state.active_instances
    );
    frame.render_widget(
      Paragraph::new(Line::from(Span::styled(
        warn,
        Style::default()
          .fg(palette.warning)
          .add_modifier(Modifier::BOLD),
      ))),
      rows[3],
    );
  }
}

fn field_line<'a>(
  label: &'a str,
  value: &'a str,
  focused: bool,
  palette: &Palette,
) -> Paragraph<'a> {
  let label_style = if focused {
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD)
  } else {
    Style::default().fg(palette.muted)
  };
  let value_style = Style::default().fg(palette.fg);
  Paragraph::new(Line::from(vec![
    Span::raw(if focused { "▌ " } else { "  " }),
    Span::styled(format!("{label:<10}"), label_style),
    Span::styled(value.to_string(), value_style),
  ]))
}

/// Compute a centred rect with `pct_x` × `pct_y` of `area`.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
  let v = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Percentage((100 - pct_y) / 2),
      Constraint::Percentage(pct_y),
      Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
  Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
      Constraint::Percentage((100 - pct_x) / 2),
      Constraint::Percentage(pct_x),
      Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cycle_ctx_walks_through_presets_then_returns_to_native() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.ctx, None);
    s.cycle_ctx_preset();
    assert_eq!(s.ctx, Some(CTX_PRESETS[0]));
    for preset in CTX_PRESETS.iter().skip(1) {
      s.cycle_ctx_preset();
      assert_eq!(s.ctx, Some(*preset));
    }
    s.cycle_ctx_preset();
    assert_eq!(s.ctx, None, "wraps back to native");
  }

  #[test]
  fn toggle_reasoning_round_trips() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(!s.reasoning);
    s.toggle_reasoning();
    assert!(s.reasoning);
    s.toggle_reasoning();
    assert!(!s.reasoning);
  }

  #[test]
  fn next_field_cycles_three_fields() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.field, PickerField::Ctx);
    s.next_field();
    assert_eq!(s.field, PickerField::Reasoning);
    s.next_field();
    assert_eq!(s.field, PickerField::Advanced);
    s.next_field();
    assert_eq!(s.field, PickerField::Ctx);
  }

  #[test]
  fn prev_field_is_inverse_of_next_field() {
    // Shift+Tab walks the form in reverse — Ctx → Advanced →
    // Reasoning → Ctx — so three calls land back on the start. This
    // is what makes the picker form feel reversible.
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.field, PickerField::Ctx);
    s.prev_field();
    assert_eq!(s.field, PickerField::Advanced);
    s.prev_field();
    assert_eq!(s.field, PickerField::Reasoning);
    s.prev_field();
    assert_eq!(s.field, PickerField::Ctx);
  }
}
