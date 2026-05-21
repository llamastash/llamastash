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

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::ipc::Client;
use crate::tui::app::{App, ConfirmAction};
use crate::tui::keybindings::{Action, Focus};
use crate::tui::oai_client::{
  embed as oai_embed, rerank as oai_rerank, spawn_chat_stream, ChatStreamMsg,
};
use crate::tui::tabs::rerank::RerankField;
use crate::tui::tabs::RightTab;
use crate::util::clipboard;

/// Catalog/status refresh cadence in the steady state. Latency
/// requirement (R29) is bounded by `POLL_INTERVAL`, not by this; the
/// refresher only governs how stale daemon snapshots may get.
const REFRESH_INTERVAL: Duration = Duration::from_millis(750);
/// Initial reconnect backoff used when the daemon is unreachable.
/// Doubles on each failure up to [`REFRESH_INTERVAL`] so a freshly
/// started daemon gets attached within ~2 s on a cold connect.
const RECONNECT_INITIAL: Duration = Duration::from_millis(120);
/// crossterm input poll interval. Kept tight so worst-case
/// key-to-redraw stays under the 16 ms target (origin: R29).
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// Commands the input pump asks the writer task to forward to the
/// daemon. Keeping this enum narrow (vs. raw JSON) lets the type
/// system enforce that the input layer never assembles a malformed
/// request.
#[derive(Debug, Clone)]
pub enum WriterCmd {
  /// `start_model` — launch the focused model with the picker's
  /// ctx / reasoning / advanced / mode fields. `reasoning: None`
  /// (round-8) means "omit the field"; the daemon then falls back
  /// to whatever the model's metadata implies.
  StartModel {
    model_path: PathBuf,
    ctx: Option<u32>,
    reasoning: Option<bool>,
    advanced: Vec<String>,
    /// Catalog-derived mode hint (chat/embedding/rerank). `None`
    /// keeps the daemon's `Chat` default — preserves backwards
    /// compatibility for the picker until catalog plumbing is
    /// wired through.
    mode: Option<crate::launch::mode::LaunchMode>,
    /// Soft port preference. Emitted as `prefer_port` on the wire;
    /// the daemon honours when free and falls back to allocate
    /// otherwise. Seeded from `last_params[path].port` so a
    /// returning user lands on the same port.
    prefer_port: Option<u16>,
  },
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
}

/// One pump of input events. Returns `true` when the App is asking
/// the loop to exit (the user pressed `q` / Ctrl+C). The `writer`
/// channel is optional so unit tests and the inline test backend
/// drive `pump_input` without spinning a daemon writer task.
pub fn pump_input(app: &mut App, evt: Event) -> bool {
  pump_input_with_writer(app, evt, None)
}

/// Variant of [`pump_input`] that hands a writer-channel handle into
/// the action dispatch. Used by the production [`run`] loop so
/// `Submit` on the launch picker actually dispatches `start_model`.
pub fn pump_input_with_writer(
  app: &mut App,
  evt: Event,
  writer: Option<&mpsc::Sender<WriterCmd>>,
) -> bool {
  match evt {
    Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key, writer),
    _ => {}
  }
  app.should_exit
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
  // chat prompt. Anything else falls through to normal dispatch.
  if app.show_help && matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
    app.show_help = false;
    return;
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
  // Resolve the bound action first; if a focus doesn't have a binding
  // for this keypress *and* it's a text-input focus, fall through to
  // the per-focus character handler so alphanumerics extend the
  // buffer instead of being silently dropped.
  let bound = app.action_for(app.focus, key.code, key.modifiers);
  match app.focus {
    Focus::Filter => handle_filter_input(app, key),
    Focus::AdvancedPanel => handle_advanced_input(app, key),
    Focus::HfDialog => handle_hf_dialog_input(app, key, writer),
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
      if app.focused_name().is_some() {
        app.open_launch_picker();
      }
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

fn handle_advanced_input(app: &mut App, key: KeyEvent) {
  use crate::tui::input_field::InputOutcome;
  let outcome = match app.advanced_panel.as_mut() {
    Some(panel) => panel.buffer.handle_key(key),
    None => return,
  };
  match outcome {
    InputOutcome::Handled => {}
    InputOutcome::Submit => app.close_advanced_panel(),
    InputOutcome::PassThrough => {
      // Field decided not to handle: Esc walks back at the panel
      // level (close), other keys are unbound.
      if matches!(key.code, KeyCode::Esc) {
        app.close_advanced_panel();
      }
    }
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
    Action::OpenLaunchPicker => app.open_launch_picker(),
    Action::OpenAdvancedPanel => app.open_advanced_panel(),
    Action::OpenHfDialog => apply_open_hf_dialog(app),
    Action::Submit => match app.focus {
      Focus::AdvancedPanel => app.close_advanced_panel(),
      Focus::EmbedInput => apply_embed_submit(app),
      Focus::RerankInput => apply_rerank_submit(app),
      // The Settings tab drives launch submission from inside the
      // right pane. The launch_picker state object is still the
      // form's source of truth.
      Focus::RightPane if app.right_tab == RightTab::Settings => {
        apply_launch_submit(app, writer);
      }
      _ => {}
    },
    Action::Cancel => {
      if app.show_help {
        app.show_help = false;
      } else if app.focus == Focus::AdvancedPanel {
        app.close_advanced_panel();
      } else if app.focus == Focus::RightPane
        && app.right_tab == RightTab::Settings
        && app.launch_picker.is_some()
        && app.focused_managed().is_some()
      {
        // `e` staged the edit-for-launch picker over a running
        // model's read-only view. Esc dismisses the picker back to
        // the live params display without leaving the Settings tab.
        // Direct null (rather than `close_launch_picker`) because
        // that helper would also flip focus back to the Models list.
        app.launch_picker = None;
      }
    }
    Action::YankUrl | Action::YankCurl | Action::YankPath => {
      let text = build_yank_text(app, action);
      if let Some(text) = text {
        match clipboard::write(&text) {
          Ok(backend) => app.show_toast(format!("yanked via {backend}")),
          Err(e) => app.show_toast(format!("clipboard unavailable: {e}; {text}")),
        }
      } else {
        // `Y` (yank-curl) is the only path that strictly requires a
        // Ready model; the smart `y` fallback yields a string for
        // any focused row, so this branch only fires for `Y`.
        app.show_toast("nothing to yank — focus a Ready model");
      }
    }
    Action::CycleTheme => {
      app.cycle_theme();
      app.show_toast(format!("theme: {}", app.options.theme.canonical()));
    }
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
      app.chat.collapse_thinks = !app.chat.collapse_thinks;
    }
    Action::ToggleAutoScroll => {
      // `s` is double-duty in the right pane:
      //  - Logs tab → toggle auto-scroll (legacy).
      //  - Settings tab → stop the focused managed launch (round-8).
      //  - Any other right tab → no-op so we don't accidentally
      //    fire a stop or scroll toggle on Chat/Embed/Rerank.
      // Only toggle auto-scroll on the Logs surface; on Settings let
      // `apply_stop_model` handle the no-managed case (it toasts).
      if app.focus == Focus::RightPane && app.right_tab == RightTab::Settings {
        apply_stop_model(app);
      } else if app.right_tab == RightTab::Logs {
        app.logs_state.auto_scroll = !app.logs_state.auto_scroll;
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
    Action::EnterEdit => {
      // Tab-aware:
      //  - Chat / Embed / Rerank: shift focus into the input buffer
      //    so subsequent keystrokes go to the prompt. The field
      //    itself enters edit mode so typing works immediately and
      //    the `Esc` walk-back is wired up.
      //  - Settings on a running launch: stage the launch picker so
      //    the user can edit next-launch params over the live
      //    read-only view (the arrow-keys path no longer
      //    auto-stages — `e` is the explicit gate).
      //  - Anywhere else: no-op.
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
      } else if app.right_tab == RightTab::Settings && app.launch_picker.is_none() {
        if app.focused_path().is_some() {
          app.open_launch_picker();
        } else {
          app.show_toast("no model focused");
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
/// used to spell out (audit §F2.1 #1).
fn apply_arrow_in_pane(app: &mut App, dir: ArrowDir) {
  match app.focus {
    Focus::RightPane => match app.right_tab {
      // Logs: scroll the log buffer.
      RightTab::Logs => match dir {
        ArrowDir::Up => app.logs_state.scroll_up(),
        ArrowDir::Down => app.logs_state.scroll_down(),
      },
      // Settings: cycle the form's fields (ctx → reasoning →
      // advanced). The picker materialisation happens in
      // `apply_next_field` / `apply_prev_field`.
      RightTab::Settings => match dir {
        ArrowDir::Up => apply_prev_field(app),
        ArrowDir::Down => apply_next_field(app),
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
  // Audit §F5 #21: the chip strip advertises `←/→:cycle value`
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

/// Auto-materialise the inline Settings picker if absent, then run
/// `f` against it. Audit §F2.1 #3 — collapses the three
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
  let name = crate::util::paths::model_display_name(&path);
  app.confirm_dialog = Some(ConfirmAction::StopModel { launch_id, name });
}

/// Stage a cancel-download confirmation. Refuses (with a toast) when
/// no pull is currently active — pressing Ctrl+X on an empty strip
/// shouldn't bring up a popup with nothing to confirm. The popup
/// payload mirrors what the strip is showing so the user reads the
/// same identifier they pressed Ctrl+X over.
fn apply_cancel_download(app: &mut App) {
  let Some(active) = app.download_strip.active.as_ref() else {
    app.show_toast("no active download to cancel");
    return;
  };
  app.confirm_dialog = Some(ConfirmAction::CancelDownload {
    repo_id: active.repo_id.clone(),
    friendly_name: active.friendly_name.clone(),
  });
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
  let display_name = crate::util::paths::model_display_name(&path);
  app.confirm_dialog = Some(ConfirmAction::DeleteModel { path, display_name });
}

/// Returns the toast message describing why a delete must refuse on
/// the focused row, or `None` when the delete should be allowed.
/// Mirrors the chip-rendering rule in `render::focused_row_is_deletable`
/// so the hint and the keybinding stay in lock-step.
fn delete_refusal_reason(app: &App) -> Option<&'static str> {
  use crate::tui::status_icons::SurfaceState;
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

/// Perform the actual file removal. Walks the path's parent chain
/// up to the HF cache root so symlinked snapshot files take the
/// underlying blob with them — otherwise the cache fills up with
/// orphan blobs after every "delete". Non-HF paths just unlink the
/// single file. Returns a human-readable summary suitable for a
/// toast.
///
/// Thin wrapper over [`delete_model_with_cache_root`] that resolves
/// the live `hf_cache_dir()` — tests pass their own root to exercise
/// the cache-gate without touching env vars.
fn delete_model_on_disk(path: &std::path::Path) -> Result<String, std::io::Error> {
  let cache_root = crate::init::download::hf_cache_dir().ok();
  delete_model_with_cache_root(path, cache_root.as_deref())
}

/// Worker for [`delete_model_on_disk`] parameterised on the HF cache
/// root. Treats `path` as part of the HF cache only when *both* the
/// directory shape (`models--*/snapshots/<rev>/`) matches *and* the
/// resolved repo dir lives under `cache_root`. Anything else — a
/// `models--*` layout outside the cache root (manually rsynced
/// backup, restored archive, surprise Docker volume), a plain user
/// path, or a `cache_root = None` build — falls through to a
/// single-file unlink so a confirmed delete can't recursively rm-rf
/// an unrelated directory tree.
fn delete_model_with_cache_root(
  path: &std::path::Path,
  cache_root: Option<&std::path::Path>,
) -> Result<String, std::io::Error> {
  use std::fs;
  if let Some(repo_dir) = hf_repo_dir_for_snapshot_path(path, cache_root) {
    fs::remove_dir_all(&repo_dir)?;
    return Ok(format!(
      "deleted HF cache for {}",
      repo_dir.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    ));
  }
  // Plain GGUF on a user path (or HF-shaped path outside the cache
  // root) — just unlink the single file.
  fs::remove_file(path)?;
  Ok(format!(
    "deleted {}",
    path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
  ))
}

/// Return the `models--<owner>--<repo>` directory the given snapshot
/// path lives under, but only when the directory shape matches the
/// HF cache layout *and* the resolved repo dir is inside `cache_root`.
/// Returns `None` for anything else, including HF-shaped layouts
/// outside the cache root — those get a single-file unlink in
/// `delete_model_with_cache_root` rather than a recursive removal.
/// Carved out as a pure helper so tests can pin the gate without
/// constructing the rest of the dispatch chain.
fn hf_repo_dir_for_snapshot_path(
  path: &std::path::Path,
  cache_root: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
  let snapshot_dir = path.parent()?;
  let snapshots_root = snapshot_dir.parent()?;
  if snapshots_root.file_name().and_then(|n| n.to_str()) != Some("snapshots") {
    return None;
  }
  let repo_dir = snapshots_root.parent()?;
  let is_hf_repo = repo_dir
    .file_name()
    .and_then(|n| n.to_str())
    .is_some_and(|n| n.starts_with("models--"));
  if !is_hf_repo {
    return None;
  }
  // Refuse to treat a `models--*/snapshots/*/file` layout as an HF
  // cache when the resolved repo dir is *not* inside the configured
  // HF cache root. Falling through to single-file unlink is the
  // conservative behaviour for unfamiliar layouts.
  let cache_root = cache_root?;
  let cache_root_canonical = cache_root
    .canonicalize()
    .unwrap_or_else(|_| cache_root.to_path_buf());
  let candidate = repo_dir.canonicalize().unwrap_or_else(|_| repo_dir.into());
  if !candidate.starts_with(&cache_root_canonical) {
    return None;
  }
  Some(candidate)
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
        format!("stop dispatched ({name})"),
        "stop failed — writer offline",
        format!("stop dispatched (no writer; launch {launch_id})"),
      );
    }
    ConfirmAction::KillDaemon => {
      dispatch_writer(
        app,
        writer,
        WriterCmd::Shutdown,
        "daemon shutdown dispatched".into(),
        "daemon shutdown failed — writer offline",
        "daemon shutdown (no writer)".into(),
      );
    }
    ConfirmAction::RestartDaemon => {
      dispatch_writer(
        app,
        writer,
        WriterCmd::RestartDaemon,
        "daemon restart dispatched".into(),
        "daemon restart failed — writer offline",
        "daemon restart (no writer)".into(),
      );
    }
    ConfirmAction::DeleteModel { path, display_name } => match delete_model_on_disk(&path) {
      Ok(_summary) => {
        // Drop the row from the cached model list so the next render
        // doesn't flash a stale entry. A real refresh re-discovers on
        // the next tick and will catch any siblings (e.g. split-shard
        // members that lived in the same HF snapshot).
        app.models.retain(|m| m.path != path);
        if app.list_cursor >= app.models.len() {
          app.list_cursor = app.models.len().saturating_sub(1);
        }
        app.show_toast(format!("deleted {display_name}"));
      }
      Err(e) => app.show_toast(format!("delete failed: {e}")),
    },
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
          app.show_toast(format!("cancelled: {cancelled_friendly_name}"));
          if let Some(promoted) = next {
            app.download_strip.install_active(&promoted);
            let abort = spawn_download_task(
              promoted,
              app.options.offline,
              app.download_strip.event_tx.clone(),
            );
            app.download_strip.active_abort = Some(abort);
          }
        }
      }
    }
    ConfirmAction::LaunchDuplicate {
      model_path,
      ctx,
      reasoning,
      advanced,
      mode,
      prefer_port,
      ..
    } => {
      let cmd = WriterCmd::StartModel {
        model_path,
        ctx,
        reasoning,
        advanced,
        mode,
        prefer_port,
      };
      dispatch_launch(app, writer, cmd);
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
      Err(_) => app.show_toast(err_msg.to_string()),
    },
    None => app.show_toast(no_writer_msg),
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
/// "no Ready model focused for `<action>`" message. Audit §1.1 #6
/// — the same lookup + toast guard was duplicated across
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

/// Trigger an OpenAI streaming chat completion against the focused
/// Ready model. Stashes the receiver on `app.chat` so the render
/// loop can drain it without blocking input.
fn apply_send_chat(app: &mut App) {
  let Some(managed) = focused_managed_or_toast(app, "chat") else {
    return;
  };
  if app.chat.prompt.buffer().trim().is_empty() {
    app.show_toast("prompt is empty");
    return;
  }
  let prompt = app.chat.prompt.buffer().to_string();
  let model_name = crate::util::paths::model_display_name(&managed.path);
  app.chat.reset_for_send();
  let rx = spawn_chat_stream(managed.port, model_name, prompt);
  app.chat.stream_rx = Some(rx);
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
  let model_name = crate::util::paths::model_display_name(&managed.path);
  let (tx, rx) = mpsc::unbounded_channel::<TabEvent>();
  app.embed.busy = true;
  app.embed.pending = Some(rx);
  let port = managed.port;
  tokio::spawn(async move {
    let result = oai_embed(port, &model_name, &input).await;
    let _ = tx.send(match result {
      Ok(r) => TabEvent::EmbedOk(r),
      Err(e) => TabEvent::EmbedErr(e),
    });
  });
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
    app.show_toast("stage at least one candidate (↓ to candidate field, Enter to add)");
    return;
  }
  let query = app.rerank.query.buffer().to_string();
  let candidates = app.rerank.candidates.clone();
  let model_name = crate::util::paths::model_display_name(&managed.path);
  let (tx, rx) = mpsc::unbounded_channel::<TabEvent>();
  app.rerank.busy = true;
  app.rerank.pending = Some(rx);
  let port = managed.port;
  tokio::spawn(async move {
    let result = oai_rerank(port, &model_name, &query, &candidates).await;
    let _ = tx.send(match result {
      Ok(r) => TabEvent::RerankOk(r),
      Err(e) => TabEvent::RerankErr(e),
    });
  });
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
      app.show_toast("favorite toggle failed — writer offline");
      return;
    }
  }
  app.show_toast(if now_favorite {
    "favorite added"
  } else {
    "favorite removed"
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
  if app.launch_picker.is_none() {
    app.open_launch_picker();
  }
  let picker = match app.launch_picker.as_ref() {
    Some(p) => p.clone(),
    None => return,
  };
  let advanced: Vec<String> = app
    .advanced_panel
    .as_ref()
    .map(|panel| {
      panel
        .argv()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    })
    .unwrap_or_default();

  // Forward the catalog's mode hint so an embedding / rerank GGUF
  // launched from the picker reaches llama-server with the right
  // mode flag. Without this the daemon defaulted to Chat for every
  // launch regardless of the catalog's classification.
  use crate::launch::mode::LaunchMode;
  // Delegate to the canonical `LaunchMode::resolve` rather than
  // re-implementing the `ModeHint -> LaunchMode` table inline — keeps
  // the TUI in lockstep with whatever the CLI / IPC paths use.
  let mode = app
    .models
    .iter()
    .find(|m| m.path == path)
    .and_then(|m| m.metadata.as_ref())
    .and_then(|md| LaunchMode::resolve(None, md.mode_hint));

  let cmd = WriterCmd::StartModel {
    model_path: path.clone(),
    ctx: picker.ctx,
    // Round-8: ModelDefault → omit the field over the wire.
    reasoning: picker.reasoning.as_wire(),
    advanced: advanced.clone(),
    mode,
    prefer_port: picker.prefer_port,
  };

  // Round-8: a Submit on a model that already has managed
  // instances stages a confirm popup instead of dispatching
  // immediately. v1 supports duplicate launches on fresh ports,
  // but a fat-finger shouldn't silently triple-launch a 14B model.
  let active_instances = app.managed.iter().filter(|m| m.path == path).count();
  if active_instances > 0 {
    let name = crate::util::paths::model_display_name(&path);
    app.confirm_dialog = Some(ConfirmAction::LaunchDuplicate {
      name,
      active_instances,
      model_path: path,
      ctx: picker.ctx,
      reasoning: picker.reasoning.as_wire(),
      advanced,
      mode,
      prefer_port: picker.prefer_port,
    });
    return;
  }

  dispatch_launch(app, writer, cmd);
}

/// Send a fully-assembled `StartModel` payload via the writer
/// channel and close the picker on success. Shared by the
/// direct-launch path and the post-confirm dispatch so both flows
/// emit the same toasts.
fn dispatch_launch(app: &mut App, writer: Option<&mpsc::Sender<WriterCmd>>, cmd: WriterCmd) {
  match writer {
    Some(tx) => match tx.try_send(cmd) {
      Ok(()) => {
        app.show_toast("launch dispatched");
        app.close_launch_picker();
      }
      Err(_) => {
        app.show_toast("launch failed — writer offline");
      }
    },
    None => {
      // No daemon attached (headless test backend, dry run, etc.).
      // Keep the picker open so the user can retry once a writer is
      // wired up rather than silently swallowing the keypress.
      app.show_toast("launch dispatched (no writer)");
    }
  }
}

fn build_yank_text(app: &App, action: Action) -> Option<String> {
  match action {
    Action::YankPath => app.focused_path().map(|p| p.display().to_string()),
    Action::YankUrl => {
      // Prefer the running URL when the model is launched; fall back
      // to the model path so `y` always yanks *something useful* —
      // a not-yet-running row still has a path the user often wants
      // to paste into a script or doc.
      if let Some(m) = app.focused_managed() {
        Some(format!("http://127.0.0.1:{}/v1", m.port))
      } else {
        app.focused_path().map(|p| p.display().to_string())
      }
    }
    Action::YankCurl => {
      let m = app.focused_managed()?;
      let url = format!("http://127.0.0.1:{}/v1", m.port);
      let model_name = crate::util::paths::model_display_name(&m.path);
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
pub enum RefreshTick {
  Catalog(Value),
  Status(Value),
  Favorites(Value),
  LastParams(Value),
  /// `logs_tail` snapshot for `launch_id`. Triggered by the
  /// dedicated [`spawn_logs_poller`] task — keeps the per-tick
  /// poll cheap when the user moves between launches.
  Logs {
    launch_id: String,
    lines: Vec<String>,
  },
  Disconnected,
}

pub fn spawn_refresher(socket: PathBuf) -> mpsc::Receiver<RefreshTick> {
  let (tx, rx) = mpsc::channel(16);
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
            let _ = tx.send(RefreshTick::Catalog(body)).await;
          }
          if let Ok(body) = client.call("status", None).await {
            let _ = tx.send(RefreshTick::Status(body)).await;
          }
          if let Ok(body) = client.call("favorite_list", None).await {
            let _ = tx.send(RefreshTick::Favorites(body)).await;
          }
          if let Ok(body) = client.call("last_params_list", None).await {
            let _ = tx.send(RefreshTick::LastParams(body)).await;
          }
          tokio::time::sleep(REFRESH_INTERVAL).await;
        }
        Err(_) => {
          let _ = tx.send(RefreshTick::Disconnected).await;
          // Exponential backoff capped at REFRESH_INTERVAL: a cold
          // daemon comes up within ~2 s; a long outage doesn't spam
          // the connect attempt at 1.3 Hz.
          tokio::time::sleep(backoff).await;
          backoff = (backoff * 2).min(REFRESH_INTERVAL);
        }
      }
    }
  });
  rx
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
          log::warn!("writer connect failed: {e}");
          continue;
        }
      };
      let (method, params) = encode_writer_cmd(cmd);
      if let Err(e) = client.call(method, Some(params)).await {
        log::warn!("writer call {method} failed: {e}");
      }
    }
  });
  tx
}

/// Two-phase daemon restart: ask the running daemon to shut down,
/// wait for the socket file to disappear, then `start_detached` a
/// fresh daemon with the same options the parent dispatcher
/// resolved. Best-effort throughout — every failure logs and the
/// TUI keeps running so the user can retry from the keymap.
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
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
  while std::time::Instant::now() < deadline {
    if Client::connect(socket).await.is_err() {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
  match crate::daemon::start_detached(opts) {
    Ok(crate::daemon::StartOutcome::AlreadyRunning(_)) => {
      log::warn!("restart: daemon is still running; restart did not spawn a new process");
    }
    Ok(_) => log::info!("restart: daemon re-spawned"),
    Err(e) => log::warn!("restart: start_detached failed: {e}"),
  }
}

fn encode_writer_cmd(cmd: WriterCmd) -> (&'static str, Value) {
  match cmd {
    WriterCmd::StartModel {
      model_path,
      ctx,
      reasoning,
      advanced,
      mode,
      prefer_port,
    } => {
      let mode_str = mode.map(|m| match m {
        crate::launch::mode::LaunchMode::Chat => "chat",
        crate::launch::mode::LaunchMode::Embedding => "embedding",
        crate::launch::mode::LaunchMode::Rerank => "rerank",
      });
      // `reasoning: None` (round-8 ModelDefault) serialises as
      // JSON null, which the daemon's `Option<bool>` parser treats
      // the same as a missing key — falling back to the model's
      // own reasoning hint.
      (
        "start_model",
        json!({
          "model_path": model_path,
          "ctx": ctx,
          "reasoning": reasoning,
          "advanced": advanced,
          "mode": mode_str,
          "prefer_port": prefer_port,
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
  }
}

/// Fully-featured TUI run-loop. Drives the App from real crossterm
/// events + a daemon refresher, rendering on each tick.
pub async fn run(
  app: App,
  socket: PathBuf,
  daemon_opts: Option<crate::daemon::DaemonOptions>,
) -> Result<()> {
  use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
  };
  use crossterm::execute;
  use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
  };
  use ratatui::backend::CrosstermBackend;
  use ratatui::Terminal;

  enable_raw_mode()?;
  let mut stdout = std::io::stdout();
  // Mouse capture is deliberately NOT enabled: when the application
  // captures mouse events, the terminal can't run its own
  // click-and-drag text selection. j/k/PgUp/PgDn/g/G cover all the
  // navigation a user would otherwise reach for the wheel; keeping
  // mouse capture off lets users copy text out of the dashboard the
  // way they would from any other terminal program.
  execute!(stdout, EnterAlternateScreen)?;
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
  let mut refresh_rx = spawn_refresher(socket.clone());
  let current_launch = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
  let mut logs_rx = spawn_logs_poller(socket.clone(), current_launch.clone());
  let writer_tx = spawn_writer(socket, daemon_opts);

  loop {
    // Mirror the focused launch id to the logs poller so the next
    // tick fetches the right buffer. `unwrap_or_else(into_inner)`
    // recovers from a poisoned mutex instead of crashing the TUI at
    // 125 Hz — the data inside is a plain `Option<String>` we can
    // safely replace regardless of who panicked previously.
    *current_launch
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner) =
      app.focused_managed().map(|m| m.launch_id.clone());

    terminal.draw(|f| crate::tui::render::render(f, &mut app))?;

    // Drain any background ticks without blocking — keeps render
    // latency tight (~16 ms target) regardless of daemon RTT.
    while let Ok(tick) = refresh_rx.try_recv() {
      apply_refresh(&mut app, tick);
    }
    while let Ok(tick) = logs_rx.try_recv() {
      apply_refresh(&mut app, tick);
    }
    drain_chat_stream(&mut app);
    drain_embed_pending(&mut app);
    drain_rerank_pending(&mut app);
    drain_hf_dialog(&mut app);
    drain_download_strip(&mut app);

    if event::poll(POLL_INTERVAL)? {
      let evt = event::read()?;
      if pump_input_with_writer(&mut app, evt, Some(&writer_tx)) {
        break;
      }
    }
  }

  // Restore the terminal even on early returns above. Pop the kitty
  // protocol flags first so the next program inheriting the tty
  // doesn't accidentally inherit the disambiguation state.
  if pushed_kitty {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
  }
  disable_raw_mode()?;
  execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
  terminal.show_cursor()?;
  Ok(())
}

fn apply_refresh(app: &mut App, tick: RefreshTick) {
  match tick {
    RefreshTick::Catalog(body) => {
      app.daemon_connected = true;
      app.ingest_list_models(&body);
    }
    RefreshTick::Status(body) => {
      app.daemon_connected = true;
      app.ingest_status(&body);
    }
    RefreshTick::Favorites(body) => {
      app.daemon_connected = true;
      app.ingest_favorites(&body);
    }
    RefreshTick::LastParams(body) => {
      app.daemon_connected = true;
      app.ingest_last_params(&body);
    }
    RefreshTick::Logs { launch_id, lines } => {
      app.daemon_connected = true;
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
) -> mpsc::Receiver<RefreshTick> {
  let (tx, rx) = mpsc::channel(8);
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
              let _ = tx.send(RefreshTick::Logs { launch_id, lines }).await;
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
  rx
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
pub async fn launch(
  theme: crate::theme::ThemeName,
  custom_palette: Option<crate::theme::Palette>,
  keymap: crate::tui::keybindings::KeyMap,
  offline: bool,
  socket: &Path,
  daemon_opts: Option<crate::daemon::DaemonOptions>,
) -> Result<()> {
  let app = App::new(crate::tui::app::AppOptions {
    theme,
    custom_palette,
    keymap,
    offline,
  });
  run(app, socket.to_path_buf(), daemon_opts).await
}

/// Drain any pending [`ChatStreamMsg`] frames into `app.chat`.
/// Returns true when the stream finished or errored this call so
/// the caller can release the receiver slot.
pub fn drain_chat_stream(app: &mut App) -> bool {
  // First collect frames without holding a long borrow over the
  // chat state, then apply them. Receiver lives on `app.chat`, so
  // splitting the borrow is the easiest way to keep the borrow
  // checker happy.
  let mut frames: Vec<ChatStreamMsg> = Vec::new();
  let mut take_rx = false;
  if let Some(rx) = app.chat.stream_rx.as_mut() {
    loop {
      match rx.try_recv() {
        Ok(msg) => {
          let terminal = matches!(
            msg,
            ChatStreamMsg::Finished { .. } | ChatStreamMsg::Error(_)
          );
          frames.push(msg);
          if terminal {
            take_rx = true;
            break;
          }
        }
        Err(mpsc::error::TryRecvError::Empty) => break,
        Err(mpsc::error::TryRecvError::Disconnected) => {
          take_rx = true;
          if app.chat.streaming {
            // Sender side dropped without a terminal frame — treat
            // as a clean finish so the UI doesn't appear stuck.
            frames.push(ChatStreamMsg::Finished {
              finish_reason: None,
            });
          }
          break;
        }
      }
    }
  }
  let mut finished = false;
  for msg in frames {
    match msg {
      ChatStreamMsg::Delta(s) => app.chat.append_delta(&s),
      ChatStreamMsg::Finished { finish_reason } => {
        app.chat.mark_finished(finish_reason);
        finished = true;
      }
      ChatStreamMsg::Error(e) => {
        app.chat.mark_error(e);
        finished = true;
      }
    }
  }
  if take_rx {
    app.chat.stream_rx = None;
  }
  finished
}

/// Outcome of one tab-event drain step.
#[derive(Debug, Clone)]
enum DrainOutcome<E> {
  /// No event ready; receiver stays.
  Empty,
  /// Event matched; receiver should be dropped.
  Event(E),
  /// Receiver disconnected; receiver should be dropped, busy flag reset.
  Disconnected,
}

/// Drain one tab's pending receiver. Returns the outcome so the
/// caller can update both the receiver slot and any per-tab state
/// (busy flag, result, error) without two-closure-with-shared-app
/// borrow checker conflicts.
fn drain_tab_pending(
  pending: &mut Option<mpsc::UnboundedReceiver<TabEvent>>,
) -> DrainOutcome<TabEvent> {
  let Some(rx) = pending.as_mut() else {
    return DrainOutcome::Empty;
  };
  match rx.try_recv() {
    Ok(evt) => {
      *pending = None;
      DrainOutcome::Event(evt)
    }
    Err(mpsc::error::TryRecvError::Empty) => DrainOutcome::Empty,
    Err(mpsc::error::TryRecvError::Disconnected) => {
      *pending = None;
      DrainOutcome::Disconnected
    }
  }
}

/// Drain the embed pending receiver. Records success or surfaces
/// the error message on the embed tab state.
pub fn drain_embed_pending(app: &mut App) {
  match drain_tab_pending(&mut app.embed.pending) {
    DrainOutcome::Event(TabEvent::EmbedOk(result)) => app.embed.record(result),
    DrainOutcome::Event(TabEvent::EmbedErr(msg)) => app.embed.record_error(msg),
    DrainOutcome::Event(_) => {}
    DrainOutcome::Disconnected => app.embed.busy = false,
    DrainOutcome::Empty => {}
  }
}

/// Drain the rerank pending receiver. Mirrors
/// [`drain_embed_pending`].
pub fn drain_rerank_pending(app: &mut App) {
  match drain_tab_pending(&mut app.rerank.pending) {
    DrainOutcome::Event(TabEvent::RerankOk(ranked)) => app.rerank.record(ranked),
    DrainOutcome::Event(TabEvent::RerankErr(msg)) => app.rerank.record_error(msg),
    DrainOutcome::Event(_) => {}
    DrainOutcome::Disconnected => app.rerank.busy = false,
    DrainOutcome::Empty => {}
  }
}

/// Open the HuggingFace pull dialog (`d` in `Focus::List`).
/// Offline state is resolved inside `App::open_hf_dialog` from
/// `app.options.offline` ∨ `LLAMASTASH_OFFLINE`, so the call site
/// stays a single line and the runtime offline value travels through
/// `AppOptions`.
fn apply_open_hf_dialog(app: &mut App) {
  app.open_hf_dialog();
}

/// Outcome of dispatching a key into the HF dialog. The router
/// resolves the state-mutating action under a single `&mut
/// state` borrow, then surfaces any side effect (toast, close) for
/// the caller to apply against `&mut App` once the borrow ends.
enum HfDialogOutcome {
  None,
  Toast(&'static str),
  EnqueuePull {
    repo: String,
    row: crate::tui::hf_dialog::PickerRow,
  },
  Close,
}

/// Per-stage key router for `Focus::HfDialog`.
///
/// Search stage routes keys through the modal `InputField` first:
/// while editing, printable chars / Backspace mutate the query and
/// Esc steps the field out of edit; while resting `e` re-enters
/// edit, Esc clears a non-empty buffer (or closes the dialog when
/// already empty), and the dialog's own keymap (`o`, `n`, `p`, …)
/// fires through. Enter always means "submit the current query /
/// row" regardless of edit mode. FilePicker and Confirm stages use
/// Esc to walk back one stage; arrow keys move the cursor.
fn handle_hf_dialog_input(app: &mut App, key: KeyEvent, _writer: Option<&mpsc::Sender<WriterCmd>>) {
  use crate::tui::hf_dialog::HfStage;
  use crate::tui::input_field::InputOutcome;
  let outcome = {
    let Some(state) = app.hf_dialog.as_mut() else {
      return;
    };
    match state.stage {
      HfStage::Search => {
        match state.handle_search_key(key) {
          InputOutcome::Handled => HfDialogOutcome::None,
          InputOutcome::Submit => match state.submit_search() {
            Some(repo_id) => {
              spawn_hf_list_repo_files(state, repo_id);
              HfDialogOutcome::None
            }
            None => HfDialogOutcome::Toast("type a query, paste a slug, or pick a row"),
          },
          InputOutcome::PassThrough => match key.code {
            // The input only PassThroughs Esc when the buffer is
            // empty and the field is resting, so this arm always
            // means "close the dialog."
            KeyCode::Esc => HfDialogOutcome::Close,
            KeyCode::Up => {
              state.move_up();
              HfDialogOutcome::None
            }
            KeyCode::Down => {
              state.move_down();
              HfDialogOutcome::None
            }
            KeyCode::Enter => match state.submit_search() {
              Some(repo_id) => {
                spawn_hf_list_repo_files(state, repo_id);
                HfDialogOutcome::None
              }
              None => HfDialogOutcome::Toast("type a query, paste a slug, or pick a row"),
            },
            KeyCode::Char('o') => {
              state.cycle_sort();
              HfDialogOutcome::None
            }
            KeyCode::Char('n') => {
              if let Some(cursor) = state.advance_page() {
                spawn_hf_search(state, cursor);
              }
              HfDialogOutcome::None
            }
            KeyCode::Char('p') => {
              if let Some(cursor) = state.retreat_page() {
                spawn_hf_search(state, cursor);
              }
              HfDialogOutcome::None
            }
            _ => HfDialogOutcome::None,
          },
        }
      }
      HfStage::FilePicker => match key.code {
        // Esc walks back to Search (per R3 Esc-navigation contract);
        // a further Esc on Search closes the dialog.
        KeyCode::Esc => {
          state.back_to_search();
          HfDialogOutcome::None
        }
        KeyCode::Up => {
          state.move_up();
          HfDialogOutcome::None
        }
        KeyCode::Down => {
          state.move_down();
          HfDialogOutcome::None
        }
        KeyCode::Enter => {
          if state.submit_picker() {
            HfDialogOutcome::None
          } else {
            HfDialogOutcome::Toast("no file selected")
          }
        }
        _ => HfDialogOutcome::None,
      },
      HfStage::Confirm => match key.code {
        KeyCode::Esc => {
          state.back_to_picker();
          HfDialogOutcome::None
        }
        KeyCode::Enter => {
          if let Some((repo, row)) = state.take_confirm_target() {
            HfDialogOutcome::EnqueuePull { repo, row }
          } else {
            HfDialogOutcome::Close
          }
        }
        _ => HfDialogOutcome::None,
      },
    }
  };
  match outcome {
    HfDialogOutcome::None => {}
    HfDialogOutcome::Toast(msg) => app.show_toast(msg),
    HfDialogOutcome::EnqueuePull { repo, row } => {
      enqueue_hf_pull(app, repo, row);
      app.close_hf_dialog();
    }
    HfDialogOutcome::Close => app.close_hf_dialog(),
  }
}

/// Push a pull onto the download-strip queue and — when no pull is
/// currently active — promote it and spawn the background download
/// task that ships progress back over the strip's mpsc.
fn enqueue_hf_pull(app: &mut App, repo: String, row: crate::tui::hf_dialog::PickerRow) {
  use crate::tui::download_strip::{DownloadEvent, QueuedPull};
  let filename = row.download_filename().to_string();
  // R116 cache-hit short-circuit: probe the HF cache before queuing
  // anything. When every requested file already lives under a single
  // snapshot dir, emit AlreadyCached directly via the strip's mpsc
  // and skip both the queue and the download spawn. Deterministic —
  // replaces the earlier "<200 ms elapsed" heuristic that conflated
  // a fast network with a real cache hit.
  if let Some(cached_path) = probe_cached_pull(&repo, &row.all_filenames()) {
    let _ = app
      .download_strip
      .event_tx
      .send(DownloadEvent::AlreadyCached {
        repo_id: repo.clone(),
        cached_path,
      });
    return;
  }
  let pull = QueuedPull {
    repo_id: repo.clone(),
    friendly_name: format!("{repo} :{filename}"),
    row,
  };
  let queue_pos = app.download_strip.enqueue(pull);
  app.show_toast(format!("pull queued: {repo} :{filename} (#{queue_pos})"));
  // Promote immediately when nothing's active so the user sees the
  // strip light up on the next render.
  if app.download_strip.active.is_none() {
    if let Some(promoted) = app.download_strip.promote_next() {
      app.download_strip.install_active(&promoted);
      let abort = spawn_download_task(
        promoted,
        app.options.offline,
        app.download_strip.event_tx.clone(),
      );
      app.download_strip.active_abort = Some(abort);
    }
  }
}

/// Probe the HF cache for every filename the pull would produce on
/// disk and, when all are present under the same snapshot directory,
/// return the path to the user-facing first file (the row's
/// `download_filename`). Used by `spawn_download_task` to short-
/// circuit a redundant pull deterministically, replacing the
/// previous elapsed-time heuristic (R116). Returns `None` when the
/// repo isn't cached, any shard is missing, or the cache root can't
/// be resolved on this platform.
fn probe_cached_pull(repo_id: &str, filenames: &[String]) -> Option<std::path::PathBuf> {
  if filenames.is_empty() {
    return None;
  }
  let cache_root = crate::init::download::hf_cache_dir().ok()?;
  let repo_dir = cache_root.join(crate::init::download::repo_folder_name(repo_id));
  let snapshots = repo_dir.join("snapshots");
  let entries = std::fs::read_dir(&snapshots).ok()?;
  for entry in entries.filter_map(|e| e.ok()) {
    let snapshot = entry.path();
    if !snapshot.is_dir() {
      continue;
    }
    // The HF cache exposes files as symlinks under `snapshots/<rev>/`.
    // A snapshot only counts as a hit when every requested filename
    // resolves there — partial caches (e.g. only shard 1) must fall
    // through to the real download path.
    if filenames.iter().all(|f| snapshot.join(f).exists()) {
      return Some(snapshot.join(&filenames[0]));
    }
  }
  None
}

/// Spawn a tokio task that calls `init::download::download_repo`
/// with a `DownloadProgress` shim relaying every callback to the
/// strip's mpsc. Caller has already run `probe_cached_pull` against
/// the requested files, so this path only fires for real downloads —
/// the cache-hit short-circuit (R116) lives next to the queue
/// enqueue, not inside the spawn.
///
/// Returns the spawned task's [`tokio::task::AbortHandle`] so the
/// `Ctrl+X:cancel download` flow can interrupt an in-flight pull
/// mid-chunk. Aborting drops hf-hub's stream future, leaves any
/// partial blob in the cache, and (because the task never sends
/// `Finished` / `Error` after the abort point) lets the strip's
/// own state transition drive the next promotion.
///
/// `offline` is the runtime-resolved offline flag (CLI `--offline` ∨
/// `LLAMASTASH_OFFLINE`). Passing `true` ensures the spawned task's
/// FetchClient short-circuits before it issues any HF traffic — the
/// pull surfaces as a clean offline error in the strip rather than
/// silently bypassing the user's chosen network policy.
fn spawn_download_task(
  pull: crate::tui::download_strip::QueuedPull,
  offline: bool,
  tx: tokio::sync::mpsc::UnboundedSender<crate::tui::download_strip::DownloadEvent>,
) -> tokio::task::AbortHandle {
  use crate::init::download::{DownloadOptions, RepoSpec};
  use crate::init::fetch;
  let handle = tokio::spawn(async move {
    let spec = match RepoSpec::parse(&format!(
      "{}:{}",
      pull.repo_id,
      pull.row.download_filename()
    )) {
      Ok(s) => s,
      Err(e) => {
        let _ = tx.send(crate::tui::download_strip::DownloadEvent::Error {
          repo_id: pull.repo_id.clone(),
          message: e.to_string(),
        });
        return;
      }
    };
    let fetch_client =
      fetch::build_with_offline_check(offline, fetch::FetchClientConfig::default())
        .unwrap_or_else(|_| fetch::FetchClient::offline());
    let progress = std::sync::Arc::new(StripProgress {
      tx: tx.clone(),
      repo_id: pull.repo_id.clone(),
      inner: std::sync::Mutex::new(StripProgressInner::default()),
    });
    let options = DownloadOptions {
      extension_filter: Some(".gguf".into()),
      estimated_bytes: pull.row.size_bytes(),
      progress: Some(
        progress.clone() as std::sync::Arc<dyn crate::init::download::DownloadProgress>
      ),
      revision: None,
    };
    match crate::init::download::download_repo(&spec, &fetch_client, &options).await {
      Ok(_) => {
        let _ = tx.send(crate::tui::download_strip::DownloadEvent::Finished {
          repo_id: pull.repo_id.clone(),
        });
      }
      Err(e) => {
        let _ = tx.send(crate::tui::download_strip::DownloadEvent::Error {
          repo_id: pull.repo_id.clone(),
          message: e.to_string(),
        });
      }
    }
  });
  handle.abort_handle()
}

/// DownloadProgress shim that forwards hf-hub callbacks into the
/// download strip's mpsc. Tracks per-file sizes resolved at the
/// listing pass so per-file finish callbacks aggregate cleanly
/// across multi-shard pulls. Byte-level progress flows via
/// `on_bytes_progress` — driven by `HfHubProgressAdapter` bridging
/// hf-hub's `Progress::update(size)` chunk callbacks into our
/// cumulative `(filename, bytes_in_file)` shape. The `bytes_total`
/// clamp inside [`StripProgressInner`] protects against the
/// (theoretical) race where a late `update` chunk lands after the
/// per-file `Finished` callback — `bytes_done.saturating_add` is
/// clamped to `bytes_total` so the strip can't overshoot 100%.
struct StripProgress {
  tx: tokio::sync::mpsc::UnboundedSender<crate::tui::download_strip::DownloadEvent>,
  repo_id: String,
  inner: std::sync::Mutex<StripProgressInner>,
}

#[derive(Default)]
struct StripProgressInner {
  /// `filename → size_bytes` snapshot captured at
  /// `on_files_resolved` time.
  file_sizes: std::collections::HashMap<String, u64>,
  bytes_total: u64,
  bytes_done: u64,
  /// Bytes credited so far for the file currently downloading.
  /// Replaces the running per-file counter every `on_bytes_progress`;
  /// reset to zero at `on_file_finished` so the next file starts
  /// fresh.
  bytes_in_current_file: u64,
}

impl crate::init::download::DownloadProgress for StripProgress {
  fn on_files_resolved(&self, files: &[(String, u64)]) {
    let mut inner = self.inner.lock().unwrap();
    inner.file_sizes = files.iter().cloned().collect();
    inner.bytes_total = files.iter().map(|(_, n)| *n).sum();
    inner.bytes_done = 0;
    let _ = self
      .tx
      .send(crate::tui::download_strip::DownloadEvent::Started {
        repo_id: self.repo_id.clone(),
        bytes_total: inner.bytes_total,
      });
  }

  fn on_file_started(&self, _filename: &str, _size: u64, _index: usize, _total: usize) {
    // Per-file byte counter resets on every file boundary; the
    // hf-hub adapter then drives `on_bytes_progress` as chunks land.
    let mut inner = self.inner.lock().unwrap();
    inner.bytes_in_current_file = 0;
  }

  fn on_file_finished(&self, filename: &str, _index: usize, _total: usize) {
    let mut inner = self.inner.lock().unwrap();
    let size = inner.file_sizes.get(filename).copied().unwrap_or(0);
    let prior_in_file = inner.bytes_in_current_file;
    // Aggregate the file's full size into the pull total (subtract any
    // partial credit `on_bytes_progress` already attributed so we don't
    // double-count).
    let credit = size.saturating_sub(prior_in_file);
    inner.bytes_done = inner
      .bytes_total
      .min(inner.bytes_done.saturating_add(credit));
    inner.bytes_in_current_file = 0;
    let bytes_done = inner.bytes_done;
    let bytes_total = inner.bytes_total;
    drop(inner);
    let _ = self
      .tx
      .send(crate::tui::download_strip::DownloadEvent::Progress {
        repo_id: self.repo_id.clone(),
        bytes_done,
        bytes_total,
      });
  }

  fn on_bytes_progress(&self, _filename: &str, bytes_in_file: u64) {
    let mut inner = self.inner.lock().unwrap();
    // Replace the running per-file count with the new cumulative
    // value. Subtract the previous in-file credit so the pull's
    // aggregate `bytes_done` only ever grows monotonically.
    let prior = inner.bytes_in_current_file;
    let delta = bytes_in_file.saturating_sub(prior);
    inner.bytes_in_current_file = bytes_in_file;
    inner.bytes_done = inner
      .bytes_total
      .min(inner.bytes_done.saturating_add(delta));
    let bytes_done = inner.bytes_done;
    let bytes_total = inner.bytes_total;
    drop(inner);
    let _ = self
      .tx
      .send(crate::tui::download_strip::DownloadEvent::Progress {
        repo_id: self.repo_id.clone(),
        bytes_done,
        bytes_total,
      });
  }
}

/// Spawn a background `init::hf_api::search` task whose result is
/// shipped back over the dialog's mpsc. Tagged with the dialog's
/// current `query_seq` so the drain can discard stale responses.
fn spawn_hf_search(state: &mut crate::tui::hf_dialog::HfDialogState, cursor: Option<String>) {
  use crate::init::hf_api;
  let tx = state.event_tx.clone();
  let query = state.input.buffer().to_string();
  let sort = state.sort;
  let seq = state.query_seq;
  let offline = state.offline;
  state.mark_dispatched();
  tokio::spawn(async move {
    let fetch_client = build_tui_fetch_client(offline);
    let evt = match hf_api::search(&fetch_client, &query, sort, cursor.as_deref()).await {
      Ok(page) => crate::tui::hf_dialog::HfDialogEvent::SearchResults { seq, page },
      Err(e) => crate::tui::hf_dialog::HfDialogEvent::SearchFailed { seq, error: e },
    };
    let _ = tx.send(evt);
  });
}

/// Spawn a background `list_repo_files` task whose result is
/// shipped back over the dialog's mpsc.
fn spawn_hf_list_repo_files(state: &mut crate::tui::hf_dialog::HfDialogState, repo_id: String) {
  use crate::init::hf_api;
  let tx = state.event_tx.clone();
  let offline = state.offline;
  tokio::spawn(async move {
    let fetch_client = build_tui_fetch_client(offline);
    let evt = match hf_api::list_repo_files(&fetch_client, &repo_id).await {
      Ok(files) => crate::tui::hf_dialog::HfDialogEvent::RepoFiles {
        repo_id: repo_id.clone(),
        files,
      },
      Err(error) => crate::tui::hf_dialog::HfDialogEvent::RepoFilesFailed {
        repo_id: repo_id.clone(),
        error,
      },
    };
    let _ = tx.send(evt);
  });
}

/// Build the dialog's FetchClient. Mirrors the wizard's resolution:
/// honour `LLAMASTASH_OFFLINE`, fall back to a fresh client with the
/// default config (host allowlist + redirect cap + body cap). The
/// `offline` arg threads the runtime's resolved offline state
/// (CLI `--offline` ∨ `LLAMASTASH_OFFLINE`) so the dialog can't make
/// network calls behind the user's back. On builder error we hand
/// back an offline stub so the dialog's network calls fail with a
/// clean typed error instead of panicking.
fn build_tui_fetch_client(offline: bool) -> crate::init::fetch::FetchClient {
  use crate::init::fetch::{build_with_offline_check, FetchClient, FetchClientConfig};
  build_with_offline_check(offline, FetchClientConfig::default())
    .unwrap_or_else(|_| FetchClient::offline())
}

/// Drain pending DownloadEvents into the strip. Promotes the next
/// queued pull when the active one finishes / errors / hits the
/// cache; surfaces a toast when a cache-hit short-circuit lands.
pub fn drain_download_strip(app: &mut App) {
  use crate::tui::download_strip::DownloadEvent;
  loop {
    let evt = app.download_strip.event_rx.try_recv();
    let evt = match evt {
      Ok(e) => e,
      Err(_) => break,
    };
    let next_pull = match evt {
      DownloadEvent::Started {
        repo_id,
        bytes_total,
      } => {
        app.download_strip.apply_started(&repo_id, bytes_total);
        None
      }
      DownloadEvent::Progress {
        repo_id,
        bytes_done,
        bytes_total,
      } => {
        app
          .download_strip
          .apply_progress(&repo_id, bytes_done, bytes_total);
        None
      }
      DownloadEvent::Finished { repo_id } => {
        let label = app
          .download_strip
          .active
          .as_ref()
          .map(|a| a.friendly_name.clone());
        let next = app.download_strip.apply_finished(&repo_id);
        if let Some(name) = label {
          app.show_toast(format!("pulled: {name}"));
        }
        next
      }
      DownloadEvent::Error { repo_id, message } => {
        app.download_strip.apply_error(&repo_id, message)
      }
      DownloadEvent::AlreadyCached {
        repo_id,
        cached_path,
      } => {
        let next = app
          .download_strip
          .apply_already_cached(&repo_id, cached_path);
        if let Some(path) = app.download_strip.pending_cache_hit.take() {
          app.show_toast(format!(
            "already downloaded — {}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
          ));
          // Select the matching catalog row (path equality) so the
          // user lands on it. Best-effort — `models` may not yet
          // reflect the just-cached file until the next refresh.
          if let Some(idx) = app.models.iter().position(|m| m.path == path) {
            // Find the row index in rendered_rows that matches this
            // model path so the cursor visibly snaps.
            let target = app.models[idx].path.clone();
            let rows = app.rendered_rows();
            if let Some(row_idx) = rows
              .iter()
              .position(|r| r.path().map(|p| p == target).unwrap_or(false))
            {
              app.list_cursor = row_idx;
            }
          }
        }
        next
      }
    };
    if let Some(pull) = next_pull {
      app.download_strip.install_active(&pull);
      let abort = spawn_download_task(
        pull,
        app.options.offline,
        app.download_strip.event_tx.clone(),
      );
      app.download_strip.active_abort = Some(abort);
    }
  }
}

/// Drain pending HF dialog events. Applies search / repo-listing
/// results to the dialog state. Idempotent — safe to call every run-
/// loop tick.
pub fn drain_hf_dialog(app: &mut App) {
  let Some(state) = app.hf_dialog.as_mut() else {
    return;
  };
  use crate::tui::hf_dialog::HfDialogEvent;
  loop {
    match state.event_rx.try_recv() {
      Ok(HfDialogEvent::SearchResults { seq, page }) => {
        state.apply_search_results(seq, page);
      }
      Ok(HfDialogEvent::SearchFailed { seq, error }) => {
        state.apply_search_failed(seq, error);
      }
      Ok(HfDialogEvent::RepoFiles { repo_id, files }) => {
        state.apply_repo_files(&repo_id, files);
      }
      Ok(HfDialogEvent::RepoFilesFailed { repo_id, error }) => {
        state.apply_repo_files_failed(&repo_id, &error);
      }
      Err(_) => break,
    }
  }
  // Debounced live search (R107): fire a dispatch once the debounce
  // window elapses since the last keystroke. Honours the `query_seq`
  // monotonicity so a stale response can still be dropped.
  if state.search_due(std::time::Instant::now()) {
    let cursor = state.current_cursor.clone();
    spawn_hf_search(state, cursor);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crossterm::event::{KeyEvent, KeyModifiers};

  fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, mods))
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
  fn delete_model_unlinks_user_path_file() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("llamastash-delete-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("victim.gguf");
    {
      let mut f = std::fs::File::create(&path).unwrap();
      writeln!(f, "fake gguf").unwrap();
    }
    assert!(path.exists());
    let summary = delete_model_on_disk(&path).expect("delete must succeed");
    assert!(summary.contains("victim.gguf"), "got `{summary}`");
    assert!(!path.exists(), "user-path file must be unlinked");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn delete_model_removes_full_hf_repo_dir_when_under_cache_root() {
    // Mimic the HF cache layout the deleter is supposed to recognise:
    //   <cache_root>/models--owner--repo/
    //     blobs/<sha>
    //     snapshots/main/file.gguf -> ../../blobs/<sha>
    // The cache-root gate is explicit here so we don't rely on the
    // ambient `HF_HOME` env var (which would race other tests).
    let cache_root =
      std::env::temp_dir().join(format!("llamastash-delete-hf-cache-{}", std::process::id()));
    let repo_dir = cache_root.join("models--owner--repo");
    let blobs = repo_dir.join("blobs");
    let snap = repo_dir.join("snapshots").join("main");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(blobs.join("sha"), b"blob").unwrap();
    let symlink_target = snap.join("file.gguf");
    #[cfg(unix)]
    std::os::unix::fs::symlink(blobs.join("sha"), &symlink_target).unwrap();
    #[cfg(not(unix))]
    std::fs::write(&symlink_target, b"blob").unwrap();
    assert!(symlink_target.exists());
    let summary = delete_model_with_cache_root(&symlink_target, Some(&cache_root))
      .expect("delete must succeed");
    assert!(summary.contains("HF cache"), "got `{summary}`");
    assert!(!repo_dir.exists(), "the whole repo dir should be gone");
    let _ = std::fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn delete_model_only_unlinks_when_hf_layout_lives_outside_cache_root() {
    // Same `models--owner--repo/snapshots/main/file.gguf` shape but
    // *not* under the configured HF cache root (think rsynced backup
    // or restored archive). The deleter must refuse to recursively
    // remove that tree and instead only unlink the single file.
    let outside =
      std::env::temp_dir().join(format!("llamastash-delete-outside-{}", std::process::id()));
    let repo_dir = outside.join("models--owner--repo");
    let snap = repo_dir.join("snapshots").join("main");
    std::fs::create_dir_all(&snap).unwrap();
    let file = snap.join("file.gguf");
    std::fs::write(&file, b"weights").unwrap();
    let other = snap.join("other.gguf");
    std::fs::write(&other, b"other").unwrap();
    // Point the cache root somewhere unrelated; the deleter must
    // fall through to single-file unlink.
    let unrelated_cache = std::env::temp_dir().join("llamastash-unrelated-cache");
    let _ = std::fs::create_dir_all(&unrelated_cache);
    let summary =
      delete_model_with_cache_root(&file, Some(&unrelated_cache)).expect("delete must succeed");
    assert!(
      !summary.contains("HF cache"),
      "non-cache HF-shaped layout must not be treated as HF cache: `{summary}`"
    );
    assert!(!file.exists(), "the target file should be unlinked");
    assert!(
      other.exists(),
      "sibling file in the same snapshot dir must NOT have been removed"
    );
    assert!(
      repo_dir.exists(),
      "the repo dir itself must NOT have been removed"
    );
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_dir_all(&unrelated_cache);
  }

  #[test]
  fn delete_model_with_no_cache_root_only_unlinks() {
    // Defense in depth: when `hf_cache_dir()` returns an error (HOME
    // unresolvable, exotic build target), the deleter must still
    // safely unlink a single file rather than recursing.
    let dir =
      std::env::temp_dir().join(format!("llamastash-delete-no-cache-{}", std::process::id()));
    let repo_dir = dir.join("models--owner--repo");
    let snap = repo_dir.join("snapshots").join("main");
    std::fs::create_dir_all(&snap).unwrap();
    let file = snap.join("file.gguf");
    std::fs::write(&file, b"weights").unwrap();
    let summary =
      delete_model_with_cache_root(&file, None).expect("no-cache-root delete must succeed");
    assert!(!summary.contains("HF cache"), "got `{summary}`");
    assert!(!file.exists());
    assert!(
      repo_dir.exists(),
      "with no cache root we must NOT remove the repo dir"
    );
    let _ = std::fs::remove_dir_all(&dir);
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
      rss_bytes: None,
      cpu_pct: None,
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
      rss_bytes: None,
      cpu_pct: None,
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
  fn d_opens_hf_dialog_and_esc_closes_it() {
    use crate::tui::hf_dialog::HfStage;
    let mut app = App::new(Default::default());
    assert!(app.hf_dialog.is_none());
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::NONE));
    let dialog = app.hf_dialog.as_ref().expect("`d` must open the HF dialog");
    assert_eq!(dialog.stage, HfStage::Search);
    assert!(
      dialog.input.is_editing(),
      "search field must auto-enter edit mode so the user can type immediately"
    );
    assert_eq!(app.focus, Focus::HfDialog);
    // Type into the search buffer.
    pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('w'), KeyModifiers::NONE));
    assert_eq!(app.hf_dialog.as_ref().map(|d| d.input.buffer()), Some("qw"));
    // First Esc: exit edit (buffer kept, dialog still open).
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    let after_first_esc = app.hf_dialog.as_ref().expect("first Esc must keep dialog");
    assert!(!after_first_esc.input.is_editing());
    assert_eq!(after_first_esc.input.buffer(), "qw");
    // Second Esc: clear buffer (still open, still resting).
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    let after_second_esc = app.hf_dialog.as_ref().expect("second Esc must keep dialog");
    assert!(after_second_esc.input.is_empty());
    // Third Esc: closes the dialog and returns focus.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.hf_dialog.is_none());
    assert_eq!(app.focus, Focus::List);
  }

  #[test]
  fn hf_dialog_o_in_resting_mode_cycles_sort_key() {
    use crate::init::hf_api::HfSortKey;
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::NONE));
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.sort),
      Some(HfSortKey::Downloads)
    );
    // First Esc exits edit so the dialog's keymap (o / n / p) fires.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('o'), KeyModifiers::NONE));
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.sort),
      Some(HfSortKey::Likes)
    );
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
  fn opening_hf_dialog_inherits_offline_flag_from_app_options() {
    // Regression: app.options.offline must propagate into the dialog
    // state at open time so the dialog renders "search disabled" and
    // its spawned fetch tasks short-circuit before HF traffic. A
    // false `app.options.offline` plus `LLAMASTASH_OFFLINE` unset
    // means the dialog stays online; a true value forces offline.
    let mut online = App::new(crate::tui::app::AppOptions {
      offline: false,
      ..Default::default()
    });
    online.open_hf_dialog();
    assert_eq!(
      online.hf_dialog.as_ref().map(|d| d.offline),
      Some(false),
      "online AppOptions must not flip the dialog into offline mode"
    );

    let mut offline = App::new(crate::tui::app::AppOptions {
      offline: true,
      ..Default::default()
    });
    offline.open_hf_dialog();
    assert_eq!(
      offline.hf_dialog.as_ref().map(|d| d.offline),
      Some(true),
      "offline AppOptions must flip the dialog into offline mode"
    );
  }

  #[test]
  fn ctrl_x_with_no_active_download_toasts_refusal() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(
      app.confirm_dialog.is_none(),
      "idle strip must not stage cancel confirm"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("no active download"),
      "expected refusal toast, got `{toast}`"
    );
  }

  #[test]
  fn ctrl_x_with_active_download_stages_cancel_confirm() {
    use crate::tui::app::ConfirmAction;
    use crate::tui::download_strip::QueuedPull;
    use crate::tui::hf_dialog::PickerRow;
    let mut app = App::new(Default::default());
    let pull = QueuedPull {
      repo_id: "owner/repo".into(),
      friendly_name: "owner/repo :model.gguf".into(),
      row: PickerRow::Single {
        filename: "model.gguf".into(),
        size_bytes: Some(123),
      },
    };
    app.download_strip.enqueue(pull);
    let promoted = app.download_strip.promote_next().unwrap();
    app.download_strip.install_active(&promoted);
    pump_input(&mut app, key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    match app
      .confirm_dialog
      .as_ref()
      .expect("cancel popup must stage")
    {
      ConfirmAction::CancelDownload {
        repo_id,
        friendly_name,
      } => {
        assert_eq!(repo_id, "owner/repo");
        assert!(friendly_name.contains("model.gguf"));
      }
      other => panic!("expected CancelDownload, got {other:?}"),
    }
  }

  #[test]
  fn confirmed_cancel_download_clears_active_and_keeps_queue() {
    // Confirm flow: stage the cancel popup, press Enter, then assert
    // the active slot is empty + the queued pull stayed in line.
    // (The queued pull is auto-promoted by apply_confirmed; with no
    // tokio runtime here we just verify the strip state.)
    use crate::tui::download_strip::QueuedPull;
    use crate::tui::hf_dialog::PickerRow;
    let mut app = App::new(Default::default());
    for (repo, file) in [("a/active", "active.gguf"), ("b/queued", "queued.gguf")] {
      app.download_strip.enqueue(QueuedPull {
        repo_id: repo.into(),
        friendly_name: format!("{repo} :{file}"),
        row: PickerRow::Single {
          filename: file.into(),
          size_bytes: Some(1),
        },
      });
    }
    let promoted = app.download_strip.promote_next().unwrap();
    app.download_strip.install_active(&promoted);
    // Stage the popup, then confirm with `y` (named cancel keys + y
    // are the confirmation chord per `handle_key`).
    pump_input(&mut app, key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(app.confirm_dialog.is_some());
    // The confirm dispatch spawns the next pull through tokio. We
    // can't run tokio here, so use a current-thread runtime to drive
    // the dispatch synchronously.
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap();
    rt.block_on(async {
      pump_input(&mut app, key(KeyCode::Char('y'), KeyModifiers::NONE));
    });
    assert!(app.confirm_dialog.is_none(), "popup must close on confirm");
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("cancelled"),
      "expected cancelled toast, got `{toast}`"
    );
    // The next pull was promoted, so active is now `b/queued`.
    let active = app
      .download_strip
      .active
      .as_ref()
      .expect("queued pull must have been promoted");
    assert_eq!(active.repo_id, "b/queued");
  }

  #[test]
  fn hf_dialog_o_while_editing_is_typed_not_cycled() {
    use crate::init::hf_api::HfSortKey;
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('d'), KeyModifiers::NONE));
    // Field is auto-edit on open, so `o` is typed.
    pump_input(&mut app, key(KeyCode::Char('o'), KeyModifiers::NONE));
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.input.buffer()),
      Some("o"),
      "`o` while editing must go into the buffer, not cycle sort"
    );
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.sort),
      Some(HfSortKey::Downloads),
      "sort must not have cycled while editing"
    );
  }

  #[test]
  fn mouse_events_are_ignored_so_terminal_owns_text_selection() {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crossterm::event::{MouseEvent, MouseEventKind};
    use std::path::PathBuf;
    // Mouse capture is intentionally disabled in `run()` so the
    // terminal can handle click-and-drag selection. Any mouse event
    // that does sneak through (e.g. when running under a test
    // harness with a wrapping terminal) must be a no-op.
    let mut app = App::new(Default::default());
    app.models = vec![DiscoveredModel {
      path: PathBuf::from("/m/a.gguf"),
      parent: PathBuf::from("/m"),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.list_cursor = 2;
    let evt = Event::Mouse(MouseEvent {
      kind: MouseEventKind::ScrollDown,
      column: 0,
      row: 0,
      modifiers: KeyModifiers::NONE,
    });
    pump_input(&mut app, evt);
    assert_eq!(app.list_cursor, 2, "mouse events must not move cursor");
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
      msg.contains("nothing to yank") || msg.contains("clipboard"),
      "yank toast must explain why: {msg}"
    );
  }

  #[test]
  fn submit_in_launch_picker_sends_start_model_through_writer() {
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
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.go_top();
    // Open picker and tweak ctx + reasoning so we can assert they
    // arrive on the wire.
    app.open_launch_picker();
    let p = app.launch_picker.as_mut().unwrap();
    p.cycle_ctx_preset();
    let expected_ctx = p.ctx;
    // Round-8: tri-state cycle — ModelDefault → On.
    p.cycle_reasoning_next();

    let (tx, mut rx) = mpsc::channel::<WriterCmd>(8);
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));

    let cmd = rx.try_recv().expect("writer must receive start_model");
    match cmd {
      WriterCmd::StartModel {
        model_path,
        ctx,
        reasoning,
        ..
      } => {
        assert_eq!(model_path, PathBuf::from("/m/qwen.gguf"));
        assert_eq!(ctx, expected_ctx);
        assert_eq!(
          reasoning,
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
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
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
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
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
      rss_bytes: None,
      cpu_pct: None,
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
  fn s_on_running_row_stages_stop_confirm_popup() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
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
  fn s_on_non_running_row_toasts_instead_of_staging_confirm() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
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
      key(KeyCode::Char('s'), KeyModifiers::NONE),
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
      key(KeyCode::Char('s'), KeyModifiers::NONE),
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
  fn ctrl_q_stages_kill_daemon_confirm() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::CONTROL));
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
      key(KeyCode::Char('q'), KeyModifiers::CONTROL),
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

    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::RightPane, "↓ must not leave the pane");
    let field = app
      .launch_picker
      .as_ref()
      .map(|p| p.field)
      .expect("↓ in Settings should materialise the picker form");
    assert_eq!(field, PickerField::Reasoning);

    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
      app.launch_picker.as_ref().unwrap().field,
      PickerField::Advanced
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
    assert_eq!(
      app.launch_picker.as_ref().expect("picker").field,
      PickerField::Advanced
    );
  }

  #[test]
  fn left_right_in_settings_cycle_focused_field_value() {
    // Round-7: ←/→ change the focused field's value (was bound to
    // pane-cycle pre-round-7). Outside Settings the keys stay
    // unbound so they don't double as pane navigation.
    use crate::tui::launch_picker::{PickerField, CTX_PRESETS};
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;

    // Auto-stages the picker on first key; cursor lands on Ctx.
    pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
    let p = app.launch_picker.as_ref().expect("picker auto-staged");
    assert_eq!(p.field, PickerField::Ctx);
    assert_eq!(p.ctx, Some(CTX_PRESETS[0]), "→ advances Ctx preset");

    pump_input(&mut app, key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(
      app.launch_picker.as_ref().unwrap().ctx,
      None,
      "← walks Ctx back to native"
    );
    // Pane focus must not have moved.
    assert_eq!(app.focus, Focus::RightPane);
  }

  #[test]
  fn arrows_in_settings_do_not_open_picker_over_running_launch() {
    // Regression: with a managed launch focused and no picker
    // staged, any arrow key used to silently call `with_picker` and
    // swap the read-only "Running launch" pane for the editable
    // form. That hid the live params behind the form and surprised
    // users. Arrows must now be a no-op in this state; `e` is the
    // explicit gate.
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
    // 2026-05-21: the `→` shortcut was removed (read as
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
    // active, so ↑/↓ walk the response viewport.
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Chat;
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.chat.scroll_offset, 1);
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.chat.scroll_offset, 2);
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.chat.scroll_offset, 1);
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
      app.chat.scroll_offset, 0,
      "Down past zero must clamp to 0, not underflow"
    );
  }

  #[test]
  fn arrow_keys_in_right_pane_scroll_embed_and_rerank_outputs() {
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Embed;
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.embed.scroll_offset, 1);
    app.right_tab = RightTab::Rerank;
    pump_input(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.rerank.scroll_offset, 1);
  }

  #[test]
  fn chat_reset_for_send_clears_scroll_offset() {
    // Sending a new prompt resets the viewport to the top so the
    // response streams from the beginning. Round-8 reset path.
    let mut app = App::new(Default::default());
    app.chat.scroll_offset = 7;
    app.chat.reset_for_send();
    assert_eq!(app.chat.scroll_offset, 0);
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
  fn s_on_settings_tab_opens_stop_confirm_for_running_launch() {
    // Round-8 dual-duty `s`: on the Logs tab it toggles
    // auto-scroll; on the Settings tab with a managed launch it
    // pops the stop-model confirm dialog (same path as `s` in the
    // Models list).
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
    match app.confirm_dialog {
      Some(crate::tui::app::ConfirmAction::StopModel { ref launch_id, .. }) => {
        assert_eq!(launch_id, "L-41100");
      }
      ref other => panic!("expected StopModel confirm, got {other:?}"),
    }
    // Auto-scroll must remain untouched — `s` on Settings is
    // routed away from the logs branch.
    assert!(
      app.logs_state.auto_scroll,
      "Settings `s` must not toggle the logs auto-scroll"
    );
  }

  #[test]
  fn s_on_settings_tab_toasts_without_managed_launch() {
    // F1 #5: pressing `s` on Settings for an unlaunched model used
    // to be a silent no-op. Now it routes through
    // `apply_stop_model` so the user sees the standard "nothing to
    // stop" toast. The chip is still gated on a managed launch,
    // but the keybinding is always live.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
    assert!(app.confirm_dialog.is_none());
    assert!(
      app.toast.is_some(),
      "Settings `s` with no managed row must toast, not silently no-op"
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
    assert!(matches!(cmd, WriterCmd::StartModel { .. }));
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
    assert!(matches!(cmd, WriterCmd::StartModel { .. }));
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
    // would after focusing the list. After Phase 1, the chain is
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
    assert!(matches!(cmd, WriterCmd::StartModel { .. }));
  }

  #[test]
  fn u_c_p_yanks_work_from_settings_tab() {
    // Round-8: `p` always yanks the focused path; `u` and `c`
    // need a running endpoint. Dispatch happens through the same
    // `apply_action` path as the Models list, so a successful
    // yank toast is enough to prove the binding routes.
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed_for_events("/m/qwen.gguf", 41100)];
    app.go_top();
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    pump_input(&mut app, key(KeyCode::Char('p'), KeyModifiers::NONE));
    assert!(app.toast_message().is_some(), "p must yank the path");
    app.toast = None;
    pump_input(&mut app, key(KeyCode::Char('u'), KeyModifiers::NONE));
    assert!(app.toast_message().is_some(), "u must yank the URL");
    app.toast = None;
    pump_input(&mut app, key(KeyCode::Char('c'), KeyModifiers::NONE));
    assert!(app.toast_message().is_some(), "c must yank the curl");
  }
}
