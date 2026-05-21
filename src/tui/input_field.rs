//! Reusable modal text-input state for the TUI.
//!
//! Modal contract (uniform across every text input in the app):
//! - resting (`!editing`):
//!   - `e` → enter edit mode
//!   - `Esc` on a non-empty buffer → clear the buffer
//!   - any other key → caller decides (sort/page/etc.)
//! - editing:
//!   - printable chars → append to buffer
//!   - `Backspace` → pop one char
//!   - `Esc` → exit edit mode (buffer kept)
//!   - `Enter` → bubbles up as `InputOutcome::Submit`
//!   - everything else (arrows, Tab, …) passes through so the
//!     caller can react (row navigation, focus cycling, …)
//!
//! The component owns *only* state and key routing; rendering and
//! styling are the caller's job so each call site keeps its
//! existing look (filter chip, chat composer borders, HF dialog
//! search line, advanced-panel extras row, …).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputField {
  buffer: String,
  editing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
  /// Key was consumed by the component (typing, backspace, mode
  /// toggle, clear). Caller should treat this as a state change but
  /// no semantic action.
  Handled,
  /// User pressed `Enter` while editing. The caller decides what
  /// "submit" means (apply filter, requery, open row, …).
  Submit,
  /// Component declined the key. Caller should fall through to its
  /// own keymap (arrows in edit mode, sort/page when resting, …).
  PassThrough,
}

impl InputField {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_text(s: impl Into<String>) -> Self {
    Self {
      buffer: s.into(),
      editing: false,
    }
  }

  pub fn buffer(&self) -> &str {
    &self.buffer
  }

  pub fn is_editing(&self) -> bool {
    self.editing
  }

  pub fn is_empty(&self) -> bool {
    self.buffer.is_empty()
  }

  pub fn clear(&mut self) {
    self.buffer.clear();
  }

  pub fn enter_edit(&mut self) {
    self.editing = true;
  }

  pub fn exit_edit(&mut self) {
    self.editing = false;
  }

  pub fn set_text(&mut self, s: impl Into<String>) {
    self.buffer = s.into();
  }

  /// Route a key event through the modal state machine. See module
  /// docs for the contract.
  pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
    if self.editing {
      self.handle_key_editing(key)
    } else {
      self.handle_key_resting(key)
    }
  }

  fn handle_key_editing(&mut self, key: KeyEvent) -> InputOutcome {
    // Modifier policy:
    // - `Esc`, `Enter`, `Backspace` accept only the bare key. A
    //   modified variant (`Ctrl+Enter`, `Shift+Enter`, `Ctrl+Esc`,
    //   …) is a chord — pass through so the action layer can route
    //   it (e.g. `Shift+Enter → Action::InsertNewline`).
    // - Printable `Char` accepts `SHIFT` only (capitals). `CONTROL`,
    //   `ALT`, `SUPER`/`META`, and the kitty-protocol `HYPER` are
    //   reserved for chorded shortcuts and must pass through.
    let bare = key.modifiers.is_empty();
    let char_allowed = (key.modifiers - KeyModifiers::SHIFT).is_empty();
    match key.code {
      KeyCode::Esc if bare => {
        self.editing = false;
        InputOutcome::Handled
      }
      KeyCode::Enter if bare => InputOutcome::Submit,
      KeyCode::Backspace if bare => {
        self.buffer.pop();
        InputOutcome::Handled
      }
      KeyCode::Char(c) if char_allowed => {
        self.buffer.push(c);
        InputOutcome::Handled
      }
      _ => InputOutcome::PassThrough,
    }
  }

  fn handle_key_resting(&mut self, key: KeyEvent) -> InputOutcome {
    // Resting mode is strictly unmodified — `Ctrl+E` is a chord
    // (reserved for the action layer); only bare `e` enters edit.
    // Bare `Esc` on a non-empty buffer clears it; everything else
    // falls through so the caller's keymap fires.
    if !key.modifiers.is_empty() {
      return InputOutcome::PassThrough;
    }
    match key.code {
      KeyCode::Char('e') => {
        self.editing = true;
        InputOutcome::Handled
      }
      KeyCode::Esc if !self.buffer.is_empty() => {
        self.buffer.clear();
        InputOutcome::Handled
      }
      _ => InputOutcome::PassThrough,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
  }

  fn key_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
  }

  #[test]
  fn new_field_is_resting_and_empty() {
    let field = InputField::new();
    assert!(!field.is_editing());
    assert!(field.is_empty());
    assert_eq!(field.buffer(), "");
  }

  #[test]
  fn with_text_seeds_buffer_without_entering_edit() {
    let field = InputField::with_text("qwen");
    assert_eq!(field.buffer(), "qwen");
    assert!(!field.is_editing());
  }

  #[test]
  fn resting_e_enters_edit_mode() {
    let mut field = InputField::new();
    assert_eq!(
      field.handle_key(key(KeyCode::Char('e'))),
      InputOutcome::Handled
    );
    assert!(field.is_editing());
    assert_eq!(field.buffer(), "");
  }

  #[test]
  fn resting_shift_e_passes_through() {
    let mut field = InputField::new();
    let outcome = field.handle_key(key_with(KeyCode::Char('E'), KeyModifiers::SHIFT));
    assert_eq!(outcome, InputOutcome::PassThrough);
    assert!(!field.is_editing());
  }

  #[test]
  fn resting_char_other_than_e_passes_through() {
    let mut field = InputField::new();
    for ch in ['a', 'n', 'p', 'o', 's', 'q'] {
      let outcome = field.handle_key(key(KeyCode::Char(ch)));
      assert_eq!(outcome, InputOutcome::PassThrough, "char {ch:?}");
      assert!(!field.is_editing());
    }
  }

  #[test]
  fn resting_esc_on_empty_buffer_passes_through() {
    let mut field = InputField::new();
    assert_eq!(
      field.handle_key(key(KeyCode::Esc)),
      InputOutcome::PassThrough
    );
  }

  #[test]
  fn resting_esc_on_non_empty_buffer_clears() {
    let mut field = InputField::with_text("hello");
    assert_eq!(field.handle_key(key(KeyCode::Esc)), InputOutcome::Handled);
    assert!(field.is_empty());
    assert!(!field.is_editing());
  }

  #[test]
  fn editing_printable_chars_append_to_buffer() {
    let mut field = InputField::new();
    field.enter_edit();
    for ch in ['q', 'w', 'e', 'n'] {
      assert_eq!(
        field.handle_key(key(KeyCode::Char(ch))),
        InputOutcome::Handled
      );
    }
    assert_eq!(field.buffer(), "qwen");
    assert!(field.is_editing());
  }

  #[test]
  fn editing_backspace_pops_buffer() {
    let mut field = InputField::with_text("qwen");
    field.enter_edit();
    assert_eq!(
      field.handle_key(key(KeyCode::Backspace)),
      InputOutcome::Handled
    );
    assert_eq!(field.buffer(), "qwe");
  }

  #[test]
  fn editing_backspace_on_empty_is_noop_handled() {
    let mut field = InputField::new();
    field.enter_edit();
    assert_eq!(
      field.handle_key(key(KeyCode::Backspace)),
      InputOutcome::Handled
    );
    assert_eq!(field.buffer(), "");
    assert!(field.is_editing());
  }

  #[test]
  fn editing_esc_exits_edit_keeps_buffer() {
    let mut field = InputField::with_text("qwen");
    field.enter_edit();
    assert_eq!(field.handle_key(key(KeyCode::Esc)), InputOutcome::Handled);
    assert!(!field.is_editing());
    assert_eq!(field.buffer(), "qwen");
  }

  #[test]
  fn editing_enter_returns_submit() {
    let mut field = InputField::new();
    field.enter_edit();
    assert_eq!(field.handle_key(key(KeyCode::Enter)), InputOutcome::Submit);
    assert!(field.is_editing(), "Submit alone should not exit edit mode");
  }

  #[test]
  fn editing_arrows_pass_through() {
    let mut field = InputField::new();
    field.enter_edit();
    for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
      assert_eq!(field.handle_key(key(code)), InputOutcome::PassThrough);
    }
  }

  #[test]
  fn editing_ctrl_char_passes_through() {
    let mut field = InputField::new();
    field.enter_edit();
    let outcome = field.handle_key(key_with(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(outcome, InputOutcome::PassThrough);
    assert_eq!(field.buffer(), "");
  }

  #[test]
  fn editing_super_meta_chars_pass_through() {
    // Cmd-style chords delivered by kitty / WezTerm protocols must
    // pass through so the action layer can dispatch them. The field
    // only accepts plain printable + SHIFT (capital letters).
    let mut field = InputField::new();
    field.enter_edit();
    for m in [
      KeyModifiers::SUPER,
      KeyModifiers::ALT,
      KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ] {
      let outcome = field.handle_key(key_with(KeyCode::Char('w'), m));
      assert_eq!(
        outcome,
        InputOutcome::PassThrough,
        "modifier {m:?} on a char must pass through"
      );
    }
    assert_eq!(field.buffer(), "", "no chord should land in the buffer");
  }

  #[test]
  fn editing_shift_char_is_typed_as_capital() {
    // SHIFT is the one modifier that must NOT pass through — it
    // carries the capital-letter signal that the field is meant to
    // type. Without this, capitals would route to the action layer
    // instead of the buffer.
    let mut field = InputField::new();
    field.enter_edit();
    let outcome = field.handle_key(key_with(KeyCode::Char('Q'), KeyModifiers::SHIFT));
    assert_eq!(outcome, InputOutcome::Handled);
    assert_eq!(field.buffer(), "Q");
  }

  #[test]
  fn editing_modified_esc_and_enter_pass_through() {
    // `Ctrl+Esc` / `Shift+Enter` are chords, not plain edit gestures.
    // They must pass so a bound action (e.g. Action::InsertNewline on
    // Shift+Enter) fires.
    let mut field = InputField::new();
    field.enter_edit();
    field.set_text("hi");
    field.enter_edit();
    assert_eq!(
      field.handle_key(key_with(KeyCode::Esc, KeyModifiers::CONTROL)),
      InputOutcome::PassThrough
    );
    assert!(
      field.is_editing(),
      "modified Esc must NOT exit edit mode (it's a chord)"
    );
    assert_eq!(
      field.handle_key(key_with(KeyCode::Enter, KeyModifiers::CONTROL)),
      InputOutcome::PassThrough,
      "Ctrl+Enter must pass through so the action layer can handle it"
    );
    // Plain Shift+Enter is still a chord — Submit is the bare Enter
    // gesture. Action::InsertNewline owns Shift+Enter at the action
    // layer.
    assert_eq!(
      field.handle_key(key_with(KeyCode::Enter, KeyModifiers::SHIFT)),
      InputOutcome::PassThrough,
      "Shift+Enter must pass so Action::InsertNewline can fire"
    );
  }

  #[test]
  fn resting_modified_esc_passes_through() {
    // A stray `Ctrl+Esc` / `Shift+Esc` on a resting buffer must not
    // silently wipe the contents — those are chords reserved for the
    // action layer (or no-ops). Only bare `Esc` clears.
    let mut field = InputField::with_text("hello");
    let outcome = field.handle_key(key_with(KeyCode::Esc, KeyModifiers::CONTROL));
    assert_eq!(outcome, InputOutcome::PassThrough);
    assert_eq!(field.buffer(), "hello", "modified Esc must NOT clear");
  }

  #[test]
  fn editing_then_esc_then_esc_clears_in_two_steps() {
    let mut field = InputField::with_text("qwen");
    field.enter_edit();
    // 1st Esc: exit edit, buffer kept.
    assert_eq!(field.handle_key(key(KeyCode::Esc)), InputOutcome::Handled);
    assert!(!field.is_editing());
    assert_eq!(field.buffer(), "qwen");
    // 2nd Esc: clear.
    assert_eq!(field.handle_key(key(KeyCode::Esc)), InputOutcome::Handled);
    assert!(field.is_empty());
    // 3rd Esc: pass through (caller walks navigation back).
    assert_eq!(
      field.handle_key(key(KeyCode::Esc)),
      InputOutcome::PassThrough
    );
  }

  #[test]
  fn set_text_overrides_buffer_without_changing_mode() {
    let mut field = InputField::new();
    field.enter_edit();
    field.set_text("hello");
    assert_eq!(field.buffer(), "hello");
    assert!(field.is_editing());
  }

  #[test]
  fn clear_empties_without_changing_mode() {
    let mut field = InputField::with_text("hello");
    field.enter_edit();
    field.clear();
    assert!(field.is_empty());
    assert!(field.is_editing());
  }
}
