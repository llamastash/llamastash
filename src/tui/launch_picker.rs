//! Launch picker form state.
//!
//! Three-field form (context length / reasoning / advanced) the
//! Settings tab renders inline in the right pane. Originally lived
//! in a centred modal overlay; round-5 (kdash-style polish) moved
//! the rendering into `tabs::settings` so the form sits side-by-side
//! with the models list. This module is the form's data carrier —
//! cursor (`field`), values (`ctx`, `reasoning`), and metadata
//! (`active_instances`, `prefer_port`).
//!
//! `Enter` on Settings dispatches `start_model` against the daemon;
//! `Esc` from `Focus::RightPane` snaps back to the model list.

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

  /// Cycle backward through ctx presets. Symmetric inverse of
  /// [`Self::cycle_ctx_preset`] so `Up` walks the list opposite to
  /// `Down`. The `None` (native) slot sits at the boundary: pressing
  /// Up on the first preset lands on `None`, then on the last preset.
  pub fn cycle_ctx_preset_prev(&mut self) {
    let next = match self.preset_idx {
      Some(0) => None,
      Some(i) => Some(i - 1),
      None => Some(CTX_PRESETS.len() - 1),
    };
    self.preset_idx = next;
    self.ctx = next.map(|i| CTX_PRESETS[i]);
  }

  pub fn toggle_reasoning(&mut self) {
    self.reasoning = !self.reasoning;
  }

  /// Cycle the focused field's value forward (Down arrow).
  /// - `Ctx` cycles through the preset list.
  /// - `Reasoning` toggles on/off.
  /// - `Advanced` is a no-op here; the dedicated `a` keystroke
  ///   opens the flag editor since "next value" is meaningless for
  ///   free-form text.
  pub fn cycle_focused_value_next(&mut self) {
    match self.field {
      PickerField::Ctx => self.cycle_ctx_preset(),
      PickerField::Reasoning => self.toggle_reasoning(),
      PickerField::Advanced => {}
    }
  }

  /// Cycle the focused field's value backward (Up arrow). Mirrors
  /// [`Self::cycle_focused_value_next`].
  pub fn cycle_focused_value_prev(&mut self) {
    match self.field {
      PickerField::Ctx => self.cycle_ctx_preset_prev(),
      PickerField::Reasoning => self.toggle_reasoning(),
      PickerField::Advanced => {}
    }
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
  fn cycle_ctx_preset_prev_is_inverse_of_cycle_ctx_preset() {
    // Up should walk the preset list in reverse. `None` (native)
    // sits at the boundary so a fresh state with `None` jumps to
    // the last preset on Up, then walks down to the first, then
    // back to `None` on the next Up.
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.ctx, None);
    s.cycle_ctx_preset_prev();
    assert_eq!(s.ctx, Some(*CTX_PRESETS.last().unwrap()));
    for preset in CTX_PRESETS.iter().rev().skip(1) {
      s.cycle_ctx_preset_prev();
      assert_eq!(s.ctx, Some(*preset));
    }
    s.cycle_ctx_preset_prev();
    assert_eq!(s.ctx, None, "wraps back to native after the first preset");
  }

  #[test]
  fn cycle_focused_value_walks_ctx_when_ctx_focused() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.field, PickerField::Ctx);
    s.cycle_focused_value_next();
    assert_eq!(
      s.ctx,
      Some(CTX_PRESETS[0]),
      "Down on Ctx should advance the preset"
    );
    s.cycle_focused_value_prev();
    assert_eq!(s.ctx, None, "Up on Ctx returns to native");
  }

  #[test]
  fn cycle_focused_value_toggles_reasoning_when_reasoning_focused() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Reasoning;
    assert!(!s.reasoning);
    s.cycle_focused_value_next();
    assert!(s.reasoning, "Down on Reasoning toggles on");
    s.cycle_focused_value_prev();
    assert!(!s.reasoning, "Up on Reasoning toggles back off");
  }

  #[test]
  fn cycle_focused_value_is_noop_when_advanced_focused() {
    // Advanced is free-form text edited in a separate panel —
    // "next value" has no meaning here, so Up/Down stay inert and
    // the user opens the editor with `a`.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Advanced;
    let snapshot = (s.ctx, s.reasoning);
    s.cycle_focused_value_next();
    s.cycle_focused_value_prev();
    assert_eq!(
      (s.ctx, s.reasoning),
      snapshot,
      "Advanced field must not bleed into Ctx/Reasoning state"
    );
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
