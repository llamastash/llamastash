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
/// one focus is active. `Ord`/`PartialOrd` exist solely so `Focus`
/// can key the `BTreeMap` inside [`KeyMap`]; the ordering is
/// derive-defined and not part of the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
  /// Right pane in a non-input mode (the Logs tab, primarily).
  /// Text-capture variants below cover the live input surfaces.
  RightPane,
  /// Chat tab prompt input — alphanumerics/backspace extend the
  /// prompt buffer; Enter sends, Shift+Enter inserts a newline (on
  /// terminals that implement the kitty keyboard protocol).
  ChatInput,
  /// Embed tab input — Enter calls /v1/embeddings on the focused
  /// model.
  EmbedInput,
  /// Rerank tab input — Tab stages a candidate, Enter rerank-calls.
  RerankInput,
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
  /// Snap focus back to the Models list. Bound to `Esc` in the
  /// right pane (closes the pane) and in the LaunchPicker /
  /// AdvancedPanel overlays.
  FocusList,
  /// Walk one step forward in the focus chain
  /// `[List, ...available_right_tabs()]`. Bound to `Tab`, `Right`,
  /// and `l`. Wraps.
  NextFocus,
  /// Walk one step backward in the focus chain. Bound to
  /// `Shift+Tab`, `Left`, and `h`. Wraps.
  PrevFocus,
  /// Send the buffered chat prompt to `/v1/chat/completions`.
  /// Bound to plain `Enter` in [`Focus::ChatInput`] — `Ctrl+Enter`
  /// can't be reliably distinguished outside terminals that ship
  /// the kitty keyboard protocol.
  SendChat,
  /// Toggle the per-message `<think>...</think>` collapse in the
  /// Chat tab (R32 reasoning-aware view).
  ToggleThinkCollapse,
  /// Toggle auto-scroll on the Logs tab.
  ToggleAutoScroll,
  /// Stage the in-progress rerank candidate buffer onto the
  /// candidate list. Bound to `Tab` in [`Focus::RerankInput`].
  StageRerankCandidate,
  /// Show or hide the modal help overlay (bound to `?`).
  ToggleHelp,
  /// Ask the daemon to stop the focused managed launch. Bound to
  /// `s` in [`Focus::List`].
  StopModel,
  /// Enter edit / text-capture mode on the active right-pane tab
  /// (Chat / Embed / Rerank). Bound to `e` in [`Focus::RightPane`].
  EnterEdit,
  /// Step back from a text-input focus to the right pane's
  /// navigation focus. Bound to `Esc` in each input focus.
  ExitEdit,
  /// Kill the daemon entirely (after a confirmation popup). Bound
  /// to `Q` (Shift+q) in the model list focus.
  KillDaemon,
  /// Jump focus to the Logs tab in the right pane. No-op (with a
  /// toast) when the focused model isn't running, since Logs is
  /// only reachable for live launches.
  FocusLogsTab,
  /// Jump focus to whichever mode-specific tab is reachable for the
  /// focused model (Chat / Embed / Rerank). No-op + toast when the
  /// model isn't running.
  FocusChatTab,
  /// Jump focus to the Settings tab in the right pane. Always
  /// available because Settings exists for every selection.
  FocusSettingsTab,
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
  (Focus::ChatInput, CHAT_INPUT_BINDINGS),
  (Focus::EmbedInput, EMBED_INPUT_BINDINGS),
  (Focus::RerankInput, RERANK_INPUT_BINDINGS),
];

const LIST_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Char('?'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleHelp,
    label: "?",
    description: "help",
  },
  Binding {
    key: KeyCode::Char('q'),
    mods: KeyModifiers::NONE,
    action: Action::Quit,
    label: "q",
    description: "quit",
  },
  Binding {
    key: KeyCode::Char('Q'),
    mods: KeyModifiers::SHIFT,
    action: Action::KillDaemon,
    label: "Q",
    description: "kill daemon",
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
    key: KeyCode::Char('u'),
    mods: KeyModifiers::NONE,
    action: Action::YankUrl,
    label: "u",
    description: "url",
  },
  Binding {
    key: KeyCode::Char('c'),
    mods: KeyModifiers::NONE,
    action: Action::YankCurl,
    label: "c",
    description: "curl",
  },
  Binding {
    key: KeyCode::Char('p'),
    mods: KeyModifiers::NONE,
    action: Action::YankPath,
    label: "p",
    description: "path",
  },
  Binding {
    key: KeyCode::Char('t'),
    mods: KeyModifiers::NONE,
    action: Action::CycleTheme,
    label: "t",
    description: "theme",
  },
  Binding {
    key: KeyCode::Char('s'),
    mods: KeyModifiers::NONE,
    action: Action::StopModel,
    label: "s",
    description: "stop",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "Tab",
    description: "next pane",
  },
  Binding {
    key: KeyCode::BackTab,
    mods: KeyModifiers::SHIFT,
    action: Action::PrevFocus,
    label: "Shift+Tab",
    description: "prev pane",
  },
  Binding {
    key: KeyCode::Right,
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "→",
    description: "next pane",
  },
  Binding {
    key: KeyCode::Left,
    mods: KeyModifiers::NONE,
    action: Action::PrevFocus,
    label: "←",
    description: "prev pane",
  },
  Binding {
    key: KeyCode::Char('l'),
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "l",
    description: "next pane",
  },
  Binding {
    key: KeyCode::Char('h'),
    mods: KeyModifiers::NONE,
    action: Action::PrevFocus,
    label: "h",
    description: "prev pane",
  },
  // Shift-letter quick-jumps so the user can hop straight to a
  // specific surface without walking the focus chain. The lowercase
  // counterparts (`m`/`l`/`c`/`s`) are already in use for unrelated
  // actions, so the shifted form is the distinct keystroke.
  Binding {
    key: KeyCode::Char('M'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusList,
    label: "M",
    description: "models",
  },
  Binding {
    key: KeyCode::Char('L'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusLogsTab,
    label: "L",
    description: "logs",
  },
  Binding {
    key: KeyCode::Char('C'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusChatTab,
    label: "C",
    description: "chat",
  },
  Binding {
    key: KeyCode::Char('S'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusSettingsTab,
    label: "S",
    description: "settings",
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
    key: KeyCode::Char('?'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleHelp,
    label: "?",
    description: "help",
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
    key: KeyCode::Char('?'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleHelp,
    label: "?",
    description: "help",
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
    key: KeyCode::Char('?'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleHelp,
    label: "?",
    description: "help",
  },
  Binding {
    key: KeyCode::Char('t'),
    mods: KeyModifiers::NONE,
    action: Action::CycleTheme,
    label: "t",
    description: "theme",
  },
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
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "launch (Settings)",
  },
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::FocusList,
    label: "Esc",
    description: "models list",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "Tab",
    description: "next pane",
  },
  Binding {
    key: KeyCode::BackTab,
    mods: KeyModifiers::SHIFT,
    action: Action::PrevFocus,
    label: "Shift+Tab",
    description: "prev pane",
  },
  Binding {
    key: KeyCode::Right,
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "→",
    description: "next pane",
  },
  Binding {
    key: KeyCode::Left,
    mods: KeyModifiers::NONE,
    action: Action::PrevFocus,
    label: "←",
    description: "prev pane",
  },
  Binding {
    key: KeyCode::Char('l'),
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "l",
    description: "next pane",
  },
  Binding {
    key: KeyCode::Char('h'),
    mods: KeyModifiers::NONE,
    action: Action::PrevFocus,
    label: "h",
    description: "prev pane",
  },
  Binding {
    key: KeyCode::Char('s'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleAutoScroll,
    label: "s",
    description: "auto-scroll",
  },
  Binding {
    key: KeyCode::Char('e'),
    mods: KeyModifiers::NONE,
    action: Action::EnterEdit,
    label: "e",
    description: "edit",
  },
  Binding {
    key: KeyCode::Char('a'),
    mods: KeyModifiers::NONE,
    action: Action::OpenAdvancedPanel,
    label: "a",
    description: "advanced",
  },
  Binding {
    key: KeyCode::Char('j'),
    mods: KeyModifiers::NONE,
    action: Action::MoveDown,
    label: "j",
    description: "scroll down",
  },
  Binding {
    key: KeyCode::Char('k'),
    mods: KeyModifiers::NONE,
    action: Action::MoveUp,
    label: "k",
    description: "scroll up",
  },
  Binding {
    key: KeyCode::Down,
    mods: KeyModifiers::NONE,
    action: Action::MoveDown,
    label: "↓",
    description: "scroll down",
  },
  Binding {
    key: KeyCode::Up,
    mods: KeyModifiers::NONE,
    action: Action::MoveUp,
    label: "↑",
    description: "scroll up",
  },
  // Mirror the LIST_BINDINGS shift-letter quick-jumps so the user
  // can hop between surfaces from either pane.
  Binding {
    key: KeyCode::Char('M'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusList,
    label: "M",
    description: "models",
  },
  Binding {
    key: KeyCode::Char('L'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusLogsTab,
    label: "L",
    description: "logs",
  },
  Binding {
    key: KeyCode::Char('C'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusChatTab,
    label: "C",
    description: "chat",
  },
  Binding {
    key: KeyCode::Char('S'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusSettingsTab,
    label: "S",
    description: "settings",
  },
];

const CHAT_INPUT_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::ExitEdit,
    label: "Esc",
    description: "exit edit",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "Tab",
    description: "next pane",
  },
  Binding {
    key: KeyCode::BackTab,
    mods: KeyModifiers::SHIFT,
    action: Action::PrevFocus,
    label: "Shift+Tab",
    description: "prev pane",
  },
  // Plain Enter so submit fires on every terminal — Ctrl+Enter is
  // ambiguous unless the kitty keyboard protocol is active, which
  // many terminals (gnome-terminal, konsole, macOS Terminal) don't
  // implement. Shift+Enter inserts a newline in the prompt buffer
  // (only distinguishable on kitty-protocol terminals; elsewhere it
  // collapses to plain Enter and submits — fine for v1).
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::SendChat,
    label: "Enter",
    description: "send",
  },
  Binding {
    key: KeyCode::Char('r'),
    mods: KeyModifiers::CONTROL,
    action: Action::ToggleThinkCollapse,
    label: "Ctrl+r",
    description: "collapse think",
  },
];

const EMBED_INPUT_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::ExitEdit,
    label: "Esc",
    description: "exit edit",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::NextFocus,
    label: "Tab",
    description: "next pane",
  },
  Binding {
    key: KeyCode::BackTab,
    mods: KeyModifiers::SHIFT,
    action: Action::PrevFocus,
    label: "Shift+Tab",
    description: "prev pane",
  },
  // Plain Enter — see CHAT_INPUT_BINDINGS for the rationale.
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "embed",
  },
];

const RERANK_INPUT_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::ExitEdit,
    label: "Esc",
    description: "exit edit",
  },
  Binding {
    key: KeyCode::Tab,
    mods: KeyModifiers::NONE,
    action: Action::StageRerankCandidate,
    label: "Tab",
    description: "stage / next pane",
  },
  Binding {
    key: KeyCode::BackTab,
    mods: KeyModifiers::SHIFT,
    action: Action::PrevFocus,
    label: "Shift+Tab",
    description: "prev pane",
  },
  // Plain Enter — see CHAT_INPUT_BINDINGS for the rationale.
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "rank",
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

// ─── Runtime keymap (config overrides) ──────────────────────────
//
// The compile-time `DEFAULT_BINDINGS` slice above stays the source
// of truth for stock bindings. The runtime `KeyMap` below clones
// those defaults into owned `Vec<Binding>`s and lets `config.yaml`
// `keybindings:` overlay `action_name → key_spec` overrides on
// top, kdash-style.
//
// Trick: a `Binding`'s label/description are `&'static str`. The
// override path leaks the rebind label via `Box::leak` so the
// resulting `&'static str` slot stays compatible. We leak at most
// one short string per user override (≤ a few dozen bytes), once
// at startup — negligible vs the operational benefit of a uniform
// `Binding` type across static + runtime entries.

use std::collections::BTreeMap;

/// Runtime keybinding table. Built once at App startup from
/// `KeyMap::default()` + `KeyMap::apply_overrides(config)`. Renderers
/// route every key through [`KeyMap::action_for`] and the help
/// overlay walks [`KeyMap::iter`] so a config tweak takes effect
/// everywhere without touching the dispatcher.
#[derive(Debug, Clone)]
pub struct KeyMap {
  by_focus: BTreeMap<Focus, Vec<Binding>>,
}

impl Default for KeyMap {
  fn default() -> Self {
    let mut by_focus: BTreeMap<Focus, Vec<Binding>> = BTreeMap::new();
    for (focus, rows) in DEFAULT_BINDINGS {
      by_focus.insert(*focus, rows.to_vec());
    }
    KeyMap { by_focus }
  }
}

impl KeyMap {
  /// Look up the action triggered by `(key, mods)` in the supplied
  /// focus. Returns `None` when nothing matches.
  pub fn action_for(&self, focus: Focus, key: KeyCode, mods: KeyModifiers) -> Option<Action> {
    self.by_focus.get(&focus).and_then(|rows| {
      rows
        .iter()
        .find(|b| b.key == key && b.mods == mods)
        .map(|b| b.action)
    })
  }

  /// Bindings the help bar should show in the supplied focus.
  pub fn bindings_for(&self, focus: Focus) -> &[Binding] {
    self.by_focus.get(&focus).map(Vec::as_slice).unwrap_or(&[])
  }

  /// Iterator over every `(focus, bindings)` pair. Replaces direct
  /// access to `DEFAULT_BINDINGS` for callers (help overlay) that
  /// walk the whole table.
  pub fn iter(&self) -> impl Iterator<Item = (Focus, &[Binding])> {
    self
      .by_focus
      .iter()
      .map(|(focus, rows)| (*focus, rows.as_slice()))
  }

  /// Overlay user-supplied `action → key_spec` pairs onto the
  /// keymap (kdash-style). For each override, every default
  /// binding for that action across all focuses is removed; the
  /// new binding is then inserted in every focus where the action
  /// previously lived. Any existing binding at the new key spec in
  /// those focuses is dropped to prevent ambiguous dispatch.
  ///
  /// Returns human-readable warnings for unknown actions and
  /// unparseable key specs; the caller forwards them to
  /// `log::warn!`.
  pub fn apply_overrides(&mut self, overrides: &BTreeMap<String, String>) -> Vec<String> {
    let mut warnings = Vec::new();
    for (raw_action, raw_spec) in overrides {
      let action = match Action::from_config_name(raw_action) {
        Some(a) => a,
        None => {
          warnings.push(format!(
            "keybindings: unknown action '{raw_action}'; valid: {}",
            Action::all_config_names().join(", ")
          ));
          continue;
        }
      };
      let spec = match parse_key_spec(raw_spec) {
        Ok(s) => s,
        Err(error) => {
          warnings.push(format!("keybindings.{raw_action}: '{raw_spec}' — {error}"));
          continue;
        }
      };
      // Leak the runtime label so the resulting Binding fits the
      // `&'static str` slot. One-time at startup, never repeated.
      let leaked_label: &'static str = Box::leak(spec.label.into_boxed_str());
      let mut any_focus_had_action = false;
      for rows in self.by_focus.values_mut() {
        let mut description: &'static str = "";
        let mut rebuilt: Vec<Binding> = Vec::with_capacity(rows.len());
        let mut had_action_here = false;
        for b in rows.iter().copied() {
          if b.action == action {
            had_action_here = true;
            if description.is_empty() {
              description = b.description;
            }
            continue;
          }
          if b.key == spec.key && b.mods == spec.mods {
            continue;
          }
          rebuilt.push(b);
        }
        if had_action_here {
          any_focus_had_action = true;
          rebuilt.push(Binding {
            key: spec.key,
            mods: spec.mods,
            action,
            label: leaked_label,
            description,
          });
        }
        *rows = rebuilt;
      }
      if !any_focus_had_action {
        warnings.push(format!(
          "keybindings.{raw_action}: action has no default binding in any focus; nothing was rebound"
        ));
      }
    }
    warnings
  }
}

/// Parsed key spec — the result of [`parse_key_spec`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeySpec {
  key: KeyCode,
  mods: KeyModifiers,
  label: String,
}

impl Action {
  /// Mapping table from config-facing snake_case names to variants.
  const CONFIG_NAMES: &'static [(&'static str, Action)] = &[
    ("quit", Action::Quit),
    ("move_up", Action::MoveUp),
    ("move_down", Action::MoveDown),
    ("page_up", Action::PageUp),
    ("page_down", Action::PageDown),
    ("go_top", Action::GoTop),
    ("go_bottom", Action::GoBottom),
    ("open_filter", Action::OpenFilter),
    ("clear_filter", Action::ClearFilter),
    ("toggle_favorite", Action::ToggleFavorite),
    ("open_launch_picker", Action::OpenLaunchPicker),
    ("open_advanced_panel", Action::OpenAdvancedPanel),
    ("submit", Action::Submit),
    ("cancel", Action::Cancel),
    ("yank_url", Action::YankUrl),
    ("yank_curl", Action::YankCurl),
    ("yank_path", Action::YankPath),
    ("cycle_theme", Action::CycleTheme),
    ("focus_list", Action::FocusList),
    ("next_focus", Action::NextFocus),
    ("prev_focus", Action::PrevFocus),
    ("send_chat", Action::SendChat),
    ("toggle_think_collapse", Action::ToggleThinkCollapse),
    ("toggle_auto_scroll", Action::ToggleAutoScroll),
    ("stage_rerank_candidate", Action::StageRerankCandidate),
    ("toggle_help", Action::ToggleHelp),
    ("stop_model", Action::StopModel),
    ("enter_edit", Action::EnterEdit),
    ("exit_edit", Action::ExitEdit),
    ("kill_daemon", Action::KillDaemon),
    ("focus_logs_tab", Action::FocusLogsTab),
    ("focus_chat_tab", Action::FocusChatTab),
    ("focus_settings_tab", Action::FocusSettingsTab),
  ];

  /// Parse a config-name (snake_case or kebab-case) into an action.
  pub fn from_config_name(raw: &str) -> Option<Action> {
    let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
    Self::CONFIG_NAMES
      .iter()
      .find(|(name, _)| *name == normalized)
      .map(|(_, action)| *action)
  }

  /// The canonical config-facing names, used in error messages.
  pub fn all_config_names() -> Vec<&'static str> {
    Self::CONFIG_NAMES.iter().map(|(n, _)| *n).collect()
  }
}

/// Parse a user-supplied key spec. Accepts case-insensitive
/// `+`-joined modifier+key tokens, e.g. `ctrl+q`, `shift+tab`,
/// `alt+enter`, named keys (`enter`, `esc`, `tab`, `backtab`,
/// `space`, `backspace`, `up`/`down`/`left`/`right`, `home`/`end`,
/// `pgup`/`pgdn`, `del`/`ins`, `f1`–`f12`), and bare single
/// characters (`q`, `?`, `/`, `Q`).
fn parse_key_spec(raw: &str) -> Result<KeySpec, String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Err("empty key spec".to_string());
  }
  let mut mods = KeyModifiers::NONE;
  let mut tokens = trimmed.split('+').map(str::trim).peekable();
  let mut key_token: Option<&str> = None;
  while let Some(tok) = tokens.next() {
    if tok.is_empty() {
      return Err(format!("empty segment in '{raw}'"));
    }
    if tokens.peek().is_some() {
      match tok.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
        "shift" => mods |= KeyModifiers::SHIFT,
        "alt" | "meta" => mods |= KeyModifiers::ALT,
        "super" | "cmd" => mods |= KeyModifiers::SUPER,
        other => return Err(format!("unknown modifier '{other}'")),
      }
    } else {
      key_token = Some(tok);
    }
  }
  let key_token = key_token.ok_or_else(|| format!("no key in '{raw}'"))?;
  let (key, implicit_shift) = parse_key_token(key_token)?;
  if implicit_shift {
    mods |= KeyModifiers::SHIFT;
  }
  Ok(KeySpec {
    label: format_key_label(&key, mods),
    key,
    mods,
  })
}

fn parse_key_token(tok: &str) -> Result<(KeyCode, bool), String> {
  if tok.chars().count() == 1 {
    let ch = tok.chars().next().unwrap();
    return Ok((KeyCode::Char(ch), ch.is_ascii_uppercase()));
  }
  let lower = tok.to_ascii_lowercase();
  let code = match lower.as_str() {
    "enter" | "return" => KeyCode::Enter,
    "esc" | "escape" => KeyCode::Esc,
    "tab" => KeyCode::Tab,
    "backtab" | "shift_tab" => KeyCode::BackTab,
    "space" => KeyCode::Char(' '),
    "backspace" | "bs" => KeyCode::Backspace,
    "up" => KeyCode::Up,
    "down" => KeyCode::Down,
    "left" => KeyCode::Left,
    "right" => KeyCode::Right,
    "home" => KeyCode::Home,
    "end" => KeyCode::End,
    "pgup" | "pageup" | "page_up" => KeyCode::PageUp,
    "pgdn" | "pgdown" | "pagedown" | "page_down" => KeyCode::PageDown,
    "delete" | "del" => KeyCode::Delete,
    "insert" | "ins" => KeyCode::Insert,
    s if s.starts_with('f') && s.len() <= 3 => {
      let n: u8 = s[1..]
        .parse()
        .map_err(|_| format!("invalid function key '{tok}'"))?;
      if !(1..=12).contains(&n) {
        return Err(format!("function key out of range: '{tok}'"));
      }
      KeyCode::F(n)
    }
    _ => return Err(format!("unknown key '{tok}'")),
  };
  Ok((code, false))
}

fn format_key_label(key: &KeyCode, mods: KeyModifiers) -> String {
  let mut out = String::new();
  if mods.contains(KeyModifiers::CONTROL) {
    out.push_str("Ctrl+");
  }
  if mods.contains(KeyModifiers::ALT) {
    out.push_str("Alt+");
  }
  if mods.contains(KeyModifiers::SUPER) {
    out.push_str("Super+");
  }
  let show_shift = mods.contains(KeyModifiers::SHIFT)
    && !matches!(key, KeyCode::Char(c) if c.is_ascii_uppercase());
  if show_shift {
    out.push_str("Shift+");
  }
  match key {
    KeyCode::Char(' ') => out.push_str("Space"),
    KeyCode::Char(c) => out.push(*c),
    KeyCode::Enter => out.push_str("Enter"),
    KeyCode::Esc => out.push_str("Esc"),
    KeyCode::Tab => out.push_str("Tab"),
    KeyCode::BackTab => out.push_str("Shift+Tab"),
    KeyCode::Backspace => out.push_str("Backspace"),
    KeyCode::Up => out.push('↑'),
    KeyCode::Down => out.push('↓'),
    KeyCode::Left => out.push('←'),
    KeyCode::Right => out.push('→'),
    KeyCode::Home => out.push_str("Home"),
    KeyCode::End => out.push_str("End"),
    KeyCode::PageUp => out.push_str("PgUp"),
    KeyCode::PageDown => out.push_str("PgDn"),
    KeyCode::Delete => out.push_str("Del"),
    KeyCode::Insert => out.push_str("Ins"),
    KeyCode::F(n) => out.push_str(&format!("F{n}")),
    other => out.push_str(&format!("{other:?}")),
  }
  out
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
    assert!(labels.contains(&"u"));
    assert!(labels.contains(&"c"));
    assert!(labels.contains(&"p"));
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

  #[test]
  fn chat_input_enter_sends() {
    // Plain Enter — Ctrl+Enter relies on the kitty keyboard protocol
    // which most terminals don't implement, so we settle on the
    // universal binding (R0).
    assert_eq!(
      action_for(Focus::ChatInput, KeyCode::Enter, KeyModifiers::NONE),
      Some(Action::SendChat),
    );
  }

  #[test]
  fn chat_input_ctrl_r_toggles_think_collapse() {
    assert_eq!(
      action_for(Focus::ChatInput, KeyCode::Char('r'), KeyModifiers::CONTROL,),
      Some(Action::ToggleThinkCollapse),
    );
  }

  #[test]
  fn embed_input_enter_submits() {
    assert_eq!(
      action_for(Focus::EmbedInput, KeyCode::Enter, KeyModifiers::NONE),
      Some(Action::Submit),
    );
    // Shift+Enter has no binding so it falls through to the per-tab
    // handler (currently a no-op for embed — single-line input).
    assert_eq!(
      action_for(Focus::EmbedInput, KeyCode::Enter, KeyModifiers::SHIFT),
      None,
    );
  }

  #[test]
  fn rerank_input_enter_submits() {
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Enter, KeyModifiers::NONE),
      Some(Action::Submit),
    );
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Enter, KeyModifiers::SHIFT),
      None,
    );
  }

  #[test]
  fn rerank_input_tab_stages_candidate() {
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Tab, KeyModifiers::NONE),
      Some(Action::StageRerankCandidate),
    );
  }

  #[test]
  fn right_pane_s_toggles_auto_scroll() {
    assert_eq!(
      action_for(Focus::RightPane, KeyCode::Char('s'), KeyModifiers::NONE,),
      Some(Action::ToggleAutoScroll),
    );
  }

  // ── KeyMap (runtime overrides) ──────────────────────────────

  #[test]
  fn keymap_default_mirrors_static_bindings() {
    // The runtime default must accept exactly the same keys the
    // legacy free function accepts — that's the invariant that lets
    // us flip events.rs / help_overlay over without behavioural drift.
    let map = KeyMap::default();
    assert_eq!(
      map.action_for(Focus::List, KeyCode::Char('q'), KeyModifiers::NONE),
      Some(Action::Quit)
    );
    assert_eq!(
      map.action_for(Focus::List, KeyCode::Char('t'), KeyModifiers::NONE),
      Some(Action::CycleTheme)
    );
    assert_eq!(
      map.action_for(Focus::Filter, KeyCode::Char('q'), KeyModifiers::NONE),
      None
    );
  }

  #[test]
  fn parse_key_spec_handles_modifier_chains() {
    let ctrl_q = parse_key_spec("ctrl+q").unwrap();
    assert_eq!(ctrl_q.key, KeyCode::Char('q'));
    assert!(ctrl_q.mods.contains(KeyModifiers::CONTROL));
    assert_eq!(ctrl_q.label, "Ctrl+q");

    let shift_tab = parse_key_spec("Shift+Tab").unwrap();
    assert_eq!(shift_tab.key, KeyCode::Tab);
    assert!(shift_tab.mods.contains(KeyModifiers::SHIFT));

    let alt_enter = parse_key_spec("alt+enter").unwrap();
    assert_eq!(alt_enter.key, KeyCode::Enter);
    assert!(alt_enter.mods.contains(KeyModifiers::ALT));

    let f5 = parse_key_spec("f5").unwrap();
    assert_eq!(f5.key, KeyCode::F(5));
  }

  #[test]
  fn parse_key_spec_uppercase_implies_shift() {
    let spec = parse_key_spec("Q").unwrap();
    assert_eq!(spec.key, KeyCode::Char('Q'));
    assert!(spec.mods.contains(KeyModifiers::SHIFT));
  }

  #[test]
  fn parse_key_spec_rejects_unknown_modifier() {
    let err = parse_key_spec("hyper+q").unwrap_err();
    assert!(err.contains("unknown modifier"));
  }

  #[test]
  fn parse_key_spec_rejects_out_of_range_function_key() {
    let err = parse_key_spec("f99").unwrap_err();
    assert!(err.contains("function key"));
  }

  #[test]
  fn action_from_config_name_accepts_snake_and_kebab() {
    assert_eq!(Action::from_config_name("quit"), Some(Action::Quit));
    assert_eq!(
      Action::from_config_name("cycle_theme"),
      Some(Action::CycleTheme)
    );
    assert_eq!(
      Action::from_config_name("cycle-theme"),
      Some(Action::CycleTheme)
    );
    assert_eq!(Action::from_config_name("nope"), None);
  }

  #[test]
  fn apply_overrides_rebinds_quit_to_ctrl_q() {
    let mut map = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("quit"), String::from("ctrl+q"))]
      .into_iter()
      .collect();
    let warnings = map.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
    // New binding fires.
    assert_eq!(
      map.action_for(Focus::List, KeyCode::Char('q'), KeyModifiers::CONTROL),
      Some(Action::Quit)
    );
    // Old binding is gone.
    assert_eq!(
      map.action_for(Focus::List, KeyCode::Char('q'), KeyModifiers::NONE),
      None
    );
  }

  #[test]
  fn apply_overrides_warns_on_unknown_action() {
    let mut map = KeyMap::default();
    let overrides: BTreeMap<String, String> =
      [(String::from("nuke_everything"), String::from("ctrl+z"))]
        .into_iter()
        .collect();
    let warnings = map.apply_overrides(&overrides);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("nuke_everything"));
  }

  #[test]
  fn apply_overrides_warns_on_unparseable_key() {
    let mut map = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("quit"), String::from("ctrl+"))]
      .into_iter()
      .collect();
    let warnings = map.apply_overrides(&overrides);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("quit"));
    // Original binding still works since the override failed.
    assert_eq!(
      map.action_for(Focus::List, KeyCode::Char('q'), KeyModifiers::NONE),
      Some(Action::Quit)
    );
  }

  #[test]
  fn apply_overrides_drops_conflicting_existing_binding() {
    // `t` defaults to CycleTheme. If the user rebinds Quit → `t`,
    // CycleTheme must lose its `t` binding so the dispatch isn't
    // ambiguous.
    let mut map = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("quit"), String::from("t"))]
      .into_iter()
      .collect();
    let warnings = map.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
      map.action_for(Focus::List, KeyCode::Char('t'), KeyModifiers::NONE),
      Some(Action::Quit)
    );
    // CycleTheme should no longer be triggered by anything (its
    // original `t` binding was claimed by Quit; no fallback exists).
    let all_quit_keys_in_list: Vec<_> = map
      .bindings_for(Focus::List)
      .iter()
      .filter(|b| b.action == Action::CycleTheme)
      .collect();
    assert!(
      all_quit_keys_in_list.is_empty(),
      "expected CycleTheme to lose its binding"
    );
  }
}
