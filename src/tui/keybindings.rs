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

// ─── Compile-time label builders ────────────────────────────────
//
// `ctrl_label!("k")` expands to `"⌃k"` on macOS and `"Ctrl+k"` on
// Linux / Windows. Same shape for `alt_label!` and `super_label!`.
// `concat!` is const-evaluable so the binding table pays no runtime
// cost; any new chord in [`DEFAULT_BINDINGS`] just uses the macro
// inline, no per-letter `const` needed.

#[macro_export]
macro_rules! ctrl_label {
  ($k:literal) => {{
    #[cfg(target_os = "macos")]
    {
      concat!("⌃", $k)
    }
    #[cfg(not(target_os = "macos"))]
    {
      concat!("Ctrl+", $k)
    }
  }};
}

#[macro_export]
macro_rules! alt_label {
  ($k:literal) => {{
    #[cfg(target_os = "macos")]
    {
      concat!("⌥", $k)
    }
    #[cfg(not(target_os = "macos"))]
    {
      concat!("Alt+", $k)
    }
  }};
}

#[macro_export]
macro_rules! super_label {
  ($k:literal) => {{
    #[cfg(target_os = "macos")]
    {
      concat!("⌘", $k)
    }
    #[cfg(not(target_os = "macos"))]
    {
      concat!("Super+", $k)
    }
  }};
}

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

/// Bitfield of [`Focus`] values, used to express which focuses a
/// [`Binding`] is active in. Eight focuses fit in a `u8`; the
/// representation is hand-rolled so we don't depend on the
/// `bitflags` crate for a 30-line abstraction.
///
/// One row per `Action` in [`DEFAULT_BINDINGS`] is paired with a
/// `FocusSet` that lists every focus the binding fires in — kdash-
/// style flat table instead of per-focus duplication. Multi-focus
/// reach (e.g. `Tab` cycles panes in every TUI focus) becomes one
/// row tagged with [`FocusSet::TUI`] rather than five copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusSet(u8);

impl FocusSet {
  pub const EMPTY: Self = Self(0);
  pub const LIST: Self = Self(1 << 0);
  pub const FILTER: Self = Self(1 << 1);
  pub const RIGHT_PANE: Self = Self(1 << 2);
  pub const CHAT_INPUT: Self = Self(1 << 3);
  pub const EMBED_INPUT: Self = Self(1 << 4);
  pub const RERANK_INPUT: Self = Self(1 << 5);
  pub const CONFIRM_POPUP: Self = Self(1 << 6);
  pub const HF_DIALOG: Self = Self(1 << 7);

  /// Models list + right pane (non-input). The "browsing" surfaces.
  pub const NAV: Self = Self(Self::LIST.0 | Self::RIGHT_PANE.0);
  /// All three text-input focuses (Chat / Embed / Rerank).
  pub const INPUT: Self = Self(Self::CHAT_INPUT.0 | Self::EMBED_INPUT.0 | Self::RERANK_INPUT.0);
  /// Every primary TUI focus — navigation + text inputs. Excludes
  /// the dedicated overlay focuses (`ConfirmPopup`, `HfDialog`,
  /// `Filter`) which capture input differently.
  pub const TUI: Self = Self(Self::NAV.0 | Self::INPUT.0);

  pub const fn contains(self, focus: Focus) -> bool {
    self.0 & focus.as_bit().0 != 0
  }

  pub const fn union(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }

  pub const fn bits(self) -> u8 {
    self.0
  }
}

impl std::ops::BitOr for FocusSet {
  type Output = Self;
  fn bitor(self, other: Self) -> Self {
    self.union(other)
  }
}

impl Focus {
  /// Single-bit `FocusSet` for this focus.
  pub const fn as_bit(self) -> FocusSet {
    match self {
      Focus::List => FocusSet::LIST,
      Focus::Filter => FocusSet::FILTER,
      Focus::RightPane => FocusSet::RIGHT_PANE,
      Focus::ChatInput => FocusSet::CHAT_INPUT,
      Focus::EmbedInput => FocusSet::EMBED_INPUT,
      Focus::RerankInput => FocusSet::RERANK_INPUT,
      Focus::ConfirmPopup => FocusSet::CONFIRM_POPUP,
      Focus::HfDialog => FocusSet::HF_DIALOG,
    }
  }
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
  Submit,
  Cancel,
  YankUrl,
  YankCurl,
  YankPath,
  CycleTheme,
  /// Cycle to the previous theme — overshoot recovery for the
  /// forward `t:theme` chord. Bound to `Shift+T`.
  CycleThemePrev,
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
  /// `Ctrl+S` in the navigation focuses.
  StopModel,
  /// HF pull dialog: cycle the sort order (Recent / Downloads /
  /// Trending). Dispatched directly by the dialog's per-stage key
  /// handler; this Action variant exists purely so the help overlay
  /// can surface the chord.
  HfCycleSort,
  /// HF pull dialog: jump to the next page of search results.
  HfNextPage,
  /// HF pull dialog: jump to the previous page of search results.
  HfPrevPage,
  /// Enter edit / text-capture mode on the active right-pane tab
  /// (Chat / Embed / Rerank). Bound to `e` in [`Focus::RightPane`].
  EnterEdit,
  /// Step back from a text-input focus to the right pane's
  /// navigation focus. Bound to `Esc` in each input focus.
  ExitEdit,
  /// Kill the daemon entirely (after a confirmation popup). Bound
  /// to `Ctrl+K` in the model list focus.
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

/// Editorial grouping for the help overlay. Independent of
/// `FocusSet` (which controls dispatch). A single [`Binding`] can
/// belong to multiple categories — e.g. `c YankCurl` surfaces under
/// `Models`, `Logs`, and `Settings`.
///
/// `Global` absorbs pane navigation, modal verbs, and shared text-
/// input affordances (Shift+Enter, Esc). Per-tab categories list
/// only the chords genuinely unique to that tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
  /// Always-on chords — quit, help, theme, daemon control, HF pull,
  /// motion, pane cycling, Shift-letter quick-jumps, Esc, edit-mode,
  /// Shift+Enter newline.
  Global,
  /// Models list — filter, favorite, launch, page-jumps.
  Models,
  /// Logs tab affordances.
  Logs,
  /// Settings tab affordances.
  Settings,
  /// Combined Chat / Embed / Rerank input-tab affordances. All three
  /// tabs accept the same shape of input (Enter:submit, Ctrl+R toggles
  /// reasoning blocks) so we render them under one heading instead of
  /// three near-empty sections.
  InputTabs,
  /// HuggingFace pull dialog modal.
  HfDialog,
}

impl Category {
  /// Display order in the help overlay. Renderer walks this list and
  /// emits one section per category that has bindings.
  pub const ALL: &'static [Category] = &[
    Category::Global,
    Category::Models,
    Category::Logs,
    Category::Settings,
    Category::InputTabs,
    Category::HfDialog,
  ];

  /// Section title shown in the help overlay.
  pub fn label(self) -> &'static str {
    match self {
      Category::Global => "General",
      Category::Models => "Models list",
      Category::Logs => "Logs tab",
      Category::Settings => "Settings tab",
      Category::InputTabs => "Chat/Embed/Rerank",
      Category::HfDialog => "HF pull dialog",
    }
  }
}

/// One binding in the flat keymap. `scopes` lists every focus the
/// binding fires in (dispatcher); `categories` lists every help-
/// overlay section it surfaces under. A single row replaces what
/// used to be a copy per focus.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
  pub key: KeyCode,
  pub mods: KeyModifiers,
  pub action: Action,
  /// Chord glyph rendered in chips and help (e.g. `↑` or `Ctrl+D`).
  pub label: &'static str,
  /// Short UI-chip text (top bar, panel-border hints). Required.
  /// Per-focus overrides via [`Action::hint_for`].
  pub hint: &'static str,
  /// Longer help-overlay text. `None` falls back to `hint`. Per-
  /// category overrides via [`Action::description_for`].
  pub description: Option<&'static str>,
  /// Focuses this binding is active in. The dispatcher filters by
  /// this; the help bar reads only the bindings whose scope contains
  /// the current focus.
  pub scopes: FocusSet,
  /// Help-overlay sections this row appears in. Empty = hidden from
  /// the overlay (still dispatches). Multi-element for actions that
  /// editorially belong to several panes.
  pub categories: &'static [Category],
}

impl Binding {
  /// Help-overlay description (falls back to `hint` when none set).
  pub fn description(&self) -> &'static str {
    self.description.unwrap_or(self.hint)
  }
}

// Shared category slices — keeps the binding table dense without
// inline `&[Category::X]` literals at every row.
const CAT_GLOBAL: &[Category] = &[Category::Global];
const CAT_MODELS: &[Category] = &[Category::Models];
const CAT_SETTINGS: &[Category] = &[Category::Settings];
const CAT_INPUT_TABS: &[Category] = &[Category::InputTabs];
const CAT_HF_DIALOG: &[Category] = &[Category::HfDialog];
/// Yank chips (`u`, `p`) appear under Models and Settings — the two
/// surfaces where the focused row has a meaningful server URL / path.
const CAT_YANK: &[Category] = &[Category::Models, Category::Settings];
/// Yank-curl (`c`/`y`) reaches Logs too — on that tab it copies the
/// whole log buffer (per [`Action::description_for`] override).
const CAT_YANK_CURL: &[Category] = &[Category::Models, Category::Logs, Category::Settings];
/// Stop launch (`Ctrl+S`) surfaces under Models and Settings — the
/// two surfaces where the focused row identifies a launch.
const CAT_STOP: &[Category] = &[Category::Models, Category::Settings];

/// Default keymap — one row per (action, key) chord, scoped to the
/// focuses where the binding fires. Replaces the v1 per-focus table
/// of 91 entries; kdash-style flat list keeps `Tab/⇧Tab/Esc/Enter`
/// from duplicating across every input focus.
///
/// Adding a new binding: pick the smallest [`FocusSet`] that
/// captures every focus the action should fire in. Repeat the
/// `Action` value if the same action has multiple chords (e.g. `q`
/// and `Ctrl+C` both map to `Quit`). Same-key collisions across
/// disjoint scopes are fine — the dispatcher walks the flat list
/// and picks the first match for the current focus.
///
/// Most alias groups (same action across N keys with shared scope /
/// hint / description) use the [`binds!`] macro below to keep the
/// source dense; chords whose scope or category diverges per chord
/// stay as explicit `Binding { ... }` literals.
///
/// Expand a `binds!` group into a literal `[Binding; N]` array with
/// shared metadata and per-chord `(key, mods, label, categories)`. Use
/// `&[]` for `categories` on alias chords you want hidden from the
/// help overlay (the user-visible row comes from the canonical chord).
macro_rules! binds {
  (
    action: $action:expr,
    scopes: $scopes:expr,
    hint: $hint:literal,
    description: $desc:expr,
    chords: [ $( ( $key:expr, $mods:expr, $label:expr, $cat:expr ) ),+ $(,)? ]
    $(,)?
  ) => {
    [ $(
      Binding {
        key: $key,
        mods: $mods,
        action: $action,
        label: $label,
        hint: $hint,
        description: $desc,
        scopes: $scopes,
        categories: $cat,
      },
    )+ ]
  };
}

/// Hidden-from-overlay marker for alias chords whose user-visible row
/// already appears via a sibling chord. Saves the `&[] as &[Category]`
/// dance at every alias site.
const NO_CAT: &[Category] = &[];

pub static DEFAULT_BINDINGS: std::sync::LazyLock<Vec<Binding>> =
  std::sync::LazyLock::new(build_default_bindings);

fn build_default_bindings() -> Vec<Binding> {
  let mut v: Vec<Binding> = Vec::new();
  // ─── Always-on chords across the nav focuses ────────────────
  v.extend_from_slice(&binds! {
    action: Action::ToggleHelp, scopes: FocusSet::NAV,
    hint: "help", description: None,
    chords: [(KeyCode::Char('?'), KeyModifiers::NONE, "?", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::Quit, scopes: FocusSet::NAV,
    hint: "quit", description: None,
    chords: [
      (KeyCode::Char('q'), KeyModifiers::NONE, "q", CAT_GLOBAL),
      (KeyCode::Char('c'), KeyModifiers::CONTROL, crate::ctrl_label!("c"), CAT_GLOBAL),
    ],
  });
  v.extend_from_slice(&binds! {
    action: Action::CycleTheme, scopes: FocusSet::NAV,
    hint: "theme", description: Some("next theme"),
    chords: [(KeyCode::Char('t'), KeyModifiers::NONE, "t", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::CycleThemePrev, scopes: FocusSet::NAV,
    hint: "prev theme", description: None,
    chords: [(KeyCode::Char('T'), KeyModifiers::SHIFT, "T", CAT_GLOBAL)],
  });
  // ─── Daemon-level / destructive (all behind Ctrl) ───────────
  v.extend_from_slice(&binds! {
    action: Action::RestartDaemon, scopes: FocusSet::NAV,
    hint: "restart", description: Some("restart daemon"),
    chords: [(KeyCode::Char('r'), KeyModifiers::CONTROL, crate::ctrl_label!("r"), CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::KillDaemon, scopes: FocusSet::LIST,
    hint: "kill", description: Some("kill daemon"),
    chords: [(KeyCode::Char('k'), KeyModifiers::CONTROL, crate::ctrl_label!("k"), CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::StopModel, scopes: FocusSet::NAV,
    hint: "stop", description: Some("stop launch"),
    chords: [(KeyCode::Char('s'), KeyModifiers::CONTROL, crate::ctrl_label!("s"), CAT_STOP)],
  });
  v.extend_from_slice(&binds! {
    action: Action::DeleteModel, scopes: FocusSet::LIST,
    hint: "delete", description: Some("delete from disk"),
    chords: [(KeyCode::Char('d'), KeyModifiers::CONTROL, crate::ctrl_label!("d"), CAT_MODELS)],
  });
  v.extend_from_slice(&binds! {
    action: Action::CancelDownload, scopes: FocusSet::NAV,
    hint: "cancel", description: Some("cancel download"),
    chords: [(KeyCode::Char('x'), KeyModifiers::CONTROL, crate::ctrl_label!("x"), CAT_GLOBAL)],
  });
  // ─── Motion (arrows + vi aliases). ↑/↓ extend into HF_DIALOG ──
  // for row selection, and into CHAT_INPUT/EMBED_INPUT so the output
  // viewport scrolls while the prompt field is focused.
  // RERANK_INPUT is excluded — that focus binds ↑/↓ to PrevField /
  // NextField for cycling between the query and candidate sub-fields.
  // k/j stay NAV-only (typing chars would conflict with editing).
  // Two `binds!` calls per direction because scope diverges per chord.
  v.extend_from_slice(&binds! {
    action: Action::MoveUp,
    scopes: FocusSet::NAV
      .union(FocusSet::HF_DIALOG)
      .union(FocusSet::CHAT_INPUT)
      .union(FocusSet::EMBED_INPUT),
    hint: "up", description: Some("up/prev"),
    chords: [(KeyCode::Up, KeyModifiers::NONE, "↑",
      &[Category::Global, Category::InputTabs])],
  });
  v.extend_from_slice(&binds! {
    action: Action::MoveUp, scopes: FocusSet::NAV,
    hint: "up", description: Some("up/prev"),
    chords: [(KeyCode::Char('k'), KeyModifiers::NONE, "k", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::MoveDown,
    scopes: FocusSet::NAV
      .union(FocusSet::HF_DIALOG)
      .union(FocusSet::CHAT_INPUT)
      .union(FocusSet::EMBED_INPUT),
    hint: "down", description: Some("down/next"),
    chords: [(KeyCode::Down, KeyModifiers::NONE, "↓",
      &[Category::Global, Category::InputTabs])],
  });
  v.extend_from_slice(&binds! {
    action: Action::MoveDown, scopes: FocusSet::NAV,
    hint: "down", description: Some("down/next"),
    chords: [(KeyCode::Char('j'), KeyModifiers::NONE, "j", CAT_GLOBAL)],
  });
  // PageDown / PageUp — PgDn/PgUp canonical, Ctrl+F/B/U vim aliases
  // grouped under Models so the help row renders as `PgDn,Ctrl+F →
  // page down` (and similarly for up). They share the same action +
  // description, so `build_sections` merges them into one row.
  v.extend_from_slice(&binds! {
    action: Action::PageDown, scopes: FocusSet::LIST,
    hint: "page down", description: None,
    chords: [
      (KeyCode::PageDown, KeyModifiers::NONE, "PgDn", CAT_MODELS),
      (KeyCode::Char('f'), KeyModifiers::CONTROL, crate::ctrl_label!("f"), CAT_MODELS),
    ],
  });
  v.extend_from_slice(&binds! {
    action: Action::PageUp, scopes: FocusSet::LIST,
    hint: "page up", description: None,
    chords: [
      (KeyCode::PageUp, KeyModifiers::NONE, "PgUp", CAT_MODELS),
      (KeyCode::Char('b'), KeyModifiers::CONTROL, crate::ctrl_label!("b"), CAT_MODELS),
      (KeyCode::Char('u'), KeyModifiers::CONTROL, crate::ctrl_label!("u"), NO_CAT),
    ],
  });
  // GoTop / GoBottom — `g`/`Home` and `G`/`End` co-primary; `0`/`$`
  // vim aliases share the Models row via the (action, description)
  // merge.
  v.extend_from_slice(&binds! {
    action: Action::GoTop, scopes: FocusSet::LIST,
    hint: "top", description: Some("top of list"),
    chords: [
      (KeyCode::Char('g'), KeyModifiers::NONE, "g", CAT_MODELS),
      (KeyCode::Home, KeyModifiers::NONE, "Home", CAT_MODELS),
      (KeyCode::Char('0'), KeyModifiers::NONE, "0", CAT_MODELS),
    ],
  });
  v.extend_from_slice(&binds! {
    action: Action::GoBottom, scopes: FocusSet::LIST,
    hint: "bottom", description: Some("bottom of list"),
    chords: [
      (KeyCode::Char('G'), KeyModifiers::SHIFT, "G", CAT_MODELS),
      (KeyCode::End, KeyModifiers::NONE, "End", CAT_MODELS),
      (KeyCode::Char('$'), KeyModifiers::NONE, "$", CAT_MODELS),
    ],
  });
  // ─── Pane navigation. Tab/Shift+Tab fire in every TUI focus;
  // vi h/l aliases NAV-only. Scope diverges → split into two calls.
  v.extend_from_slice(&binds! {
    action: Action::NextFocus, scopes: FocusSet::TUI,
    hint: "next pane", description: None,
    chords: [(KeyCode::Tab, KeyModifiers::NONE, TAB_LABEL, CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::NextFocus, scopes: FocusSet::NAV,
    hint: "next pane", description: None,
    chords: [(KeyCode::Char('l'), KeyModifiers::NONE, "l", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::PrevFocus, scopes: FocusSet::TUI,
    hint: "prev pane", description: None,
    chords: [(KeyCode::BackTab, KeyModifiers::SHIFT, SHIFT_TAB_LABEL, CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::PrevFocus, scopes: FocusSet::NAV,
    hint: "prev pane", description: None,
    chords: [(KeyCode::Char('h'), KeyModifiers::NONE, "h", CAT_GLOBAL)],
  });
  // gt / gT — vim "tab next/prev" aliases for NextFocus/PrevFocus.
  // Dispatch lives in `events::handle_key`'s `pending_g_prefix` state
  // machine, not here, because they're a two-key sequence and this
  // table is single-chord. We surface them as display-only rows using
  // `KeyCode::Null` (which terminals never emit) so the help overlay's
  // `(action, description)` merge folds them into the existing
  // `Tab,l,gt → next pane` and `Shift+Tab,h,gT → prev pane` rows.
  // Same NAV scope as h/l so the row lands in the same focus group.
  v.extend_from_slice(&binds! {
    action: Action::NextFocus, scopes: FocusSet::NAV,
    hint: "next pane", description: None,
    chords: [(KeyCode::Null, KeyModifiers::NONE, "gt", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::PrevFocus, scopes: FocusSet::NAV,
    hint: "prev pane", description: None,
    chords: [(KeyCode::Null, KeyModifiers::NONE, "gT", CAT_GLOBAL)],
  });
  // ─── Shift-letter quick-jumps (navigation policy: Shift = navigate)
  v.extend_from_slice(&binds! {
    action: Action::FocusList, scopes: FocusSet::NAV,
    hint: "models", description: Some("models list"),
    chords: [(KeyCode::Char('M'), KeyModifiers::SHIFT, "M", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::FocusLogsTab, scopes: FocusSet::NAV,
    hint: "logs", description: Some("logs tab"),
    chords: [(KeyCode::Char('L'), KeyModifiers::SHIFT, "L", CAT_GLOBAL)],
  });
  // FocusChatTab — Shift+C/E/R all jump to the same tab; merge into
  // one help row via the (action, description) grouping in the overlay.
  v.extend_from_slice(&binds! {
    action: Action::FocusChatTab, scopes: FocusSet::NAV,
    hint: "chat/embed/rerank", description: Some("chat/embed/rerank"),
    chords: [
      (KeyCode::Char('C'), KeyModifiers::SHIFT, "C", CAT_GLOBAL),
      (KeyCode::Char('E'), KeyModifiers::SHIFT, "E", CAT_GLOBAL),
      (KeyCode::Char('R'), KeyModifiers::SHIFT, "R", CAT_GLOBAL),
    ],
  });
  v.extend_from_slice(&binds! {
    action: Action::FocusSettingsTab, scopes: FocusSet::NAV,
    hint: "settings", description: Some("settings tab"),
    chords: [(KeyCode::Char('S'), KeyModifiers::SHIFT, "S", CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::OpenHfDialog, scopes: FocusSet::NAV,
    hint: "pull", description: Some("pull from HF"),
    chords: [(KeyCode::Char('P'), KeyModifiers::SHIFT, "P", CAT_GLOBAL)],
  });
  // ─── Filter / favorite (LIST only) ──────────────────────────
  v.extend_from_slice(&binds! {
    action: Action::OpenFilter, scopes: FocusSet::LIST,
    hint: "filter", description: Some("open filter input"),
    chords: [(KeyCode::Char('/'), KeyModifiers::NONE, "/", CAT_MODELS)],
  });
  v.extend_from_slice(&binds! {
    action: Action::ToggleFavorite, scopes: FocusSet::LIST,
    hint: "favorite", description: Some("toggle favorite ★"),
    chords: [(KeyCode::Char('f'), KeyModifiers::NONE, "f", CAT_MODELS)],
  });
  // ─── Yank / copy. `y` is a vim-style alias for `c`. ─────────
  v.extend_from_slice(&binds! {
    action: Action::YankUrl, scopes: FocusSet::NAV,
    hint: "url", description: Some("copy server URL"),
    chords: [(KeyCode::Char('u'), KeyModifiers::NONE, "u", CAT_YANK)],
  });
  v.extend_from_slice(&binds! {
    action: Action::YankCurl, scopes: FocusSet::NAV,
    hint: "curl", description: Some("copy curl command"),
    chords: [
      (KeyCode::Char('c'), KeyModifiers::NONE, "c", CAT_YANK_CURL),
      (KeyCode::Char('y'), KeyModifiers::NONE, "y", CAT_YANK_CURL),
    ],
  });
  v.extend_from_slice(&binds! {
    action: Action::YankPath, scopes: FocusSet::NAV,
    hint: "path", description: Some("copy file path"),
    chords: [(KeyCode::Char('p'), KeyModifiers::NONE, "p", CAT_YANK)],
  });
  // ─── Right-pane affordances (Settings / Logs) ───────────────
  v.extend_from_slice(&binds! {
    action: Action::EnterEdit, scopes: FocusSet::RIGHT_PANE,
    hint: "edit", description: Some("enter edit mode"),
    chords: [
      (KeyCode::Char('e'), KeyModifiers::NONE, "e", CAT_GLOBAL),
      // `i` is the vim alias for `e`; merges into the same General
      // row via the (action, description) grouping in the help
      // overlay so it reads `e,i → enter edit mode`.
      (KeyCode::Char('i'), KeyModifiers::NONE, "i", CAT_GLOBAL),
    ],
  });
  v.extend_from_slice(&binds! {
    action: Action::ToggleAutoScroll, scopes: FocusSet::RIGHT_PANE,
    hint: "auto-scroll", description: None,
    chords: [(KeyCode::Char('s'), KeyModifiers::NONE, "s", &[Category::Logs])],
  });
  v.extend_from_slice(&binds! {
    action: Action::CycleValueNext, scopes: FocusSet::RIGHT_PANE,
    hint: "next value", description: None,
    chords: [(KeyCode::Right, KeyModifiers::NONE, "→", CAT_SETTINGS)],
  });
  v.extend_from_slice(&binds! {
    action: Action::CycleValuePrev, scopes: FocusSet::RIGHT_PANE,
    hint: "prev value", description: None,
    chords: [(KeyCode::Left, KeyModifiers::NONE, "←", CAT_SETTINGS)],
  });
  // ─── Enter — four Action variants across disjoint scopes ────
  v.extend_from_slice(&binds! {
    action: Action::OpenLaunchPicker, scopes: FocusSet::LIST,
    hint: "launch", description: Some("launch focused model"),
    chords: [(KeyCode::Enter, KeyModifiers::NONE, ENTER_LABEL, CAT_MODELS)],
  });
  v.extend_from_slice(&binds! {
    action: Action::Submit,
    scopes: FocusSet::FILTER
      .union(FocusSet::RIGHT_PANE)
      .union(FocusSet::EMBED_INPUT)
      .union(FocusSet::RERANK_INPUT)
      .union(FocusSet::CONFIRM_POPUP)
      .union(FocusSet::HF_DIALOG),
    hint: "submit", description: Some("submit"),
    chords: [(KeyCode::Enter, KeyModifiers::NONE, ENTER_LABEL, &[
      Category::Models, Category::Settings, Category::InputTabs, Category::HfDialog,
    ])],
  });
  // SendChat owns CHAT_INPUT-Enter; hidden from the overlay because
  // the merged Chat/Embed/Rerank section surfaces it via Submit above.
  v.extend_from_slice(&binds! {
    action: Action::SendChat, scopes: FocusSet::CHAT_INPUT,
    hint: "send", description: Some("send chat"),
    chords: [(KeyCode::Enter, KeyModifiers::NONE, ENTER_LABEL, NO_CAT)],
  });
  v.extend_from_slice(&binds! {
    action: Action::InsertNewline, scopes: FocusSet::INPUT,
    hint: "newline", description: Some("insert newline"),
    chords: [(KeyCode::Enter, KeyModifiers::SHIFT, SHIFT_ENTER_LABEL, CAT_GLOBAL)],
  });
  // Plain `r` toggles `<think>` collapse on the Chat tab; the handler
  // gates by `right_tab == RightTab::Chat` so the binding stays inert
  // on Embed/Rerank/Logs/Settings. Scope covers both `RIGHT_PANE`
  // (browsing focus) and `CHAT_INPUT` (input focus in resting state) —
  // `InputField::handle_key_resting` lets unmodified non-`e`/`Esc`
  // chars fall through to the action layer, so `r` fires the toggle
  // whenever the prompt isn't actively editing. While the field IS
  // editing, `handle_key_editing` captures `r` as a typed char before
  // the action layer ever sees it, so this doesn't shadow typing.
  v.extend_from_slice(&binds! {
    action: Action::ToggleThinkCollapse,
    scopes: FocusSet::RIGHT_PANE.union(FocusSet::CHAT_INPUT),
    hint: "toggle reasoning", description: Some("toggle <think> blocks"),
    chords: [(KeyCode::Char('r'), KeyModifiers::NONE, "r", CAT_INPUT_TABS)],
  });
  // ─── Esc — five disjoint actions across the focus families. ──
  // The help overlay surfaces a single merged row under Global via
  // the FocusList Esc; the rest dispatch but hide (empty `categories`).
  v.extend_from_slice(&binds! {
    action: Action::FocusList, scopes: FocusSet::RIGHT_PANE,
    hint: "models list", description: Some("cancel/clear/back"),
    chords: [(KeyCode::Esc, KeyModifiers::NONE, ESC_LABEL, CAT_GLOBAL)],
  });
  v.extend_from_slice(&binds! {
    action: Action::ClearFilter, scopes: FocusSet::FILTER,
    hint: "clear", description: Some("clear filter"),
    chords: [(KeyCode::Esc, KeyModifiers::NONE, ESC_LABEL, NO_CAT)],
  });
  v.extend_from_slice(&binds! {
    action: Action::ExitEdit, scopes: FocusSet::INPUT,
    hint: "exit edit", description: None,
    chords: [(KeyCode::Esc, KeyModifiers::NONE, ESC_LABEL, NO_CAT)],
  });
  v.extend_from_slice(&binds! {
    action: Action::Cancel, scopes: FocusSet::CONFIRM_POPUP,
    hint: "cancel", description: Some("cancel prompt"),
    chords: [(KeyCode::Esc, KeyModifiers::NONE, ESC_LABEL, NO_CAT)],
  });
  v.extend_from_slice(&binds! {
    action: Action::Cancel, scopes: FocusSet::HF_DIALOG,
    hint: "close", description: Some("close dialog"),
    chords: [(KeyCode::Esc, KeyModifiers::NONE, ESC_LABEL, NO_CAT)],
  });
  // ─── Rerank field cycle (↑↓ inside rerank input only). ──────
  // Collides with MoveUp/MoveDown; disjoint scopes (RERANK_INPUT vs
  // NAV|HF_DIALOG) keep them clean. Hidden from the overlay.
  v.extend_from_slice(&binds! {
    action: Action::NextField, scopes: FocusSet::RERANK_INPUT,
    hint: "next field", description: None,
    chords: [(KeyCode::Down, KeyModifiers::NONE, "↓", NO_CAT)],
  });
  v.extend_from_slice(&binds! {
    action: Action::PrevField, scopes: FocusSet::RERANK_INPUT,
    hint: "prev field", description: None,
    chords: [(KeyCode::Up, KeyModifiers::NONE, "↑", NO_CAT)],
  });
  // ─── HF dialog stage chords (display-only). ────────────────
  // The dialog's per-stage handler captures `o`/`n`/`p` directly; these
  // rows exist only so the help overlay can list them.
  v.extend_from_slice(&binds! {
    action: Action::HfCycleSort, scopes: FocusSet::HF_DIALOG,
    hint: "sort", description: Some("cycle sort order"),
    chords: [(KeyCode::Char('o'), KeyModifiers::NONE, "o", CAT_HF_DIALOG)],
  });
  v.extend_from_slice(&binds! {
    action: Action::HfNextPage, scopes: FocusSet::HF_DIALOG,
    hint: "next page", description: Some("next page"),
    chords: [(KeyCode::Char('n'), KeyModifiers::NONE, "n", CAT_HF_DIALOG)],
  });
  v.extend_from_slice(&binds! {
    action: Action::HfPrevPage, scopes: FocusSet::HF_DIALOG,
    hint: "prev page", description: Some("prev page"),
    chords: [(KeyCode::Char('p'), KeyModifiers::NONE, "p", CAT_HF_DIALOG)],
  });
  v
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
///
/// Storage is a flat `Vec<Binding>` (one row per chord+scope);
/// `per_focus` caches per-focus snapshots so `bindings_for(focus)`
/// can return a `&[Binding]` slice. Rebuilt after `apply_overrides`.
#[derive(Debug, Clone)]
pub struct KeyMap {
  flat: Vec<Binding>,
  per_focus: BTreeMap<Focus, Vec<Binding>>,
}

impl Default for KeyMap {
  fn default() -> Self {
    let flat: Vec<Binding> = DEFAULT_BINDINGS.to_vec();
    let per_focus = build_per_focus(&flat);
    KeyMap { flat, per_focus }
  }
}

/// Rebuild the per-focus cache from the flat list. Called on
/// construction and after every override pass.
fn build_per_focus(flat: &[Binding]) -> BTreeMap<Focus, Vec<Binding>> {
  let mut per_focus: BTreeMap<Focus, Vec<Binding>> = BTreeMap::new();
  for focus in [
    Focus::List,
    Focus::Filter,
    Focus::RightPane,
    Focus::ChatInput,
    Focus::EmbedInput,
    Focus::RerankInput,
    Focus::ConfirmPopup,
    Focus::HfDialog,
  ] {
    let rows: Vec<Binding> = flat
      .iter()
      .filter(|b| b.scopes.contains(focus))
      .copied()
      .collect();
    per_focus.insert(focus, rows);
  }
  per_focus
}

impl KeyMap {
  /// Look up the action triggered by `(key, mods)` in the supplied
  /// focus. Returns `None` when nothing matches.
  pub fn action_for(&self, focus: Focus, key: KeyCode, mods: KeyModifiers) -> Option<Action> {
    // SHIFT normalization for character keys: a shifted character
    // (`?` = Shift+/, `P` = Shift+p) already encodes the shift in the
    // character itself, and terminals disagree on whether SHIFT is
    // *also* reported. Windows Terminal sets `KeyModifiers::SHIFT` for
    // shifted symbols like `?`, while most Unix terminals report
    // `NONE` — so a binding registered as `(Char('?'), NONE)` never
    // matched the Windows `(Char('?'), SHIFT)` event and `?` failed to
    // open the help overlay. Mask SHIFT off `Char` keys on both the
    // event and the binding so the match is platform-independent;
    // non-char chords (Shift+Tab→BackTab, Shift+Enter) keep SHIFT.
    let norm = |k: KeyCode, m: KeyModifiers| {
      if matches!(k, KeyCode::Char(_)) {
        m.difference(KeyModifiers::SHIFT)
      } else {
        m
      }
    };
    let want = norm(key, mods);
    self.per_focus.get(&focus).and_then(|rows| {
      rows
        .iter()
        .find(|b| b.key == key && norm(b.key, b.mods) == want)
        .map(|b| b.action)
    })
  }

  /// Bindings the help bar should show in the supplied focus.
  pub fn bindings_for(&self, focus: Focus) -> &[Binding] {
    self.per_focus.get(&focus).map(Vec::as_slice).unwrap_or(&[])
  }

  /// Iterator over every `(focus, bindings)` pair. Replaces direct
  /// access to `DEFAULT_BINDINGS` for callers (help overlay) that
  /// walk the whole table.
  pub fn iter(&self) -> impl Iterator<Item = (Focus, &[Binding])> {
    self
      .per_focus
      .iter()
      .map(|(focus, rows)| (*focus, rows.as_slice()))
  }

  /// Iterator over every flat binding (no focus filtering). Used by
  /// tests that need to walk the source-of-truth table.
  pub fn flat(&self) -> &[Binding] {
    &self.flat
  }

  /// Overlay user-supplied `action → key_spec` pairs onto the
  /// keymap (kdash-style). For each override, every default
  /// binding for that action is removed; a new binding is inserted
  /// with the same scopes. Any existing binding at the new chord
  /// inside those scopes is also dropped to prevent ambiguous
  /// dispatch (and a warning surfaces the conflict).
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
      // Find every existing row for this action; capture the union
      // of their scopes plus the first hint / description / categories
      // we encounter so the override keeps the UI text.
      let mut combined_scopes = FocusSet::EMPTY;
      let mut hint: &'static str = "";
      let mut description: Option<&'static str> = None;
      let mut categories: &'static [Category] = &[];
      for b in self.flat.iter() {
        if b.action == action {
          combined_scopes = combined_scopes.union(b.scopes);
          if hint.is_empty() {
            hint = b.hint;
            description = b.description;
            categories = b.categories;
          }
        }
      }
      if combined_scopes.bits() == 0 {
        warnings.push(format!(
          "keybindings.{raw_action}: action has no default binding; nothing was rebound"
        ));
        continue;
      }
      // Leak the runtime label so the resulting Binding fits the
      // `&'static str` slot. One-time at startup, never repeated.
      let leaked_label: &'static str = Box::leak(spec.label.into_boxed_str());
      // Drop every existing binding for this action, plus any other
      // binding sitting on the new chord within the same scope set
      // (would otherwise cause ambiguous dispatch).
      self.flat.retain(|b| {
        if b.action == action {
          return false;
        }
        // Same chord, overlapping scope → drop. Keep when scopes
        // are disjoint (e.g. Esc means different things in
        // ConfirmPopup vs HfDialog).
        if b.key == spec.key && b.mods == spec.mods && b.scopes.bits() & combined_scopes.bits() != 0
        {
          return false;
        }
        true
      });
      self.flat.push(Binding {
        key: spec.key,
        mods: spec.mods,
        action,
        label: leaked_label,
        hint,
        description,
        scopes: combined_scopes,
        categories,
      });
    }
    self.per_focus = build_per_focus(&self.flat);
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
    ("submit", Action::Submit),
    ("cancel", Action::Cancel),
    ("yank_url", Action::YankUrl),
    ("yank_curl", Action::YankCurl),
    ("yank_path", Action::YankPath),
    ("cycle_theme", Action::CycleTheme),
    ("cycle_theme_prev", Action::CycleThemePrev),
    ("open_hf_dialog", Action::OpenHfDialog),
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

  /// Per-focus description override for actions whose meaning
  /// varies across focuses. `Submit` is the main example —
  /// "launch" in the filter, "send" in chat, "embed" in embed,
  /// "open / confirm" in the HF dialog, etc. Returns `None` when
  /// the generic [`Binding::description`] is the right label.
  ///
  /// Help-bar / overlay callers prefer this over the binding's
  /// raw description so the chip text matches what the action
  /// will actually do in the current pane.
  pub fn hint_for(self, focus: Focus) -> Option<&'static str> {
    match (self, focus) {
      (Action::Submit, Focus::Filter) => Some("apply"),
      (Action::Submit, Focus::RightPane) => Some("launch/save"),
      (Action::Submit, Focus::EmbedInput) => Some("embed"),
      (Action::Submit, Focus::RerankInput) => Some("query/add candidate"),
      (Action::Submit, Focus::ConfirmPopup) => Some("confirm"),
      (Action::Submit, Focus::HfDialog) => Some("open/confirm"),
      // Motion in the right pane is scroll on the Logs tab, field
      // cycle on Settings. The hint surfaces the live tab via the
      // chip-rendering callers (right pane reads `app.right_tab`).
      (Action::MoveUp, Focus::RightPane) => Some("scroll up"),
      (Action::MoveDown, Focus::RightPane) => Some("scroll down"),
      (Action::Cancel, Focus::HfDialog) => Some("close"),
      (Action::FocusList, Focus::RightPane) => Some("models list"),
      _ => None,
    }
  }

  /// Per-category description override for the help overlay. Lets a
  /// single `Binding` row surface under several categories with
  /// section-specific wording.
  pub fn description_for(self, category: Category) -> Option<&'static str> {
    match (self, category) {
      // Submit reshapes per editorial section. The merged
      // Chat/Embed/Rerank section reads the binding's plain "submit"
      // text — no per-tab override.
      (Action::Submit, Category::Models) => Some("apply filter"),
      (Action::Submit, Category::Settings) => Some("launch/save"),
      (Action::Submit, Category::HfDialog) => Some("open/confirm"),
      // Yank `c` on the Logs tab copies the entire log buffer rather
      // than the curl command.
      (Action::YankCurl, Category::Logs) => Some("copy logs"),
      _ => None,
    }
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
/// Unicode glyph rendered in place of the `Shift+` modifier text.
/// `⇧` (U+21E7) is the standard "Shift" symbol used by macOS
/// keyboard-shortcut docs and is present in every monospace font
/// shipped with terminals — no Nerd Font required. Centralised so
/// renderers, tests, and binding tables can all reference the same
/// character. No `+` joiner — `⇧⏎` and `⇧⇥` / `⇧↹` read better
/// than `⇧+Enter` / `⇧+Tab` and match the macOS HIG convention.
pub const SHIFT_GLYPH: &str = "⇧";

// ─── Platform-conditional key labels ───────────────────────────
//
// Tab and the modifier prefixes differ between PC and Apple
// keyboards. macOS uses tight `⌃ ⌥ ⌘ ⇥` glyphs (no `+` joiner) that
// every Mac user recognises from system menus. PC keyboards print
// `↹` on the Tab keycap (two-arrow style) and don't have `⌃` /
// `⌘` markings, so we keep the textual `Ctrl+` / `Alt+` / `Super+`
// joiner there. `⇧` and the navigation glyphs (`↑↓←→`, `⏎`) are
// universal — same on every platform — so they live as plain
// `pub const` without cfg-gating.
//
// One source of truth for renderers, the static binding tables,
// `format_key_label` (config-override path), and tests.

/// `KeyCode::Enter` label. Universal Unicode "return" symbol
/// (U+23CE) used in macOS docs, Linux desktop environments, and
/// most modern keyboard cheatsheets.
pub const ENTER_LABEL: &str = "⏎";

/// `KeyCode::Esc` label. Kept as text on both platforms — the
/// `⎋` glyph isn't printed on physical Esc keys and isn't widely
/// recognised outside macOS docs.
pub const ESC_LABEL: &str = "Esc";

/// `KeyCode::Tab` label. PC keycaps print `↹` (U+21B9, two
/// arrows hitting bars in opposite directions); macOS uses `⇥`
/// (U+21E5, single arrow to bar). Both are in every monospace
/// font.
#[cfg(target_os = "macos")]
pub const TAB_LABEL: &str = "⇥";
#[cfg(not(target_os = "macos"))]
pub const TAB_LABEL: &str = "↹";

/// `KeyCode::BackTab` label. Equivalent to `Shift+Tab`. The
/// shift glyph (`⇧`) prefixes the platform-appropriate Tab
/// glyph, no `+` joiner.
#[cfg(target_os = "macos")]
pub const SHIFT_TAB_LABEL: &str = "⇧⇥";
#[cfg(not(target_os = "macos"))]
pub const SHIFT_TAB_LABEL: &str = "⇧↹";

/// `Shift+Enter` chord label (e.g. newline-in-input). Same on
/// both platforms.
pub const SHIFT_ENTER_LABEL: &str = "⇧⏎";

/// `Ctrl` modifier prefix. macOS uses the `⌃` (U+2303) glyph
/// with no `+` joiner — it sits tight against the key letter
/// (`⌃k`) the way Apple's HIG renders it. PC keeps `Ctrl+` since
/// `⌃` is not printed on any PC keyboard.
#[cfg(target_os = "macos")]
pub const CTRL_PREFIX: &str = "⌃";
#[cfg(not(target_os = "macos"))]
pub const CTRL_PREFIX: &str = "Ctrl+";

/// `Alt` modifier prefix. macOS uses `⌥` (U+2325, "option"
/// glyph) without `+`. PC keeps `Alt+`.
#[cfg(target_os = "macos")]
pub const ALT_PREFIX: &str = "⌥";
#[cfg(not(target_os = "macos"))]
pub const ALT_PREFIX: &str = "Alt+";

/// `Super` / `Cmd` modifier prefix. macOS uses `⌘` (U+2318).
/// No default binding uses this, but config overrides can —
/// hence the entry.
#[cfg(target_os = "macos")]
pub const SUPER_PREFIX: &str = "⌘";
#[cfg(not(target_os = "macos"))]
pub const SUPER_PREFIX: &str = "Super+";

fn format_key_label(key: &KeyCode, mods: KeyModifiers) -> String {
  let mut out = String::new();
  if mods.contains(KeyModifiers::CONTROL) {
    out.push_str(CTRL_PREFIX);
  }
  if mods.contains(KeyModifiers::ALT) {
    out.push_str(ALT_PREFIX);
  }
  if mods.contains(KeyModifiers::SUPER) {
    out.push_str(SUPER_PREFIX);
  }
  // Suppress an explicit Shift prefix when the key already encodes
  // Shift in its name or glyph:
  //  - Uppercase `Char` (terminal emits Shift+letter as `Char(C)`)
  //  - `BackTab` (the named code for Shift+Tab — adding the prefix
  //    again would render as `⇧⇧⇥`, doubly emphasising Shift).
  let key_already_encodes_shift =
    matches!(key, KeyCode::Char(c) if c.is_ascii_uppercase()) || matches!(key, KeyCode::BackTab);
  let show_shift = mods.contains(KeyModifiers::SHIFT) && !key_already_encodes_shift;
  if show_shift {
    out.push_str(SHIFT_GLYPH);
  }
  match key {
    KeyCode::Char(' ') => out.push_str("Space"),
    KeyCode::Char(c) => out.push(*c),
    KeyCode::Enter => out.push_str(ENTER_LABEL),
    KeyCode::Esc => out.push_str(ESC_LABEL),
    KeyCode::Tab => out.push_str(TAB_LABEL),
    KeyCode::BackTab => out.push_str(SHIFT_TAB_LABEL),
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
    DEFAULT_BINDINGS
      .iter()
      .find(|b| b.scopes.contains(focus) && b.key == key && b.mods == mods)
      .map(|b| b.action)
  }

  /// Helper: default bindings list for a focus, filtered from the
  /// flat slice. Allocates because the flat list isn't pre-grouped
  /// per focus — fine in tests.
  fn bindings_for(focus: Focus) -> Vec<Binding> {
    DEFAULT_BINDINGS
      .iter()
      .filter(|b| b.scopes.contains(focus))
      .copied()
      .collect()
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
  fn shift_p_in_list_focus_opens_hf_dialog() {
    // Navigation policy: "Pull" lives behind Shift+P (Shift =
    // navigate). The `d` key is reserved for Ctrl+D = delete.
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('P'), KeyModifiers::SHIFT),
      Some(Action::OpenHfDialog),
    );
  }

  #[test]
  fn bare_d_no_longer_opens_hf_dialog() {
    // Pin the retirement so a future refactor can't accidentally
    // re-add `d` → OpenHfDialog.
    assert_eq!(
      action_for(Focus::List, KeyCode::Char('d'), KeyModifiers::NONE),
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
  fn r_toggles_think_collapse_in_right_pane_and_chat_input() {
    assert_eq!(
      action_for(Focus::RightPane, KeyCode::Char('r'), KeyModifiers::NONE),
      Some(Action::ToggleThinkCollapse),
    );
    // Also bound under ChatInput so the resting-mode pass-through
    // from `InputField` reaches the action layer. While editing,
    // `r` is consumed by `handle_key_editing` as a typed character
    // before the action layer ever runs — no conflict.
    assert_eq!(
      action_for(Focus::ChatInput, KeyCode::Char('r'), KeyModifiers::NONE),
      Some(Action::ToggleThinkCollapse),
    );
    // The old `Ctrl+R` chord is gone.
    assert_eq!(
      action_for(Focus::ChatInput, KeyCode::Char('r'), KeyModifiers::CONTROL),
      None,
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
    assert_eq!(ctrl_q.label, format!("{CTRL_PREFIX}q"));

    // "Shift+Tab" parses to KeyCode::Tab + SHIFT modifier. The
    // formatter renders that as `⇧` + TAB_LABEL (no `+` joiner) —
    // which is byte-identical to SHIFT_TAB_LABEL.
    let shift_tab = parse_key_spec("Shift+Tab").unwrap();
    assert_eq!(shift_tab.key, KeyCode::Tab);
    assert!(shift_tab.mods.contains(KeyModifiers::SHIFT));
    assert_eq!(
      shift_tab.label, SHIFT_TAB_LABEL,
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
    // "Shift". No `+` joiner — `⇧q` reads cleaner than `⇧+q` and
    // matches the macOS HIG.
    assert_eq!(
      format_key_label(&KeyCode::Char('q'), KeyModifiers::SHIFT),
      "⇧q"
    );
  }

  #[test]
  fn format_key_label_renders_shift_as_nerd_font_glyph() {
    // Regression guard: every Shift-aware label must surface the
    // glyph form. The legacy `Shift+` text must never reappear.
    let shift_tab = format_key_label(&KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(shift_tab, SHIFT_TAB_LABEL);
    assert!(
      !shift_tab.contains("Shift"),
      "Shift+ text must not appear in any key label: {shift_tab}"
    );
    let shift_enter = format_key_label(&KeyCode::Enter, KeyModifiers::SHIFT);
    assert_eq!(shift_enter, SHIFT_ENTER_LABEL);
    assert!(
      !shift_enter.contains("Shift"),
      "Shift+ text must not appear in any key label: {shift_enter}"
    );
  }

  #[test]
  fn format_key_label_ctrl_char_renders_with_ctrl_prefix() {
    assert_eq!(
      format_key_label(&KeyCode::Char('c'), KeyModifiers::CONTROL),
      crate::ctrl_label!("c")
    );
  }

  #[test]
  fn format_key_label_ctrl_shift_lowercase_emits_both_prefixes() {
    // Ctrl + Shift + c. On PC this is `Ctrl+⇧c` (note: no `+` between
    // the shift glyph and the char). On macOS it's `⌃⇧c`.
    let mods = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    let expected = format!("{CTRL_PREFIX}⇧c");
    assert_eq!(format_key_label(&KeyCode::Char('c'), mods), expected);
  }

  #[test]
  fn format_key_label_alt_char_renders_with_alt_prefix() {
    let expected = format!("{ALT_PREFIX}x");
    assert_eq!(
      format_key_label(&KeyCode::Char('x'), KeyModifiers::ALT),
      expected
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
      ENTER_LABEL
    );
    assert_eq!(
      format_key_label(&KeyCode::Esc, KeyModifiers::NONE),
      ESC_LABEL
    );
    assert_eq!(
      format_key_label(&KeyCode::Tab, KeyModifiers::NONE),
      TAB_LABEL
    );
    assert_eq!(
      format_key_label(&KeyCode::BackTab, KeyModifiers::SHIFT),
      SHIFT_TAB_LABEL
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
    // string is canonical — easier to grep for in help dumps. On PC
    // the prefixes carry their own `+` joiner; on macOS the glyphs
    // sit tight against each other.
    let mods = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
    let expected = format!("{CTRL_PREFIX}{ALT_PREFIX}{SUPER_PREFIX}F5");
    assert_eq!(format_key_label(&KeyCode::F(5), mods), expected);
  }

  #[test]
  fn format_key_label_combines_all_modifiers_with_shift_glyph_last() {
    // Full chain — Shift sits in the glyph slot last, no `+` joiner.
    let mods =
      KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::SHIFT;
    let expected = format!("{CTRL_PREFIX}{ALT_PREFIX}{SUPER_PREFIX}⇧F5");
    assert_eq!(format_key_label(&KeyCode::F(5), mods), expected);
  }
}
