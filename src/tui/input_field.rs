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
    match key.code {
      KeyCode::Esc => {
        self.editing = false;
        InputOutcome::Handled
      }
      KeyCode::Enter => InputOutcome::Submit,
      KeyCode::Backspace => {
        self.buffer.pop();
        InputOutcome::Handled
      }
      KeyCode::Char(c)
        if !key.modifiers.contains(KeyModifiers::CONTROL)
          && !key.modifiers.contains(KeyModifiers::ALT) =>
      {
        self.buffer.push(c);
        InputOutcome::Handled
      }
      _ => InputOutcome::PassThrough,
    }
  }

  fn handle_key_resting(&mut self, key: KeyEvent) -> InputOutcome {
    match (key.code, key.modifiers) {
      (KeyCode::Char('e'), m) if m.is_empty() => {
        self.editing = true;
        InputOutcome::Handled
      }
      (KeyCode::Esc, _) if !self.buffer.is_empty() => {
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
