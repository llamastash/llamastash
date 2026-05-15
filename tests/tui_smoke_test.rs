//! Headless TUI smoke tests against `ratatui::backend::TestBackend`.
//!
//! Each test renders one frame in a known state and asserts on
//! either the visible glyph layout or the App's post-render state.
//! No real terminal, no daemon — proves the render pipeline + key
//! handling stay coherent when the surrounding process changes.

use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use llamatui::discovery::{DiscoveredModel, ModelSource};
use llamatui::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamatui::theme::ThemeName;
use llamatui::tui::app::{App, AppOptions};
use llamatui::tui::events::pump_input;
use llamatui::tui::render::render;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn fake_model(path: &str, parent: &str) -> DiscoveredModel {
  DiscoveredModel {
    path: PathBuf::from(path),
    parent: PathBuf::from(parent),
    source: ModelSource::UserPath,
    metadata: Some(ModelMetadata {
      arch: Some("llama".into()),
      total_parameters: Some(7_000_000_000),
      parameter_label: Some("7B".into()),
      quant: Quant::Q4_K,
      native_ctx: Some(8192),
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: None,
      mode_hint: ModeHint::Chat,
      weights_bytes: Some(4_200_000_000),
    }),
    parse_error: None,
    split_siblings: Vec::new(),
  }
}

fn key(code: KeyCode, mods: KeyModifiers) -> Event {
  Event::Key(KeyEvent::new(code, mods))
}

fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
  let backend = TestBackend::new(width, height);
  let mut terminal = Terminal::new(backend).expect("test terminal");
  terminal.draw(|f| render(f, app)).expect("render");
  let buf = terminal.backend().buffer();
  let mut out = String::with_capacity((width as usize + 1) * height as usize);
  for y in 0..buf.area.height {
    for x in 0..buf.area.width {
      out.push_str(buf[(x, y)].symbol());
    }
    out.push('\n');
  }
  out
}

#[test]
fn empty_app_renders_banner_help_and_empty_state() {
  let mut app = App::new(AppOptions::default());
  let frame = render_to_string(&mut app, 100, 20);
  assert!(frame.contains("llamatui"), "banner missing: {frame}");
  assert!(
    frame.contains("daemon: connecting"),
    "connection pill missing: {frame}"
  );
  assert!(
    frame.contains("No GGUFs surfaced"),
    "empty-state hint missing: {frame}"
  );
  // Help bar surfaces the canonical hotkeys.
  assert!(
    frame.contains("q") && frame.contains("/"),
    "help bar missing q + /: {frame}"
  );
}

#[test]
fn populated_app_renders_directory_groups_and_status_glyph() {
  let mut app = App::new(AppOptions::default());
  app.daemon_connected = true;
  app.models = vec![
    fake_model("/m/x/qwen.gguf", "/m/x"),
    fake_model("/m/y/phi.gguf", "/m/y"),
  ];
  let frame = render_to_string(&mut app, 120, 20);
  assert!(frame.contains("/m/x"), "directory group header missing");
  assert!(frame.contains("qwen"), "qwen row missing");
  assert!(frame.contains("phi"), "phi row missing");
  assert!(frame.contains("daemon: connected"));
}

#[test]
fn favorites_render_above_directory_groups() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![
    fake_model("/m/x/qwen.gguf", "/m/x"),
    fake_model("/m/y/phi.gguf", "/m/y"),
  ];
  app.favorites = vec![PathBuf::from("/m/x/qwen.gguf")];
  let frame = render_to_string(&mut app, 120, 20);
  let fav_pos = frame.find("Favorites").expect("favorites header rendered");
  let dir_pos = frame.find("/m/y").expect("directory header rendered");
  assert!(
    fav_pos < dir_pos,
    "favorites must render above directory groups (fav={fav_pos}, dir={dir_pos})"
  );
  // The ★ glyph is also surfaced on the favorited row.
  assert!(frame.contains("★"));
}

#[test]
fn slash_opens_filter_and_keystrokes_extend_buffer() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![
    fake_model("/m/qwen.gguf", "/m"),
    fake_model("/m/phi.gguf", "/m"),
  ];
  pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
  for ch in "qwen".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  assert_eq!(app.filter_buffer, "qwen");
  let frame = render_to_string(&mut app, 100, 20);
  assert!(
    frame.contains("qwen"),
    "filtered row remains visible: {frame}"
  );
  assert!(
    !frame.contains("phi"),
    "phi row must be filtered out: {frame}"
  );
}

#[test]
fn enter_on_model_opens_launch_picker_overlay() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  // Snap to the first model row (skipping the header).
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Launch · qwen"),
    "launch picker title missing: {frame}"
  );
  assert!(frame.contains("Context"));
  assert!(frame.contains("Reasoning"));
  assert!(frame.contains("Advanced"));
}

#[test]
fn theme_cycle_swaps_palette_without_restart() {
  let mut app = App::new(AppOptions {
    theme: ThemeName::Macchiato,
  });
  pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
  assert_ne!(app.options.theme, ThemeName::Macchiato);
  // Render still produces a coherent frame with the new theme.
  let frame = render_to_string(&mut app, 80, 12);
  assert!(frame.contains("llamatui"));
}

#[test]
fn right_pane_starts_on_logs_tab_for_unlaunched_model() {
  use llamatui::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  let frame = render_to_string(&mut app, 120, 24);
  assert_eq!(app.right_tab, RightTab::Logs, "Logs is the default tab");
  assert!(
    frame.contains("Logs"),
    "Logs tab label must render in the right pane strip: {frame}"
  );
}

#[test]
fn ready_chat_model_exposes_chat_tab_via_cycle() {
  use llamatui::tui::app::ManagedRow;
  use llamatui::tui::status_icons::SurfaceState;
  use llamatui::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
  }];
  app.go_top();
  let tabs = app.available_right_tabs();
  assert!(tabs.contains(&RightTab::Chat));
  // Cycle from Logs → Chat.
  app.cycle_right_tab();
  assert_eq!(app.right_tab, RightTab::Chat);
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Chat"),
    "Chat tab body must render once selected: {frame}"
  );
}

#[test]
fn external_row_surfaces_via_ingest_status_external_array() {
  use llamatui::tui::status_icons::SurfaceState;
  use serde_json::json;
  let mut app = App::new(AppOptions::default());
  app.ingest_status(&json!({
    "models": [],
    "external": [{
      "pid": 4242,
      "cmdline": "llama-server -m /opt/llms/foo.gguf",
      "model_path": "/opt/llms/foo.gguf",
    }],
  }));
  let rows = app.external_rows();
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0].state, SurfaceState::External);
  assert_eq!(rows[0].launch_id, "ext-4242");
}

#[test]
fn launch_picker_seeds_from_persisted_last_params() {
  use serde_json::json;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.ingest_last_params(&json!({
    "last_params": [{
      "id": {"path": "/m/qwen.gguf", "header_blake3": "00".repeat(32)},
      "model_path": "/m/qwen.gguf",
      "params": {
        "model_path": "/m/qwen.gguf",
        "mode": "chat",
        "ctx": 8192,
        "reasoning": true,
        "advanced": ["--threads", "8"],
      },
    }],
  }));
  app.go_top();
  app.open_launch_picker();
  let picker = app.launch_picker.as_ref().expect("picker open");
  assert_eq!(picker.ctx, Some(8192));
  assert!(
    picker.reasoning,
    "reasoning toggle must seed from last_params"
  );
  let advanced = app
    .advanced_panel
    .as_ref()
    .expect("advanced panel seeded")
    .buffer
    .clone();
  assert!(
    advanced.contains("--threads"),
    "advanced flags must seed from last_params: {advanced}"
  );
}

#[test]
fn picker_warns_when_focused_model_already_has_active_instance() {
  use llamatui::tui::app::ManagedRow;
  use llamatui::tui::status_icons::SurfaceState;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41101,
    state: SurfaceState::Ready,
  }];
  app.go_top();
  app.open_launch_picker();
  assert_eq!(
    app.launch_picker.as_ref().unwrap().active_instances,
    1,
    "active instance count must reach the picker"
  );
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("already running"),
    "picker must surface duplicate-launch heads-up: {frame}"
  );
}

#[test]
fn list_pane_renders_est_mem_badge() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  let frame = render_to_string(&mut app, 120, 12);
  // Fixture sets weights_bytes = 4_200_000_000 → ~3.9 GiB. The
  // badge formatter prints in binary GiB so the visible value is
  // "3.9G", not the raw "4.2G" the SI count would suggest.
  assert!(
    frame.contains("3.9G"),
    "est-mem badge must render alongside arch/quant: {frame}"
  );
}

#[test]
fn narrow_terminal_does_not_crash_render() {
  // Plan edge case: terminal width 60 cols → renderer must
  // tolerate the constraint without panicking. We don't pin the
  // exact truncation strategy here — just that the call succeeds.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/very-long-model-name-2.5b-Q4_K_M.gguf", "/m")];
  let frame = render_to_string(&mut app, 60, 12);
  assert!(frame.contains("llamatui"));
}
