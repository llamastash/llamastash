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

/// Tri-state reasoning selector (round-8). `ModelDefault` means
/// "don't send the reasoning flag at all" — the daemon falls back
/// to whatever the model's metadata implies. `On` / `Off` are
/// explicit user choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningSetting {
  /// Don't pass `--reasoning` (or its disable counterpart). The
  /// daemon decides based on the GGUF's `reasoning_hint`. Round-8
  /// default for a fresh picker so a user who never touches the
  /// field doesn't silently override the model's expectation.
  #[default]
  ModelDefault,
  On,
  Off,
}

impl ReasoningSetting {
  /// Human-readable label rendered in the Settings tab.
  pub fn label(self) -> &'static str {
    match self {
      ReasoningSetting::ModelDefault => "model default",
      ReasoningSetting::On => "on",
      ReasoningSetting::Off => "off",
    }
  }

  /// Wire-side encoding. `None` means "omit the field" (model
  /// default); the daemon then falls back to its own logic.
  pub fn as_wire(self) -> Option<bool> {
    match self {
      ReasoningSetting::ModelDefault => None,
      ReasoningSetting::On => Some(true),
      ReasoningSetting::Off => Some(false),
    }
  }

  /// Cycle forward: ModelDefault → On → Off → wrap.
  pub fn next(self) -> Self {
    match self {
      ReasoningSetting::ModelDefault => ReasoningSetting::On,
      ReasoningSetting::On => ReasoningSetting::Off,
      ReasoningSetting::Off => ReasoningSetting::ModelDefault,
    }
  }

  /// Cycle backward — symmetric inverse of [`Self::next`].
  pub fn prev(self) -> Self {
    match self {
      ReasoningSetting::ModelDefault => ReasoningSetting::Off,
      ReasoningSetting::On => ReasoningSetting::ModelDefault,
      ReasoningSetting::Off => ReasoningSetting::On,
    }
  }

  /// Seed from the daemon's persisted `last_params.reasoning: bool`.
  /// Used by `open_launch_picker` so a returning user lands back
  /// on the exact value they shipped previously.
  pub fn from_persisted(prev: bool) -> Self {
    if prev {
      ReasoningSetting::On
    } else {
      ReasoningSetting::Off
    }
  }
}

/// State of the launch picker. Cheap to clone — the App owns one
/// and rebuilds it whenever the focus opens onto a new model.
#[derive(Debug, Clone)]
pub struct LaunchPickerState {
  /// Display name of the focused model (rendered in the title).
  pub model_name: String,
  /// Selected ctx length. `None` lets the supervisor honour the
  /// GGUF's native `context_length` (no `-c` flag).
  pub ctx: Option<u32>,
  /// Reasoning bundle: model-default / on / off (round-8 tri-state).
  pub reasoning: ReasoningSetting,
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
      reasoning: ReasoningSetting::default(),
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

  /// Advance the reasoning tri-state: ModelDefault → On → Off → wrap.
  pub fn cycle_reasoning_next(&mut self) {
    self.reasoning = self.reasoning.next();
  }

  /// Walk the reasoning tri-state backwards.
  pub fn cycle_reasoning_prev(&mut self) {
    self.reasoning = self.reasoning.prev();
  }

  /// Cycle the focused field's value forward (Down arrow).
  /// - `Ctx` cycles through the preset list.
  /// - `Reasoning` walks the tri-state.
  /// - `Advanced` is a no-op here; the dedicated `a` keystroke
  ///   opens the flag editor since "next value" is meaningless for
  ///   free-form text.
  pub fn cycle_focused_value_next(&mut self) {
    match self.field {
      PickerField::Ctx => self.cycle_ctx_preset(),
      PickerField::Reasoning => self.cycle_reasoning_next(),
      PickerField::Advanced => {}
    }
  }

  /// Cycle the focused field's value backward (Up arrow). Mirrors
  /// [`Self::cycle_focused_value_next`].
  pub fn cycle_focused_value_prev(&mut self) {
    match self.field {
      PickerField::Ctx => self.cycle_ctx_preset_prev(),
      PickerField::Reasoning => self.cycle_reasoning_prev(),
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
  fn reasoning_default_for_new_picker_is_model_default() {
    // Round-8: a fresh picker shows "model default" rather than
    // the legacy `off` — opening the form for a chat model with a
    // reasoning hint won't silently turn it off.
    let s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.reasoning, ReasoningSetting::ModelDefault);
    assert_eq!(s.reasoning.label(), "model default");
    assert_eq!(s.reasoning.as_wire(), None);
  }

  #[test]
  fn reasoning_cycle_walks_tri_state_in_both_directions() {
    let mut s = LaunchPickerState::for_model("qwen");
    // Forward: ModelDefault → On → Off → wrap.
    s.cycle_reasoning_next();
    assert_eq!(s.reasoning, ReasoningSetting::On);
    s.cycle_reasoning_next();
    assert_eq!(s.reasoning, ReasoningSetting::Off);
    s.cycle_reasoning_next();
    assert_eq!(s.reasoning, ReasoningSetting::ModelDefault);
    // Backward: ModelDefault → Off → On → wrap.
    s.cycle_reasoning_prev();
    assert_eq!(s.reasoning, ReasoningSetting::Off);
    s.cycle_reasoning_prev();
    assert_eq!(s.reasoning, ReasoningSetting::On);
    s.cycle_reasoning_prev();
    assert_eq!(s.reasoning, ReasoningSetting::ModelDefault);
  }

  #[test]
  fn reasoning_setting_wire_encoding_omits_model_default() {
    assert_eq!(ReasoningSetting::ModelDefault.as_wire(), None);
    assert_eq!(ReasoningSetting::On.as_wire(), Some(true));
    assert_eq!(ReasoningSetting::Off.as_wire(), Some(false));
  }

  #[test]
  fn reasoning_setting_from_persisted_maps_bool_round_trip() {
    assert_eq!(ReasoningSetting::from_persisted(true), ReasoningSetting::On);
    assert_eq!(
      ReasoningSetting::from_persisted(false),
      ReasoningSetting::Off
    );
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
  fn cycle_focused_value_walks_reasoning_tri_state_when_reasoning_focused() {
    // Round-8: Reasoning is now ModelDefault → On → Off → wrap.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Reasoning;
    assert_eq!(s.reasoning, ReasoningSetting::ModelDefault);
    s.cycle_focused_value_next();
    assert_eq!(
      s.reasoning,
      ReasoningSetting::On,
      "Down on Reasoning walks to On"
    );
    s.cycle_focused_value_prev();
    assert_eq!(
      s.reasoning,
      ReasoningSetting::ModelDefault,
      "Up on Reasoning walks back to ModelDefault"
    );
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
