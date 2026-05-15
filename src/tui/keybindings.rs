//! Default keybinding map for the TUI shell (Unit 6).
//!
//! Bindings are scoped to a [`Focus`] so the help bar can show
//! only what's relevant in the current focus. Keys are stored as
//! `(KeyCode, KeyModifiers)` so we can reflect Ctrl/Alt
//! combinations literally; the user-facing label comes alongside.
//!
//! The plan calls for config-driven overrides (`config.yaml
//! keybindings:`); v1 ships the static default and will surface a
//! follow-up to overlay user overrides without breaking the help
//! bar.

use crossterm::event::{KeyCode, KeyModifiers};

/// Where the user's focus is on screen. Drives which key bindings
/// are accepted *and* which ones the help bar surfaces. Distinct
/// from "what's rendered" — multiple overlays can stack but only
/// one focus is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
  /// Browsing the model list (default focus on TUI launch).
  List,
  /// Filter input is captured live; alphanumerics extend the filter
  /// buffer instead of triggering hotkeys.
  Filter,
  /// Launch picker overlay — Ctx / Reasoning / Advanced.
  LaunchPicker,
  /// Advanced flags panel.
  AdvancedPanel,
  /// Right pane (Logs / Chat / Embed / Rerank — Unit 7 owns this).
  RightPane,
}

/// Symbolic action a binding triggers. Renderers / event handlers
/// match on this rather than the raw key so config overrides only
/// touch the table, not the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
  Quit,
  MoveUp,
  MoveDown,
  PageUp,
  PageDown,
  GoTop,
  GoBottom,
  OpenFilter,
  ClearFilter,
  ToggleFavorite,
  OpenLaunchPicker,
  OpenAdvancedPanel,
  Submit,
  Cancel,
  YankUrl,
  YankCurl,
  YankPath,
  CycleTheme,
  FocusRightPane,
  FocusList,
  /// Cycle the right-pane tab (Logs ↔ Chat / Embed / Rerank when
  /// the focused model is Ready). Owned by Unit 7.
  CycleTab,
}

/// One binding in the table.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
  pub key: KeyCode,
  pub mods: KeyModifiers,
  pub action: Action,
  /// Short label rendered in the help bar (e.g. `↑` or `Ctrl+D`).
  pub label: &'static str,
  /// One-word description shown next to the label
  /// (e.g. `up` or `quit`).
  pub description: &'static str,
}

/// Default keymap. Static so the help bar can iterate without
/// allocating; config overrides land in a follow-up that overlays
/// changes onto a clone of this slice.
pub const DEFAULT_BINDINGS: &[(Focus, &[Binding])] = &[
  (Focus::List, LIST_BINDINGS),
  (Focus::Filter, FILTER_BINDINGS),
  (Focus::LaunchPicker, LAUNCH_PICKER_BINDINGS),
  (Focus::AdvancedPanel, ADVANCED_BINDINGS),
  (Focus::RightPane, RIGHT_PANE_BINDINGS),
];

const LIST_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Char('q'),
    mods: KeyModifiers::NONE,
    action: Action::Quit,
    label: "q",
    description: "quit",
  },
  Binding {
    key: KeyCode::Char('c'),
    mods: KeyModifiers::CONTROL,
    action: Action::Quit,
    label: "Ctrl+C",
    description: "quit",
  },
  Binding {
    key: KeyCode::Up,
    mods: KeyModifiers::NONE,
    action: Action::MoveUp,
    label: "↑",
    description: "up",
  },
  Binding {
    key: KeyCode::Char('k'),
    mods: KeyModifiers::NONE,
    action: Action::MoveUp,
    label: "k",
    description: "up",
  },
  Binding {
    key: KeyCode::Down,
    mods: KeyModifiers::NONE,
    action: Action::MoveDown,
    label: "↓",
    description: "down",
  },
  Binding {
    key: KeyCode::Char('j'),
    mods: KeyModifiers::NONE,
    action: Action::MoveDown,
    label: "j",
    description: "down",
  },
  Binding {
    key: KeyCode::PageUp,
    mods: KeyModifiers::NONE,
    action: Action::PageUp,
    label: "PgUp",
    description: "page up",
  },
  Binding {
    key: KeyCode::PageDown,
    mods: KeyModifiers::NONE,
    action: Action::PageDown,
    label: "PgDn",
    description: "page down",
  },
  Binding {
    key: KeyCode::Char('g'),
    mods: KeyModifiers::NONE,
    action: Action::GoTop,
    label: "g",
    description: "top",
  },
  Binding {
    key: KeyCode::Char('G'),
    mods: KeyModifiers::SHIFT,
    action: Action::GoBottom,
    label: "G",
    description: "bottom",
  },
  Binding {
    key: KeyCode::Char('/'),
    mods: KeyModifiers::NONE,
    action: Action::OpenFilter,
    label: "/",
    description: "filter",
  },
  Binding {
    key: KeyCode::Char('f'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleFavorite,
    label: "f",
    description: "favorite",
  },
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::OpenLaunchPicker,
    label: "Enter",
    description: "launch",
  },
  Binding {
    key: KeyCode::Char('a'),
    mods: KeyModifiers::NONE,
    action: Action::OpenAdvancedPanel,
    label: "a",
    description: "advanced",
  },
  Binding {
    key: KeyCode::Char('y'),
    mods: KeyModifiers::NONE,
    action: Action::YankUrl,
    label: "y",
    description: "yank url",
  },
  Binding {
    key: KeyCode::Char('Y'),
    mods: KeyModifiers::SHIFT,
    action: Action::YankCurl,
    label: "Y",
    description: "yank curl",
  },
  Binding {
    key: KeyCode::Char('p'),
    mods: KeyModifiers::NONE,
    action: Action::YankPath,
    label: "p",
    description: "yank path",
  },
  Binding {
    key: KeyCode::Char('t'),
    mods: KeyModifiers::NONE,
    action: Action::CycleTheme,
    label: "t",
    description: "theme",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::FocusRightPane,
    label: "Tab",
    description: "right pane",
  },
];

const FILTER_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::ClearFilter,
    label: "Esc",
    description: "clear",
  },
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "apply",
  },
];

const LAUNCH_PICKER_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::Cancel,
    label: "Esc",
    description: "cancel",
  },
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "launch",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::MoveDown,
    label: "Tab",
    description: "next field",
  },
  Binding {
    key: KeyCode::Char('a'),
    mods: KeyModifiers::NONE,
    action: Action::OpenAdvancedPanel,
    label: "a",
    description: "advanced",
  },
];

const ADVANCED_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::Cancel,
    label: "Esc",
    description: "back",
  },
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "save",
  },
];

const RIGHT_PANE_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::FocusList,
    label: "Esc",
    description: "list",
  },
  Binding {
    key: KeyCode::BackTab,
    mods: KeyModifiers::SHIFT,
    action: Action::FocusList,
    label: "Shift+Tab",
    description: "list",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::CycleTab,
    label: "Tab",
    description: "next tab",
  },
];

/// Look up the action triggered by `(key, mods)` in the supplied
/// focus. Returns `None` when nothing matches.
pub fn action_for(focus: Focus, key: KeyCode, mods: KeyModifiers) -> Option<Action> {
  for binding in bindings_for(focus) {
    if binding.key == key && binding.mods == mods {
      return Some(binding.action);
    }
  }
  None
}

/// Bindings the help bar should show in the supplied focus.
pub fn bindings_for(focus: Focus) -> &'static [Binding] {
  for (f, b) in DEFAULT_BINDINGS {
    if *f == focus {
      return b;
    }
  }
  &[]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn list_focus_resolves_quit_via_q_or_ctrl_c() {
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('q'), KeyModifiers::NONE),
      Some(Action::Quit)
    );
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('c'), KeyModifiers::CONTROL),
      Some(Action::Quit)
    );
  }

  #[test]
  fn filter_focus_does_not_inherit_list_quit() {
    // While the user is typing in the filter, `q` must extend the
    // buffer rather than quit. Bindings table is per-focus.
    assert_eq!(
      action_for(Focus::Filter, KeyCode::Char('q'), KeyModifiers::NONE),
      None
    );
  }

  #[test]
  fn list_bindings_include_navigation_filter_launch_yank_theme() {
    let bs = bindings_for(Focus::List);
    let labels: Vec<&str> = bs.iter().map(|b| b.label).collect();
    assert!(labels.contains(&"q"));
    assert!(labels.contains(&"↑"));
    assert!(labels.contains(&"j"));
    assert!(labels.contains(&"/"));
    assert!(labels.contains(&"f"));
    assert!(labels.contains(&"y"));
    assert!(labels.contains(&"t"));
  }

  #[test]
  fn launch_picker_focus_can_open_advanced() {
    assert_eq!(
      action_for(Focus::LaunchPicker, KeyCode::Char('a'), KeyModifiers::NONE),
      Some(Action::OpenAdvancedPanel)
    );
  }

  #[test]
  fn right_pane_esc_returns_to_list() {
    assert_eq!(
      action_for(Focus::RightPane, KeyCode::Esc, KeyModifiers::NONE),
      Some(Action::FocusList)
    );
  }
}
