//! Event loop bridging crossterm input and IPC notifications into
//! [`super::app::App`] state transitions.
//!
//! Two background tasks talk to the daemon:
//! - the **refresher** polls `list_models` / `status` / `favorite_list`
//!   on a tick and forwards snapshots through `RefreshTick`;
//! - the **writer** owns a fresh `Client` per command and forwards
//!   `WriterCmd` requests (`start_model`, `favorite_add/remove`) so
//!   the input pump can issue mutations without blocking the render
//!   loop. The writer reconnects per command (local Unix socket is
//!   cheap) so a transient daemon restart doesn't poison the
//!   long-lived channel.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ipc::Client;
use crate::tui::app::{App, ConfirmAction};
use crate::tui::hf_pull::{
  apply_cancel_download, apply_download_event, apply_hf_dialog_event, apply_open_hf_dialog,
  handle_hf_dialog_input, service_hf_dialog_debounce, spawn_download_task,
};
use crate::tui::keybindings::{Action, Focus};
use crate::tui::oai_client::{
  embed as oai_embed, rerank as oai_rerank, spawn_chat_stream, ChatStreamMsg,
};
use crate::tui::tabs::rerank::RerankField;
use crate::tui::tabs::RightTab;
use crate::util::clipboard;
use anyhow::Result;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Catalog/status refresh cadence in the steady state. Governs how
/// stale daemon snapshots may get; the run loop itself is event-driven
/// and wakes immediately on real input or subsystem updates.
const REFRESH_INTERVAL: Duration = Duration::from_millis(750);
/// Initial reconnect backoff used when the daemon is unreachable.
/// Doubles on each failure up to [`REFRESH_INTERVAL`] so a freshly
/// started daemon gets attached within ~2 s on a cold connect.
const RECONNECT_INITIAL: Duration = Duration::from_millis(120);
/// Background-input-thread tick cadence. The crossterm poll thread
/// blocks for up to this long waiting for a key/mouse/paste/resize
/// event; if the wait expires it emits one [`Event::Tick`] so any
/// time-based UI work (e.g. download throughput averages) still moves.
/// Idle CPU is bounded by this interval, not by any inline poll —
/// the main loop blocks on `recv` so an idle TUI consumes ~0% CPU
/// between ticks.
const TICK_RATE: Duration = Duration::from_millis(250);

/// Unified event funnel for the TUI run loop.
///
/// Every signal the loop reacts to — terminal input, periodic ticks,
/// daemon-state refreshes, chat-stream chunks, embed/rerank results,
/// HF-dialog updates, download progress — arrives on a single
/// `mpsc::Receiver<Event>`. The loop blocks on `recv` so an idle TUI
/// consumes no CPU; redraws happen only when an event landed or a
/// keystroke flips the dirty flag inside `pump_input_with_writer`.
///
/// Variant wrappers (rather than a flat enum) preserve subsystem
/// boundaries: `oai_client`, `hf_dialog`, `download_strip`, and the
/// tab modules keep their own narrow event enums and just construct
/// the wrapping `Event` at the push site.
#[derive(Debug)]
pub enum Event {
  /// Terminal input — key, mouse, paste, resize. Pushed by the
  /// background crossterm-poll thread.
  Input(TermEvent),
  /// Periodic wake when no input arrives within `TICK_RATE`.
  Tick,
  /// Daemon-state update (catalog, status, favorites, last-params,
  /// logs) plus the writer-task error surface. Pushed by
  /// `spawn_refresher`, `spawn_logs_poller`, and `spawn_writer`.
  Refresh(RefreshTick),
  /// Chat-completion stream chunk. Pushed by the
  /// `oai_client::spawn_chat_stream` helper.
  ChatStream(ChatStreamMsg),
  /// Embed or rerank one-shot result. Pushed by the per-tab
  /// `tokio::spawn` shim in `apply_embed_submit` / `apply_rerank_submit`.
  Tab(crate::tui::tabs::TabEvent),
  /// HF browser dialog update (search results, repo file listing).
  /// Pushed by `spawn_hf_search` and `spawn_hf_list_repo_files`.
  HfDialog(crate::tui::hf_dialog::HfDialogEvent),
  /// HF download progress / completion. Pushed by
  /// `spawn_download_task`.
  Download(crate::tui::download_strip::DownloadEvent),
}

/// Commands the input pump asks the writer task to forward to the
/// daemon. Keeping this enum narrow (vs. raw JSON) lets the type
/// system enforce that the input layer never assembles a malformed
/// request.
#[derive(Debug, Clone)]
pub enum WriterCmd {
  /// `start_model` — launch the focused model with the picker's
  /// ctx / reasoning / typed-knob overrides / extras / mode fields.
  /// `reasoning: None` (inside `args`) means "omit the field"; the
  /// daemon then falls back to whatever the model's metadata implies.
  /// Boxed (shared with `ConfirmAction::LaunchDuplicate`) so the large
  /// `crate::launch::knobs::KnobSet` payload doesn't bloat the other tiny variants.
  StartModel(Box<crate::tui::app::StartModelArgs>),
  /// `stop_model` — graceful shutdown of the supplied launch.
  /// Dispatched by the `s` hotkey when the cursor sits on a
  /// running managed row.
  StopModel { launch_id: String },
  /// `shutdown` — ask the daemon itself to exit. Dispatched by
  /// the `Q` hotkey after the user confirms the popup.
  Shutdown,
  /// `Ctrl+R:restart daemon` — shut the running daemon down and
  /// re-spawn a fresh one with the same options. Dispatched by
  /// the `Ctrl+R` hotkey after the user confirms the popup.
  RestartDaemon,
  /// `favorite_add` for the supplied model path. The TUI flips its
  /// local view optimistically; an RPC failure is surfaced via the
  /// writer task's `warn!` log and the next `favorite_list` refresh
  /// snaps the row back to daemon truth.
  FavoriteAdd(PathBuf),
  /// `favorite_remove` for the supplied model path.
  FavoriteRemove(PathBuf),
  /// `presets_save` — write the captured launch settings to
  /// `config.yaml` under `name` for `model_path` (`Ctrl+P`). `knobs`
  /// carries ctx/reasoning folded in plus any Auto markers; `extras` is
  /// the argv tail; `backend` / `server` pin the launch identity (which
  /// engine and which build the preset was captured against). The next
  /// `presets_all` refresh reflects it. Boxed (like `StartModel`) so the
  /// large `crate::launch::knobs::KnobSet` doesn't bloat the other tiny variants.
  SavePreset(Box<SavePresetCmd>),
}

/// Payload for [`WriterCmd::SavePreset`].
#[derive(Debug, Clone)]
pub struct SavePresetCmd {
  pub model_path: PathBuf,
  pub name: String,
  pub knobs: crate::launch::knobs::KnobSet,
  pub extras: Vec<String>,
  /// Backend this preset pins — launch identity, like the CLI's
  /// `presets save` sends. `None` leaves the daemon to re-derive it.
  pub backend: Option<String>,
  /// Server (build/binary) this preset pins — identity, like `backend`.
  pub server: Option<String>,
}

/// One pump of input events. Returns `true` when the App is asking
/// the loop to exit (the user pressed `q` / Ctrl+C). The `writer`
/// channel is optional so unit tests and the inline test backend
/// drive `pump_input` without spinning a daemon writer task.
pub fn pump_input(app: &mut App, evt: TermEvent) -> bool {
  pump_input_with_writer(app, evt, None)
}

/// Variant of [`pump_input`] that hands a writer-channel handle into
/// the action dispatch. Used by the production [`run`] loop so
/// `Submit` on the launch picker actually dispatches `start_model`.
pub fn pump_input_with_writer(
  app: &mut App,
  evt: TermEvent,
  writer: Option<&mpsc::Sender<WriterCmd>>,
) -> bool {
  match evt {
    TermEvent::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key, writer),
    TermEvent::Mouse(m) => handle_mouse(app, m, writer),
    _ => {}
  }
  app.should_exit
}

/// Mouse-event dispatch. Active only when the user opted in via
/// `mouse_focus: true` in `config.yaml` or `--mouse-focus`. The
/// input thread filters out drag / motion / button-up at the
/// source so only `Down(Left)` and the two wheel kinds reach here.
///
/// The contract:
/// - `Down(Left)` moves focus / switches tab as described under
///   [`handle_mouse_click`].
/// - Wheel up/down is a verbatim replay of `Action::MoveUp` /
///   `Action::MoveDown` — i.e. whatever `↑` / `↓` would do in the
///   current focus. That keeps the wheel and the arrow keys
///   identical in every flow, with no extra contract to remember.
/// - Mouse events while a true modal owns input (`hf_dialog`,
///   `confirm_dialog`, the help overlay) are ignored — those flows
///   own their own dismissal contract and a stray press must not
///   be able to confirm a destructive action. `launch_picker` is
///   intentionally *not* in this gate: the picker is inlined into
///   the Settings tab, not a modal, and gating it here would lock
///   out the mouse the moment a wheel-driven field cycle auto-
///   materialised the picker (the previous bug — the TUI looked
///   hung even though keyboard input still worked).
fn handle_mouse(
  app: &mut App,
  m: crossterm::event::MouseEvent,
  writer: Option<&mpsc::Sender<WriterCmd>>,
) {
  use crossterm::event::{MouseButton, MouseEventKind};
  if app.hf_dialog.is_some()
    || app.confirm_dialog.is_some()
    || app.save_preset_dialog.is_some()
    || app.show_help
  {
    return;
  }
  match m.kind {
    MouseEventKind::Down(MouseButton::Left) => handle_mouse_click(app, m.column, m.row),
    MouseEventKind::ScrollUp => apply_action(app, Action::MoveUp, writer),
    MouseEventKind::ScrollDown => apply_action(app, Action::MoveDown, writer),
    _ => {}
  }
}

/// Apply a left-click at `(x, y)`. Right-pane tab strip wins over
/// the right-pane body which wins over the Models list; hits outside
/// every tracked rect are dropped silently.
fn handle_mouse_click(app: &mut App, x: u16, y: u16) {
  let hits = app.hit_rects.borrow().clone();
  for (tab, rect) in &hits.right_tabs {
    if point_in_rect(x, y, *rect) {
      app.right_tab = *tab;
      app.focus = Focus::RightPane;
      return;
    }
  }
  if point_in_rect(x, y, hits.right_pane) {
    app.focus = Focus::RightPane;
    return;
  }
  if point_in_rect(x, y, hits.list_pane) {
    app.focus = Focus::List;
  }
}

/// `true` when `(x, y)` falls inside `rect`. An empty rect (width or
/// height == 0) never matches — the renderer uses that to signal
/// "this surface isn't on screen this frame" (e.g. the right pane
/// when it's hidden).
fn point_in_rect(x: u16, y: u16, rect: ratatui::layout::Rect) -> bool {
  if rect.width == 0 || rect.height == 0 {
    return false;
  }
  x >= rect.x
    && x < rect.x.saturating_add(rect.width)
    && y >= rect.y
    && y < rect.y.saturating_add(rect.height)
}

/// Top-level key dispatcher.
///
/// **Esc walk-back precedence (R3 / item-8 contract):** the user
/// expects a single `Esc` to peel one layer off the navigation
/// tree, no matter where they are. The order below resolves
/// ambiguities so the highest-priority surface owns the chord:
/// 1. Help overlay open → close the overlay (this function).
/// 2. Confirm popup open → cancel (this function).
/// 3. HF dialog open → stage walk-back (`handle_hf_dialog_input`).
/// 4. Modal text input editing → exit edit (`InputField::handle_key`
///    inside each focus handler).
/// 5. Modal text input resting with content → clear buffer.
/// 6. Modal text input empty → close the input or step focus back.
/// 7. `RightPane` focused → `Action::FocusList` returns to the list.
/// 8. `List` focused → no-op (already at the root).
///
/// Layers 4–6 live inside the input field's state machine and only
/// apply to inputs that have been migrated to [`InputField`]
/// (currently `filter_input` and the HF dialog search field; the
/// chat / embed / rerank composers + advanced-panel extras input
/// migrate in a follow-up).
fn handle_key(app: &mut App, key: KeyEvent, writer: Option<&mpsc::Sender<WriterCmd>>) {
  // Help dialog owns Esc and `?` ahead of every focus-specific
  // routing: when it's open, the user expects Esc to dismiss it
  // even if they were in the middle of typing into a filter or
  // chat prompt. Motion keys scroll the overlay so it stays
  // readable on terminals too short to fit every category.
  if app.show_help {
    match key.code {
      KeyCode::Esc | KeyCode::Char('?') => {
        app.show_help = false;
        app.help_scroll = 0;
        return;
      }
      KeyCode::Down | KeyCode::Char('j') => {
        app.help_scroll = app.help_scroll.saturating_add(1);
        return;
      }
      KeyCode::Up | KeyCode::Char('k') => {
        app.help_scroll = app.help_scroll.saturating_sub(1);
        return;
      }
      KeyCode::PageDown => {
        app.help_scroll = app.help_scroll.saturating_add(10);
        return;
      }
      KeyCode::PageUp => {
        app.help_scroll = app.help_scroll.saturating_sub(10);
        return;
      }
      KeyCode::Home => {
        app.help_scroll = 0;
        return;
      }
      _ => {}
    }
  }
  // Confirmation dialog steals all input. Submit (default `Enter`)
  // or `y` / `Y` confirms; everything else cancels. We treat
  // `n` / `N` as the named cancel keys so the popup's "Esc / n
  // cancel" hint matches an intentional binding, while every other
  // key still cancels via the foot-gun-resistant catchall so a
  // stray keypress can't drop a running model or kill the daemon.
  if app.confirm_dialog.is_some() {
    let bound = app.action_for(Focus::ConfirmPopup, key.code, key.modifiers);
    let confirmed = matches!(bound, Some(Action::Submit))
      || matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
    if confirmed {
      let pending = app.confirm_dialog.take();
      if let Some(action) = pending {
        apply_confirmed(app, action, writer);
      }
    } else {
      app.confirm_dialog = None;
    }
    return;
  }
  // Save-preset dialog steals input while open (text entry → overwrite
  // confirm). Routed here, ahead of focus dispatch, like the HF dialog.
  if app.save_preset_dialog.is_some() {
    handle_save_preset_input(app, key, writer);
    return;
  }
  // An open Settings inline edit owns input. All keys route to the
  // editor (so `Esc` cancels the edit, `e` inserts a literal `e`,
  // arrows do nothing rather than firing global actions, etc.) — only
  // `Enter` falls through so `Action::Submit` can run its
  // commit-then-launch flow in one place.
  if app.focus == Focus::RightPane
    && settings_inline_edit_open(app)
    && !matches!(key.code, KeyCode::Enter)
  {
    handle_settings_inline_edit(app, key);
    return;
  }
  // Vim `g`-prefix dispatcher. `g` in LIST or RIGHT_PANE focus queues
  // the prefix; in LIST the canonical `g → GoTop` still fires
  // immediately so the single-stroke vim motion isn't laggy. After
  // `g` is queued, the next key resolves:
  //   - `t`        → `Action::NextFocus` (same as `Tab` / `l`)
  //   - `T` (⇧t)   → `Action::PrevFocus` (same as `Shift+Tab` / `h`)
  //   - anything   → drop the prefix and fall through to normal dispatch
  // gt/gT are vim-flavored aliases for the full Tab cycle, not a
  // separate right-pane-tabs-only walker. Side-effect: `gt` from LIST
  // also moves the list cursor to top (the queued GoTop fired on the
  // first `g`), then focus advances — harmless.
  if app.pending_g_prefix {
    app.pending_g_prefix = false;
    match (key.code, key.modifiers) {
      (KeyCode::Char('t'), KeyModifiers::NONE) => {
        apply_action(app, Action::NextFocus, writer);
        return;
      }
      (KeyCode::Char('T'), KeyModifiers::SHIFT) => {
        apply_action(app, Action::PrevFocus, writer);
        return;
      }
      _ => {}
    }
  }
  if matches!(app.focus, Focus::List | Focus::RightPane)
    && matches!(key.code, KeyCode::Char('g'))
    && key.modifiers == KeyModifiers::NONE
  {
    app.pending_g_prefix = true;
    // In LIST, `g` is canonically bound to GoTop — fire it
    // immediately so the single-stroke motion stays snappy. In
    // RightPane, `g` is unbound and just sets the prefix.
    if app.focus == Focus::List {
      apply_action(app, Action::GoTop, writer);
    }
    return;
  }
  // Resolve the bound action first; if a focus doesn't have a binding
  // for this keypress *and* it's a text-input focus, fall through to
  // the per-focus character handler so alphanumerics extend the
  // buffer instead of being silently dropped.
  let bound = app.action_for(app.focus, key.code, key.modifiers);
  match app.focus {
    Focus::Filter => handle_filter_input(app, key),
    Focus::HfDialog => handle_hf_dialog_input(app, key, writer),
    Focus::RightPane if settings_inline_edit_open(app) && bound.is_none() => {
      handle_settings_inline_edit(app, key);
    }
    Focus::ChatInput | Focus::EmbedInput | Focus::RerankInput => {
      // Modal text-input focuses give the field first crack at every
      // key so the `Esc` walk-back (exit-edit → clear → close) wins
      // over the static action binding. The field returns `false`
      // (PassThrough) for Tab / Shift+Enter / final-Esc-at-root so
      // those still dispatch through `apply_action`.
      if !handle_tab_input(app, key) {
        if let Some(action) = bound {
          apply_action(app, action, writer);
        }
      }
    }
    _ => {
      if let Some(action) = bound {
        apply_action(app, action, writer);
      }
    }
  }
}

/// Open the inline edit on the focused Settings row.
///
/// Seeds the buffer with the row's current effective value so the user types
/// from what they see rather than from blank. Rows with nothing to type into
/// (the two selector rows, and boolean knobs, which cycle) are filtered out by
/// [`LaunchPickerState::focused_is_editable`] before we get here.
fn open_focused_inline_edit(app: &mut App) {
  use crate::tui::launch_picker::PickerField;
  let Some(picker) = app.launch_picker.as_mut() else {
    return;
  };
  if !picker.focused_is_editable() {
    return;
  }
  match picker.field {
    PickerField::Knob(id) => {
      let initial = picker.buffer_seed(id);
      picker.inline_edit.open(PickerField::Knob(id), initial);
    }
    PickerField::Extras => {
      let joined = picker
        .extras
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
      picker.extras_input.set_text(joined);
      picker.extras_input.enter_edit();
    }
    // Filtered out by the `focused_is_editable` guard above.
    PickerField::Preset | PickerField::Server => {}
  }
}

/// Commit the open inline edit. Returns true when a commit closed
/// the editor — caller can then proceed to a `Submit` action.
///
/// Validation is the knob's own: [`LaunchPickerState::commit_text`] runs the
/// same `parse_value` the CLI does, so a value the editor accepts is exactly
/// one `start --flag` would, and the error text under the row is the one the
/// CLI prints.
fn commit_inline_edit(app: &mut App) -> bool {
  use crate::tui::launch_picker::PickerField;
  let Some(picker) = app.launch_picker.as_mut() else {
    return false;
  };
  if picker.extras_input.is_editing() {
    let extras: Vec<std::ffi::OsString> = picker
      .extras_input
      .buffer()
      .split_whitespace()
      .map(std::ffi::OsString::from)
      .collect();
    picker.extras = extras;
    picker.extras_input.clear();
    picker.extras_input.exit_edit();
    return true;
  }
  let Some(field) = picker.inline_edit.field else {
    return false;
  };
  let buffer = picker.inline_edit.input.buffer().trim().to_string();
  // An empty buffer resets the row to inherit from the resolver chain — the
  // same semantics as Backspace, reached through `e → delete → Enter`.
  let result: Result<(), String> = match field {
    PickerField::Knob(id) => picker.commit_text(id, &buffer),
    // Never open an inline edit, so never reach a commit.
    PickerField::Preset | PickerField::Server | PickerField::Extras => Ok(()),
  };
  match result {
    Ok(()) => {
      picker.inline_edit.close();
      true
    }
    Err(msg) => {
      picker.inline_edit.error = Some(msg);
      false
    }
  }
}

fn settings_inline_edit_open(app: &App) -> bool {
  app.right_tab == RightTab::Settings
    && app
      .launch_picker
      .as_ref()
      .map(|p| p.inline_edit.is_open() || p.extras_input.is_editing())
      .unwrap_or(false)
}

fn handle_settings_inline_edit(app: &mut App, key: KeyEvent) {
  use crate::tui::input_field::InputOutcome;
  let Some(picker) = app.launch_picker.as_mut() else {
    return;
  };
  // Route the key through the same `InputField` modal state machine
  // that drives chat / embed / rerank / HF search / filter — so the
  // typed-knob inline edit honours the same `e:edit / Esc:walk-back /
  // Enter:Submit` contract uniformly.
  //
  // The error field is sticky from the previous commit attempt;
  // any keystroke that the input handles (a fresh char, backspace,
  // exit-edit) clears it so the user sees their next attempt
  // unobstructed.
  let outcome = if picker.extras_input.is_editing() {
    picker.extras_input.handle_key(key)
  } else {
    let r = picker.inline_edit.input.handle_key(key);
    if matches!(r, InputOutcome::Handled) {
      picker.inline_edit.error = None;
    }
    r
  };
  match outcome {
    InputOutcome::Handled => {}
    InputOutcome::Submit => {
      commit_inline_edit(app);
    }
    InputOutcome::PassThrough => {
      // `InputField` reports PassThrough for `Esc` once the buffer
      // is empty / edit mode is already off — for the picker that
      // means "close the inline edit entirely". The extras row also
      // walks back to the picker, not to the model list, since the
      // picker is still staged.
      if matches!(key.code, KeyCode::Esc) {
        if picker.extras_input.is_editing() {
          picker.extras_input.exit_edit();
          picker.extras_input.clear();
        } else {
          picker.inline_edit.close();
        }
      }
    }
  }
}

/// Text-capture handler for the chat / embed / rerank prompt
/// buffers. Each tab's input is a modal
/// [`crate::tui::input_field::InputField`]. The dispatcher routes
/// keys here ahead of the action layer so the field's `Esc`
/// walk-back (exit-edit → clear → close) wins over the static
/// `Esc:exit_edit` binding. Keys the field declines
/// (`InputOutcome::PassThrough`) fall through to the bound
/// action — Tab cycles fields, Shift+Enter inserts a newline,
/// final-Esc-at-root triggers `Action::ExitEdit`.
///
/// Returns `true` when the field consumed the key — caller skips
/// the action-layer dispatch in that case.
fn handle_tab_input(app: &mut App, key: KeyEvent) -> bool {
  use crate::tui::input_field::InputOutcome;
  let outcome = match app.focus {
    Focus::ChatInput => app.chat.prompt.handle_key(key),
    Focus::EmbedInput => app.embed.input.handle_key(key),
    Focus::RerankInput => match app.rerank.field {
      RerankField::Query => app.rerank.query.handle_key(key),
      RerankField::Candidate => app.rerank.candidate_buffer.handle_key(key),
    },
    _ => return false,
  };
  match outcome {
    InputOutcome::Handled => true,
    InputOutcome::Submit => {
      match app.focus {
        Focus::ChatInput => apply_send_chat(app),
        Focus::EmbedInput => apply_embed_submit(app),
        Focus::RerankInput => apply_rerank_submit(app),
        _ => {}
      }
      true
    }
    InputOutcome::PassThrough => false,
  }
}

fn handle_filter_input(app: &mut App, key: KeyEvent) {
  use crate::tui::input_field::InputOutcome;
  match app.filter_input.handle_key(key) {
    InputOutcome::Handled => {}
    InputOutcome::Submit => {
      // Filter is a *live* predicate (filter_input.buffer() applies
      // on every keystroke via `rendered_rows()`), so Enter carries
      // no "apply" semantics — it drills into the focused result
      // row by opening the launch picker. Exit edit first so the
      // user's typing doesn't continue feeding the filter once
      // focus moves to the right pane; when no row is focused
      // (header / empty result set) just drop back to the list
      // with the filter buffer intact.
      app.filter_input.exit_edit();
      app.focus = Focus::List;
      // `drill_into_focused_model` is the right semantic here:
      // running rows show the read-only view (no auto-stage), idle
      // rows stage the picker so the next Enter launches.
      app.drill_into_focused_model();
    }
    InputOutcome::PassThrough => match key.code {
      // Resting + empty buffer + Esc → close the filter entirely
      // (back to the list). Other resting Esc cases are handled
      // inside the InputField (clears the buffer).
      KeyCode::Esc => app.clear_filter(),
      // Arrow / vi-aliased navigation while the filter is focused
      // must still move the list cursor so the user can scroll the
      // filtered results without leaving the filter focus. The
      // InputField passes arrows through in both editing and
      // resting modes (no in-buffer cursor model), so this is the
      // single place that wires the gesture. `j`/`k` only fire in
      // resting mode — the InputField captures them as typed chars
      // while editing.
      KeyCode::Up | KeyCode::Char('k') => app.move_up(),
      KeyCode::Down | KeyCode::Char('j') => app.move_down(),
      KeyCode::PageUp => app.move_by(-10),
      KeyCode::PageDown => app.move_by(10),
      _ => {}
    },
  }
}

fn apply_action(app: &mut App, action: Action, writer: Option<&mpsc::Sender<WriterCmd>>) {
  match action {
    Action::Quit => app.should_exit = true,
    Action::MoveDown => apply_arrow_in_pane(app, ArrowDir::Down),
    Action::MoveUp => apply_arrow_in_pane(app, ArrowDir::Up),
    Action::PageUp => app.move_by(-10),
    Action::PageDown => app.move_by(10),
    Action::GoTop => app.go_top(),
    Action::GoBottom => app.go_bottom(),
    Action::OpenFilter => app.open_filter(),
    Action::ClearFilter => app.clear_filter(),
    Action::ToggleFavorite => apply_toggle_favorite(app, writer),
    Action::OpenLaunchPicker => app.drill_into_focused_model(),
    Action::OpenHfDialog => apply_open_hf_dialog(app),
    Action::Submit => match app.focus {
      Focus::EmbedInput => apply_embed_submit(app),
      Focus::RerankInput => apply_rerank_submit(app),
      Focus::RightPane if app.right_tab == RightTab::Settings => {
        // If an inline edit is open, Enter commits it first. Only
        // proceed to launch when the commit succeeded (otherwise the
        // edit stays open with the inline error visible).
        if settings_inline_edit_open(app) {
          if commit_inline_edit(app) {
            // Commit closed the edit; the user can press Enter
            // again to launch with the new value.
            return;
          }
          return;
        }
        apply_launch_submit(app, writer);
      }
      _ => {}
    },
    Action::Cancel => {
      if app.show_help {
        app.show_help = false;
      } else if app.focus == Focus::RightPane
        && app.right_tab == RightTab::Settings
        && app.launch_picker.is_some()
        && app.focused_managed().is_some()
      {
        app.launch_picker = None;
      }
    }
    Action::YankUrl | Action::YankCurl | Action::YankPath => {
      // `c` doubles up on the Logs tab: copy the full log buffer
      // instead of the curl one-liner. Mirrors the tab-aware
      // dispatch already in place for `s` (ToggleAutoScroll). Any
      // other tab (Settings / Chat / Embed / Rerank) falls through
      // to the original yank handler.
      if matches!(action, Action::YankCurl)
        && app.focus == Focus::RightPane
        && app.right_tab == RightTab::Logs
      {
        let lines = &app.logs_state.lines;
        if lines.is_empty() {
          app.show_toast("no log lines yet");
        } else {
          let n = lines.len();
          let text = lines.join("\n");
          match clipboard::write(&text) {
            Ok(_) => app.show_toast(format!("copied logs ({n} lines)")),
            Err(e) => app.show_error_toast(format!("clipboard unavailable: {e}")),
          }
        }
        return;
      }
      let text = build_yank_text(app, action);
      if let Some(text) = text {
        let label = match action {
          Action::YankUrl => "URL",
          Action::YankCurl => "curl",
          Action::YankPath => "path",
          _ => "",
        };
        match clipboard::write(&text) {
          Ok(_) => app.show_toast(format!("copied {label}")),
          Err(e) => {
            // Curl payloads are long enough to drown the toast on a
            // clipboard failure; trim aggressively for those while
            // keeping URLs / paths intact (they're already short).
            let preview = if text.len() > 80 {
              format!("{}…", &text[..80])
            } else {
              text
            };
            app.show_error_toast(format!("clipboard unavailable: {e}; {preview}"));
          }
        }
      } else {
        // `c` (yank-curl) is the only path that strictly requires a
        // Ready model; the smart `y/c` fallback yields a string for
        // any focused row, so this branch only fires for `c`.
        app.show_toast("nothing to copy — focus a Ready model");
      }
    }
    Action::CycleTheme => {
      app.cycle_theme();
      app.show_toast(format!("theme → {}", app.options.theme.canonical()));
    }
    Action::CycleThemePrev => {
      app.cycle_theme_prev();
      app.show_toast(format!("theme → {}", app.options.theme.canonical()));
    }
    // No toast: the split visibly changes, so a toast would be redundant noise.
    Action::CyclePaneRatio => app.cycle_left_pane_ratio(),
    Action::ToggleHelp => app.toggle_help(),
    Action::FocusList => {
      // When `e` staged an edit-for-launch picker over a running
      // model's read-only Settings view, Esc-on-RightPane discards
      // the staging back to the live params display instead of
      // leaving the right pane entirely. A second Esc (or any
      // FocusList press once the picker is gone) then jumps to the
      // Models list — same as before. We can't call
      // `close_launch_picker` here because that helper also flips
      // focus back to `List`, which is precisely what we want to
      // suppress in the edit-over-running case.
      if app.focus == Focus::RightPane
        && app.right_tab == RightTab::Settings
        && app.launch_picker.is_some()
        && app.focused_managed().is_some()
      {
        app.launch_picker = None;
      } else {
        app.focus = Focus::List;
      }
    }
    Action::NextFocus => cycle_focus(app, FocusDir::Next),
    Action::PrevFocus => cycle_focus(app, FocusDir::Prev),
    Action::SendChat => apply_send_chat(app),
    Action::ToggleThinkCollapse => {
      // `r` is bound across the right pane but only meaningful on the
      // Chat tab; ignore on Logs/Settings/Embed/Rerank so a stray
      // press doesn't silently flip hidden state.
      if app.right_tab == RightTab::Chat {
        app.chat.collapse_thinks = !app.chat.collapse_thinks;
        // Toast the resulting state so the flip isn't silent — the
        // reasoning blocks may be off-screen when `r` is pressed.
        let msg = if app.chat.collapse_thinks {
          "reasoning collapsed"
        } else {
          "reasoning shown"
        };
        app.show_toast(msg);
      }
    }
    Action::ToggleAutoScroll => {
      // `s` toggles the Logs auto-scroll. Stop lives on `Ctrl+S`
      // (destructive policy) so bare `s` has a single meaning.
      if app.right_tab == RightTab::Logs {
        app.logs_state.auto_scroll = !app.logs_state.auto_scroll;
        let msg = if app.logs_state.auto_scroll {
          "auto-scroll on"
        } else {
          "auto-scroll off"
        };
        app.show_toast(msg);
      }
    }
    Action::StageRerankCandidate => {
      if app.rerank.field == RerankField::Candidate {
        app.rerank.stage_candidate();
      } else {
        app.rerank.cycle_field();
      }
    }
    Action::StopModel => apply_stop_model(app),
    Action::KillDaemon => {
      app.confirm_dialog = Some(ConfirmAction::KillDaemon);
    }
    Action::RestartDaemon => {
      app.confirm_dialog = Some(ConfirmAction::RestartDaemon);
    }
    Action::DeleteModel => apply_delete_model(app),
    Action::CancelDownload => apply_cancel_download(app),
    Action::SavePreset => {
      // Settings pane always saves (a running model's live knobs, or the
      // about-to-launch form). The Models list only saves from a running
      // row — an idle row there has no concrete settings to pin yet.
      if app.focused_path().is_none() {
        app.show_toast("no model focused — nothing to save");
      } else if app.focus == Focus::List && app.focused_managed().is_none() {
        app.show_toast("open Settings to save launch params, or pick a running model");
      } else {
        app.open_save_preset_dialog();
      }
    }
    Action::EnterEdit => {
      // Tab-aware:
      //  - Chat / Embed / Rerank: shift focus into the input buffer
      //    so subsequent keystrokes go to the prompt. The field
      //    itself enters edit mode so typing works immediately and
      //    the `Esc` walk-back is wired up.
      //  - Settings on a running launch with no picker yet: stage
      //    the launch picker so the user can edit next-launch params
      //    over the live read-only view (the arrow-keys path no
      //    longer auto-stages — `e` is the explicit gate).
      //  - Settings on the editable form: open the focused row's
      //    inline edit (numeric / enum row → typing buffer; extras
      //    row → free-text horizontal-scroll buffer).
      if let Some(target) = edit_focus_for_tab(app.right_tab) {
        app.focus = target;
        match target {
          Focus::ChatInput => app.chat.prompt.enter_edit(),
          Focus::EmbedInput => app.embed.input.enter_edit(),
          Focus::RerankInput => match app.rerank.field {
            RerankField::Query => app.rerank.query.enter_edit(),
            RerankField::Candidate => app.rerank.candidate_buffer.enter_edit(),
          },
          _ => {}
        }
      } else if app.right_tab == RightTab::Settings {
        if app.launch_picker.is_none() {
          if app.focused_path().is_some() {
            app.open_launch_picker();
          } else {
            app.show_toast("no model focused");
          }
        } else {
          open_focused_inline_edit(app);
        }
      }
    }
    Action::ExitEdit => {
      // Final Esc at the input root walks one step further back:
      // exit edit on whatever field was active and return focus to
      // the right-pane chain so Tab/Shift+Tab/h/l resume working.
      match app.focus {
        Focus::ChatInput => app.chat.prompt.exit_edit(),
        Focus::EmbedInput => app.embed.input.exit_edit(),
        Focus::RerankInput => {
          app.rerank.query.exit_edit();
          app.rerank.candidate_buffer.exit_edit();
        }
        _ => {}
      }
      app.focus = Focus::RightPane;
    }
    Action::FocusLogsTab => apply_focus_logs_tab(app),
    Action::FocusChatTab => apply_focus_chat_tab(app),
    Action::FocusSettingsTab => apply_focus_settings_tab(app),
    Action::InsertNewline => {
      // Force-insert a newline into whichever modal field is in
      // focus. Skips the input component's modifier filter so
      // Shift+Enter still works even when the field is resting
      // (resting + Shift+Enter would otherwise PassThrough and
      // hit nothing).
      fn push_newline(field: &mut crate::tui::input_field::InputField) {
        let mut next = String::from(field.buffer());
        next.push('\n');
        let editing = field.is_editing();
        field.set_text(next);
        if editing {
          field.enter_edit();
        }
      }
      match app.focus {
        Focus::ChatInput => push_newline(&mut app.chat.prompt),
        Focus::EmbedInput => push_newline(&mut app.embed.input),
        Focus::RerankInput => match app.rerank.field {
          RerankField::Query => push_newline(&mut app.rerank.query),
          RerankField::Candidate => push_newline(&mut app.rerank.candidate_buffer),
        },
        _ => {}
      }
    }
    // ↑/↓ cycle the cursor across the form's input fields. Only
    // meaningful in the Settings tab (cycles ctx / reasoning /
    // advanced) and the Rerank input (cycles query / candidate).
    // Elsewhere it's a no-op so the chord doesn't accidentally
    // double as something else.
    Action::NextField => apply_next_field(app),
    Action::PrevField => apply_prev_field(app),
    // ←/→ change the focused field's value in the Settings tab.
    // Only the Settings tab dispatches them; outside Settings the
    // keys stay unbound so they don't double as pane navigation.
    Action::CycleValueNext => apply_cycle_value(app, ValueDir::Next),
    Action::CycleValuePrev => apply_cycle_value(app, ValueDir::Prev),
    // Space toggles the focused GPU on the picker's multi-GPU Device row.
    Action::ToggleDevice => apply_toggle_device(app),
    // HF dialog stage chords (`o`, `n`, `p`) dispatch via the dialog's
    // own per-stage handler in `handle_hf_dialog_input`. The Action
    // variants exist only so the help overlay can list them — if one
    // ever escapes to the generic dispatcher, it's a no-op.
    Action::HfCycleSort | Action::HfNextPage | Action::HfPrevPage => {}
  }
}

enum ValueDir {
  Next,
  Prev,
}

#[derive(Clone, Copy)]
enum ArrowDir {
  Up,
  Down,
}

/// Per-pane policy for what an ↑/↓ arrow does. Centralises the five
/// near-identical match arms `Action::MoveUp` and `Action::MoveDown`
/// used to spell out.
fn apply_arrow_in_pane(app: &mut App, dir: ArrowDir) {
  match app.focus {
    Focus::RightPane => match app.right_tab {
      // Logs: scroll the log buffer.
      RightTab::Logs => match dir {
        ArrowDir::Up => app.logs_state.scroll_up(),
        ArrowDir::Down => app.logs_state.scroll_down(),
      },
      // Settings: cycle the form's fields when the picker is
      // editable; scroll the read-only running-launch view when the
      // focused model has a managed launch and no picker is staged.
      // Arrows have no field semantics in the running view, so
      // claiming them for scroll is collision-free and more
      // discoverable than asking users to remember PageUp/PageDown.
      RightTab::Settings => match dir {
        ArrowDir::Up => {
          if running_view_is_locked(app) {
            app
              .running_view_scroll
              .set(app.running_view_scroll.get().saturating_sub(1));
          } else {
            apply_prev_field(app)
          }
        }
        ArrowDir::Down => {
          if running_view_is_locked(app) {
            app
              .running_view_scroll
              .set(app.running_view_scroll.get().saturating_add(1));
          } else {
            apply_next_field(app)
          }
        }
      },
      // Round-8: Chat/Embed/Rerank output viewports scroll on the
      // same arrow keys as Logs while focus stays on the right
      // pane (no edit mode).
      RightTab::Chat => match dir {
        ArrowDir::Up => app.chat.scroll_up(),
        ArrowDir::Down => app.chat.scroll_down(),
      },
      RightTab::Embed => match dir {
        ArrowDir::Up => app.embed.scroll_up(),
        ArrowDir::Down => app.embed.scroll_down(),
      },
      RightTab::Rerank => match dir {
        ArrowDir::Up => app.rerank.scroll_up(),
        ArrowDir::Down => app.rerank.scroll_down(),
      },
    },
    // Composer focuses: ↑/↓ scroll the tab's output viewport. The
    // input field doesn't use arrows (no in-buffer cursor), so they
    // scroll the response area while focus stays on the prompt — which
    // is where the user sits right after asking a question. Rerank
    // keeps ↑/↓ for its query/candidate field cycle, so it is not
    // routed here (it scrolls via the right-pane focus instead).
    Focus::ChatInput => match dir {
      ArrowDir::Up => app.chat.scroll_up(),
      ArrowDir::Down => app.chat.scroll_down(),
    },
    Focus::EmbedInput => match dir {
      ArrowDir::Up => app.embed.scroll_up(),
      ArrowDir::Down => app.embed.scroll_down(),
    },
    _ => match dir {
      ArrowDir::Up => app.move_up(),
      ArrowDir::Down => app.move_down(),
    },
  }
}

fn apply_cycle_value(app: &mut App, dir: ValueDir) {
  if !(app.focus == Focus::RightPane && app.right_tab == RightTab::Settings) {
    return;
  }
  if running_view_is_locked(app) {
    return;
  }
  // the chip strip advertises `←/→:cycle value`
  // even when the focused field (e.g. Advanced) is non-cyclable.
  // Toast on miss so the user understands why nothing changed.
  with_picker(app, |p| match dir {
    ValueDir::Next => p.cycle_focused_value_next(),
    ValueDir::Prev => p.cycle_focused_value_prev(),
  });
  let cyclable = app
    .launch_picker
    .as_ref()
    .map(|p| p.focused_field_is_cyclable())
    .unwrap_or(false);
  if !cyclable {
    app.show_toast("nothing to cycle — focused field has no preset values");
  }
}

/// `Space` on the picker's Device row toggles the cursor GPU in/out of the
/// multi-select. Inert everywhere else — only fires in the Settings tab with the
/// editable picker focused on the (multi-GPU) Device row.
fn apply_toggle_device(app: &mut App) {
  use crate::tui::launch_picker::PickerField;
  if !(app.focus == Focus::RightPane && app.right_tab == RightTab::Settings) {
    return;
  }
  if running_view_is_locked(app) {
    return;
  }
  with_picker(app, |p| {
    // `Space` only means "toggle this GPU" on the row that owns the device
    // selection; the knob declares itself as that row.
    let on_device_row = matches!(p.field, PickerField::Knob(id)
      if p.def(id).is_some_and(|d| d.concept == Some(crate::launch::knobs::Concept::Device)));
    if on_device_row {
      p.toggle_focused_device();
    }
  });
}

/// Auto-materialise the inline Settings picker if absent, then run
/// `f` against it. Collapses the three
/// `if app.launch_picker.is_none() { app.open_launch_picker(); } if
/// let Some(p) = app.launch_picker.as_mut() { ... }` blocks.
fn with_picker<F: FnOnce(&mut crate::tui::launch_picker::LaunchPickerState)>(app: &mut App, f: F) {
  if app.launch_picker.is_none() {
    app.open_launch_picker();
  }
  if let Some(p) = app.launch_picker.as_mut() {
    f(p);
  }
}

fn apply_next_field(app: &mut App) {
  match app.focus {
    Focus::RerankInput => app.rerank.cycle_field(),
    Focus::RightPane if app.right_tab == RightTab::Settings => {
      if running_view_is_locked(app) {
        return;
      }
      with_picker(app, |p| p.next_field());
    }
    _ => {}
  }
}

fn apply_prev_field(app: &mut App) {
  match app.focus {
    // Rerank is a 2-field cycle, so prev and next land on the same
    // place. Calling `cycle_field` keeps the implementation honest
    // (one source of truth) rather than duplicating the toggle.
    Focus::RerankInput => app.rerank.cycle_field(),
    Focus::RightPane if app.right_tab == RightTab::Settings => {
      if running_view_is_locked(app) {
        return;
      }
      with_picker(app, |p| p.prev_field());
    }
    _ => {}
  }
}

/// True when the Settings tab is showing the read-only running-launch
/// view (focused row has a managed launch + no picker is staged) —
/// the case where arrow keys must NOT silently swap the pane to the
/// next-launch editor. The user originally saw the live params and
/// expected `↑/↓/←/→` to scroll or do nothing; auto-staging the
/// picker hid the running params behind the form and surprised them.
/// `e` (Action::EnterEdit) is the explicit opt-in to start editing.
fn running_view_is_locked(app: &App) -> bool {
  app.right_tab == RightTab::Settings
    && app.launch_picker.is_none()
    && app.focused_managed().is_some()
}

/// `L` quick-jump: park focus on the Logs tab when it's reachable.
/// Logs is only available for running launches, so we toast and
/// stay put for an unlaunched selection.
fn apply_focus_logs_tab(app: &mut App) {
  if app.available_right_tabs().contains(&RightTab::Logs) {
    app.right_tab = RightTab::Logs;
    app.focus = Focus::RightPane;
  } else {
    app.show_toast("Logs unavailable — focus a running model");
  }
}

/// `C` quick-jump: park focus on whichever mode-specific tab is
/// reachable for the focused model (Chat for chat models, Embed
/// for embedding models, Rerank for rerank models). Toasts when
/// the selection isn't a running model.
fn apply_focus_chat_tab(app: &mut App) {
  let tabs = app.available_right_tabs();
  let target = [RightTab::Chat, RightTab::Embed, RightTab::Rerank]
    .into_iter()
    .find(|t| tabs.contains(t));
  match target {
    Some(t) => {
      app.right_tab = t;
      app.focus = Focus::RightPane;
    }
    None => app.show_toast("Chat/Embed/Rerank unavailable — focus a running model"),
  }
}

/// `S` quick-jump: park focus on the Settings tab. Settings is
/// always reachable so this never fails — even on an empty
/// selection the renderer shows the editable launch form.
fn apply_focus_settings_tab(app: &mut App) {
  app.right_tab = RightTab::Settings;
  app.focus = Focus::RightPane;
}

/// Stage a stop-model confirmation. The actual `stop_model` RPC is
/// only dispatched after the user accepts the popup via
/// [`apply_confirmed`]. No-op when the cursor isn't on a running
/// row — surfaces a toast so the user understands why nothing
/// changed.
fn apply_stop_model(app: &mut App) {
  let managed = match app.focused_managed() {
    Some(m) => m,
    None => {
      app.show_toast("nothing to stop — focus a running model");
      return;
    }
  };
  let launch_id = managed.launch_id.clone();
  let path = managed.path.clone();
  let name = app.display_name_for(&path);
  app.confirm_dialog = Some(ConfirmAction::StopModel { launch_id, name });
}

/// Stage a delete-model confirmation. Refuses (with a toast) when
/// the focused row points at a file something else is actively
/// reading — a supervised managed launch, an external read-only
/// `llama-server`, or a row that's still in an Error/Loading/
/// Launching state with a pending file handle. The toast names the
/// reason so the user knows whether to wait, stop, or kill the
/// external owner.
fn apply_delete_model(app: &mut App) {
  let Some(path) = app.focused_path() else {
    app.show_toast("nothing to delete — focus a model row");
    return;
  };
  if let Some(reason) = delete_refusal_reason(app) {
    app.show_toast(reason);
    return;
  }
  let display_name = app.display_name_for(&path);
  let plan = Box::new(crate::tui::delete::plan_for_path(&path, &app.models));
  app.confirm_dialog = Some(ConfirmAction::DeleteModel { plan, display_name });
}

/// Returns the toast message describing why a delete must refuse on
/// the focused row, or `None` when the delete should be allowed.
/// Mirrors the chip-rendering rule in `render::focused_row_is_deletable`
/// so the hint and the keybinding stay in lock-step.
fn delete_refusal_reason(app: &App) -> Option<&'static str> {
  use crate::tui::status_icons::SurfaceState;
  // Lemonade registry models are served by the umbrella, not stored as a local
  // GGUF — there is nothing on disk to unlink, and llamastash does not manage
  // the Lemonade registry. Refuse with guidance instead of a failed `unlink`.
  if let Some(path) = app.focused_path() {
    if crate::backend::lemonade::registry_name_from_path(&path).is_some() {
      return Some("model is managed by Lemonade — delete it via Lemonade");
    }
  }
  if let Some(managed) = app.focused_managed() {
    return Some(match managed.state {
      SurfaceState::Ready | SurfaceState::Loading | SurfaceState::Launching => {
        "model is running — stop the launch first"
      }
      SurfaceState::Error => "launch is in error — stop it first, then delete",
      // Stopped/NotLaunched routes through the managed-table but is
      // free to delete; fall through.
      _ => return None,
    });
  }
  // External (read-only daemon-tracked process). Not in `managed`,
  // so we walk `external` by path.
  if app
    .external
    .iter()
    .any(|e| Some(&e.path) == app.focused_path().as_ref())
  {
    return Some("model is open in an external process — close it first");
  }
  None
}

/// Route a key to the open save-preset dialog. Name stage: typing edits
/// the buffer, `Enter` saves (or steps to the confirm stage when the name
/// already resolves — an overwrite or an arch shadow), `Esc` cancels.
/// Confirm stage: `Enter`/`y` commits, `Esc`/`n` returns to the name input.
fn handle_save_preset_input(
  app: &mut App,
  key: KeyEvent,
  writer: Option<&mpsc::Sender<WriterCmd>>,
) {
  use crate::tui::input_field::InputOutcome;
  use crate::tui::save_preset_dialog::SaveStage;
  let Some(dialog) = app.save_preset_dialog.as_mut() else {
    return;
  };
  match dialog.stage {
    SaveStage::Name => {
      // Esc cancels the whole dialog (ahead of the input's exit-edit).
      if matches!(key.code, KeyCode::Esc) {
        app.save_preset_dialog = None;
        return;
      }
      match dialog.input.handle_key(key) {
        InputOutcome::Submit => {
          if dialog.name().is_empty() {
            dialog.error = Some("name must not be empty".into());
            return;
          }
          dialog.error = None;
          if dialog.name_needs_confirm() {
            dialog.stage = SaveStage::Confirm;
            return;
          }
          // New name → fall through to commit (borrow released below).
        }
        InputOutcome::Handled => {
          dialog.error = None;
          return;
        }
        InputOutcome::PassThrough => return,
      }
    }
    SaveStage::Confirm => match key.code {
      KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {} // commit below
      KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
        dialog.stage = SaveStage::Name;
        return;
      }
      _ => return,
    },
  }
  commit_save_preset(app, writer);
}

/// Dispatch the captured preset to `presets_save` and close the dialog.
fn commit_save_preset(app: &mut App, writer: Option<&mpsc::Sender<WriterCmd>>) {
  let Some(dialog) = app.save_preset_dialog.take() else {
    return;
  };
  let name = dialog.name();
  dispatch_writer(
    app,
    writer,
    WriterCmd::SavePreset(Box::new(SavePresetCmd {
      model_path: dialog.model_path,
      name: name.clone(),
      knobs: dialog.knobs,
      extras: dialog.extras,
      backend: dialog.backend,
      server: dialog.server,
    })),
    format!("saved preset `{name}`"),
    "save failed — writer offline",
    "save unavailable — no daemon writer attached".into(),
  );
}

/// Apply a confirmed [`ConfirmAction`] — dispatches the writer
/// command and shows an outcome toast. Called from [`handle_key`]
/// when the user presses `y` / Enter in the confirm dialog.
fn apply_confirmed(app: &mut App, action: ConfirmAction, writer: Option<&mpsc::Sender<WriterCmd>>) {
  match action {
    ConfirmAction::StopModel { launch_id, name } => {
      dispatch_writer(
        app,
        writer,
        WriterCmd::StopModel {
          launch_id: launch_id.clone(),
        },
        format!("stopping {name}…"),
        "stop failed — writer offline",
        "stop unavailable — no daemon writer attached".into(),
      );
    }
    ConfirmAction::KillDaemon => {
      dispatch_writer(
        app,
        writer,
        WriterCmd::Shutdown,
        "daemon shutting down…".into(),
        "daemon shutdown failed — writer offline",
        "daemon shutdown unavailable — no writer attached".into(),
      );
    }
    ConfirmAction::RestartDaemon => {
      dispatch_writer(
        app,
        writer,
        WriterCmd::RestartDaemon,
        "daemon restarting…".into(),
        "daemon restart failed — writer offline",
        "daemon restart unavailable — no writer attached".into(),
      );
    }
    ConfirmAction::DeleteModel { plan, display_name } => {
      match crate::tui::delete::execute(&plan) {
        Ok(_summary) => {
          // Drop every row the plan removed from the cached model list so
          // the next render doesn't flash a stale entry — a whole-repo
          // delete takes other revisions of the same repo with it. A real
          // refresh re-discovers on the next tick.
          app.models.retain(|m| {
            m.path != plan.primary
              && plan
                .hf_repo_dir
                .as_ref()
                .is_none_or(|repo| !m.path.starts_with(repo))
          });
          if app.list_cursor >= app.models.len() {
            app.list_cursor = app.models.len().saturating_sub(1);
          }
          app.show_toast(format!("deleted {display_name}"));
        }
        Err(e) => app.show_error_toast(format!("delete failed: {e}")),
      }
    }
    ConfirmAction::CancelDownload { .. } => {
      use crate::tui::download_strip::CancelOutcome;
      match app.download_strip.cancel_active() {
        CancelOutcome::NothingActive => {
          // Race between the popup being staged and the active pull
          // finishing on its own. No-op + low-key toast so the user
          // understands the confirm landed.
          app.show_toast("download already finished");
        }
        CancelOutcome::Cancelled {
          cancelled_friendly_name,
          next,
          ..
        } => {
          app.show_toast(format!("cancelled {cancelled_friendly_name}"));
          if let Some(promoted) = next {
            app.download_strip.install_active(&promoted);
            if let Some(tx) = app.events_tx.clone() {
              let abort = spawn_download_task(promoted, app.options.offline, tx);
              app.download_strip.active_abort = Some(abort);
            }
          }
        }
      }
    }
    ConfirmAction::LaunchDuplicate { name, args, .. } => {
      dispatch_launch(app, writer, WriterCmd::StartModel(args), name);
    }
  }
}

/// Small helper to centralise the writer-channel try_send +
/// toast-on-result pattern shared by stop-model and shutdown.
fn dispatch_writer(
  app: &mut App,
  writer: Option<&mpsc::Sender<WriterCmd>>,
  cmd: WriterCmd,
  ok_msg: String,
  err_msg: &'static str,
  no_writer_msg: String,
) {
  match writer {
    Some(tx) => match tx.try_send(cmd) {
      Ok(()) => app.show_toast(ok_msg),
      Err(_) => app.show_error_toast(err_msg.to_string()),
    },
    None => app.show_error_toast(no_writer_msg),
  }
}

// `TabEvent` moved to `tui::tabs::TabEvent` to close the circular
// import: tab modules now point downward to `tui::tabs` for the
// event type instead of reaching back up into `tui::events`.
pub use crate::tui::tabs::TabEvent;

#[derive(Clone, Copy)]
enum FocusDir {
  Next,
  Prev,
}

/// Walk one step in the focus chain `[List, ...available_right_tabs()]`.
/// `Tab`/`Right`/`l` step `Next`, `Shift+Tab`/`Left`/`h` step `Prev`.
/// The right pane is force-opened on entry so the user lands on a
/// visible target — the cursor may be on a not-yet-running model
/// when they Tab in, and the pane has to render its first frame.
fn cycle_focus(app: &mut App, dir: FocusDir) {
  let tabs = app.available_right_tabs();
  if tabs.is_empty() {
    app.focus = Focus::List;
    return;
  }
  // Build the chain: List is slot 0, each available tab follows.
  let chain_len = tabs.len() + 1;
  let current_pos: usize = match app.focus {
    Focus::List => 0,
    _ => tabs
      .iter()
      .position(|t| *t == app.right_tab)
      .map(|i| i + 1)
      .unwrap_or(0),
  };
  let next_pos = match dir {
    FocusDir::Next => (current_pos + 1) % chain_len,
    FocusDir::Prev => (current_pos + chain_len - 1) % chain_len,
  };
  if next_pos == 0 {
    app.focus = Focus::List;
  } else {
    let tab = tabs[next_pos - 1];
    app.right_tab = tab;
    // KDash-style edit mode: navigation lands on RightPane focus
    // regardless of which tab is active. The user presses `e` to
    // enter the tab's text-input focus (ChatInput/EmbedInput/
    // RerankInput) and `Esc` to step back out. The right pane is
    // always visible (it follows the cursor) so there's no force
    // -open flag to toggle.
    app.focus = Focus::RightPane;
  }
}

/// Edit-mode focus for a right-pane tab, when the user presses
/// `e` on a tab that captures text. `None` means the tab has no
/// editable surface (Logs / Settings), so `e` is a no-op there.
fn edit_focus_for_tab(tab: RightTab) -> Option<Focus> {
  match tab {
    RightTab::Chat => Some(Focus::ChatInput),
    RightTab::Embed => Some(Focus::EmbedInput),
    RightTab::Rerank => Some(Focus::RerankInput),
    RightTab::Logs | RightTab::Settings => None,
  }
}

/// Resolve the focused managed row or toast a context-specific
/// "no Ready model focused for `<action>`" message. The same lookup +
/// toast guard was duplicated across
/// `apply_send_chat`, `apply_embed_submit`, `apply_rerank_submit`
/// and the right-pane yank handlers.
fn focused_managed_or_toast(app: &mut App, action: &str) -> Option<crate::tui::app::ManagedRow> {
  match app.focused_managed() {
    Some(m) => Some(m.clone()),
    None => {
      app.show_toast(format!("no Ready model focused for {action}"));
      None
    }
  }
}

/// Clone the unified events channel for a spawn site that requires
/// it. Returns `None` in lib unit tests (which legitimately drive
/// `apply_*` without a tokio runtime) and panics in any other debug
/// build — a `None` reached outside `cfg(test)` means `run()` forgot
/// to prime `events_tx`, which would otherwise silently swallow the
/// submit. Release builds get the same `None` no-op so users never
/// see a TUI crash from a code-path bug.
fn require_events_tx(app: &App, op: &'static str) -> Option<mpsc::Sender<Event>> {
  let tx = app.events_tx.clone();
  if tx.is_none() {
    #[cfg(all(not(test), debug_assertions))]
    panic!("events_tx must be primed by run() before {op}");
    #[cfg(any(test, not(debug_assertions)))]
    let _ = op;
  }
  tx
}

/// The model name to put on an OpenAI request against a managed row's
/// port.
///
/// A backend that mints its own identity for the path owns the answer: that
/// name is exactly what it advertises on `/v1/models`. Deriving one from the
/// filename instead breaks two ways — `model_display_name`'s `file_stem`
/// mangles dotted registry names (`qwen3.5-4b-FLM` → `qwen3`), and for a
/// directory-shaped model it yields the snapshot's revision hash, which a
/// name-validating server rejects outright. Only a plain weight file falls
/// through to the filename.
fn served_model_name(path: &std::path::Path) -> String {
  if let Some((crate::backend::identity::ModelIdentity::Backend(id), _)) =
    crate::backend::synthetic_identity_for_path(path)
  {
    return id.name;
  }
  crate::util::paths::model_display_name(path)
}

/// Trigger an OpenAI streaming chat completion against the focused
/// Ready model. Stashes the receiver on `app.chat` so the render
/// loop can drain it without blocking input.
fn apply_send_chat(app: &mut App) {
  let Some(managed) = focused_managed_or_toast(app, "chat") else {
    return;
  };
  if app.chat.prompt.buffer().trim().is_empty() {
    app.show_toast("chat prompt is empty");
    return;
  }
  let prompt = app.chat.prompt.buffer().to_string();
  let model_name = served_model_name(&managed.path);
  app.chat.reset_for_send();
  if let Some(tx) = require_events_tx(app, "chat submit") {
    spawn_chat_stream(managed.port, model_name, prompt, tx);
  }
}

/// One-shot embedding call. Spawns a background task; the result is
/// captured straight into `app.embed` because `EmbedTabState` lives
/// on `App`.
fn apply_embed_submit(app: &mut App) {
  let Some(managed) = focused_managed_or_toast(app, "embed") else {
    return;
  };
  if app.embed.input.buffer().trim().is_empty() {
    app.show_toast("embed input is empty");
    return;
  }
  let input = app.embed.input.buffer().to_string();
  let model_name = served_model_name(&managed.path);
  app.embed.busy = true;
  let port = managed.port;
  if let Some(events_tx) = require_events_tx(app, "embed submit") {
    tokio::spawn(async move {
      let result = oai_embed(port, &model_name, &input).await;
      let evt = match result {
        Ok(r) => TabEvent::EmbedOk(r),
        Err(e) => TabEvent::EmbedErr(e),
      };
      let _ = events_tx.send(Event::Tab(evt)).await;
    });
  }
}

/// Dispatch Enter on the Rerank tab. Behaviour branches on the
/// focused sub-field:
///
/// - Candidate field → stage the buffer onto the candidates list.
///   Stays in the candidate field so the user can keep typing the
///   next candidate without an extra ↓ press. Empty buffers toast
///   and stay put.
/// - Query field → fire the actual `/v1/rerank` call. Auto-stages
///   any in-progress candidate buffer first so the common
///   "type a candidate, Tab to candidate field, Enter to rerank"
///   flow doesn't need an explicit add step.
fn apply_rerank_submit(app: &mut App) {
  // Candidate-field Enter is the "add this candidate" gesture.
  // We dispatch this before checking the focused-managed gate so
  // a user can stage candidates even when no model is running yet
  // (rerank submit itself still requires a running endpoint).
  if app.rerank.field == RerankField::Candidate {
    let staged = app.rerank.stage_candidate();
    if !staged {
      app.show_toast("type a candidate first");
    }
    return;
  }

  let Some(managed) = focused_managed_or_toast(app, "rerank") else {
    return;
  };
  if app.rerank.query.buffer().trim().is_empty() {
    app.show_toast("rerank query is empty");
    return;
  }
  // Auto-stage any in-progress candidate buffer the user has
  // typed but not yet committed — saves a keystroke for the
  // common case of typing the last candidate then pressing
  // Enter in the query field.
  app.rerank.stage_candidate();
  if app.rerank.candidates.is_empty() {
    let next = app.resolve_label(Focus::RerankInput, Action::NextField, "↓");
    let submit = app.resolve_label(Focus::RerankInput, Action::Submit, "Enter");
    app.show_toast(format!(
      "stage at least one candidate ({next} to candidate field, {submit} to add)"
    ));
    return;
  }
  let query = app.rerank.query.buffer().to_string();
  let candidates = app.rerank.candidates.clone();
  let model_name = served_model_name(&managed.path);
  app.rerank.busy = true;
  let port = managed.port;
  if let Some(events_tx) = require_events_tx(app, "rerank submit") {
    tokio::spawn(async move {
      let result = oai_rerank(port, &model_name, &query, &candidates).await;
      let evt = match result {
        Ok(r) => TabEvent::RerankOk(r),
        Err(e) => TabEvent::RerankErr(e),
      };
      let _ = events_tx.send(Event::Tab(evt)).await;
    });
  }
}

/// Toggle the favorite for the focused model. Always applies the
/// optimistic local flip so the next render reflects the press; if a
/// writer is wired, also forward the corresponding IPC mutation so
/// the daemon's `favorite_list` reflects the change before the next
/// 750 ms refresh overwrites the local state.
fn apply_toggle_favorite(app: &mut App, writer: Option<&mpsc::Sender<WriterCmd>>) {
  let p = match app.focused_path() {
    Some(p) => p,
    None => return,
  };
  let now_favorite = if app.favorites.contains(&p) {
    app.favorites.retain(|f| f != &p);
    false
  } else {
    app.favorites.push(p.clone());
    true
  };
  if let Some(tx) = writer {
    let cmd = if now_favorite {
      WriterCmd::FavoriteAdd(p.clone())
    } else {
      WriterCmd::FavoriteRemove(p.clone())
    };
    if tx.try_send(cmd).is_err() {
      // Writer task died — revert the optimistic toggle so the UI
      // doesn't lie about persisted state.
      if now_favorite {
        app.favorites.retain(|f| f != &p);
      } else {
        app.favorites.push(p);
      }
      app.show_error_toast("favorite toggle failed — writer offline");
      return;
    }
  }
  let name = app.display_name_for(&p);
  app.show_toast(if now_favorite {
    format!("favorited {name}")
  } else {
    format!("unfavorited {name}")
  });
}

/// Submit on the launch picker. Assembles the IPC `start_model`
/// payload from picker + advanced-panel fields and sends it via the
/// writer channel. Closes the picker on success; surfaces an
/// explanatory toast when the writer isn't attached or the channel
/// is closed.
fn apply_launch_submit(app: &mut App, writer: Option<&mpsc::Sender<WriterCmd>>) {
  let path = match app.focused_path() {
    Some(p) => p,
    None => {
      app.show_toast("no model focused");
      app.close_launch_picker();
      return;
    }
  };
  // Stage the picker on-demand when the user lands on Settings via
  // Tab/Left/Shift+S and hits Enter without first cycling fields.
  // Without this, Enter on a fresh Settings focus silently dropped
  // because `launch_picker` was None — the user had to tap an arrow
  // first to materialise the form, then Enter to launch.
  //
  // Running rows opt out of the auto-stage: Enter without a staged
  // picker just shows the read-only running view. Otherwise tapping
  // Enter on a running row would silently launch a duplicate before
  // the user has any chance to edit params — the chip strip leads
  // with `e:edit for launch` precisely so the user stages
  // intentionally before dispatching.
  if app.launch_picker.is_none() {
    if app.focused_managed().is_some() {
      return;
    }
    app.open_launch_picker();
  }
  let picker = match app.launch_picker.as_ref() {
    Some(p) => p.clone(),
    None => return,
  };
  let knobs = picker.user_knobs.clone();
  let extras: Vec<String> = picker
    .extras
    .iter()
    .map(|s| s.to_string_lossy().into_owned())
    .collect();

  use crate::gguf::metadata::ModeHint;
  use crate::launch::mode::LaunchMode;
  let hint = app
    .models
    .iter()
    .find(|m| m.path == path)
    .and_then(|m| m.metadata.as_ref())
    .map(|md| md.mode_hint)
    .unwrap_or(ModeHint::Unknown);
  let mode = LaunchMode::resolve(picker.mode_intent(), hint);

  // ctx and reasoning ride inside `knobs` now; the wire payload also
  // carries dedicated top-level fields for backward compat with
  // scripted clients, so project them out of `knobs` for the call.
  let name = app.display_name_for(&path);
  let active_instances = app.managed.iter().filter(|m| m.path == path).count();
  // The TUI flattens the resolved form client-side, so a form launch is an
  // explicit selection (daemon won't re-apply the default/last_params). The
  // `auto` cycle stop is pure fit.
  let selection = match picker.preset_stop {
    crate::tui::launch_picker::PresetStop::Auto => "auto",
    _ => "explicit",
  };
  let args = Box::new(crate::tui::app::StartModelArgs {
    ctx: knobs.u32(crate::launch::knobs::resolve_id("ctx-size").expect("ctx knob")),
    reasoning: knobs.bool(crate::launch::knobs::resolve_id("reasoning").expect("reasoning knob")),
    model_path: path,
    knobs,
    extras,
    mode,
    prefer_port: picker.prefer_port,
    // Scoping value, not a user override: a ds4 scope sends `Auto` so the
    // daemon routes (and the split-half guard fires). See `launch_backend`.
    backend: picker.launch_backend(),
    selection,
    // Chosen server build (or `None` for the priority default). The daemon
    // derives the binary — and, when `backend` is `Auto`, the backend — from it.
    server: picker.selected_server.clone(),
    // MTP is an ordinary declared knob in the editor; the wire params still
    // carry it as a typed sibling, so project it back out the way a preset does.
    mtp: picker.mtp_intent(),
  });

  if active_instances > 0 {
    app.confirm_dialog = Some(ConfirmAction::LaunchDuplicate {
      name,
      active_instances,
      args,
    });
    return;
  }

  dispatch_launch(app, writer, WriterCmd::StartModel(args), name);
}

/// Send a fully-assembled `StartModel` payload via the writer
/// channel and close the picker on success. Shared by the
/// direct-launch path and the post-confirm dispatch so both flows
/// emit the same toasts.
fn dispatch_launch(
  app: &mut App,
  writer: Option<&mpsc::Sender<WriterCmd>>,
  cmd: WriterCmd,
  name: String,
) {
  match writer {
    Some(tx) => match tx.try_send(cmd) {
      Ok(()) => {
        app.show_toast(format!("launching {name}…"));
        app.close_launch_picker();
      }
      Err(_) => {
        app.show_error_toast("launch failed — writer offline");
      }
    },
    None => {
      // No daemon attached (headless test backend, dry run, etc.).
      // Keep the picker open so the user can retry once a writer is
      // wired up rather than silently swallowing the keypress.
      app.show_error_toast("launch unavailable — no daemon writer attached");
    }
  }
}

/// Base `…/v1` URL a *copy-paste curl* should point at for the running model
/// `m`.
///
/// Prefers the **port-stable proxy** when it's listening *and auth-free*: one
/// origin that survives relaunches (the direct backend port is ephemeral),
/// speaks `/v1/` for every backend (it translates to lemonade's `/api/v1/`
/// internally), and routes by the model id. But an **auth-enforced** proxy
/// would `401` a keyless pasted command, so in that case (or when the proxy is
/// off) it falls back to the backend's own loopback port, which takes no auth
/// and always answers. Either way the yanked command runs as-is. The `u` (URL)
/// yank deliberately keeps the raw backend port; only `c` (curl) prefers the
/// proxy.
fn curl_base_url(app: &App, m: &crate::tui::app::ManagedRow) -> String {
  let proxy = app
    .daemon_info
    .proxy
    .as_ref()
    // Only a listening, explicitly auth-free proxy is safe to paste keyless.
    // `auth: None` (older daemon, unknown) is treated as not-safe.
    .filter(|p| p.status == "listening" && p.auth.as_deref() == Some("none"))
    .and_then(|p| p.listen.as_deref());
  match proxy {
    Some(listen) => format!("http://{listen}/v1"),
    None => format!("http://127.0.0.1:{}/v1", m.port),
  }
}

fn build_yank_text(app: &App, action: Action) -> Option<String> {
  match action {
    Action::YankPath => app.focused_path().map(|p| p.display().to_string()),
    Action::YankUrl => {
      // The raw backend endpoint. Fall back to the model path so `y` always
      // yanks *something useful* — a not-yet-running row still has a path the
      // user often wants to paste into a script or doc.
      if let Some(m) = app.focused_managed() {
        Some(format!("http://127.0.0.1:{}/v1", m.port))
      } else {
        app.focused_path().map(|p| p.display().to_string())
      }
    }
    Action::YankCurl => {
      let m = app.focused_managed()?;
      let url = curl_base_url(app, m);
      let model_name = served_model_name(&m.path);
      Some(format!(
        "curl -s -H 'Content-Type: application/json' -d '{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}]}}' {}/chat/completions",
        model_name,
        url
      ))
    }
    _ => None,
  }
}

/// Background refresher that polls the daemon for catalog + status
/// snapshots and forwards them as `RefreshTick`s to the run loop.
#[derive(Debug)]
pub enum RefreshTick {
  Catalog(Value),
  Status(Value),
  Favorites(Value),
  LastParams(Value),
  /// Raw config `presets:` map (`presets_all`). The app resolves each
  /// model's effective set client-side to seed the picker's preset cycle.
  Presets(Value),
  /// `logs_tail` snapshot for `launch_id`. Triggered by the
  /// dedicated [`spawn_logs_poller`] task — keeps the per-tick
  /// poll cheap when the user moves between launches.
  Logs {
    launch_id: String,
    lines: Vec<String>,
  },
  Disconnected,
  /// Failure surfaced by the writer task after dispatching a
  /// `WriterCmd`. The UI thread renders these as toasts with an
  /// actionable hint where one is available. Without this signal a
  /// failed `start_model` (e.g. `llama-server` not configured)
  /// would only land in the daemon log — the user would see
  /// "launch dispatched" and nothing else.
  WriterError {
    method: &'static str,
    message: String,
  },
  /// A non-fatal advisory from a successful writer call (a `start_model`
  /// dropped-knob / KV-blind / ssd-streaming warning) surfaced as a toast.
  WriterInfo {
    message: String,
  },
}

pub fn spawn_refresher(socket: PathBuf, tx: mpsc::Sender<Event>) {
  tokio::spawn(async move {
    let mut backoff = RECONNECT_INITIAL;
    loop {
      match Client::connect(&socket).await {
        Ok(mut client) => {
          // Reset backoff on a successful connect — the next
          // connect-failure (if any) starts fresh.
          backoff = RECONNECT_INITIAL;
          if tx.is_closed() {
            return;
          }
          if let Ok(body) = client.call("list_models", None).await {
            let _ = tx.send(Event::Refresh(RefreshTick::Catalog(body))).await;
          }
          if let Ok(body) = client.call("status", None).await {
            let _ = tx.send(Event::Refresh(RefreshTick::Status(body))).await;
          }
          if let Ok(body) = client.call("favorite_list", None).await {
            let _ = tx.send(Event::Refresh(RefreshTick::Favorites(body))).await;
          }
          if let Ok(body) = client.call("last_params_list", None).await {
            let _ = tx.send(Event::Refresh(RefreshTick::LastParams(body))).await;
          }
          if let Ok(body) = client.call("presets_all", None).await {
            let _ = tx.send(Event::Refresh(RefreshTick::Presets(body))).await;
          }
          tokio::time::sleep(REFRESH_INTERVAL).await;
        }
        Err(_) => {
          let _ = tx.send(Event::Refresh(RefreshTick::Disconnected)).await;
          // Exponential backoff capped at REFRESH_INTERVAL: a cold
          // daemon comes up within ~2 s; a long outage doesn't spam
          // the connect attempt at 1.3 Hz.
          tokio::time::sleep(backoff).await;
          backoff = (backoff * 2).min(REFRESH_INTERVAL);
        }
      }
    }
  });
}

/// Bound on outstanding writer commands. The TUI dispatches at human
/// speed (keypresses) so 64 covers any realistic burst without
/// letting a stuck daemon balloon memory.
const WRITER_CHANNEL_CAPACITY: usize = 64;

/// Spawn the writer task and return the sender that callers push
/// [`WriterCmd`]s into. The task reconnects per command; the local
/// Unix socket makes that cheap and removes the "writer holds a
/// stale client across a daemon restart" failure mode.
///
/// `daemon_opts` is the spawn payload used when a `RestartDaemon`
/// command lands — the writer task re-spawns the daemon with the
/// same options the parent CLI dispatcher resolved at startup, so
/// `--model-path`, `--no-scan`, port range, etc. survive the
/// restart. `None` falls back to platform defaults.
///
/// The channel is bounded so a wedged daemon + scripted rapid input
/// can't exhaust process memory. Callers using `try_send` get an
/// `Err(Full)` and can either drop the action or surface a toast.
pub fn spawn_writer(
  socket: PathBuf,
  daemon_opts: Option<crate::daemon::DaemonOptions>,
  feedback: Option<mpsc::Sender<Event>>,
) -> mpsc::Sender<WriterCmd> {
  let (tx, mut rx) = mpsc::channel::<WriterCmd>(WRITER_CHANNEL_CAPACITY);
  tokio::spawn(async move {
    while let Some(cmd) = rx.recv().await {
      if matches!(cmd, WriterCmd::RestartDaemon) {
        handle_restart_daemon(&socket, daemon_opts.clone()).await;
        continue;
      }
      let mut client = match Client::connect(&socket).await {
        Ok(c) => c,
        Err(e) => {
          let message = format!("writer connect failed: {e}");
          log::warn!("{message}");
          if let Some(fb) = &feedback {
            // `cmd` is consumed by `encode_writer_cmd` below, but
            // connect-time we never reached that — surface a generic
            // method label so the toast still tells the user
            // something happened.
            let _ = fb
              .send(Event::Refresh(RefreshTick::WriterError {
                method: "connect",
                message,
              }))
              .await;
          }
          continue;
        }
      };
      let (method, params) = encode_writer_cmd(cmd);
      match client.call(method, Some(params)).await {
        Err(e) => {
          let message = format!("{e}");
          log::warn!("writer call {method} failed: {message}");
          if let Some(fb) = &feedback {
            let _ = fb
              .send(Event::Refresh(RefreshTick::WriterError { method, message }))
              .await;
          }
        }
        // Surface a successful `start_model`'s advisories (dropped knobs,
        // deepseek4 KV-blind, ssd_streaming bypass) as a toast — the TUI leg
        // of R6 / D-admission. Other methods carry no warnings.
        Ok(resp) => {
          if method == "start_model" {
            if let Some(ws) = resp.get("warnings").and_then(|v| v.as_array()) {
              let joined = ws
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("; ");
              if !joined.is_empty() {
                if let Some(fb) = &feedback {
                  let _ = fb
                    .send(Event::Refresh(RefreshTick::WriterInfo { message: joined }))
                    .await;
                }
              }
            }
          }
        }
      }
    }
  });
  tx
}

/// Two-phase daemon restart: ask the running daemon to shut down,
/// wait until it has fully released its lockfile, then
/// `start_detached` a fresh daemon with the same options the parent
/// dispatcher resolved. Best-effort throughout — every failure logs
/// and the TUI keeps running so the user can retry from the keymap.
///
/// Why poll the lockfile and not just the socket: the daemon's
/// cleanup sequence is (1) accept-loop exit, (2) up to 2s connection
/// drain, (3) `stop_all_managed`, (4) remove socket file,
/// (5) drop lockfile. Waiting only for the socket to become
/// unconnectable can return before step 5, so the replacement child's
/// `acquire` contends on the still-held `flock`, exits with
/// `AlreadyRunning`, and `start_detached` reports a failure. The user
/// then sees "daemon connecting…" indefinitely until they retry.
async fn handle_restart_daemon(
  socket: &std::path::Path,
  daemon_opts: Option<crate::daemon::DaemonOptions>,
) {
  match Client::connect(socket).await {
    Ok(mut client) => {
      if let Err(e) = client.call("shutdown", None).await {
        log::warn!("restart: shutdown call failed: {e}");
      }
    }
    Err(e) => log::warn!("restart: connect-for-shutdown failed: {e}"),
  }
  let opts = match daemon_opts {
    Some(o) => o,
    None => match crate::daemon::DaemonOptions::from_defaults() {
      Ok(o) => o,
      Err(e) => {
        log::warn!("restart: default DaemonOptions: {e}");
        return;
      }
    },
  };
  // Wait for the old daemon to fully release its lockfile. 8s covers
  // the worst case (2s connection drain + 5s `stop_all_managed`
  // grace + cleanup margin); children that stop cleanly return well
  // before the grace deadline.
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
  while std::time::Instant::now() < deadline {
    if crate::daemon::existing_daemon_pid(&opts.state_dir).is_none() {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  match crate::daemon::start_detached(opts) {
    Ok(crate::daemon::StartOutcome::AlreadyRunning(_)) => {
      log::warn!("restart: daemon is still running; restart did not spawn a new process");
    }
    Ok(_) => log::info!("restart: daemon re-spawned"),
    Err(e) => log::warn!("restart: start_detached failed: {e}"),
  }
}

/// Encode a [`WriterCmd`] into the `(method, params)` JSON-RPC call the
/// daemon writer task sends. Pure and total over the command — exposed so
/// the knob-parity test can pin the wire payload a surface actually emits
/// (the TUI save path dropped `backend` / `server` while every knob test
/// stayed green, because nothing checked this shape).
pub fn encode_writer_cmd(cmd: WriterCmd) -> (&'static str, Value) {
  match cmd {
    WriterCmd::StartModel(args) => {
      let crate::tui::app::StartModelArgs {
        model_path,
        ctx,
        reasoning,
        knobs,
        extras,
        mode,
        prefer_port,
        backend,
        selection,
        server,
        mtp,
      } = *args;
      let mode_str = mode.map(|m| match m {
        crate::launch::mode::LaunchMode::Chat => "chat",
        crate::launch::mode::LaunchMode::Embedding => "embedding",
        crate::launch::mode::LaunchMode::Rerank => "rerank",
      });
      (
        "start_model",
        json!({
          "model_path": model_path,
          "ctx": ctx,
          "reasoning": reasoning,
          "knobs": knobs,
          "extras": extras,
          "mode": mode_str,
          "prefer_port": prefer_port,
          // Per-model backend override; serde → "auto"/"llamacpp".
          "backend": backend,
          // Drives daemon default-preset + last_params inheritance.
          "selection": selection,
          // Chosen server build id; daemon ignores `null` / stale ids.
          "server": server,
          // MTP intent from the picker's cycle row (auto/on/off).
          "mtp": mtp.label(),
        }),
      )
    }
    WriterCmd::StopModel { launch_id } => ("stop_model", json!({ "launch_id": launch_id })),
    WriterCmd::Shutdown => ("shutdown", json!({})),
    // RestartDaemon is handled directly in `spawn_writer` (it's a
    // two-phase shutdown + start_detached, not a single RPC). The
    // dispatcher short-circuits before reaching this encoder, so
    // hitting this arm is a programmer error — log and emit a
    // no-op JSON-RPC `version` call as the safest fallback.
    WriterCmd::RestartDaemon => {
      log::warn!("encode_writer_cmd reached RestartDaemon (should be short-circuited)");
      ("version", json!({}))
    }
    WriterCmd::FavoriteAdd(p) => ("favorite_add", json!({ "model_path": p })),
    WriterCmd::FavoriteRemove(p) => ("favorite_remove", json!({ "model_path": p })),
    WriterCmd::SavePreset(cmd) => {
      // `knobs` already folds ctx/reasoning in; the daemon stores them
      // as-is (it only folds the separate ctx/reasoning params, which we
      // omit here). Auto markers survive the round-trip.
      let mut payload = json!({
        "model_path": cmd.model_path,
        "name": cmd.name,
        "knobs": cmd.knobs,
        "extras": cmd.extras,
      });
      // `backend` / `server` pin the launch identity (which engine / build
      // this preset was captured against) — the same identity the CLI's
      // `presets save` sends, so a TUI-captured preset reproduces its run
      // instead of silently falling to the daemon's default server. Omit
      // them when unset (matching the CLI) so "no identity" encodes the
      // same way from both clients.
      if let Some(b) = &cmd.backend {
        payload["backend"] = json!(b);
      }
      if let Some(sv) = &cmd.server {
        payload["server"] = json!(sv);
      }
      ("presets_save", payload)
    }
  }
}

/// Background thread that owns the crossterm input stream. Blocks on
/// `event::poll(timeout)` and emits one [`Event::Input`] per real
/// event or one [`Event::Tick`] per [`TICK_RATE`] when no input
/// arrived. Pattern lifted from kdash so an idle TUI blocks in the
/// kernel rather than spinning a poll loop.
fn spawn_input_thread(tx: mpsc::Sender<Event>) {
  use std::time::Instant;
  std::thread::spawn(move || {
    let mut last_tick = Instant::now();
    loop {
      let timeout = TICK_RATE
        .checked_sub(last_tick.elapsed())
        .unwrap_or(Duration::ZERO);
      match event::poll(timeout) {
        Ok(true) => match event::read() {
          Ok(evt) => {
            // Filter motion / drag / button-up at the source.
            // `crossterm::EnableMouseCapture` turns on mode 1003
            // (any-event tracking), so every mouse waggle fires a
            // `Moved` event — thousands per second under a moving
            // cursor. Forwarding them all would flood the 256-slot
            // event channel and force a redraw per tick, livelocking
            // the run loop into what feels like a hang. The TUI only
            // ever acts on `Down(Left)` and the two wheel kinds, so
            // dropping the rest at the source is free of behaviour
            // change and keeps the downstream dispatch trivial.
            if let TermEvent::Mouse(ref m) = evt {
              use crossterm::event::{MouseButton, MouseEventKind};
              let actionable = matches!(
                m.kind,
                MouseEventKind::Down(MouseButton::Left)
                  | MouseEventKind::ScrollUp
                  | MouseEventKind::ScrollDown
              );
              if !actionable {
                continue;
              }
            }
            if tx.blocking_send(Event::Input(evt)).is_err() {
              return;
            }
          }
          Err(_) => return,
        },
        Ok(false) => {}
        Err(_) => return,
      }
      if last_tick.elapsed() >= TICK_RATE {
        if tx.blocking_send(Event::Tick).is_err() {
          return;
        }
        last_tick = Instant::now();
      }
    }
  });
}

/// Capacity for the unified TUI event channel. Sized for a worst-case
/// burst of chat-stream tokens (one chunk per couple of ms) plus
/// daemon refreshes plus input — 256 covers any realistic spike
/// without forcing the chat task to await on backpressure. Progress
/// events from the download strip use `try_send` so the firehose
/// drops frames before this fills.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Fully-featured TUI run-loop. Drives the App from a single mpsc
/// funnel: a background thread pumps crossterm input + tick events,
/// and every subsystem (daemon refresher, log poller, chat stream,
/// embed/rerank, HF dialog, download progress) pushes into the same
/// channel. The main loop blocks on `recv` so an idle TUI consumes
/// no CPU; redraws only happen when an event actually changed state.
pub async fn run(
  app: App,
  socket: PathBuf,
  daemon_opts: Option<crate::daemon::DaemonOptions>,
) -> Result<()> {
  use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
  };
  use crossterm::execute;
  use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
  };
  use ratatui::backend::CrosstermBackend;
  use ratatui::Terminal;

  enable_raw_mode()?;
  let mut stdout = std::io::stdout();
  // Mouse capture is off by default: when the application captures
  // mouse events, the terminal can't run its own click-and-drag
  // text selection. j/k/PgUp/PgDn/g/G cover all the navigation a
  // user would otherwise reach for the wheel, so the default keeps
  // the dashboard copy-friendly.
  //
  // `mouse_focus: true` in `config.yaml` (or `--mouse-focus`) opts
  // into mouse capture: left-click on the Models list / right pane
  // / tab label moves focus / switches tab, and wheel up/down
  // replays the `↑`/`↓` action in the current focus. Mouse drag /
  // motion / button-up are filtered out at the input-thread layer
  // so a bypass-modifier text selection (Shift on iTerm2 /
  // Alacritty / foot / wezterm, Option on Apple Terminal) still
  // works even with capture enabled.
  let mouse_capture = app.options.mouse_focus;
  execute!(stdout, EnterAlternateScreen)?;
  if mouse_capture {
    execute!(stdout, EnableMouseCapture)?;
  }
  // Opt into the kitty keyboard protocol so Shift+Enter (and any
  // other Shift/Ctrl + Enter variants) arrive as distinct events on
  // supporting terminals (kitty / foot / wezterm / alacritty). On
  // terminals that don't implement the protocol the escape sequence
  // is silently ignored, so this is safe everywhere. Paired with a
  // PopKeyboardEnhancementFlags below.
  let pushed_kitty = execute!(
    stdout,
    PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
  )
  .is_ok();
  let backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(backend)?;

  let mut app = app;
  let (events_tx, mut events_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
  app.events_tx = Some(events_tx.clone());

  // Background producers.
  spawn_input_thread(events_tx.clone());
  spawn_refresher(socket.clone(), events_tx.clone());
  let current_launch = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
  spawn_logs_poller(socket.clone(), current_launch.clone(), events_tx.clone());
  let writer_tx = spawn_writer(socket, daemon_opts, Some(events_tx.clone()));

  // Prime the screen once before blocking on the event channel so
  // the user sees the dashboard frame even before the first refresh
  // tick lands.
  terminal.draw(|f| crate::tui::render::render(f, &mut app))?;

  while let Some(evt) = events_rx.recv().await {
    // Fold this event plus every other one already queued (a burst of
    // stream deltas, refresh ticks) into a single batch, then draw at
    // most once — see `drain_and_handle`.
    let needs_redraw = drain_and_handle(&mut app, evt, &mut events_rx, &writer_tx);
    if app.should_exit {
      break;
    }
    // Mirror the focused launch id to the logs poller so its next
    // tick fetches the right buffer (post-batch focus is the truth).
    *current_launch
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner) =
      app.focused_managed().map(|m| m.launch_id.clone());
    if needs_redraw {
      terminal.draw(|f| crate::tui::render::render(f, &mut app))?;
    }
  }

  // Restore the terminal even on early returns above. Pop the kitty
  // protocol flags first so the next program inheriting the tty
  // doesn't accidentally inherit the disambiguation state.
  if pushed_kitty {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
  }
  if mouse_capture {
    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
  }
  disable_raw_mode()?;
  execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
  terminal.show_cursor()?;
  Ok(())
}

/// Handle `first`, then drain every event already queued behind it
/// and fold them in, returning whether the batch warrants a redraw.
///
/// Coalescing is what keeps input responsive while a chat completion
/// streams: `spawn_chat_stream` pushes one `Delta` per token, so a
/// fast model floods the 256-slot channel. Drawing once per event
/// would mean a queued scroll/keystroke waits behind the whole token
/// backlog (each forcing a full re-render), which reads as sluggish
/// or frozen scrolling mid-stream. Applying all ready events first,
/// then drawing once, collapses the burst into a single frame and
/// services the queued input in the same pass.
fn drain_and_handle(
  app: &mut App,
  first: Event,
  events_rx: &mut mpsc::Receiver<Event>,
  writer_tx: &mpsc::Sender<WriterCmd>,
) -> bool {
  let mut needs_redraw = handle_event(app, first, writer_tx);
  while !app.should_exit {
    match events_rx.try_recv() {
      Ok(evt) => needs_redraw |= handle_event(app, evt, writer_tx),
      Err(_) => break,
    }
  }
  needs_redraw
}

/// Dispatch one [`Event`] into the right subsystem handler.
/// Returns `true` when the resulting state change warrants a fresh
/// redraw — `false` only for pure ticks that didn't service any
/// debounced work. Centralising the decision here keeps the run-loop
/// body the kdash-style three liner (block on recv → dispatch → draw
/// if dirty) and gives a single place to flip the dirty-flag policy
/// per variant if a future tab needs to skip redraws cheaply.
fn handle_event(app: &mut App, evt: Event, writer_tx: &mpsc::Sender<WriterCmd>) -> bool {
  match evt {
    Event::Input(term_evt) => {
      pump_input_with_writer(app, term_evt, Some(writer_tx));
      true
    }
    Event::Tick => {
      // The HF dialog's debounced live search fires off the tick
      // rather than carrying its own timer task — the unified loop
      // wakes at TICK_RATE so the check is essentially free.
      let hf_search_dispatched = service_hf_dialog_debounce(app);
      // Time-decay UI (toast TTL, strip error linger) needs a
      // periodic redraw so it visibly disappears even when no other
      // events are landing. Bounded by `TICK_RATE` (4 Hz) so the
      // idle-CPU win from the refactor is preserved.
      hf_search_dispatched || tick_has_time_decay_ui(app)
    }
    Event::Refresh(tick) => {
      apply_refresh(app, tick);
      true
    }
    Event::ChatStream(msg) => {
      apply_chat_stream(app, msg);
      true
    }
    Event::Tab(tab_evt) => {
      apply_tab_event(app, tab_evt);
      true
    }
    Event::HfDialog(hf_evt) => {
      apply_hf_dialog_event(app, hf_evt);
      true
    }
    Event::Download(dl_evt) => {
      apply_download_event(app, dl_evt);
      true
    }
  }
}

/// A daemon answered an RPC: flip the connected flag and drop any
/// startup auto-spawn refusal — a live daemon supersedes the stale
/// "couldn't start one" message in the Daemon panel.
fn mark_daemon_connected(app: &mut App) {
  app.daemon_connected = true;
  app.daemon_start_error = None;
}

fn apply_refresh(app: &mut App, tick: RefreshTick) {
  match tick {
    RefreshTick::Catalog(body) => {
      mark_daemon_connected(app);
      app.ingest_list_models(&body);
    }
    RefreshTick::Status(body) => {
      mark_daemon_connected(app);
      app.ingest_status(&body);
    }
    RefreshTick::Favorites(body) => {
      mark_daemon_connected(app);
      app.ingest_favorites(&body);
    }
    RefreshTick::LastParams(body) => {
      mark_daemon_connected(app);
      app.ingest_last_params(&body);
    }
    RefreshTick::Presets(body) => {
      mark_daemon_connected(app);
      app.ingest_presets(&body);
    }
    RefreshTick::Logs { launch_id, lines } => {
      mark_daemon_connected(app);
      // Replace the buffer only when the snapshot matches the
      // launch the user currently has focused. Otherwise the poll
      // raced a focus change; drop on the floor.
      if app
        .focused_managed()
        .map(|m| m.launch_id == launch_id)
        .unwrap_or(false)
      {
        app.logs_state.set_tail(launch_id, lines);
      }
    }
    RefreshTick::Disconnected => {
      app.daemon_connected = false;
    }
    RefreshTick::WriterError { method, message } => {
      app.show_error_toast(writer_error_toast(method, &message));
    }
    RefreshTick::WriterInfo { message } => {
      app.show_toast(message);
    }
  }
}

/// Build a user-facing toast for a writer-task failure. `start_model`
/// gets a dedicated hint when the daemon reports the launch
/// environment isn't configured — mirrors `cli::start`'s message so
/// the TUI and CLI guide users to the same fix.
fn writer_error_toast(method: &str, message: &str) -> String {
  let lower = message.to_lowercase();
  let needs_binary_hint = method == "start_model"
    && (lower.contains("launch environment") || lower.contains("llama-server"));
  if needs_binary_hint {
    format!("launch failed: {message}\nhint: set LLAMASTASH_LLAMA_SERVER or pass --llama-server")
  } else {
    format!("{method} failed: {message}")
  }
}

/// Cadence used by the dedicated logs poller. Slower than
/// [`REFRESH_INTERVAL`] because log lines arrive at the daemon's
/// stderr cadence; we just need them visibly fresh.
const LOGS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn a task that polls `logs_tail` for the currently focused
/// launch. The caller updates `current` with the launch the user
/// is looking at; the task reads that on each tick so it follows
/// focus without restarting.
pub fn spawn_logs_poller(
  socket: PathBuf,
  current: std::sync::Arc<std::sync::Mutex<Option<String>>>,
  tx: mpsc::Sender<Event>,
) {
  tokio::spawn(async move {
    // Exponential backoff mirrors `spawn_refresher` so a daemon
    // outage doesn't produce a 2 Hz connect-attempt rate. Reset on
    // any successful connect.
    let mut backoff = RECONNECT_INITIAL;
    loop {
      let launch_id = current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
      if let Some(launch_id) = launch_id {
        match Client::connect(&socket).await {
          Ok(mut client) => {
            backoff = RECONNECT_INITIAL;
            if let Ok(body) = client
              .call(
                "logs_tail",
                Some(json!({ "launch_id": &launch_id, "lines": 200 })),
              )
              .await
            {
              let lines: Vec<String> = body
                .get("lines")
                .and_then(Value::as_array)
                .map(|a| {
                  a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
                })
                .unwrap_or_default();
              let _ = tx
                .send(Event::Refresh(RefreshTick::Logs { launch_id, lines }))
                .await;
            }
          }
          Err(_) => {
            // Daemon unreachable — back off before the next attempt.
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(REFRESH_INTERVAL);
            if tx.is_closed() {
              return;
            }
            continue;
          }
        }
      }
      tokio::time::sleep(LOGS_POLL_INTERVAL).await;
      if tx.is_closed() {
        return;
      }
    }
  });
}

/// Public escape-hatch for tests + the smoke test that drive the
/// loop manually with a chosen socket.
pub fn refresh_apply(app: &mut App, tick: RefreshTick) {
  apply_refresh(app, tick);
}

/// Convenience used by `cli::dispatch`: build the App with the
/// loaded config and a connected (or-not-yet-connected) daemon
/// socket. Splitting it out keeps the binary entry small and the
/// call testable.
#[allow(clippy::too_many_arguments)]
pub async fn launch(
  theme: crate::theme::ThemeName,
  custom_palette: Option<crate::theme::Palette>,
  keymap: crate::tui::keybindings::KeyMap,
  offline: bool,
  mouse_focus: bool,
  left_pane_ratios: Vec<u16>,
  socket: &Path,
  daemon_opts: Option<crate::daemon::DaemonOptions>,
  daemon_start_error: Option<String>,
) -> Result<()> {
  let mut app = App::new(crate::tui::app::AppOptions {
    theme,
    custom_palette,
    keymap,
    offline,
    mouse_focus,
    left_pane_ratios,
  });
  // Startup auto-spawn refused (backend fail-fast precheck) — the TUI
  // runs daemon-less and the Daemon panel explains why. Cleared by
  // `mark_daemon_connected` if a daemon comes up later.
  app.daemon_start_error = daemon_start_error;
  run(app, socket.to_path_buf(), daemon_opts).await
}

/// Apply one [`ChatStreamMsg`] to `app.chat`. Replaces the prior
/// `drain_chat_stream(app)` polling helper: the unified `Event` loop
/// hands each chunk in one at a time, so the receiver lives nowhere
/// on app state.
pub fn apply_chat_stream(app: &mut App, msg: ChatStreamMsg) {
  match msg {
    ChatStreamMsg::Delta(s) => app.chat.append_delta(&s),
    ChatStreamMsg::Finished { finish_reason } => app.chat.mark_finished(finish_reason),
    ChatStreamMsg::Error(e) => app.chat.mark_error(e),
  }
}

/// Apply one [`TabEvent`] (embed or rerank result) to the right pane.
/// Replaces `drain_embed_pending` / `drain_rerank_pending` — the
/// unified loop hands one event in at a time.
pub fn apply_tab_event(app: &mut App, evt: TabEvent) {
  match evt {
    TabEvent::EmbedOk(result) => app.embed.record(result),
    TabEvent::EmbedErr(msg) => app.embed.record_error(msg),
    TabEvent::RerankOk(ranked) => app.rerank.record(ranked),
    TabEvent::RerankErr(msg) => app.rerank.record_error(msg),
  }
}

/// `true` when a `Tick` event should still trigger a redraw because
/// some time-decay UI element is on screen — a toast that may have
/// just hit `TOAST_TTL`, or a download-strip error that may have
/// just hit `ERROR_LINGER`. Without this, those elements would only
/// disappear on the next non-Tick event (~750 ms via the refresher),
/// which is visibly laggy. Cheap predicate, runs at most once per
/// `TICK_RATE`.
fn tick_has_time_decay_ui(app: &App) -> bool {
  app.toast_message().is_some() || app.download_strip.last_error.is_some()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crossterm::event::{KeyEvent, KeyModifiers};

  fn key(code: KeyCode, mods: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent::new(code, mods))
  }

  #[test]
  fn ctrl_d_on_user_path_model_stages_delete_confirm() {
    use crate::tui::app::ConfirmAction;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    match app
      .confirm_dialog
      .as_ref()
      .expect("confirm popup must stage")
    {
      ConfirmAction::DeleteModel { display_name, .. } => {
        assert!(
          display_name.contains("qwen"),
          "got display name `{display_name}`"
        );
      }
      other => panic!("expected DeleteModel confirm, got {other:?}"),
    }
  }

  #[test]
  fn ctrl_d_on_lemonade_model_refuses_with_toast() {
    // A Lemonade model is a registry entry (synthetic `lemonade://` path), not
    // a local GGUF — Ctrl+D must refuse with guidance, not stage a delete that
    // would fail on a non-existent file.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events(
      "lemonade://Llama-3.1-8B",
      "lemonade://",
    )];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(
      app.confirm_dialog.is_none(),
      "Lemonade registry row must not stage a delete"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("Lemonade"),
      "expected a Lemonade-delete toast, got `{toast}`"
    );
  }

  #[test]
  fn arrows_scroll_chat_output_while_composing() {
    // in the chat tab the response wouldn't scroll with the
    // arrow keys — ↑/↓ were unbound in the composer focus, so they
    // never reached the chat viewport. They must scroll the output
    // while focus stays on the prompt (where the user sits after
    // sending), not move the model list.
    let mut app = App::new(Default::default());
    app.focus = Focus::ChatInput;
    assert_eq!(app.chat.scroll_offset.get(), 0);
    // `scroll_offset` is the top-of-viewport line: ↓ moves toward the
    // end (increments), ↑ moves back toward the top (decrements). At
    // the top (offset 0) ↑ is a no-op — it must not scroll the wrong
    // way (the original bug pushed the offset *up* on ↑).
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
      app.chat.scroll_offset.get(),
      0,
      "Up at the top of the response must stay pinned, not scroll down"
    );
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
      app.chat.scroll_offset.get(),
      1,
      "Down in the chat composer must scroll the response toward the end"
    );
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
      app.chat.scroll_offset.get(),
      0,
      "Up must scroll the response back toward the top"
    );
    assert_eq!(
      app.focus,
      Focus::ChatInput,
      "scrolling must not change focus"
    );

    // Embed composer scrolls its own output the same way.
    app.focus = Focus::EmbedInput;
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
      app.embed.scroll_offset.get(),
      1,
      "Down in the embed composer must scroll its output toward the end"
    );
  }

  #[test]
  fn ctrl_d_on_error_managed_model_refuses_with_toast() {
    use crate::tui::app::ManagedRow;
    use crate::tui::status_icons::SurfaceState;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ManagedRow {
      launch_id: "L-error".into(),
      path: PathBuf::from("/m/qwen.gguf"),
      port: 41100,
      state: SurfaceState::Error,
      device: None,
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(
      app.confirm_dialog.is_none(),
      "error-state row must not stage delete"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("error"),
      "expected error-specific toast, got `{toast}`"
    );
  }

  #[test]
  fn ctrl_d_on_external_model_refuses_with_toast() {
    use crate::tui::app::ManagedRow;
    use crate::tui::status_icons::SurfaceState;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    // External rows live on `app.external`, not `app.managed`.
    app.external = vec![ManagedRow {
      launch_id: "ext-1".into(),
      path: PathBuf::from("/m/qwen.gguf"),
      port: 41200,
      state: SurfaceState::External,
      device: None,
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(
      app.confirm_dialog.is_none(),
      "external-process row must not stage delete"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("external"),
      "expected external-specific toast, got `{toast}`"
    );
  }

  #[test]
  fn ctrl_d_on_running_model_refuses_with_toast() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(
      app.confirm_dialog.is_none(),
      "running model must not stage delete"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("stop the launch"),
      "expected stop-first toast, got `{toast}`"
    );
  }

  #[test]
  fn ctrl_p_on_idle_models_row_toasts_not_opens() {
    // Models pane (Focus::List) on an idle row has no live settings to
    // capture — Ctrl+P toasts, pointing the user at the Settings pane.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    assert_eq!(app.focus, Focus::List);
    pump_input(&mut app, key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(
      app.save_preset_dialog.is_none(),
      "idle Models row must not open the save dialog"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("Settings") || toast.contains("running model"),
      "expected a guidance toast, got `{toast}`"
    );
  }

  #[test]
  fn ctrl_p_on_running_models_row_opens_save_dialog() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(
      app.save_preset_dialog.is_some(),
      "running Models row opens the save dialog on Ctrl+P"
    );
  }

  #[test]
  fn ctrl_p_in_settings_pane_opens_for_idle_model() {
    // Settings pane always allows a save — an idle model's about-to-launch
    // form is captured via the default picker.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = crate::tui::tabs::RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(
      app.save_preset_dialog.is_some(),
      "Settings pane opens the save dialog even for an idle model"
    );
  }

  fn vim_test_app() -> App {
    let mut app = App::new(Default::default());
    app.models = (0..30)
      .map(|i| fake_model_for_events(&format!("/m/model-{i:02}.gguf"), "/m"))
      .collect();
    app.focus = Focus::List;
    app.list_cursor = 0;
    app
  }

  #[test]
  fn vim_page_aliases_fire_pgdn_pgup() {
    // Ctrl+F / Ctrl+B alias PgDn / PgUp; Ctrl+U collapses to PgUp
    // since the list scroller has no half-page concept.
    let mut app = vim_test_app();
    let start = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert!(
      app.list_cursor > start,
      "Ctrl+F must page down (cursor moved from {start} to {})",
      app.list_cursor
    );
    let after_down = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert!(
      app.list_cursor < after_down,
      "Ctrl+B must page up (cursor moved back)"
    );
    let after_up = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    pump_input(&mut app, key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(
      app.list_cursor <= after_up,
      "Ctrl+U must page up too (cursor must not stay below the Ctrl+B landing)"
    );
  }

  #[test]
  fn vim_zero_dollar_aliases_jump_top_and_bottom() {
    // `go_top` / `go_bottom` land on the first / last *selectable* row,
    // not raw index 0 — `app.models[0]` may sit behind a group header.
    let mut app = vim_test_app();
    pump_input(&mut app, key(KeyCode::Char('g'), KeyModifiers::NONE));
    let top = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Char('$'), KeyModifiers::NONE));
    let bottom = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Char('0'), KeyModifiers::NONE));
    assert_eq!(app.list_cursor, top, "`0` must mirror `g` (top)");
    assert!(bottom > top, "`$` should have moved past the top row");
  }

  #[test]
  fn vim_i_in_right_pane_enters_edit_mode() {
    // Mirrors the `e:edit` path. We use the Embed tab because Chat
    // requires a Ready managed launch — Embed accepts edit without one.
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Embed;
    pump_input(&mut app, key(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::EmbedInput, "`i` must open the input");
  }

  #[test]
  fn vim_gt_mirrors_tab_from_list_focus() {
    // `gt` reuses the Tab focus-cycle path (`Action::NextFocus`). From
    // LIST that walks to the first reachable right-pane focus. The
    // queued `g → GoTop` still fires immediately so the single-stroke
    // motion stays snappy; the theme must NOT cycle (original bug).
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/a.gguf", "/m")];
    app.focus = Focus::List;
    let starting_focus = app.focus;
    let starting_theme = app.options.theme;
    pump_input(&mut app, key(KeyCode::Char('g'), KeyModifiers::NONE));
    assert!(app.pending_g_prefix, "`g` in LIST must queue the prefix");
    pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
    assert!(!app.pending_g_prefix);
    assert_ne!(
      app.focus, starting_focus,
      "`gt` must walk focus forward, just like Tab"
    );
    assert_eq!(
      app.options.theme, starting_theme,
      "`gt` must NOT also cycle the theme — that was the original bug"
    );
  }

  #[test]
  fn vim_gt_mirrors_tab_from_right_pane() {
    // `gt` from RightPane advances focus just like Tab does. `gT`
    // (shift) walks backward.
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    let starting_focus = app.focus;
    pump_input(&mut app, key(KeyCode::Char('g'), KeyModifiers::NONE));
    assert!(app.pending_g_prefix, "`g` in right pane queues the prefix");
    pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
    assert!(!app.pending_g_prefix);
    // Focus chain has at least List + RightPane, so NextFocus always
    // moves somewhere. Don't pin the exact destination — the chain
    // depends on which right tabs are reachable.
    assert_ne!(app.focus, starting_focus, "`gt` must advance focus");

    // gT walks back to the original.
    pump_input(&mut app, key(KeyCode::Char('g'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('T'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, starting_focus, "`gT` must reverse the cycle");
  }

  #[test]
  fn vim_g_prefix_drops_on_unrelated_second_key() {
    // After `g`, any non-`t`/`T` key clears the prefix and the second
    // key falls through to normal dispatch.
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    pump_input(&mut app, key(KeyCode::Char('g'), KeyModifiers::NONE));
    assert!(app.pending_g_prefix);
    pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(
      !app.pending_g_prefix,
      "prefix must clear even when the second key isn't t/T"
    );
    assert!(app.should_exit, "fallthrough `q` must still quit");
  }

  #[test]
  fn filter_focus_three_esc_walk_back_chain() {
    // Lock down the Esc walk-back contract for the filter input
    // documented in handle_key:
    //   1st Esc → exit edit (buffer kept, focus stays)
    //   2nd Esc → clear buffer (focus stays)
    //   3rd Esc → close filter (focus walks back to List)
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
    pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('w'), KeyModifiers::NONE));
    assert!(app.filter_input.is_editing());
    assert_eq!(app.filter_input.buffer(), "qw");
    // 1st Esc.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter, "filter still focused");
    assert!(
      !app.filter_input.is_editing(),
      "edit must exit on first Esc"
    );
    assert_eq!(app.filter_input.buffer(), "qw", "buffer must survive");
    // 2nd Esc.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter, "filter still focused");
    assert!(
      app.filter_input.is_empty(),
      "buffer must clear on second Esc"
    );
    // 3rd Esc.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
      app.focus,
      Focus::List,
      "third Esc must walk focus back to List"
    );
  }

  #[test]
  fn chat_input_three_esc_walk_back_chain() {
    // Same contract for the chat composer (representative of
    // chat/embed/rerank). The third Esc dispatches `Action::ExitEdit`
    // which exits edit on every tab field + flips focus back to
    // RightPane.
    let mut app = App::new(Default::default());
    app.right_tab = RightTab::Chat;
    app.focus = Focus::RightPane;
    // `e` activates the chat-input focus (auto-enters edit).
    pump_input(&mut app, key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::ChatInput);
    assert!(app.chat.prompt.is_editing());
    // Type something so the second Esc has buffer content to clear.
    pump_input(&mut app, key(KeyCode::Char('h'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(app.chat.prompt.buffer(), "hi");
    // 1st Esc — exit edit.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::ChatInput, "focus stays on chat input");
    assert!(!app.chat.prompt.is_editing());
    assert_eq!(app.chat.prompt.buffer(), "hi");
    // 2nd Esc — clear.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.chat.prompt.is_empty());
    assert_eq!(
      app.focus,
      Focus::ChatInput,
      "focus stays on chat input after clear"
    );
    // 3rd Esc — exit chat input back to the right pane.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
      app.focus,
      Focus::RightPane,
      "third Esc must walk focus back to RightPane"
    );
  }

  #[test]
  fn filter_enter_opens_launch_picker_on_focused_row() {
    // Filter is a live predicate (rows update on every keystroke),
    // so Enter has no "apply" semantics. Instead it drills into the
    // focused result by opening the launch picker — same affordance
    // as `Enter` on the model list. The filter buffer survives the
    // drill-in so the user can dismiss the picker and keep scrolling
    // the filtered results.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    // Open filter, type a query, then Enter.
    pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('w'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
    assert!(app.filter_input.is_editing());
    pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
    // Picker is open + focus moved to the right pane's Settings tab.
    assert!(
      app.launch_picker.is_some(),
      "Enter on filter must open the launch picker for the focused row"
    );
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Settings);
    // Filter buffer survives so the user keeps the predicate after
    // dismissing the picker.
    assert_eq!(app.filter_input.buffer(), "qw");
    assert!(
      !app.filter_input.is_editing(),
      "filter must have exited edit before drilling into picker"
    );
  }

  #[test]
  fn filter_enter_on_empty_results_only_exits_edit() {
    // With no rows matching the filter, Enter falls back to "stop
    // editing + return to list" — no picker (there's nothing to
    // launch), no toast.
    let mut app = App::new(Default::default());
    app.models = vec![]; // empty catalog → zero rows after filter.
    pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('z'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.launch_picker.is_none());
    assert_eq!(app.focus, Focus::List);
    assert!(!app.filter_input.is_editing());
  }

  #[test]
  fn filter_focus_arrow_keys_move_the_list_cursor() {
    // Up/Down (and the vi `k`/`j` aliases) must scroll the filtered
    // model list while focus stays on the filter input, in both
    // editing and resting modes. The InputField passes arrows
    // through; `handle_filter_input` is the single wire that turns
    // those passthroughs into list-cursor movement.
    let mut app = App::new(Default::default());
    app.models = vec![
      fake_model_for_events("/m/a.gguf", "/m"),
      fake_model_for_events("/m/b.gguf", "/m"),
      fake_model_for_events("/m/c.gguf", "/m"),
    ];
    app.go_top();
    let cursor_before = app.list_cursor;
    // Enter filter focus (auto-enters edit).
    pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
    assert!(app.filter_input.is_editing(), "filter must auto-edit");
    // Down while editing → list cursor advances.
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert!(
      app.list_cursor > cursor_before,
      "Down arrow in filter edit mode must advance the list cursor"
    );
    let mid_cursor = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert!(
      app.list_cursor < mid_cursor,
      "Up arrow in filter edit mode must rewind the list cursor"
    );
    // Now leave edit mode (resting). `j` is captured as a typed char
    // while editing, but in resting mode it goes through to the list
    // cursor.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
      !app.filter_input.is_editing(),
      "first Esc must exit edit (filter still focused)"
    );
    let before_j = app.list_cursor;
    pump_input(&mut app, key(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(
      app.list_cursor > before_j,
      "`j` in resting filter must scroll the list, got cursor {before_j} → {}",
      app.list_cursor
    );
  }

  #[test]
  fn drag_up_and_moved_remain_no_ops_even_with_capture_on() {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use std::path::PathBuf;
    // Wheel events scroll (covered by the wheel tests below). Drag,
    // Up, and Moved must stay no-ops so a user holding the
    // terminal's bypass modifier to copy text doesn't accidentally
    // scrub the list as they drag a selection box.
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.models = vec![DiscoveredModel {
      path: PathBuf::from("/m/a.gguf"),
      parent: PathBuf::from("/m"),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }];
    app.list_cursor = 2;
    let original_focus = app.focus;
    for kind in [
      MouseEventKind::Drag(MouseButton::Left),
      MouseEventKind::Up(MouseButton::Left),
      MouseEventKind::Moved,
    ] {
      pump_input(
        &mut app,
        TermEvent::Mouse(MouseEvent {
          kind,
          column: 0,
          row: 0,
          modifiers: KeyModifiers::NONE,
        }),
      );
    }
    assert_eq!(app.list_cursor, 2, "drag/up/moved must not move cursor");
    assert_eq!(
      app.focus, original_focus,
      "drag/up/moved must not change focus"
    );
  }

  #[test]
  fn left_click_inside_list_rect_focuses_models_list() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.focus = Focus::RightPane;
    {
      let mut hits = app.hit_rects.borrow_mut();
      hits.list_pane = Rect::new(0, 5, 40, 20);
      hits.right_pane = Rect::new(40, 5, 40, 20);
    }
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert_eq!(app.focus, Focus::List);
  }

  #[test]
  fn left_click_inside_right_pane_focuses_right_pane() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.focus = Focus::List;
    {
      let mut hits = app.hit_rects.borrow_mut();
      hits.list_pane = Rect::new(0, 5, 40, 20);
      hits.right_pane = Rect::new(40, 5, 40, 20);
    }
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 50,
        row: 10,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert_eq!(app.focus, Focus::RightPane);
  }

  #[test]
  fn left_click_on_tab_label_switches_right_tab() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.right_tab = RightTab::Settings;
    {
      let mut hits = app.hit_rects.borrow_mut();
      hits.list_pane = Rect::new(0, 5, 40, 20);
      hits.right_pane = Rect::new(40, 5, 40, 20);
      hits.right_tabs = vec![
        (RightTab::Settings, Rect::new(42, 5, 8, 1)),
        (RightTab::Logs, Rect::new(53, 5, 4, 1)),
      ];
    }
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 54,
        row: 5,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert_eq!(app.right_tab, RightTab::Logs);
    assert_eq!(app.focus, Focus::RightPane);
  }

  #[test]
  fn left_click_while_confirm_dialog_open_is_ignored() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.focus = Focus::RightPane;
    app.confirm_dialog = Some(ConfirmAction::KillDaemon);
    {
      let mut hits = app.hit_rects.borrow_mut();
      hits.list_pane = Rect::new(0, 5, 40, 20);
    }
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert_eq!(
      app.focus,
      Focus::RightPane,
      "click must not steal focus while a confirm dialog owns input"
    );
    assert!(
      app.confirm_dialog.is_some(),
      "click must not dismiss the dialog"
    );
  }

  #[test]
  fn mouse_events_still_dispatch_when_inline_launch_picker_is_open() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    // Regression test: the inline Settings launch picker is NOT a
    // modal. A wheel-driven field cycle materialises the picker on
    // the first tick; if we gated mouse input on
    // `launch_picker.is_some()` the second tick (and every one
    // after) would be silently dropped, leaving the TUI looking
    // hung even though keyboard input kept working.
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    app.launch_picker = Some(crate::tui::launch_picker::LaunchPickerState::for_model("m"));
    {
      let mut hits = app.hit_rects.borrow_mut();
      hits.list_pane = Rect::new(0, 5, 40, 20);
      hits.right_pane = Rect::new(40, 5, 40, 20);
    }
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert_eq!(
      app.focus,
      Focus::List,
      "mouse click must still dispatch when the inline picker is open — \
       it's not a modal"
    );
  }

  #[test]
  fn wheel_in_list_focus_moves_cursor_like_arrow_keys() {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crossterm::event::{MouseEvent, MouseEventKind};
    use std::path::PathBuf;
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.models = (0..5)
      .map(|i| DiscoveredModel {
        path: PathBuf::from(format!("/m/{i}.gguf")),
        parent: PathBuf::from("/m"),
        source: ModelSource::UserPath,
        metadata: None,
        parse_error: None,
        split_siblings: Vec::new(),
        display_label: None,
        multimodal: None,
        supported_backends: Vec::new(),
        mtp_head: None,
      })
      .collect();
    app.focus = Focus::List;
    app.list_cursor = 2;
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert!(
      app.list_cursor > 2,
      "wheel down in List focus should advance cursor (was 2 → {})",
      app.list_cursor
    );
  }

  #[test]
  fn wheel_in_logs_focus_scrolls_logs_buffer() {
    use crossterm::event::{MouseEvent, MouseEventKind};
    let mut app = App::new(Default::default());
    app.options.mouse_focus = true;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Logs;
    app.logs_state.set_tail(
      "L1".to_string(),
      (0..500).map(|i| format!("line {i}")).collect(),
    );
    assert_eq!(app.logs_state.scroll_offset, 0);
    pump_input(
      &mut app,
      TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
      }),
    );
    assert!(
      app.logs_state.scroll_offset > 0,
      "wheel up in Logs focus should advance the scroll offset (got {})",
      app.logs_state.scroll_offset
    );
    assert!(
      !app.logs_state.auto_scroll,
      "wheel-up must disable auto-scroll, matching the keyboard contract"
    );
  }

  #[test]
  fn q_in_list_focus_sets_should_exit() {
    let mut app = App::new(Default::default());
    let exit = pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(exit);
    assert!(app.should_exit);
  }

  #[test]
  fn slash_opens_filter_focus() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
  }

  #[test]
  fn typing_in_filter_extends_buffer() {
    let mut app = App::new(Default::default());
    app.open_filter();
    for ch in "qwen".chars() {
      pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(app.filter_input.buffer(), "qwen");
  }

  #[test]
  fn esc_in_filter_walks_back_edit_then_clear_then_close() {
    let mut app = App::new(Default::default());
    app.open_filter();
    app.filter_input.set_text("qwen");
    assert!(app.filter_input.is_editing());
    // 1st Esc: exit edit (buffer kept, focus on filter).
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
    assert!(!app.filter_input.is_editing());
    assert_eq!(app.filter_input.buffer(), "qwen");
    // 2nd Esc: clear buffer (still resting, still on filter).
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
    assert!(app.filter_input.is_empty());
    // 3rd Esc: close filter, return to list.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::List);
  }

  #[test]
  fn t_cycles_theme_and_emits_toast() {
    let mut app = App::new(Default::default());
    let original = app.options.theme;
    pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
    assert_ne!(app.options.theme, original);
    assert!(
      app
        .toast_message()
        .map(|s| s.contains("theme"))
        .unwrap_or(false),
      "theme cycle should toast: {:?}",
      app.toast_message()
    );
  }

  #[test]
  fn yank_url_with_no_managed_focus_shows_helpful_toast() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('u'), KeyModifiers::NONE));
    let msg = app.toast_message().unwrap();
    assert!(
      msg.contains("nothing to copy") || msg.contains("clipboard"),
      "copy toast must explain why: {msg}"
    );
  }

  #[test]
  fn writer_error_toast_adds_llama_server_hint_for_start_model_env_failure() {
    let toast = writer_error_toast(
      "start_model",
      "daemon launch environment not configured (binary / port range / log dir missing)",
    );
    assert!(toast.starts_with("launch failed:"), "got {toast:?}");
    assert!(
      toast.contains("LLAMASTASH_LLAMA_SERVER"),
      "missing env hint: {toast:?}"
    );
  }

  #[test]
  fn writer_error_toast_skips_llama_server_hint_for_unrelated_method() {
    let toast = writer_error_toast("favorite_add", "permission denied");
    assert_eq!(toast, "favorite_add failed: permission denied");
  }

  #[test]
  fn writer_error_toast_skips_hint_for_start_model_failures_unrelated_to_binary() {
    let toast = writer_error_toast("start_model", "port allocation exhausted");
    assert_eq!(toast, "start_model failed: port allocation exhausted");
  }

  #[test]
  fn apply_refresh_writer_error_renders_a_toast() {
    let mut app = App::new(crate::tui::app::AppOptions::default());
    apply_refresh(
      &mut app,
      RefreshTick::WriterError {
        method: "start_model",
        message: "daemon launch environment not configured".into(),
      },
    );
    let toast = app.toast_message().expect("toast must be set");
    assert!(toast.contains("launch failed"), "got {toast:?}");
  }

  #[test]
  fn submit_in_launch_picker_sends_start_model_through_writer() {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
    use crate::tui::launch_picker::PickerField;

    let mut app = App::new(Default::default());
    app.models = vec![DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: ModelSource::UserPath,
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        total_parameters: None,
        parameter_label: None,
        quant: Quant::Q4_K,
        quant_label: None,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }];
    app.go_top();
    // Open picker and tweak ctx + reasoning so we can assert they
    // arrive on the wire.
    app.open_launch_picker();
    let p = app.launch_picker.as_mut().unwrap();
    // ctx is fit-governed, so its ring inserts an Auto stop first
    // (Inherited → Auto → presets…); step twice to land on a preset.
    p.field = PickerField::Knob(crate::launch::knobs::kid("ctx-size"));
    p.cycle_focused_value_next(); // → Auto
    p.cycle_focused_value_next(); // → first ctx preset
    let expected_ctx = p.user_knobs.u32(crate::launch::knobs::kid("ctx-size"));
    assert!(expected_ctx.is_some(), "ctx should be a concrete preset");
    // reasoning is not fit-governed → no Auto stop (Inherited → on → off);
    // one step lands on `on`.
    p.field = PickerField::Knob(crate::launch::knobs::kid("reasoning"));
    p.cycle_focused_value_next(); // → on

    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));

    let cmd = rx.try_recv().expect("writer must receive start_model");
    match cmd {
      WriterCmd::StartModel(args) => {
        assert_eq!(args.model_path, PathBuf::from("/m/qwen.gguf"));
        assert_eq!(args.ctx, expected_ctx);
        assert_eq!(
          args.reasoning,
          Some(true),
          "On reasoning must serialise as Some(true)"
        );
      }
      other => panic!("expected StartModel, got {other:?}"),
    }
    assert!(
      app.launch_picker.is_none(),
      "submit must close the picker on success"
    );
  }

  #[test]
  fn toggle_favorite_sends_favorite_add_through_writer() {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

    let mut app = App::new(Default::default());
    app.models = vec![DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: ModelSource::UserPath,
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        total_parameters: None,
        parameter_label: None,
        quant: Quant::Q4_K,
        quant_label: None,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }];
    app.go_top();
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('f'), KeyModifiers::NONE),
      Some(&tx),
    );
    let add_cmd = rx.try_recv().expect("writer must receive favorite_add");
    assert!(
      matches!(&add_cmd, WriterCmd::FavoriteAdd(p) if p.as_path() == Path::new("/m/qwen.gguf"))
    );
    // Second press toggles off → favorite_remove.
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('f'), KeyModifiers::NONE),
      Some(&tx),
    );
    let remove_cmd = rx.try_recv().expect("writer must receive favorite_remove");
    assert!(
      matches!(&remove_cmd, WriterCmd::FavoriteRemove(p) if p.as_path() == Path::new("/m/qwen.gguf"))
    );
  }

  // ── regression coverage for the round-3 fixes ──────────────────

  fn fake_model_for_events(path: &str, parent: &str) -> crate::discovery::DiscoveredModel {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
    DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: ModelSource::UserPath,
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        total_parameters: None,
        parameter_label: None,
        quant: Quant::Q4_K,
        quant_label: None,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }
  }

  fn ready_managed_for_events(path: &str, port: u16) -> crate::tui::app::ManagedRow {
    use crate::tui::app::ManagedRow;
    use crate::tui::status_icons::SurfaceState;
    ManagedRow {
      launch_id: format!("L-{port}"),
      path: PathBuf::from(path),
      port,
      state: SurfaceState::Ready,
      device: None,
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }
  }

  #[test]
  fn esc_closes_help_dialog_from_any_focus() {
    // Help dialog steals Esc / `?` ahead of every focus, so the
    // user can dismiss it even mid-edit. Cover the three focuses
    // most likely to swallow Esc: List, Filter, ChatInput.
    for focus in [Focus::List, Focus::Filter, Focus::ChatInput] {
      let mut app = App::new(Default::default());
      app.focus = focus;
      app.show_help = true;
      pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
      assert!(
        !app.show_help,
        "Esc must close the help dialog from focus {focus:?}"
      );
    }
  }

  #[test]
  fn question_mark_closes_help_from_any_focus() {
    // The dialog title hints both Esc and `?` as close keys.
    let mut app = App::new(Default::default());
    app.show_help = true;
    pump_input(&mut app, key(KeyCode::Char('?'), KeyModifiers::NONE));
    assert!(!app.show_help, "`?` must toggle the help dialog off");
  }

  #[test]
  fn question_mark_with_shift_modifier_opens_help() {
    // Windows Terminal reports `?` (Shift+/) as `(Char('?'), SHIFT)`,
    // unlike most Unix terminals which report `NONE`. The `?` binding
    // is registered with `NONE`, so without SHIFT-normalization in
    // `action_for` the overlay never opened on Windows. Regression
    // guard for that platform discrepancy.
    let mut app = App::new(Default::default());
    assert!(!app.show_help);
    pump_input(&mut app, key(KeyCode::Char('?'), KeyModifiers::SHIFT));
    assert!(
      app.show_help,
      "`?` arriving with the SHIFT modifier (Windows) must open help"
    );
  }

  #[test]
  fn ctrl_s_on_running_row_stages_stop_confirm_popup() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    match app.confirm_dialog {
      Some(crate::tui::app::ConfirmAction::StopModel {
        ref launch_id,
        ref name,
      }) => {
        assert_eq!(launch_id, "L-41100");
        assert_eq!(name, "qwen");
      }
      ref other => panic!("expected StopModel confirm, got {other:?}"),
    }
  }

  #[test]
  fn ctrl_s_on_non_running_row_toasts_instead_of_staging_confirm() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(app.confirm_dialog.is_none(), "no managed row = no popup");
    let toast = app.toast_message().unwrap_or("");
    assert!(toast.contains("nothing to stop"), "toast: {toast}");
  }

  #[test]
  fn confirm_dialog_y_dispatches_stop_to_writer() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(4);
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('s'), KeyModifiers::CONTROL),
      Some(&tx),
    );
    assert!(app.confirm_dialog.is_some(), "confirm popup primed");
    // Press `y` to confirm; writer should receive a StopModel cmd.
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('y'), KeyModifiers::NONE),
      Some(&tx),
    );
    assert!(app.confirm_dialog.is_none(), "popup cleared on confirm");
    let cmd = rx.try_recv().expect("writer must receive stop");
    assert!(matches!(cmd, WriterCmd::StopModel { launch_id } if launch_id == "L-41100"));
  }

  #[test]
  fn confirm_dialog_esc_cancels_without_dispatching() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(4);
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('s'), KeyModifiers::CONTROL),
      Some(&tx),
    );
    pump_input_with_writer(&mut app, key(KeyCode::Esc, KeyModifiers::NONE), Some(&tx));
    assert!(app.confirm_dialog.is_none(), "popup cleared on Esc");
    assert!(
      rx.try_recv().is_err(),
      "no StopModel must reach the writer on cancel"
    );
  }

  #[test]
  fn ctrl_k_stages_kill_daemon_confirm() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(matches!(
      app.confirm_dialog,
      Some(crate::tui::app::ConfirmAction::KillDaemon)
    ));
  }

  #[test]
  fn kill_daemon_confirm_dispatches_shutdown_to_writer() {
    let mut app = App::new(Default::default());
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(4);
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('k'), KeyModifiers::CONTROL),
      Some(&tx),
    );
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    let cmd = rx.try_recv().expect("writer must receive shutdown");
    assert!(matches!(cmd, WriterCmd::Shutdown));
  }

  #[test]
  fn ctrl_r_stages_restart_daemon_confirm() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(matches!(
      app.confirm_dialog,
      Some(crate::tui::app::ConfirmAction::RestartDaemon)
    ));
  }

  #[test]
  fn restart_daemon_confirm_dispatches_to_writer() {
    let mut app = App::new(Default::default());
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(4);
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('r'), KeyModifiers::CONTROL),
      Some(&tx),
    );
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    let cmd = rx.try_recv().expect("writer must receive restart");
    assert!(matches!(cmd, WriterCmd::RestartDaemon));
  }

  #[test]
  fn tab_in_list_focus_walks_chain_into_right_pane_navigation_mode() {
    // Edit-mode rule: Tab lands on RightPane focus (not ChatInput),
    // even when the active tab is Chat. The user must press `e` to
    // start typing.
    let mut app = App::new(Default::default());
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::RightPane);
  }

  #[test]
  fn e_in_right_pane_enters_chat_input_when_chat_tab_active() {
    use crate::tui::tabs::RightTab;
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Chat;
    pump_input(&mut app, key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::ChatInput);
  }

  #[test]
  fn esc_in_chat_input_exits_to_right_pane_navigation() {
    let mut app = App::new(Default::default());
    app.focus = Focus::ChatInput;
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::RightPane);
  }

  #[test]
  fn smart_y_yanks_path_when_no_managed_launch_focused() {
    // The smart `y` fallback yields the *path* for a not-running
    // row, so `y` always copies something useful. We can't observe
    // the clipboard from a test, but we can verify `build_yank_text`
    // returns the path.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    let text = super::build_yank_text(&app, Action::YankUrl).expect("path fallback");
    assert!(text.ends_with("qwen.gguf"));
  }

  #[test]
  fn smart_y_yanks_url_when_managed_launch_focused() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    let text = super::build_yank_text(&app, Action::YankUrl).expect("url");
    assert_eq!(text, "http://127.0.0.1:41100/v1");
  }

  #[test]
  fn curl_yank_prefers_proxy_port_while_url_keeps_backend_port() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();

    let proxy = |status: &str, auth: &str| crate::tui::app::ProxyInfo {
      enabled: true,
      listen: Some("127.0.0.1:11435".to_string()),
      status: status.to_string(),
      bind_error: None,
      auth: Some(auth.to_string()),
    };

    // No proxy → curl falls back to the raw backend port.
    let curl_direct = super::build_yank_text(&app, Action::YankCurl).expect("curl");
    assert!(
      curl_direct.contains("http://127.0.0.1:41100/v1/chat/completions"),
      "curl fallback must hit the backend port: {curl_direct}"
    );

    // Listening + auth-free → curl targets the port-stable proxy.
    app.daemon_info.proxy = Some(proxy("listening", "none"));
    let curl_proxied = super::build_yank_text(&app, Action::YankCurl).expect("curl");
    assert!(
      curl_proxied.contains("http://127.0.0.1:11435/v1/chat/completions"),
      "curl must route through an auth-free proxy: {curl_proxied}"
    );

    // Listening but AUTH ENFORCED → a keyless proxy curl would 401, so fall
    // back to the backend port (which takes no auth).
    app.daemon_info.proxy = Some(proxy("listening", "enforced"));
    let curl_authed = super::build_yank_text(&app, Action::YankCurl).expect("curl");
    assert!(
      curl_authed.contains("http://127.0.0.1:41100/v1/chat/completions"),
      "curl must avoid an auth-enforced proxy: {curl_authed}"
    );

    // url always keeps the raw backend port, regardless of proxy state.
    let url = super::build_yank_text(&app, Action::YankUrl).expect("url");
    assert_eq!(
      url, "http://127.0.0.1:41100/v1",
      "url yank keeps the raw backend port"
    );
  }

  #[test]
  fn shift_m_jumps_focus_to_models_list_from_right_pane() {
    use crate::tui::tabs::RightTab;
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('M'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, Focus::List, "Shift+M must focus the models list");
  }

  #[test]
  fn shift_s_focuses_settings_tab_from_models_list_with_no_running_model() {
    use crate::tui::tabs::RightTab;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    // Settings is always reachable — even without a running launch.
    pump_input(&mut app, key(KeyCode::Char('S'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Settings);
  }

  #[test]
  fn shift_l_focuses_logs_tab_only_when_model_is_running() {
    use crate::tui::tabs::RightTab;
    // Not running → Shift+L toasts and stays put.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('L'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, Focus::List, "no running model = no jump");
    assert!(
      app
        .toast_message()
        .map(|s| s.contains("Logs unavailable"))
        .unwrap_or(false),
      "toast should explain the gate: {:?}",
      app.toast_message()
    );

    // Running → Shift+L parks focus on Logs.
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    pump_input(&mut app, key(KeyCode::Char('L'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Logs);
  }

  #[test]
  fn shift_c_focuses_mode_tab_when_running_else_toasts() {
    use crate::tui::tabs::RightTab;
    // Not running → toast + no jump.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('C'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, Focus::List);
    assert!(
      app
        .toast_message()
        .map(|s| s.contains("Chat/Embed/Rerank unavailable"))
        .unwrap_or(false),
      "toast should explain the gate: {:?}",
      app.toast_message()
    );

    // Running chat model → Shift+C parks focus on the Chat tab.
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    pump_input(&mut app, key(KeyCode::Char('C'), KeyModifiers::SHIFT));
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Chat);
  }

  #[test]
  fn shift_r_and_shift_e_are_aliases_for_shift_c() {
    // A model only ever exposes one of Chat/Embed/Rerank — `C`,
    // `R`, and `E` all map through `apply_focus_chat_tab` and land
    // on whichever mode tab is reachable. (Daemon restart lives on
    // `Ctrl+R` — see `ctrl_r_stages_restart_daemon_confirm`.)
    use crate::tui::tabs::RightTab;
    for letter in ['R', 'E'] {
      let mut app = App::new(Default::default());
      app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
      app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
      app.go_top();
      app.focus = Focus::List;
      app.right_tab = RightTab::Settings;
      pump_input(&mut app, key(KeyCode::Char(letter), KeyModifiers::SHIFT));
      assert_eq!(
        app.focus,
        Focus::RightPane,
        "Shift+{letter} should park focus on the right pane"
      );
      assert_eq!(
        app.right_tab,
        RightTab::Chat,
        "Shift+{letter} should land on whichever mode tab is live"
      );
    }
  }

  #[test]
  fn up_down_in_settings_tab_cycle_picker_fields() {
    // Round-7 navigation model: ↑/↓ in `Focus::RightPane` while
    // right_tab == Settings cycle the launch-picker form fields
    // (ctx → reasoning → advanced → ctx). ←/→ are reserved for
    // value cycling on the focused field; Tab cycles panes.
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;

    // The cursor opens on the Preset row, so the first ↓ moves onto Ctx.
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::RightPane, "↓ must not leave the pane");
    let field = app
      .launch_picker
      .as_ref()
      .map(|p| p.field)
      .expect("↓ in Settings should materialise the picker form");
    assert_eq!(
      field,
      PickerField::Knob(crate::launch::knobs::kid("ctx-size"))
    );

    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
      app.launch_picker.as_ref().unwrap().field,
      PickerField::Knob(crate::launch::knobs::kid("reasoning"))
    );
  }

  #[test]
  fn up_in_settings_tab_cycles_picker_fields_backward() {
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;

    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::RightPane);
    // The cursor opens on the Preset row (the form's lead), so Up wraps
    // backward to the last row (Extras).
    assert_eq!(
      app.launch_picker.as_ref().expect("picker").field,
      PickerField::Extras
    );
  }

  #[test]
  fn arrows_in_settings_do_not_open_picker_over_running_launch() {
    // Regression: with a managed launch focused and no picker
    // staged, any arrow key used to silently call `with_picker` and
    // swap the read-only "Running launch" pane for the editable
    // form. That hid the live params behind the form and surprised
    // users. ↑/↓ now scroll the read-only view (the running-launch
    // panel has ~17 rows and can overflow short viewports); ←/→
    // remain no-ops. `e` is the explicit gate to start editing.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
      pump_input(&mut app, key(code, KeyModifiers::NONE));
      assert!(
        app.launch_picker.is_none(),
        "{code:?} must not stage a picker while a managed launch is focused"
      );
    }
  }

  #[test]
  fn arrows_in_settings_scroll_read_only_running_view() {
    // ↑/↓ over a running launch with no picker staged now drive the
    // read-only view's scroll offset so the user can walk past
    // viewport-clipped knob rows without `e`-staging the editor.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    assert_eq!(app.running_view_scroll.get(), 0);
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
      app.running_view_scroll.get(),
      1,
      "↓ must scroll one row down"
    );
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.running_view_scroll.get(), 2);
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.running_view_scroll.get(), 1, "↑ must scroll one row up");
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.running_view_scroll.get(), 0, "↑ saturates at 0");
  }

  #[test]
  fn e_in_settings_opens_picker_over_running_launch() {
    // `e` (Action::EnterEdit) on the Settings tab is the explicit
    // gate that replaces the old auto-stage-on-arrow behaviour.
    // With a managed launch focused, it stages the picker so the
    // user can edit next-launch params over the read-only running
    // view.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    assert!(app.launch_picker.is_none());
    pump_input(&mut app, key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(
      app.launch_picker.is_some(),
      "`e` on Settings must stage the launch picker over a running launch"
    );
    assert_eq!(app.focus, Focus::RightPane, "focus must stay on the pane");
  }

  #[test]
  fn enter_after_editing_ctx_writes_typed_value_to_user_knobs() {
    // Regression: ctx commit silently dropped the typed value because
    // the u32 parse arm in `commit_inline_edit` missed `crate::launch::knobs::kid("ctx-size")`
    // — it fell through to the catch-all `_ => Ok(())` arm, the edit
    // closed, and the picker re-rendered the resolved (default) value.
    // The fix makes the inner match exhaustive on `KnobField` so a
    // future drift fails to compile rather than silently swallowing
    // user input.
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
      app.launch_picker.as_ref().expect("picker").field,
      PickerField::Preset,
      "default focus leads on the preset row"
    );
    // Move down to the ctx row to exercise its inline edit.
    app.launch_picker.as_mut().unwrap().field =
      PickerField::Knob(crate::launch::knobs::kid("ctx-size"));
    // `e` opens inline edit; type a fresh value; Enter commits.
    pump_input(&mut app, key(KeyCode::Char('e'), KeyModifiers::NONE));
    {
      let edit = &mut app.launch_picker.as_mut().expect("picker open").inline_edit;
      edit.input.clear();
      edit.input.set_text("65536");
      edit.input.enter_edit();
    }
    pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
    let committed = app.launch_picker.as_ref().expect("picker still staged");
    assert_eq!(
      committed
        .user_knobs
        .u32(crate::launch::knobs::kid("ctx-size")),
      Some(65536),
      "ctx commit must write the typed value to user_knobs, not silently drop it"
    );
    assert!(
      !committed.inline_edit.is_open(),
      "successful commit closes the inline edit"
    );
  }

  #[test]
  fn esc_in_edit_for_launch_mode_closes_picker_instead_of_leaving_pane() {
    // Esc on RightPane normally returns focus to the Models list.
    // When `e` staged the launch picker over a running launch,
    // Esc must instead discard the picker — same rationale as
    // closing the Advanced panel: dismiss the modal interaction
    // first, leave the pane only on a second press.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(app.launch_picker.is_some());

    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
      app.launch_picker.is_none(),
      "first Esc must close the edit-for-launch picker"
    );
    assert_eq!(
      app.focus,
      Focus::RightPane,
      "first Esc must keep focus on the pane, not jump to Models list"
    );

    // A second Esc should fall through to the standard
    // `FocusList` behaviour now that the picker is gone.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::List, "second Esc returns to Models list");
  }

  #[test]
  fn left_arrow_on_models_list_is_unbound() {
    // Round-8 reintroduces an asymmetric arrow surface on the
    // Models list: `→` enters the right pane (mirrors kdash-style
    // "open the panel to my right"); `←` stays unbound because
    // Esc handles the return path and pane-cycle still works via
    // Tab/⇧+Tab/h/l.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    assert_eq!(app.focus, Focus::List);
    pump_input(&mut app, key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::List, "← must not change focus from List");
  }

  #[test]
  fn right_arrow_on_models_list_does_not_change_focus() {
    // The `→` shortcut is intentionally unbound (it read as
    // "cycle value" everywhere else and the asymmetric pane-jump
    // confused users). Pane focus moves via Tab / Shift+Tab / `l`
    // / `h` instead. Verify a stray Right keystroke is a no-op.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    assert_eq!(app.focus, Focus::List);
    pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(
      app.focus,
      Focus::List,
      "→ must NOT move focus off Models (binding removed)"
    );
  }

  #[test]
  fn tab_in_right_pane_logs_tab_cycles_to_next_pane() {
    // Round-7 makes Tab universal: in any focus / tab combo, Tab
    // moves to the next pane. In Logs that means jumping past the
    // right pane back to the list.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Logs;
    pump_input(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
    // Focus advances along the chain — for a Ready chat model the
    // chain is [List, Settings, Logs, Chat]; Tab from Logs lands
    // on the next entry (still RightPane focus, right_tab moves).
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Chat, "Tab walks the right tabs");
    assert!(
      app.launch_picker.is_none(),
      "Tab in Logs must not materialise the picker"
    );
  }

  #[test]
  fn arrow_keys_in_right_pane_scroll_chat_output() {
    // Round-8: Chat/Embed/Rerank output panes get arrow-key
    // scroll mirroring the Logs pane. Editing focus is not
    // active, so ↑/↓ walk the response viewport. ↓ moves toward
    // the end (offset increments), ↑ back toward the top.
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Chat;
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.chat.scroll_offset.get(), 1);
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.chat.scroll_offset.get(), 2);
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.chat.scroll_offset.get(), 1);
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
      app.chat.scroll_offset.get(),
      0,
      "Up past zero must clamp to 0, not underflow"
    );
  }

  #[test]
  fn arrow_keys_in_right_pane_scroll_embed_and_rerank_outputs() {
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Embed;
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.embed.scroll_offset.get(), 1);
    app.right_tab = RightTab::Rerank;
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.rerank.scroll_offset.get(), 1);
  }

  #[test]
  fn chat_reset_for_send_clears_scroll_offset() {
    // Sending a new prompt resets the viewport to the top so the
    // response streams from the beginning. Round-8 reset path.
    let mut app = App::new(Default::default());
    app.chat.scroll_offset.set(7);
    app.chat.reset_for_send();
    assert_eq!(app.chat.scroll_offset.get(), 0);
  }

  #[test]
  fn logs_scroll_keys_in_right_pane_disable_auto_scroll() {
    use crate::tui::tabs::RightTab;
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Logs;
    app.logs_state.lines = (0..40).map(|i| format!("line {i}")).collect();
    app.logs_state.auto_scroll = true;
    pump_input(&mut app, key(KeyCode::Char('k'), KeyModifiers::NONE));
    assert!(
      !app.logs_state.auto_scroll,
      "scrolling up disables auto-scroll"
    );
    assert_eq!(app.logs_state.scroll_offset, 1);
    pump_input(&mut app, key(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.logs_state.scroll_offset, 0);
    assert!(
      app.logs_state.auto_scroll,
      "returning to tail re-enables auto-scroll"
    );
  }

  #[test]
  fn ctrl_s_on_settings_tab_opens_stop_confirm_for_running_launch() {
    // Destructive policy: Stop lives on Ctrl+S in both List and
    // RightPane. Bare `s` on Settings is the Logs auto-scroll
    // toggle and nothing else.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    match app.confirm_dialog {
      Some(crate::tui::app::ConfirmAction::StopModel { ref launch_id, .. }) => {
        assert_eq!(launch_id, "L-41100");
      }
      ref other => panic!("expected StopModel confirm, got {other:?}"),
    }
    // Auto-scroll must remain untouched — Ctrl+S on Settings is
    // routed away from the logs branch.
    assert!(
      app.logs_state.auto_scroll,
      "Settings Ctrl+S must not toggle the logs auto-scroll"
    );
  }

  #[test]
  fn ctrl_s_on_settings_tab_toasts_without_managed_launch() {
    // Stop on a non-running selection toasts instead of silently
    // no-oping — same path as Ctrl+S anywhere else.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(app.confirm_dialog.is_none());
    assert!(
      app.toast.is_some(),
      "Settings Ctrl+S with no managed row must toast, not silently no-op"
    );
  }

  #[test]
  fn s_on_chat_tab_does_not_flip_logs_auto_scroll() {
    // F1 #1 (P0 regression fix): the fall-through `else` of
    // `ToggleAutoScroll` used to flip `logs_state.auto_scroll` for
    // every RightPane focus that wasn't Settings — including Chat
    // / Embed / Rerank. Repro: focus a running model, switch to
    // Chat, press `s`, switch back to Logs → auto_scroll silently
    // flipped. Gate the toggle on `RightTab::Logs` to close it.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.logs_state.auto_scroll = true;
    for tab in [RightTab::Chat, RightTab::Embed, RightTab::Rerank] {
      app.right_tab = tab;
      pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
      assert!(
        app.logs_state.auto_scroll,
        "`s` on {tab:?} must not toggle the Logs auto-scroll"
      );
    }
  }

  #[test]
  fn s_on_logs_tab_toasts_the_new_auto_scroll_state() {
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Logs;
    app.logs_state.lines = (0..10).map(|i| format!("line {i}")).collect();
    app.logs_state.auto_scroll = true;
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
    assert!(!app.logs_state.auto_scroll);
    assert_eq!(app.toast_message(), Some("auto-scroll off"));
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
    assert!(app.logs_state.auto_scroll);
    assert_eq!(app.toast_message(), Some("auto-scroll on"));
  }

  #[test]
  fn r_on_chat_tab_toasts_the_new_reasoning_state() {
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Chat;
    let initial = app.chat.collapse_thinks;
    pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::NONE));
    let expected = if app.chat.collapse_thinks {
      "reasoning collapsed"
    } else {
      "reasoning shown"
    };
    assert_ne!(app.chat.collapse_thinks, initial, "r must flip the state");
    assert_eq!(app.toast_message(), Some(expected));
  }

  #[test]
  fn launch_submit_with_running_instance_stages_confirm_popup() {
    // Round-8: pressing Enter on the Settings tab for a model
    // that's already running stages a LaunchDuplicate confirm
    // popup instead of dispatching immediately. The writer must
    // not see StartModel until the user explicitly confirms.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.open_launch_picker();
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    // Confirm popup primed; no writer traffic yet.
    match app.confirm_dialog {
      Some(crate::tui::app::ConfirmAction::LaunchDuplicate {
        ref name,
        active_instances,
        ..
      }) => {
        assert_eq!(active_instances, 1);
        assert_eq!(name, "qwen");
      }
      ref other => panic!("expected LaunchDuplicate confirm, got {other:?}"),
    }
    assert!(
      rx.try_recv().is_err(),
      "writer must not see a launch until user confirms"
    );
    // `y` confirms — writer should now see a StartModel.
    pump_input_with_writer(
      &mut app,
      key(KeyCode::Char('y'), KeyModifiers::NONE),
      Some(&tx),
    );
    let cmd = rx.try_recv().expect("writer must receive start_model");
    assert!(matches!(cmd, WriterCmd::StartModel(_)));
    assert!(
      app.confirm_dialog.is_none(),
      "popup must clear after confirm"
    );
  }

  #[test]
  fn launch_submit_with_running_instance_cancels_cleanly_on_esc() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.open_launch_picker();
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    assert!(app.confirm_dialog.is_some(), "popup primed");
    pump_input_with_writer(&mut app, key(KeyCode::Esc, KeyModifiers::NONE), Some(&tx));
    assert!(app.confirm_dialog.is_none(), "popup cleared on Esc");
    assert!(
      rx.try_recv().is_err(),
      "no StartModel should reach the writer on cancel"
    );
    // Picker stays open so the user can adjust + retry.
    assert!(app.launch_picker.is_some());
  }

  #[test]
  fn launch_submit_without_running_instance_dispatches_directly() {
    // No managed launch → no confirm popup. The writer receives
    // StartModel on the first Enter press from the Settings tab.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.open_launch_picker();
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    let cmd = rx.try_recv().expect("writer must receive start_model");
    assert!(matches!(cmd, WriterCmd::StartModel(_)));
    assert!(app.confirm_dialog.is_none(), "no confirm popup expected");
  }

  #[test]
  fn enter_on_settings_without_prior_arrow_press_still_launches() {
    // Repro the user-reported bug: focus a model, Tab/Left/Shift+S
    // to land on the Settings tab, press Enter — must launch.
    // Pre-fix, Enter early-returned when `launch_picker == None`
    // and the user had to tap an arrow first to materialise the
    // form. With the auto-stage in `apply_launch_submit`, the
    // first Enter dispatches StartModel directly.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    // Walk to Settings via Tab (cycle_focus), exactly like the user
    // would after focusing the list. The chain is
    // [List, Settings] for an unlaunched selection so one Tab lands.
    pump_input(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Settings);
    assert!(
      app.launch_picker.is_none(),
      "picker must not be staged just by tabbing into Settings"
    );
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    let cmd = rx.try_recv().expect("Enter must dispatch StartModel");
    assert!(matches!(cmd, WriterCmd::StartModel(_)));
  }

  #[test]
  fn u_c_p_yanks_work_from_settings_tab() {
    // Round-8: `p` always yanks the focused path; `u` and `c`
    // need a running endpoint. Dispatch happens through the same
    // `apply_action` path as the Models list, so a successful
    // yank toast is enough to prove the binding routes. The toast
    // text names the thing copied (`copied URL/curl/path via …`)
    // on success or surfaces a `clipboard unavailable` fallback
    // when no backend is reachable — assert on the label in either
    // shape so the wording can't silently regress.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('p'), KeyModifiers::NONE));
    let path_toast = app
      .toast_message()
      .expect("p must yank the path")
      .to_string();
    assert!(
      path_toast.contains("copied path") || path_toast.contains("clipboard unavailable"),
      "p toast should name the copied thing or surface a clipboard failure; got: {path_toast}"
    );
    app.toast = None;
    pump_input(&mut app, key(KeyCode::Char('u'), KeyModifiers::NONE));
    let url_toast = app
      .toast_message()
      .expect("u must yank the URL")
      .to_string();
    assert!(
      url_toast.contains("copied URL") || url_toast.contains("clipboard unavailable"),
      "u toast should name the copied thing or surface a clipboard failure; got: {url_toast}"
    );
    app.toast = None;
    pump_input(&mut app, key(KeyCode::Char('c'), KeyModifiers::NONE));
    let curl_toast = app
      .toast_message()
      .expect("c must yank the curl")
      .to_string();
    assert!(
      curl_toast.contains("copied curl") || curl_toast.contains("clipboard unavailable"),
      "c toast should name the copied thing or surface a clipboard failure; got: {curl_toast}"
    );
  }

  #[test]
  fn c_on_logs_tab_copies_log_buffer() {
    // `c` is double-duty: on the Settings tab (and elsewhere) it
    // yanks the curl one-liner; on the Logs tab it copies the
    // full log buffer to the system clipboard. Tab-aware dispatch
    // mirrors the existing `s` (ToggleAutoScroll) precedent.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Logs;
    app.logs_state.lines = vec!["line one".to_string(), "line two".to_string()];
    pump_input(&mut app, key(KeyCode::Char('c'), KeyModifiers::NONE));
    let logs_toast = app
      .toast_message()
      .expect("c on Logs tab must produce a toast")
      .to_string();
    assert!(
      logs_toast.contains("copied logs") || logs_toast.contains("clipboard unavailable"),
      "logs-copy toast should mention logs or a clipboard failure; got: {logs_toast}"
    );
    // Empty buffer surfaces a dedicated "no log lines yet" toast
    // rather than copying an empty string.
    app.logs_state.lines.clear();
    app.toast = None;
    pump_input(&mut app, key(KeyCode::Char('c'), KeyModifiers::NONE));
    let empty_toast = app
      .toast_message()
      .expect("c on empty Logs tab must produce a toast")
      .to_string();
    assert!(
      empty_toast.contains("no log lines yet"),
      "empty-buffer toast should explain there's nothing to copy; got: {empty_toast}"
    );
  }

  // ── handle_event dirty-flag policy ─────────────────────────────

  /// `handle_event` needs a writer channel; tests don't drive the
  /// daemon so the receiver is dropped after the call returns. The
  /// sender is non-buffering enough for any synchronous push the
  /// dispatch path might emit (none of the input fixtures here do).
  fn fresh_writer_channel() -> (mpsc::Sender<WriterCmd>, mpsc::Receiver<WriterCmd>) {
    mpsc::channel::<WriterCmd>(8)
  }

  #[tokio::test]
  async fn handle_event_input_always_requests_redraw() {
    let mut app = App::new(Default::default());
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    let evt = Event::Input(key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(
      handle_event(&mut app, evt, &writer_tx),
      "input events must request a redraw"
    );
  }

  #[tokio::test]
  async fn handle_event_refresh_always_requests_redraw() {
    let mut app = App::new(Default::default());
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    let evt = Event::Refresh(RefreshTick::Disconnected);
    assert!(
      handle_event(&mut app, evt, &writer_tx),
      "refresh ticks must request a redraw"
    );
  }

  #[tokio::test]
  async fn handle_event_chat_stream_requests_redraw() {
    let mut app = App::new(Default::default());
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    let evt = Event::ChatStream(ChatStreamMsg::Delta("hi".into()));
    assert!(
      handle_event(&mut app, evt, &writer_tx),
      "chat stream chunks must request a redraw"
    );
  }

  #[tokio::test]
  async fn handle_event_idle_tick_skips_redraw() {
    // Empty app, no toast, no hf dialog, no strip error → a pure
    // Tick must NOT request a redraw. This is the load-bearing
    // case for the idle-CPU goal.
    let mut app = App::new(Default::default());
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    assert!(
      !handle_event(&mut app, Event::Tick, &writer_tx),
      "idle Tick should not request a redraw"
    );
  }

  #[tokio::test]
  async fn handle_event_tick_with_active_toast_requests_redraw() {
    // Toast is time-decayed (TOAST_TTL on App::expire_toast). Without
    // a redraw on Tick, the toast would only disappear on the next
    // non-Tick event — at idle this is the ~750ms refresher cadence,
    // which is visibly laggy. The Tick must keep the redraw alive
    // until the toast either expires or another event lands.
    let mut app = App::new(Default::default());
    app.show_toast("yanked");
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    assert!(
      handle_event(&mut app, Event::Tick, &writer_tx),
      "Tick with an active toast must request a redraw"
    );
  }

  #[tokio::test]
  async fn handle_event_tick_with_strip_error_requests_redraw() {
    // Download-strip lingering error fades after ERROR_LINGER. Same
    // logic as the toast: Tick must keep redrawing so the strip can
    // visibly clear when the linger window elapses.
    let mut app = App::new(Default::default());
    app.download_strip.last_error = Some(("rate-limited".to_string(), std::time::Instant::now()));
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    assert!(
      handle_event(&mut app, Event::Tick, &writer_tx),
      "Tick with a strip error must request a redraw so the linger window can elapse"
    );
  }

  #[tokio::test]
  async fn drain_and_handle_coalesces_queued_stream_deltas() {
    // A streaming completion pushes one Delta per token. The run loop
    // must fold the whole queued burst into a single batch — one
    // redraw, all deltas applied — instead of a redraw per token.
    // Otherwise a queued scroll/keystroke waits behind the backlog and
    // scrolling reads as frozen mid-stream.
    let mut app = App::new(Default::default());
    app.chat.reset_for_send();
    let (writer_tx, _writer_rx) = fresh_writer_channel();
    let (tx, mut rx) = mpsc::channel::<Event>(16);
    for s in ["a", "b", "c"] {
      tx.send(Event::ChatStream(ChatStreamMsg::Delta(s.into())))
        .await
        .unwrap();
    }
    drop(tx); // close so the drain stops once the queued burst is spent
    let first = rx.recv().await.expect("first queued delta");
    let needs_redraw = drain_and_handle(&mut app, first, &mut rx, &writer_tx);
    assert!(needs_redraw, "a stream batch must request a redraw");
    assert_eq!(
      app.chat.response, "abc",
      "every queued delta must be applied within the single coalesced batch"
    );
  }
}
