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
    Focus::ChatInput | Focus::EmbedInput | Focus::RerankInput if bound.is_none() => {
      handle_tab_input(app, key);
    }
    _ => {
      if let Some(action) = bound {
        apply_action(app, action, writer);
      }
    }
  }
}

/// Text-capture handler for the chat / embed / rerank prompt
/// buffers. Bound actions (Enter, Tab, Esc, etc.) are routed
/// through [`apply_action`] *before* this is called — see
/// [`handle_key`] — so alphanumerics fall through to the buffer
/// without trampling the surrounding keybindings. Shift+Enter is
/// also bound (`Action::InsertNewline`); it never reaches this
/// fallthrough.
fn handle_tab_input(app: &mut App, key: KeyEvent) {
  match (app.focus, key.code) {
    (Focus::ChatInput, KeyCode::Backspace) => {
      app.chat.prompt.pop();
    }
    (Focus::ChatInput, KeyCode::Char(ch)) => {
      app.chat.prompt.push(ch);
    }
    (Focus::EmbedInput, KeyCode::Backspace) => {
      app.embed.input.pop();
    }
    (Focus::EmbedInput, KeyCode::Char(ch)) => {
      app.embed.input.push(ch);
    }
    (Focus::RerankInput, KeyCode::Backspace) => match app.rerank.field {
      RerankField::Query => {
        app.rerank.query.pop();
      }
      RerankField::Candidate => {
        app.rerank.candidate_buffer.pop();
      }
    },
    (Focus::RerankInput, KeyCode::Char(ch)) => match app.rerank.field {
      RerankField::Query => app.rerank.query.push(ch),
      RerankField::Candidate => app.rerank.candidate_buffer.push(ch),
    },
    _ => {}
  }
}

fn handle_filter_input(app: &mut App, key: KeyEvent) {
  match key.code {
    KeyCode::Esc => {
      app.clear_filter();
    }
    KeyCode::Enter => {
      app.focus = Focus::List;
    }
    KeyCode::Backspace => {
      app.filter_buffer.pop();
    }
    KeyCode::Char(ch) => {
      app.filter_buffer.push(ch);
    }
    _ => {}
  }
}

fn handle_advanced_input(app: &mut App, key: KeyEvent) {
  let panel = match &mut app.advanced_panel {
    Some(p) => p,
    None => return,
  };
  match key.code {
    KeyCode::Esc => app.close_advanced_panel(),
    KeyCode::Enter => app.close_advanced_panel(),
    KeyCode::Backspace => panel.backspace(),
    KeyCode::Char(ch) => panel.insert(ch),
    _ => {}
  }
}

fn apply_action(app: &mut App, action: Action, writer: Option<&mpsc::Sender<WriterCmd>>) {
  match action {
    Action::Quit => app.should_exit = true,
    Action::MoveDown => match app.focus {
      Focus::RightPane if app.right_tab == RightTab::Logs => {
        app.logs_state.scroll_down();
      }
      // Settings tab: ↑/↓ cycle the form's fields (ctx →
      // reasoning → advanced). The actual NextField/PrevField
      // dispatch handles the picker materialisation.
      Focus::RightPane if app.right_tab == RightTab::Settings => apply_next_field(app),
      // Round-8: Chat/Embed/Rerank output viewports scroll on the
      // same arrow keys as the Logs pane while focus stays on
      // the right pane (no edit mode).
      Focus::RightPane if app.right_tab == RightTab::Chat => app.chat.scroll_down(),
      Focus::RightPane if app.right_tab == RightTab::Embed => app.embed.scroll_down(),
      Focus::RightPane if app.right_tab == RightTab::Rerank => app.rerank.scroll_down(),
      Focus::RightPane => {}
      _ => app.move_down(),
    },
    Action::MoveUp => match app.focus {
      Focus::RightPane if app.right_tab == RightTab::Logs => {
        app.logs_state.scroll_up();
      }
      Focus::RightPane if app.right_tab == RightTab::Settings => apply_prev_field(app),
      Focus::RightPane if app.right_tab == RightTab::Chat => app.chat.scroll_up(),
      Focus::RightPane if app.right_tab == RightTab::Embed => app.embed.scroll_up(),
      Focus::RightPane if app.right_tab == RightTab::Rerank => app.rerank.scroll_up(),
      Focus::RightPane => {}
      _ => app.move_up(),
    },
    Action::PageUp => app.move_by(-10),
    Action::PageDown => app.move_by(10),
    Action::GoTop => app.go_top(),
    Action::GoBottom => app.go_bottom(),
    Action::OpenFilter => app.open_filter(),
    Action::ClearFilter => app.clear_filter(),
    Action::ToggleFavorite => apply_toggle_favorite(app, writer),
    Action::OpenLaunchPicker => app.open_launch_picker(),
    Action::OpenAdvancedPanel => app.open_advanced_panel(),
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
    Action::FocusList => app.focus = Focus::List,
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
    Action::EnterEdit => {
      // Only entry into a tab that actually captures text. Logs /
      // Settings stay in RightPane focus.
      if let Some(target) = edit_focus_for_tab(app.right_tab) {
        app.focus = target;
      }
    }
    Action::ExitEdit => {
      // Step back from a text-input focus to the surrounding right
      // pane navigation focus. Keystrokes resume hitting the chain
      // (Tab / Shift+Tab / h / l) instead of the buffer.
      app.focus = Focus::RightPane;
    }
    Action::FocusLogsTab => apply_focus_logs_tab(app),
    Action::FocusChatTab => apply_focus_chat_tab(app),
    Action::FocusSettingsTab => apply_focus_settings_tab(app),
    Action::InsertNewline => match app.focus {
      Focus::ChatInput => app.chat.prompt.push('\n'),
      Focus::EmbedInput => app.embed.input.push('\n'),
      Focus::RerankInput => match app.rerank.field {
        RerankField::Query => app.rerank.query.push('\n'),
        RerankField::Candidate => app.rerank.candidate_buffer.push('\n'),
      },
      _ => {}
    },
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

fn apply_cycle_value(app: &mut App, dir: ValueDir) {
  if !(app.focus == Focus::RightPane && app.right_tab == RightTab::Settings) {
    return;
  }
  if app.launch_picker.is_none() {
    app.open_launch_picker();
  }
  if let Some(p) = app.launch_picker.as_mut() {
    match dir {
      ValueDir::Next => p.cycle_focused_value_next(),
      ValueDir::Prev => p.cycle_focused_value_prev(),
    }
  }
}

fn apply_next_field(app: &mut App) {
  match app.focus {
    Focus::RerankInput => app.rerank.cycle_field(),
    Focus::RightPane if app.right_tab == RightTab::Settings => {
      if app.launch_picker.is_none() {
        app.open_launch_picker();
      }
      if let Some(p) = app.launch_picker.as_mut() {
        p.next_field();
      }
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
      if app.launch_picker.is_none() {
        app.open_launch_picker();
      }
      if let Some(p) = app.launch_picker.as_mut() {
        p.prev_field();
      }
    }
    _ => {}
  }
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
  app.confirm_dialog = Some(ConfirmAction::StopModel {
    launch_id: managed.launch_id.clone(),
    name: crate::util::paths::model_display_name(&managed.path),
  });
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

/// Trigger an OpenAI streaming chat completion against the focused
/// Ready model. Stashes the receiver on `app.chat` so the render
/// loop can drain it without blocking input.
fn apply_send_chat(app: &mut App) {
  let managed = match app.focused_managed() {
    Some(m) => m.clone(),
    None => {
      app.show_toast("no Ready model focused for chat");
      return;
    }
  };
  if app.chat.prompt.trim().is_empty() {
    app.show_toast("prompt is empty");
    return;
  }
  let prompt = app.chat.prompt.clone();
  let model_name = crate::util::paths::model_display_name(&managed.path);
  app.chat.reset_for_send();
  let rx = spawn_chat_stream(managed.port, model_name, prompt);
  app.chat.stream_rx = Some(rx);
}

/// One-shot embedding call. Spawns a background task; the result is
/// captured straight into `app.embed` because `EmbedTabState` lives
/// on `App`.
fn apply_embed_submit(app: &mut App) {
  let managed = match app.focused_managed() {
    Some(m) => m.clone(),
    None => {
      app.show_toast("no Ready model focused for embed");
      return;
    }
  };
  if app.embed.input.trim().is_empty() {
    app.show_toast("embed input is empty");
    return;
  }
  let input = app.embed.input.clone();
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

/// One-shot rerank call. Same async pattern as `apply_embed_submit`.
fn apply_rerank_submit(app: &mut App) {
  let managed = match app.focused_managed() {
    Some(m) => m.clone(),
    None => {
      app.show_toast("no Ready model focused for rerank");
      return;
    }
  };
  if app.rerank.query.trim().is_empty() {
    app.show_toast("rerank query is empty");
    return;
  }
  // Auto-stage any in-progress candidate buffer the user hasn't
  // pressed Tab on yet — saves a keystroke for the common case.
  app.rerank.stage_candidate();
  if app.rerank.candidates.is_empty() {
    let stage_key = app
      .hint(Focus::RerankInput, Action::StageRerankCandidate)
      .map(|chip| {
        // chip is "Tab:cycle field/stage candidate" — pull off
        // the label so the toast reads "(Tab to add)".
        chip.split(':').next().unwrap_or("Tab").to_string()
      })
      .unwrap_or_else(|| "Tab".to_string());
    app.show_toast(format!("stage at least one candidate ({stage_key} to add)"));
    return;
  }
  let query = app.rerank.query.clone();
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
    app.confirm_dialog = Some(ConfirmAction::LaunchDuplicate {
      name: crate::util::paths::model_display_name(&path),
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
      Some(format!(
        "curl -s -H 'Content-Type: application/json' -d '{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}]}}' {}/chat/completions",
        crate::util::paths::model_display_name(&m.path),
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
/// The channel is bounded so a wedged daemon + scripted rapid input
/// can't exhaust process memory. Callers using `try_send` get an
/// `Err(Full)` and can either drop the action or surface a toast.
pub fn spawn_writer(socket: PathBuf) -> mpsc::Sender<WriterCmd> {
  let (tx, mut rx) = mpsc::channel::<WriterCmd>(WRITER_CHANNEL_CAPACITY);
  tokio::spawn(async move {
    while let Some(cmd) = rx.recv().await {
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
    WriterCmd::FavoriteAdd(p) => ("favorite_add", json!({ "model_path": p })),
    WriterCmd::FavoriteRemove(p) => ("favorite_remove", json!({ "model_path": p })),
  }
}

/// Fully-featured TUI run-loop. Drives the App from real crossterm
/// events + a daemon refresher, rendering on each tick.
pub async fn run(app: App, socket: PathBuf) -> Result<()> {
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
  let writer_tx = spawn_writer(socket);

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
  socket: &Path,
) -> Result<()> {
  let app = App::new(crate::tui::app::AppOptions {
    theme,
    custom_palette,
    keymap,
  });
  run(app, socket.to_path_buf()).await
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

#[cfg(test)]
mod tests {
  use super::*;
  use crossterm::event::{KeyEvent, KeyModifiers};

  fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, mods))
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
    app.focus = Focus::Filter;
    for ch in "qwen".chars() {
      pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(app.filter_buffer, "qwen");
  }

  #[test]
  fn esc_in_filter_clears_and_returns_focus() {
    let mut app = App::new(Default::default());
    app.focus = Focus::Filter;
    app.filter_buffer = "qwen".into();
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::List);
    assert!(app.filter_buffer.is_empty());
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
  fn capital_q_stages_kill_daemon_confirm() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('Q'), KeyModifiers::SHIFT));
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
      key(KeyCode::Char('Q'), KeyModifiers::SHIFT),
      Some(&tx),
    );
    pump_input_with_writer(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), Some(&tx));
    let cmd = rx.try_recv().expect("writer must receive shutdown");
    assert!(matches!(cmd, WriterCmd::Shutdown));
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
  fn right_arrow_on_models_list_enters_right_pane() {
    let mut app = App::new(Default::default());
    app.models = vec![fake_model_for_events("/m/qwen.gguf", "/m")];
    app.go_top();
    assert_eq!(app.focus, Focus::List);
    pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(
      app.focus,
      Focus::RightPane,
      "→ from Models must focus the right pane (round-8)"
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
    // chain is [List, Logs, Chat, Settings]; Tab from Logs lands
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
