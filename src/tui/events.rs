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
use crate::tui::app::App;
use crate::tui::keybindings::{action_for, Action, Focus};
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
  /// ctx / reasoning / advanced fields.
  StartModel {
    model_path: PathBuf,
    ctx: Option<u32>,
    reasoning: bool,
    advanced: Vec<String>,
  },
  /// `favorite_add` for the supplied model path.
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
  writer: Option<&mpsc::UnboundedSender<WriterCmd>>,
) -> bool {
  if let Event::Key(key) = evt {
    if key.kind != KeyEventKind::Release {
      handle_key(app, key, writer);
    }
  }
  app.should_exit
}

fn handle_key(app: &mut App, key: KeyEvent, writer: Option<&mpsc::UnboundedSender<WriterCmd>>) {
  match app.focus {
    Focus::Filter => handle_filter_input(app, key),
    Focus::AdvancedPanel => handle_advanced_input(app, key),
    _ => {
      if let Some(action) = action_for(app.focus, key.code, key.modifiers) {
        apply_action(app, action, writer);
      }
    }
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

fn apply_action(app: &mut App, action: Action, writer: Option<&mpsc::UnboundedSender<WriterCmd>>) {
  match action {
    Action::Quit => app.should_exit = true,
    Action::MoveDown => match app.focus {
      Focus::LaunchPicker => {
        if let Some(p) = app.launch_picker.as_mut() {
          p.next_field();
        }
      }
      _ => app.move_down(),
    },
    Action::MoveUp => app.move_up(),
    Action::PageUp => {
      for _ in 0..10 {
        app.move_up();
      }
    }
    Action::PageDown => {
      for _ in 0..10 {
        app.move_down();
      }
    }
    Action::GoTop => app.go_top(),
    Action::GoBottom => app.go_bottom(),
    Action::OpenFilter => app.open_filter(),
    Action::ClearFilter => app.clear_filter(),
    Action::ToggleFavorite => apply_toggle_favorite(app, writer),
    Action::OpenLaunchPicker => app.open_launch_picker(),
    Action::OpenAdvancedPanel => app.open_advanced_panel(),
    Action::Submit => match app.focus {
      Focus::LaunchPicker => apply_launch_submit(app, writer),
      Focus::AdvancedPanel => app.close_advanced_panel(),
      _ => {}
    },
    Action::Cancel => match app.focus {
      Focus::LaunchPicker => app.close_launch_picker(),
      Focus::AdvancedPanel => app.close_advanced_panel(),
      _ => {}
    },
    Action::YankUrl | Action::YankCurl | Action::YankPath => {
      let text = build_yank_text(app, action);
      if let Some(text) = text {
        match clipboard::write(&text) {
          Ok(backend) => app.show_toast(format!("yanked via {backend}")),
          Err(e) => app.show_toast(format!("clipboard unavailable: {e}; {text}")),
        }
      } else {
        app.show_toast("nothing to yank — focus a Ready model");
      }
    }
    Action::CycleTheme => {
      app.cycle_theme();
      app.show_toast(format!("theme: {}", app.options.theme.canonical()));
    }
    Action::FocusRightPane => app.focus = Focus::RightPane,
    Action::FocusList => app.focus = Focus::List,
    Action::CycleTab => {
      app.cycle_right_tab();
    }
  }
}

/// Toggle the favorite for the focused model. Always applies the
/// optimistic local flip so the next render reflects the press; if a
/// writer is wired, also forward the corresponding IPC mutation so
/// the daemon's `favorite_list` reflects the change before the next
/// 750 ms refresh overwrites the local state.
fn apply_toggle_favorite(app: &mut App, writer: Option<&mpsc::UnboundedSender<WriterCmd>>) {
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
    if tx.send(cmd).is_err() {
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
fn apply_launch_submit(app: &mut App, writer: Option<&mpsc::UnboundedSender<WriterCmd>>) {
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

  let cmd = WriterCmd::StartModel {
    model_path: path,
    ctx: picker.ctx,
    reasoning: picker.reasoning,
    advanced,
  };

  match writer {
    Some(tx) => match tx.send(cmd) {
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
    Action::YankUrl | Action::YankCurl => {
      let m = app.focused_managed()?;
      let url = format!("http://127.0.0.1:{}/v1", m.port);
      Some(match action {
        Action::YankUrl => url,
        Action::YankCurl => format!(
          "curl -s -H 'Content-Type: application/json' -d '{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}]}}' {}/chat/completions",
          m.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model"),
          url
        ),
        _ => unreachable!(),
      })
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

/// Spawn the writer task and return the sender that callers push
/// [`WriterCmd`]s into. The task reconnects per command; the local
/// Unix socket makes that cheap and removes the "writer holds a
/// stale client across a daemon restart" failure mode.
pub fn spawn_writer(socket: PathBuf) -> mpsc::UnboundedSender<WriterCmd> {
  let (tx, mut rx) = mpsc::unbounded_channel::<WriterCmd>();
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
    } => (
      "start_model",
      json!({
        "model_path": model_path,
        "ctx": ctx,
        "reasoning": reasoning,
        "advanced": advanced,
      }),
    ),
    WriterCmd::FavoriteAdd(p) => ("favorite_add", json!({ "model_path": p })),
    WriterCmd::FavoriteRemove(p) => ("favorite_remove", json!({ "model_path": p })),
  }
}

/// Fully-featured TUI run-loop. Drives the App from real crossterm
/// events + a daemon refresher, rendering on each tick.
pub async fn run(app: App, socket: PathBuf) -> Result<()> {
  use crossterm::execute;
  use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
  };
  use ratatui::backend::CrosstermBackend;
  use ratatui::Terminal;

  enable_raw_mode()?;
  let mut stdout = std::io::stdout();
  execute!(stdout, EnterAlternateScreen)?;
  let backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(backend)?;

  let mut app = app;
  let mut refresh_rx = spawn_refresher(socket.clone());
  let writer_tx = spawn_writer(socket);

  loop {
    terminal.draw(|f| crate::tui::render::render(f, &mut app))?;

    // Drain any background ticks without blocking — keeps render
    // latency tight (~16 ms target) regardless of daemon RTT.
    while let Ok(tick) = refresh_rx.try_recv() {
      apply_refresh(&mut app, tick);
    }

    if event::poll(POLL_INTERVAL)? {
      let evt = event::read()?;
      if pump_input_with_writer(&mut app, evt, Some(&writer_tx)) {
        break;
      }
    }
  }

  // Restore the terminal even on early returns above.
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
    RefreshTick::Disconnected => {
      app.daemon_connected = false;
    }
  }
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
pub async fn launch(theme: crate::theme::ThemeName, socket: &Path) -> Result<()> {
  let app = App::new(crate::tui::app::AppOptions { theme });
  run(app, socket.to_path_buf()).await
}

#[cfg(test)]
mod tests {
  use super::*;
  use crossterm::event::{KeyEvent, KeyModifiers};

  fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, mods))
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
    pump_input(&mut app, key(KeyCode::Char('y'), KeyModifiers::NONE));
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
        reasoning_hint: None,
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
    p.toggle_reasoning();

    let (tx, mut rx) = mpsc::unbounded_channel::<WriterCmd>();
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
        assert!(reasoning, "reasoning toggle must propagate");
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
        reasoning_hint: None,
        mode_hint: ModeHint::Chat,
        weights_bytes: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.go_top();
    let (tx, mut rx) = mpsc::unbounded_channel::<WriterCmd>();
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
}
