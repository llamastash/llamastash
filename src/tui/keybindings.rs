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
  /// Modal "are you sure?" popup (stop model / kill daemon). Only
  /// Submit / Cancel are bindable; the hardcoded `y` / `n`
  /// char-matches in [`super::events`] remain as the foot-gun
  /// resistant fallback so a stray keypress doesn't confirm a
  /// destructive action.
  ConfirmPopup,
  /// HuggingFace pull dialog (R104). The per-stage key router lives
  /// in `events.rs` because the dialog's `Search` / `FilePicker` /
  /// `Confirm` stages each shadow a subset of the global keymap
  /// (typing extends the query buffer; arrows move row cursors;
  /// `o` cycles sort; `n`/`p` paginate).
  HfDialog,
}

/// Symbolic action a binding triggers. Renderers / event handlers
/// match on this rather than the raw key so config overrides only
/// touch the table, not the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
  Quit,
  /// Open the HuggingFace pull dialog (R104). Bound to `Shift+D` in
  /// [`Focus::List`] — mirrors the other Shift-letter quick-jumps
  /// (`M / L / C / R / E / S`). The dialog itself handles its
  /// per-stage keys.
  OpenHfDialog,
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
  /// right pane (closes the pane) and in the AdvancedPanel
  /// overlay. The LaunchPicker is no longer a modal — Settings
  /// renders the form inline (round-6).
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
  /// to `Ctrl+Q` in the model list focus. (Pre-2026-05 this lived on
  /// `Shift+Q`; remapped so an accidental capital `Q` while typing
  /// doesn't surface the kill prompt.)
  KillDaemon,
  /// Restart the daemon (after a confirmation popup): shut the
  /// current daemon down and re-spawn a fresh one with the same
  /// options. Bound to `Ctrl+R` in the model list focus so the
  /// `Shift+R` mnemonic stays free for the Rerank tab alias.
  RestartDaemon,
  /// Delete the focused model from disk after confirmation. Bound
  /// to `Ctrl+D` in [`Focus::List`]; refuses (with a toast) when
  /// the focused row is a running managed launch.
  DeleteModel,
  /// Cancel the currently-active HF download after confirmation.
  /// Bound to `Ctrl+X` everywhere it makes sense (List + RightPane)
  /// so the download strip's chip is reachable from either side of
  /// the dashboard. Queued pulls behind the active one stay in the
  /// queue — a second `Ctrl+X` cancels whichever pull was promoted
  /// next. No-op (toast) when no pull is active.
  CancelDownload,
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
  /// Insert a literal newline into the active text-input buffer
  /// (Chat / Embed / Rerank). Bound to `Shift+Enter` so plain
  /// `Enter` keeps its submit semantics. Only fires on terminals
  /// that implement the kitty keyboard protocol — elsewhere
  /// Shift+Enter collapses to plain Enter and triggers Submit.
  InsertNewline,
  /// Cycle to the next input field within the focused pane. Bound
  /// to `↓` in the Right pane (cycles the Settings-tab launch
  /// form) and in the Rerank input (toggles query ↔ candidate).
  /// `Tab` belongs to pane-cycle now, so field-cycle migrates to
  /// the arrow keys, matching system-form conventions (Windows /
  /// macOS settings dialogs, HTML `<form>`).
  NextField,
  /// Cycle to the previous input field within the focused pane.
  /// Bound to `↑` in the Right pane and the Rerank input.
  PrevField,
  /// Cycle the focused field's value forward. Bound to `→` in the
  /// Right pane; dispatches only when the active right tab is
  /// Settings (Ctx preset, Reasoning toggle).
  CycleValueNext,
  /// Cycle the focused field's value backward. Bound to `←` in the
  /// Right pane; dispatches only when the active right tab is
  /// Settings.
  CycleValuePrev,
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
  (Focus::AdvancedPanel, ADVANCED_BINDINGS),
  (Focus::RightPane, RIGHT_PANE_BINDINGS),
  (Focus::ChatInput, CHAT_INPUT_BINDINGS),
  (Focus::EmbedInput, EMBED_INPUT_BINDINGS),
  (Focus::RerankInput, RERANK_INPUT_BINDINGS),
  (Focus::ConfirmPopup, CONFIRM_POPUP_BINDINGS),
  (Focus::HfDialog, HF_DIALOG_BINDINGS),
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
    key: KeyCode::Char('r'),
    mods: KeyModifiers::CONTROL,
    action: Action::RestartDaemon,
    label: "Ctrl+R",
    description: "restart daemon",
  },
  Binding {
    key: KeyCode::Char('q'),
    mods: KeyModifiers::CONTROL,
    action: Action::KillDaemon,
    label: "Ctrl+Q",
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
    key: KeyCode::Home,
    mods: KeyModifiers::NONE,
    action: Action::GoTop,
    label: "Home",
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
    key: KeyCode::End,
    mods: KeyModifiers::NONE,
    action: Action::GoBottom,
    label: "End",
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
  // Tab/⇧+Tab cycle panes (Models → Logs → Chat/Embed/Rerank →
  // Settings → wrap). ↑/↓ scroll the list cursor. The `→` chord is
  // deliberately unbound on the Models list: arrow-right reads as
  // "cycle value" everywhere else (Settings tab) and surfaced as
  // an asymmetric pane-jump it confused users into pressing it for
  // every navigation. Pane cycle is reachable via Tab/Shift+Tab or
  // the vi `h`/`l` aliases (still bound below); Enter launches the
  // focused model into the right pane.
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
    label: "⇧+Tab",
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
    // `apply_focus_chat_tab` (events.rs) jumps to whichever mode tab
    // is live for the focused model — Chat for chat models, Embed
    // for embedding-only models, Rerank for rerankers. The
    // description has to mirror that so the help pane doesn't lie.
    description: "chat/embed/rerank",
  },
  // R / E mirror C: a model only ever exposes one of
  // Chat/Embed/Rerank at a time, so all three keys map to the same
  // "jump to mode tab" action. Lets a user with muscle memory for
  // "press R for rerank" or "press E for embed" land on the right
  // tab without thinking. The daemon-restart hotkey lives on Ctrl+R
  // so this mnemonic stays free.
  Binding {
    key: KeyCode::Char('E'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusChatTab,
    label: "E",
    description: "chat/embed/rerank",
  },
  Binding {
    key: KeyCode::Char('R'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusChatTab,
    label: "R",
    description: "chat/embed/rerank",
  },
  Binding {
    key: KeyCode::Char('S'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusSettingsTab,
    label: "S",
    description: "settings",
  },
  // R104: `d` opens the HuggingFace pull dialog. Lowercase so the
  // dialog is reachable with a single un-modified keystroke; Ctrl+D
  // is reserved for delete-model on the focused row.
  Binding {
    key: KeyCode::Char('d'),
    mods: KeyModifiers::NONE,
    action: Action::OpenHfDialog,
    label: "d",
    description: "pull from HF",
  },
  // Ctrl+D deletes the focused model after a confirmation popup.
  // Refusal-on-running is handled inside `apply_delete_model` so the
  // binding stays uniform; the on-screen hint chip only renders for
  // non-running rows (see `list_pane` hints).
  Binding {
    key: KeyCode::Char('d'),
    mods: KeyModifiers::CONTROL,
    action: Action::DeleteModel,
    label: "Ctrl+D",
    description: "delete model",
  },
  // Ctrl+X cancels the currently-active HF download after a confirm
  // popup. Queued downloads keep their place; the user presses Ctrl+X
  // again on the next promoted pull. Refusal-on-idle is handled in
  // `apply_cancel_download` so the binding is uniform and the chip
  // strip on the download line is the discoverability surface.
  Binding {
    key: KeyCode::Char('x'),
    mods: KeyModifiers::CONTROL,
    action: Action::CancelDownload,
    label: "Ctrl+X",
    description: "cancel download",
  },
];

/// HF pull dialog (R104). The per-stage routing (Search / FilePicker
/// / Confirm) lives in `events::handle_hf_dialog_input` because the
/// dialog has stage-specific semantics that the static binding table
/// can't capture (typing extends the query buffer; `n`/`p` paginate
/// only in Search; Backspace toggles between sub-stages). What
/// *does* belong here are the always-on keys the help bar needs to
/// surface and the help-overlay needs to enumerate.
const HF_DIALOG_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::Cancel,
    label: "Esc",
    description: "close",
  },
  Binding {
    key: KeyCode::Up,
    mods: KeyModifiers::NONE,
    action: Action::MoveUp,
    label: "↑",
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
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "open / confirm",
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
  // Filter is a live predicate (applies on every keystroke), so
  // Enter has no apply semantics — it drills into the focused
  // result row by opening the launch picker. See
  // `handle_filter_input`'s `Submit` arm.
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "launch",
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
    label: "⇧+Tab",
    description: "prev pane",
  },
  // ←/→ change the focused Settings field's value. Outside the
  // Settings tab the action is a no-op (see `apply_cycle_value`),
  // so the keys don't double as pane navigation anywhere.
  Binding {
    key: KeyCode::Right,
    mods: KeyModifiers::NONE,
    action: Action::CycleValueNext,
    label: "→",
    description: "cycle value",
  },
  Binding {
    key: KeyCode::Left,
    mods: KeyModifiers::NONE,
    action: Action::CycleValuePrev,
    label: "←",
    description: "cycle value",
  },
  // vi aliases stay — h/l remain the canonical pane-cycle pair for
  // users who don't want to leave the home row.
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
  // `s` carries two meanings, dispatched tab-aware in
  // `apply_action`: on the Logs tab it toggles auto-scroll; on the
  // Settings tab it stops the focused launch. The binding row sits
  // under `ToggleAutoScroll` so the dispatcher reads a single
  // action; the per-tab semantics live in the handler.
  Binding {
    key: KeyCode::Char('s'),
    mods: KeyModifiers::NONE,
    action: Action::ToggleAutoScroll,
    label: "s",
    description: "auto-scroll",
  },
  // Round-8: yank affordances reachable from the right pane so the
  // Settings tab can surface `p / u / c` without forcing the user
  // back to the Models list. Yank handlers already check
  // `focused_managed()` and emit toasts on misses, so the bindings
  // stay safe on Logs / Settings without a managed launch.
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
  // Arrows are the canonical surface — the Settings tab uses ↑/↓
  // for field-cycle and the Logs tab uses them for scroll. Vi
  // aliases `j`/`k` follow so home-row users have a fallback.
  // Ordering matters: `hint()` returns the first binding it finds,
  // so the chip strip surfaces the arrow glyphs.
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
    // Mirrors the LIST_BINDINGS entry — `apply_focus_chat_tab` picks
    // the first available of Chat/Embed/Rerank. Description must
    // stay in sync across the two binding tables so the help
    // overlay's `resolve_one` lifts a consistent string.
    description: "chat/embed/rerank",
  },
  // R / E aliases for C — mirrors LIST_BINDINGS so the user can
  // press the mnemonic for Rerank / Embed from the right pane too.
  // Daemon restart is on Ctrl+R (handled in LIST_BINDINGS).
  Binding {
    key: KeyCode::Char('R'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusChatTab,
    label: "R",
    description: "chat/embed/rerank",
  },
  Binding {
    key: KeyCode::Char('r'),
    mods: KeyModifiers::CONTROL,
    action: Action::RestartDaemon,
    label: "Ctrl+R",
    description: "restart daemon",
  },
  Binding {
    key: KeyCode::Char('E'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusChatTab,
    label: "E",
    description: "chat/embed/rerank",
  },
  Binding {
    key: KeyCode::Char('S'),
    mods: KeyModifiers::SHIFT,
    action: Action::FocusSettingsTab,
    label: "S",
    description: "settings",
  },
  // Mirror LIST_BINDINGS so Ctrl+X cancels the active download from
  // either pane. `apply_cancel_download` gates idle / non-active
  // states so the binding stays uniform across focuses.
  Binding {
    key: KeyCode::Char('x'),
    mods: KeyModifiers::CONTROL,
    action: Action::CancelDownload,
    label: "Ctrl+X",
    description: "cancel download",
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
    label: "⇧+Tab",
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
    key: KeyCode::Enter,
    mods: KeyModifiers::SHIFT,
    action: Action::InsertNewline,
    label: "⇧+Enter",
    description: "newline",
  },
  Binding {
    key: KeyCode::Char('r'),
    mods: KeyModifiers::CONTROL,
    action: Action::ToggleThinkCollapse,
    label: "Ctrl+r",
    description: "toggle reasoning",
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
    label: "⇧+Tab",
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
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::SHIFT,
    action: Action::InsertNewline,
    label: "⇧+Enter",
    description: "newline",
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
  // Tab / Shift+Tab cycle panes everywhere — including inside the
  // rerank input. Staging a candidate now rides on Enter while the
  // candidate field is focused (see `apply_rerank_submit`), so the
  // dedicated `+` / `=` chords retire.
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
    label: "⇧+Tab",
    description: "prev pane",
  },
  // ↑/↓ cycle the input field (query ↔ candidate). Two fields →
  // both directions land on the same place, but binding both keeps
  // the arrow surface symmetric with the Settings tab's field
  // cycle.
  Binding {
    key: KeyCode::Down,
    mods: KeyModifiers::NONE,
    action: Action::NextField,
    label: "↓",
    description: "next field",
  },
  Binding {
    key: KeyCode::Up,
    mods: KeyModifiers::NONE,
    action: Action::PrevField,
    label: "↑",
    description: "prev field",
  },
  // Plain Enter is dual-duty on the Rerank tab:
  //   - in the Query field   → dispatch `/v1/rerank`
  //   - in the Candidate field → stage the buffer onto the list
  // The dispatcher (`apply_rerank_submit`) branches on the
  // focused field so a single muscle-memory chord drives both
  // tasks without a dedicated `+:stage` chord.
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "query/add candidate",
  },
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::SHIFT,
    action: Action::InsertNewline,
    label: "⇧+Enter",
    description: "newline",
  },
];

const CONFIRM_POPUP_BINDINGS: &[Binding] = &[
  Binding {
    key: KeyCode::Enter,
    mods: KeyModifiers::NONE,
    action: Action::Submit,
    label: "Enter",
    description: "confirm",
  },
  Binding {
    key: KeyCode::Esc,
    mods: KeyModifiers::NONE,
    action: Action::Cancel,
    label: "Esc",
    description: "cancel",
  },
];

/// Bindings the help bar should show in the supplied focus.
/// Looks at the compile-time defaults — runtime keymap overrides
/// flow through [`KeyMap::bindings_for`] instead.
#[cfg(test)]
fn default_bindings_for(focus: Focus) -> &'static [Binding] {
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
    ("restart_daemon", Action::RestartDaemon),
    ("delete_model", Action::DeleteModel),
    ("cancel_download", Action::CancelDownload),
    ("focus_logs_tab", Action::FocusLogsTab),
    ("focus_chat_tab", Action::FocusChatTab),
    ("focus_settings_tab", Action::FocusSettingsTab),
    ("insert_newline", Action::InsertNewline),
    ("next_field", Action::NextField),
    ("prev_field", Action::PrevField),
    ("cycle_value_next", Action::CycleValueNext),
    ("cycle_value_prev", Action::CycleValuePrev),
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

/// Unicode glyph rendered in place of the `Shift+` modifier text.
/// `⇧` (U+21E7) is the standard "Shift" symbol used by macOS
/// keyboard-shortcut docs and is present in every monospace font
/// shipped with terminals — no Nerd Font required. Centralised so
/// renderers, tests, and binding tables can all reference the same
/// character. The trailing `+` joiner mirrors `Ctrl+` / `Alt+` so
/// the modifier+key relationship stays visually consistent.
pub const SHIFT_GLYPH: &str = "⇧";

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
  // Suppress an explicit Shift prefix when the key already encodes
  // Shift in its name or glyph:
  //  - Uppercase `Char` (terminal emits Shift+letter as `Char(C)`)
  //  - `BackTab` (the named code for Shift+Tab — adding the prefix
  //    again would render as `⇧+⇧+Tab`, doubly emphasising Shift).
  let key_already_encodes_shift =
    matches!(key, KeyCode::Char(c) if c.is_ascii_uppercase()) || matches!(key, KeyCode::BackTab);
  let show_shift = mods.contains(KeyModifiers::SHIFT) && !key_already_encodes_shift;
  if show_shift {
    out.push_str(SHIFT_GLYPH);
    out.push('+');
  }
  match key {
    KeyCode::Char(' ') => out.push_str("Space"),
    KeyCode::Char(c) => out.push(*c),
    KeyCode::Enter => out.push_str("Enter"),
    KeyCode::Esc => out.push_str("Esc"),
    KeyCode::Tab => out.push_str("Tab"),
    KeyCode::BackTab => {
      out.push_str(SHIFT_GLYPH);
      out.push_str("+Tab");
    }
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

  /// Helper: resolve `(focus, key, mods)` against the compile-time
  /// defaults — what `KeyMap::default()` produces at startup. Used by
  /// the binding-shape tests below; production reads through
  /// `App::action_for` which routes via the active `KeyMap`.
  fn action_for(focus: Focus, key: KeyCode, mods: KeyModifiers) -> Option<Action> {
    default_bindings_for(focus)
      .iter()
      .find(|b| b.key == key && b.mods == mods)
      .map(|b| b.action)
  }

  /// Helper: default bindings slice for a focus.
  fn bindings_for(focus: Focus) -> &'static [Binding] {
    default_bindings_for(focus)
  }

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
  fn lowercase_d_in_list_focus_opens_hf_dialog() {
    // Round-13: HF pull moved from Shift+D to lowercase `d` so it's
    // reachable with a single unmodified keystroke. Ctrl+D is now
    // bound to delete-model (see below).
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('d'), KeyModifiers::NONE),
      Some(Action::OpenHfDialog),
    );
  }

  #[test]
  fn d_is_not_shadowed_in_any_sub_focus() {
    // The dialog steals input via `Focus::HfDialog`; the binding
    // must not also fire under another focus (e.g. RightPane or
    // the input modes) where it would collide with an unrelated
    // action.
    use crate::tui::keybindings::Focus::*;
    for focus in [
      Filter,
      AdvancedPanel,
      RightPane,
      ChatInput,
      EmbedInput,
      RerankInput,
      ConfirmPopup,
      HfDialog,
    ] {
      let action = action_for(focus, KeyCode::Char('d'), KeyModifiers::NONE);
      assert!(
        action.is_none() || action == Some(Action::OpenHfDialog),
        "`d` collision under {focus:?}: {action:?}"
      );
    }
  }

  #[test]
  fn shift_d_no_longer_opens_hf_dialog() {
    // Round-13 retired Shift+D in favour of lowercase `d`. Pin the
    // removal so a future refactor doesn't accidentally re-add it.
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('D'), KeyModifiers::SHIFT),
      None,
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
  fn list_focus_home_and_end_alias_go_top_and_go_bottom() {
    // Home / End mirror the vi-style `g` / `G` bindings so users who
    // reach for nav keys instead of vi motions land in the same
    // place. Both pairs must resolve to the same action so the
    // dispatcher stays single-pathed.
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('g'), KeyModifiers::NONE),
      Some(Action::GoTop)
    );
    assert_eq!(
      action_for(Focus::List, KeyCode::Home, KeyModifiers::NONE),
      Some(Action::GoTop)
    );
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('G'), KeyModifiers::SHIFT),
      Some(Action::GoBottom)
    );
    assert_eq!(
      action_for(Focus::List, KeyCode::End, KeyModifiers::NONE),
      Some(Action::GoBottom)
    );
  }

  #[test]
  fn list_right_arrow_enters_right_pane_but_left_is_unbound() {
    // 2026-05-21: the `→` shortcut from the Models list was removed
    // — it read as "cycle value" everywhere else (Settings tab) and
    // the asymmetric pane-jump confused users. Pane cycle is now
    // reachable via Tab / Shift+Tab / `h` / `l` only. Left was
    // already unbound; Right joins it.
    assert_eq!(
      action_for(Focus::List, KeyCode::Right, KeyModifiers::NONE),
      None,
      "Right arrow must NOT open the right pane (removed)"
    );
    assert_eq!(
      action_for(Focus::List, KeyCode::Left, KeyModifiers::NONE),
      None,
      "Left arrow stays unbound in Models"
    );
    assert_eq!(
      action_for(Focus::List, KeyCode::Tab, KeyModifiers::NONE),
      Some(Action::NextFocus),
      "Tab remains the canonical pane-cycle chord"
    );
  }

  #[test]
  fn right_pane_focus_can_open_advanced_from_settings_tab() {
    // The Settings tab in the right pane hosts the launch form;
    // `a` opens the Advanced flags editor without leaving Settings.
    // (Previously this binding lived on the dead `Focus::LaunchPicker`
    // modal — the assertion moved with the binding.)
    assert_eq!(
      action_for(Focus::RightPane, KeyCode::Char('a'), KeyModifiers::NONE),
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
    // Shift+Enter inserts a newline (only distinguishable on kitty-
    // protocol terminals; elsewhere it collapses to plain Enter).
    assert_eq!(
      action_for(Focus::EmbedInput, KeyCode::Enter, KeyModifiers::SHIFT),
      Some(Action::InsertNewline),
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
      Some(Action::InsertNewline),
    );
  }

  #[test]
  fn rerank_input_plus_and_equals_no_longer_bound_to_stage() {
    // Round-9 dropped the dedicated `+` / `=` stage chords. Enter
    // in the candidate field now stages the buffer (dispatched by
    // `apply_rerank_submit` based on focused field). The action is
    // kept on the dispatch table so a user keymap override can
    // restore an explicit stage chord if they want.
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Char('+'), KeyModifiers::SHIFT),
      None,
    );
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Char('='), KeyModifiers::NONE),
      None,
    );
  }

  #[test]
  fn rerank_input_tab_cycles_panes_not_stages_candidate() {
    // The pre-round-7 Tab → StageRerankCandidate binding moved to
    // `+` / `=`. Tab must now flow the universal pane-cycle path.
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Tab, KeyModifiers::NONE),
      Some(Action::NextFocus),
    );
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::BackTab, KeyModifiers::SHIFT),
      Some(Action::PrevFocus),
    );
  }

  #[test]
  fn rerank_input_up_down_cycle_fields() {
    // ↑/↓ replace Shift+Tab as the field-cycle surface inside the
    // rerank input — symmetric with the Settings tab.
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Down, KeyModifiers::NONE),
      Some(Action::NextField),
    );
    assert_eq!(
      action_for(Focus::RerankInput, KeyCode::Up, KeyModifiers::NONE),
      Some(Action::PrevField),
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
    assert_eq!(
      shift_tab.label, "⇧+Tab",
      "Shift modifier renders as the ⇧ Unicode glyph"
    );

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

  // ── format_key_label rendering ────────────────────────────────
  //
  // Every hint chip, help row, and title pulls its key text from
  // `format_key_label` (or the per-binding `label` literal that
  // mirrors it). Pin the rendered string for every match arm so a
  // refactor of the formatter fails loudly instead of silently
  // drifting through every surface.

  #[test]
  fn format_key_label_plain_char_renders_as_char() {
    assert_eq!(
      format_key_label(&KeyCode::Char('q'), KeyModifiers::NONE),
      "q"
    );
  }

  #[test]
  fn format_key_label_uppercase_char_omits_shift_prefix() {
    // crossterm reports Shift+letter as a `Char('Q')` plus
    // SHIFT modifier — surfacing `Shift+Q` would be redundant.
    assert_eq!(
      format_key_label(&KeyCode::Char('Q'), KeyModifiers::SHIFT),
      "Q"
    );
  }

  #[test]
  fn format_key_label_lowercase_char_with_shift_keeps_prefix() {
    // Shift on a lowercase char is unusual (the terminal would
    // normally upcase it) but possible with kitty-protocol — keep
    // the modifier visible so the binding stays unambiguous. The
    // visible prefix is the Unicode glyph ⇧ (U+21E7), not the word
    // "Shift".
    assert_eq!(
      format_key_label(&KeyCode::Char('q'), KeyModifiers::SHIFT),
      "⇧+q"
    );
  }

  #[test]
  fn format_key_label_renders_shift_as_nerd_font_glyph() {
    // Regression guard: every Shift-aware label must surface the
    // glyph form. The legacy `Shift+` text must never reappear.
    let shift_tab = format_key_label(&KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(shift_tab, "⇧+Tab");
    assert!(
      !shift_tab.contains("Shift"),
      "Shift+ text must not appear in any key label: {shift_tab}"
    );
    let shift_enter = format_key_label(&KeyCode::Enter, KeyModifiers::SHIFT);
    assert_eq!(shift_enter, "⇧+Enter");
    assert!(
      !shift_enter.contains("Shift"),
      "Shift+ text must not appear in any key label: {shift_enter}"
    );
  }

  #[test]
  fn format_key_label_ctrl_char_renders_with_ctrl_prefix() {
    assert_eq!(
      format_key_label(&KeyCode::Char('c'), KeyModifiers::CONTROL),
      "Ctrl+c"
    );
  }

  #[test]
  fn format_key_label_ctrl_shift_lowercase_emits_both_prefixes() {
    let mods = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    assert_eq!(format_key_label(&KeyCode::Char('c'), mods), "Ctrl+⇧+c");
  }

  #[test]
  fn format_key_label_alt_char_renders_with_alt_prefix() {
    assert_eq!(
      format_key_label(&KeyCode::Char('x'), KeyModifiers::ALT),
      "Alt+x"
    );
  }

  #[test]
  fn format_key_label_space_renders_as_word() {
    assert_eq!(
      format_key_label(&KeyCode::Char(' '), KeyModifiers::NONE),
      "Space"
    );
  }

  #[test]
  fn format_key_label_named_keys_render_with_their_word() {
    assert_eq!(
      format_key_label(&KeyCode::Enter, KeyModifiers::NONE),
      "Enter"
    );
    assert_eq!(format_key_label(&KeyCode::Esc, KeyModifiers::NONE), "Esc");
    assert_eq!(format_key_label(&KeyCode::Tab, KeyModifiers::NONE), "Tab");
    assert_eq!(
      format_key_label(&KeyCode::BackTab, KeyModifiers::SHIFT),
      "⇧+Tab"
    );
    assert_eq!(
      format_key_label(&KeyCode::Backspace, KeyModifiers::NONE),
      "Backspace"
    );
    assert_eq!(
      format_key_label(&KeyCode::Delete, KeyModifiers::NONE),
      "Del"
    );
    assert_eq!(
      format_key_label(&KeyCode::Insert, KeyModifiers::NONE),
      "Ins"
    );
  }

  #[test]
  fn format_key_label_arrows_render_as_glyphs() {
    assert_eq!(format_key_label(&KeyCode::Up, KeyModifiers::NONE), "↑");
    assert_eq!(format_key_label(&KeyCode::Down, KeyModifiers::NONE), "↓");
    assert_eq!(format_key_label(&KeyCode::Left, KeyModifiers::NONE), "←");
    assert_eq!(format_key_label(&KeyCode::Right, KeyModifiers::NONE), "→");
  }

  #[test]
  fn format_key_label_home_end_pageup_pagedown() {
    assert_eq!(format_key_label(&KeyCode::Home, KeyModifiers::NONE), "Home");
    assert_eq!(format_key_label(&KeyCode::End, KeyModifiers::NONE), "End");
    assert_eq!(
      format_key_label(&KeyCode::PageUp, KeyModifiers::NONE),
      "PgUp"
    );
    assert_eq!(
      format_key_label(&KeyCode::PageDown, KeyModifiers::NONE),
      "PgDn"
    );
  }

  #[test]
  fn format_key_label_function_keys_render_with_number() {
    assert_eq!(format_key_label(&KeyCode::F(1), KeyModifiers::NONE), "F1");
    assert_eq!(format_key_label(&KeyCode::F(7), KeyModifiers::NONE), "F7");
    assert_eq!(format_key_label(&KeyCode::F(12), KeyModifiers::NONE), "F12");
  }

  #[test]
  fn format_key_label_combines_ctrl_alt_super_modifiers_in_order() {
    // Order is fixed (Ctrl → Alt → Super → Shift) so the rendered
    // string is canonical — easier to grep for in help dumps.
    let mods = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
    assert_eq!(format_key_label(&KeyCode::F(5), mods), "Ctrl+Alt+Super+F5");
  }

  #[test]
  fn format_key_label_combines_all_modifiers_with_shift_glyph_last() {
    // Full chain — Shift sits in the Nerd Font glyph slot last.
    let mods =
      KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::SHIFT;
    assert_eq!(
      format_key_label(&KeyCode::F(5), mods),
      "Ctrl+Alt+Super+⇧+F5"
    );
  }
}
