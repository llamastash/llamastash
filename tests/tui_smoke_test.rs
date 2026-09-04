//! Headless TUI smoke tests against `ratatui::backend::TestBackend`.
//!
//! Each test renders one frame in a known state and asserts on
//! either the visible glyph layout or the App's post-render state.
//! No real terminal, no daemon — proves the render pipeline + key
//! handling stay coherent when the surrounding process changes.

use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use llamastash::discovery::{DiscoveredModel, ModelSource};
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::theme::ThemeName;
use llamastash::tui::app::{App, AppOptions};
use llamastash::tui::events::pump_input;
use llamastash::tui::keybindings::KeyMap;
use llamastash::tui::render::render;
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
      quant_label: None,
      native_ctx: Some(8192),
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: false,
      mode_hint: ModeHint::Chat,
      weights_bytes: Some(4_200_000_000),
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
fn empty_app_renders_title_info_and_empty_state() {
  let mut app = App::new(AppOptions::default());
  // Wider terminal so the full title row (brand + hint strip) fits.
  // The hint strip grew with the Q:kill daemon chip and now needs
  // ~120 cells to coexist with the connecting-daemon label.
  // Need ≥ MIN_HEIGHT_FOR_INFO_ROW (24) rows for the Host/Daemon/Logo
  // strip to render. 20-row terminals drop the info row by design.
  let frame = render_to_string(&mut app, 130, 24);
  assert!(
    frame.contains("LlamaStash"),
    "title row missing brand: {frame}"
  );
  assert!(
    frame.contains("daemon connecting"),
    "title row missing daemon-connecting label: {frame}"
  );
  assert!(
    frame.contains("No GGUFs surfaced"),
    "empty-state hint missing: {frame}"
  );
  // Global hint strip surfaces the canonical hotkeys on the title row.
  assert!(
    frame.contains("?:help") && frame.contains("q:quit"),
    "title row hint strip missing ?:help / q:quit: {frame}"
  );
  // Info row renders the three side-by-side panels.
  assert!(frame.contains("Host"), "info row missing Host: {frame}");
  assert!(frame.contains("Daemon"), "info row missing Daemon: {frame}");
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
  assert!(frame.contains("m/x"), "directory group header missing");
  assert!(frame.contains("qwen"), "qwen row missing");
  assert!(frame.contains("phi"), "phi row missing");
  // Connected daemon shows the bare `daemon` label (no `connecting…`
  // suffix) on the accent-bg title row.
  assert!(
    frame.contains("● daemon"),
    "title row missing connected daemon dot: {frame}"
  );
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
  let dir_pos = frame.find("m/y").expect("directory header rendered");
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
  assert_eq!(app.filter_input.buffer(), "qwen");
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
fn enter_on_model_opens_inline_launch_picker_in_settings_tab() {
  // The launch picker no longer pops a centred modal — pressing
  // Enter on a Models row parks focus on the right pane's Settings
  // tab and renders the picker form inline (kdash-style). The
  // surrounding right pane (with the picker form) shares one frame
  // with the Models list, so assertions check the inline form
  // fields rather than a `Launch · qwen` modal title.
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert_eq!(app.focus, Focus::RightPane);
  assert_eq!(app.right_tab, RightTab::Settings);
  assert!(app.launch_picker.is_some(), "picker state must materialise");
  // Tall enough for the whole inline picker in one frame — the badge, every
  // row llama.cpp declares under its group headers, extras, and the launch-chip
  // footer — so nothing the assertions look for scrolls off-screen.
  let frame = render_to_string(&mut app, 120, 42);
  assert!(
    frame.contains("Launch settings"),
    "Settings tab heading missing: {frame}"
  );
  assert!(
    frame.contains("ctx-size") && frame.contains("reasoning") && frame.contains("extras"),
    "inline picker fields missing: {frame}"
  );
}

#[test]
fn arrows_in_settings_tab_cycle_fields_and_values() {
  // Round-7 navigation model: ↑/↓ in the Settings tab cycle the
  // form's fields (ctx → reasoning → typed knobs → extras), and ←/→ cycle
  // the focused field's value. Tab cycles panes universally.
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::launch_picker::PickerField;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert_eq!(app.focus, Focus::RightPane);
  assert_eq!(app.right_tab, RightTab::Settings);
  let picker = app.launch_picker.as_ref().expect("picker");
  assert_eq!(
    picker.field,
    PickerField::Preset,
    "default focus leads on the preset row"
  );
  // ↓ moves the cursor onto the Ctx row.
  pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
  assert_eq!(
    app.launch_picker.as_ref().unwrap().field,
    PickerField::Knob(llamastash::launch::knobs::kid("ctx-size"))
  );
  assert_eq!(
    app
      .launch_picker
      .as_ref()
      .unwrap()
      .user_knobs
      .u32(llamastash::launch::knobs::kid("ctx-size")),
    None,
    "ctx defaults to native (no user override)"
  );
  // → advances the focused field's value; the first ring stop is Auto.
  pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
  assert!(
    app
      .launch_picker
      .as_ref()
      .unwrap()
      .user_knobs
      .is_auto(llamastash::launch::knobs::kid("ctx-size")),
    "the first ring stop is Auto"
  );
  // ← walks it back to inherited.
  pump_input(&mut app, key(KeyCode::Left, KeyModifiers::NONE));
  assert_eq!(
    app
      .launch_picker
      .as_ref()
      .unwrap()
      .user_knobs
      .u32(llamastash::launch::knobs::kid("ctx-size")),
    None
  );
  // ↓ moves the cursor to the next field.
  pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
  assert_eq!(
    app.launch_picker.as_ref().unwrap().field,
    PickerField::Knob(llamastash::launch::knobs::kid("reasoning"))
  );
  // reasoning is not fit-governed → no Auto stop; the ring is
  // Inherited → on → off, so → lands on `on`.
  pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
  assert_eq!(
    app
      .launch_picker
      .as_ref()
      .unwrap()
      .user_knobs
      .bool(llamastash::launch::knobs::kid("reasoning")),
    Some(true)
  );
}

#[test]
fn arrow_on_settings_auto_stages_picker_when_none() {
  // Landing on Settings via `Shift+S` doesn't open the picker,
  // but any field-cycle or value-cycle keystroke immediately
  // should — lets the user start editing without first reaching
  // for Enter.
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  // Shift+S jump (focus settings) without going through Enter.
  pump_input(&mut app, key(KeyCode::Char('S'), KeyModifiers::SHIFT));
  assert_eq!(app.focus, Focus::RightPane);
  assert_eq!(app.right_tab, RightTab::Settings);
  assert!(
    app.launch_picker.is_none(),
    "Shift+S alone must not stage the picker"
  );
  // ↓ (field-cycle) auto-stages.
  pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
  assert!(
    app.launch_picker.is_some(),
    "↓ on Settings must auto-stage the picker"
  );
  // Same auto-stage holds for the value axis.
  app.launch_picker = None;
  pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
  assert!(
    app.launch_picker.is_some(),
    "→ on Settings must auto-stage the picker"
  );
}

#[test]
fn launch_picker_modal_no_longer_renders() {
  // Round-6 removed the centred launch-picker overlay. Pressing
  // Enter on a model stages the form inline in the Settings tab —
  // the old `Launch · qwen` modal title must not appear in any
  // rendered frame.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    !frame.contains("Launch · qwen"),
    "no centred modal should render: {frame}"
  );
  // The inline Settings heading is the canonical surface.
  assert!(
    frame.contains("Launch settings"),
    "inline Settings heading must render instead: {frame}"
  );
}

#[test]
fn settings_focused_cyclable_field_renders_arrow_glyphs() {
  // The user needs a visible hint that Up/Down cycle the focused
  // field's value. The Settings render wraps cyclable fields'
  // values in `◀ … ▶` when the row is focused; non-cyclable
  // (Advanced) stays plain.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  // Cursor lands on the Preset row (it leads the form). Render wide enough
  // that the focused row's `◀ value ▶` hint isn't clipped by the right pane.
  let frame = render_to_string(&mut app, 160, 24);
  assert!(
    frame.contains("◀") && frame.contains("▶"),
    "focused cyclable field must show ◀ … ▶ value hint: {frame}"
  );
}

#[test]
fn theme_cycle_swaps_palette_without_restart() {
  let mut app = App::new(AppOptions {
    theme: ThemeName::Macchiato,
    custom_palette: None,
    keymap: KeyMap::default(),
    ..Default::default()
  });
  pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
  assert_ne!(app.options.theme, ThemeName::Macchiato);
  // Render still produces a coherent frame with the new theme.
  // 80x20 sits exactly at the dashboard floor — anything smaller
  // surfaces the "too small" placeholder instead.
  let frame = render_to_string(&mut app, 80, 20);
  assert!(frame.contains("LlamaStash"));
}

#[test]
fn right_pane_shows_settings_only_for_unlaunched_model() {
  // Pre-launch, the right pane has nothing live to surface for the
  // model — Logs / Chat / Embed / Rerank all need a running
  // supervisor. The tab set collapses to just Settings so the user
  // can configure and dispatch the launch from the same pane.
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  assert_eq!(
    app.available_right_tabs(),
    vec![RightTab::Settings],
    "unlaunched selection collapses to Settings only"
  );
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Settings"),
    "Settings tab must render in the right pane: {frame}"
  );
  // Logs / Chat / Embed / Rerank labels are gated on Ready state,
  // so none of them should render for the unlaunched model.
  assert!(
    !frame.contains(" Logs "),
    "Logs label must not render pre-launch: {frame}"
  );
}

#[test]
fn ready_chat_model_exposes_chat_tab_via_cycle() {
  use llamastash::tui::app::ManagedRow;
  use llamastash::tui::status_icons::SurfaceState;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
    device: None,
    rss_bytes: None,
    cpu_pct: None,
    ..Default::default()
  }];
  app.go_top();
  let tabs = app.available_right_tabs();
  assert!(tabs.contains(&RightTab::Chat));
  // The focus chain is [List, Settings, Logs, Chat] and the cursor
  // starts on the list, so three Tabs reach Chat.
  for _ in 0..3 {
    pump_input(&mut app, key(KeyCode::Tab, KeyModifiers::NONE));
  }
  assert_eq!(app.right_tab, RightTab::Chat);
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Chat"),
    "Chat tab body must render once selected: {frame}"
  );
}

#[test]
fn external_row_surfaces_via_ingest_status_external_array() {
  use llamastash::tui::status_icons::SurfaceState;
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
        "knobs": {"ctx": 8192, "reasoning": true, "threads": 8},
        "extras": ["--rope-freq-base", "10000"],
      },
    }],
  }));
  app.go_top();
  app.open_launch_picker();
  let picker = app.launch_picker.as_ref().expect("picker open");
  assert_eq!(
    picker
      .user_knobs
      .u32(llamastash::launch::knobs::kid("ctx-size")),
    Some(8192)
  );
  assert_eq!(
    picker
      .user_knobs
      .bool(llamastash::launch::knobs::kid("reasoning")),
    Some(true),
    "reasoning toggle must seed from last_params via user_knobs"
  );
  let extras: Vec<String> = picker
    .extras
    .iter()
    .map(|s| s.to_string_lossy().into_owned())
    .collect();
  assert!(
    extras.iter().any(|s| s == "--rope-freq-base"),
    "extras must seed from last_params: {extras:?}"
  );
}

#[test]
fn picker_warns_when_focused_model_already_has_active_instance() {
  // Submitting the picker on a model that already has a running
  // launch is allowed (v1 supports duplicate launches on fresh
  // ports), but the Settings tab must surface a heads-up so the
  // user isn't surprised. Running paths drop out of Favorites /
  // folder groups, so the only row for this model is the Running
  // entry itself — that's where the cursor lands.
  use llamastash::tui::app::ManagedRow;
  use llamastash::tui::status_icons::SurfaceState;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41101,
    state: SurfaceState::Ready,
    device: None,
    rss_bytes: None,
    cpu_pct: None,
    ..Default::default()
  }];
  // Row layout: [TableHeader, Header(▶ Running), Model(L1)].
  app.list_cursor = 2;
  app.open_launch_picker();
  assert_eq!(
    app.launch_picker.as_ref().unwrap().active_instances,
    1,
    "active instance count must reach the picker"
  );
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("already running"),
    "Settings tab must surface duplicate-launch heads-up: {frame}"
  );
}

#[test]
fn list_pane_renders_est_mem_badge() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  let frame = render_to_string(&mut app, 120, 20);
  // Fixture sets weights_bytes = 4_200_000_000 → ~3.9 GiB. The
  // badge formatter prints in binary GiB so the visible value is
  // "3.9G", not the raw "4.2G" the SI count would suggest.
  assert!(
    frame.contains("3.9G"),
    "est-mem badge must render alongside arch/quant: {frame}"
  );
}

#[test]
fn typing_into_chat_input_extends_prompt_buffer() {
  // Tab/Shift+Tab now cycle form fields rather than pane focus
  // (see `kdash-style polish round 5`), so this test drives the
  // pane chain with → / e instead: → moves Models → RightPane on
  // the Chat tab, then `e` enters edit mode → ChatInput.
  use llamastash::tui::app::ManagedRow;
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::status_icons::SurfaceState;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
    device: None,
    rss_bytes: None,
    cpu_pct: None,
    ..Default::default()
  }];
  app.go_top();
  // Shift+C is the canonical jump to the Chat tab (gated on a
  // running model). Combines the Models → RightPane pane hop and
  // the right_tab selection into one keystroke so the test doesn't
  // depend on the tab-cycle default landing on Logs.
  pump_input(&mut app, key(KeyCode::Char('C'), KeyModifiers::SHIFT));
  assert_eq!(app.focus, Focus::RightPane);
  assert_eq!(
    app.right_tab,
    RightTab::Chat,
    "Shift+C jumps to the Chat tab"
  );
  pump_input(&mut app, key(KeyCode::Char('e'), KeyModifiers::NONE));
  assert_eq!(
    app.focus,
    Focus::ChatInput,
    "`e` enters edit on the Chat tab"
  );
  for ch in "hello".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  assert_eq!(app.chat.prompt.buffer(), "hello");
}

#[test]
fn r_toggles_think_collapse_across_chat_focus_modes() {
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RightPane;
  app.right_tab = RightTab::Chat;
  // Browsing focus on Chat tab → `r` flips the toggle.
  assert!(!app.chat.collapse_thinks);
  pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::NONE));
  assert!(app.chat.collapse_thinks);
  pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::NONE));
  assert!(!app.chat.collapse_thinks);
  // ChatInput focus in resting (non-editing) state also fires the
  // toggle — the `InputField`'s resting handler lets `r` fall
  // through to the action layer.
  app.focus = Focus::ChatInput;
  assert!(!app.chat.prompt.is_editing());
  pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::NONE));
  assert!(app.chat.collapse_thinks);
  // Once editing, `r` is captured as a typed character; the toggle
  // must NOT flip and the buffer must grow.
  app.chat.prompt.enter_edit();
  let before = app.chat.collapse_thinks;
  pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::NONE));
  assert_eq!(app.chat.collapse_thinks, before);
  assert!(app.chat.prompt.buffer().contains('r'));
}

#[test]
fn s_in_right_pane_toggles_logs_auto_scroll() {
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RightPane;
  // Default right tab is Settings now — flip to Logs so `s`
  // exercises the auto-scroll toggle the test names.
  app.right_tab = RightTab::Logs;
  assert!(app.logs_state.auto_scroll);
  pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
  assert!(!app.logs_state.auto_scroll);
}

#[test]
fn rerank_enter_in_candidate_field_stages_buffer() {
  // Round-9: the `+` / `=` dedicated stage chords retire. Enter
  // in the candidate field now stages the typed candidate onto
  // the list — no extra chord required. Enter in the query field
  // still dispatches `/v1/rerank`.
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::tabs::rerank::RerankField;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RerankInput;
  // Modal field needs edit mode before typing lands in the buffer.
  app.rerank.query.enter_edit();
  // Type a query then ↓ to the candidate field.
  for ch in "what?".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
  assert_eq!(app.rerank.field, RerankField::Candidate);
  app.rerank.candidate_buffer.enter_edit();
  for ch in "doc one".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  // Enter in the candidate field — stages the buffer onto the
  // list, clears the buffer, stays in the candidate field so the
  // user can keep typing.
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert_eq!(app.rerank.query.buffer(), "what?");
  assert_eq!(app.rerank.candidates, vec!["doc one".to_string()]);
  assert!(app.rerank.candidate_buffer.is_empty());
  assert_eq!(app.rerank.field, RerankField::Candidate);
}

#[test]
fn rerank_enter_in_candidate_field_with_empty_buffer_toasts() {
  // Empty candidate buffer → no add, just a hint toast so the
  // user understands why nothing changed.
  use llamastash::tui::keybindings::Focus;
  use llamastash::tui::tabs::rerank::RerankField;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RerankInput;
  app.rerank.field = RerankField::Candidate;
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert!(app.rerank.candidates.is_empty());
  assert!(app.toast_message().is_some());
}

#[test]
fn narrow_terminal_does_not_crash_render() {
  // 60x20 is the compact floor — the renderer paints the full
  // dashboard (left list, no right pane) without panicking.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/very-long-model-name-2.5b-Q4_K_M.gguf", "/m")];
  let frame = render_to_string(&mut app, 60, 20);
  assert!(frame.contains("LlamaStash"));
}

#[test]
fn compact_width_hides_right_pane_and_keeps_only_name_ctx_size() {
  // At 60 cells the dashboard is in compact mode — the right pane
  // is hidden (focus lives on the List), so the list gets the
  // whole 60 cells. With PREFERRED_NAME_W (33) reserved for the
  // model name first, the rank picker fits Ctx + Size only —
  // Quant (rank 30) overflows the 22-cell budget. The user's
  // primary signal (name) gets ~40 cells; data shrinks to the
  // two highest-priority columns. Pins three contracts from the
  // rendered surface: compact pane mode, ranked drop, and the
  // name-comfort priority over data density.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  let frame = render_to_string(&mut app, 60, 20);
  for label in [" Ctx ", " Size "] {
    assert!(
      frame.contains(label),
      "header {label:?} should survive at 60 cells: {frame}"
    );
  }
  // Anchored on surrounding spaces so it doesn't false-match
  // strings like `Models [1]` or `model`.
  for label in [" Quant ", " Arch ", " Mode ", " Port "] {
    assert!(
      !frame.contains(label),
      "header {label:?} should drop at 60 cells (name takes priority): {frame}"
    );
  }
  // Settings / Logs tab labels live only inside the right pane —
  // their absence proves the pane is hidden in compact mode.
  assert!(
    !frame.contains("Settings"),
    "right pane should be hidden in compact mode by default: {frame}"
  );
}

#[test]
fn compact_width_opens_right_pane_when_focus_drills_in() {
  // Drill-in via `Enter` on a model row: open_launch_picker moves
  // focus to RightPane → the right pane appears in compact mode.
  // The list collapses to 35% of the body so the right pane gets
  // the larger slice for content.
  use llamastash::tui::keybindings::Focus;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  // Cursor on the only model row (after TableHeader + folder header).
  app.list_cursor = 2;
  app.open_launch_picker();
  assert_eq!(app.focus, Focus::RightPane);
  let frame = render_to_string(&mut app, 60, 20);
  // Settings tab is now visible — the drill-in surfaces the right
  // pane.
  assert!(
    frame.contains("Settings"),
    "right pane should appear after open_launch_picker in compact mode: {frame}"
  );
}

#[test]
fn compact_width_closes_right_pane_when_focus_returns_to_list() {
  // Round-trip: drill in, then Esc back to the list. close_launch_picker
  // moves focus to Focus::List → right pane disappears.
  use llamastash::tui::keybindings::Focus;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.list_cursor = 2;
  app.open_launch_picker();
  assert_eq!(app.focus, Focus::RightPane);
  app.close_launch_picker();
  assert_eq!(app.focus, Focus::List);
  let frame = render_to_string(&mut app, 60, 20);
  assert!(
    !frame.contains("Settings"),
    "right pane should be hidden after close_launch_picker in compact mode: {frame}"
  );
}

#[test]
fn wide_width_always_shows_right_pane_regardless_of_focus() {
  // At ≥100 cells the right pane renders no matter where focus is —
  // mirrors the established wide-mode contract.
  use llamastash::tui::keybindings::Focus;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  // Focus on List — in compact mode this would hide the pane.
  app.focus = Focus::List;
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Settings"),
    "right pane must render in wide mode even with focus on List: {frame}"
  );
}

#[test]
fn show_toast_paints_visible_bar_near_bottom() {
  // Regression: the kdash refactor (5005b4c) removed the bottom
  // help-bar toast slot and nothing else painted the field, so every
  // copy / theme-cycle / refusal toast was silently invisible. The
  // current renderer floats a single-line accent bar above the
  // bottom edge — verify the text actually lands in the frame.
  let mut app = App::new(AppOptions::default());
  app.show_toast("copied URL via x11");
  let frame = render_to_string(&mut app, 130, 24);
  assert!(
    frame.contains("copied URL via x11"),
    "toast text missing from rendered frame:\n{frame}"
  );
}

/// Render one frame and return the set of distinct background colours
/// found on cells whose symbol is part of `needle`. Lets a test prove
/// the toast bar paints on the expected palette slot rather than only
/// that the glyphs landed.
fn toast_bar_bg_colors(app: &mut App, needle: &str) -> Vec<ratatui::style::Color> {
  let backend = TestBackend::new(130, 24);
  let mut terminal = Terminal::new(backend).expect("test terminal");
  terminal.draw(|f| render(f, app)).expect("render");
  let buf = terminal.backend().buffer();
  // The toast paints ` {msg} ` on a filled bar; sample the bg of every
  // cell on rows that contain the message text.
  let mut rows_with_msg = std::collections::BTreeSet::new();
  for y in 0..buf.area.height {
    let mut row = String::new();
    for x in 0..buf.area.width {
      row.push_str(buf[(x, y)].symbol());
    }
    if row.contains(needle) {
      rows_with_msg.insert(y);
    }
  }
  let mut colors = Vec::new();
  for y in rows_with_msg {
    for x in 0..buf.area.width {
      let cell = &buf[(x, y)];
      if cell.symbol().trim().is_empty() {
        continue;
      }
      if !colors.contains(&cell.bg) {
        colors.push(cell.bg);
      }
    }
  }
  colors
}

#[test]
fn error_toast_paints_on_error_slot_not_accent() {
  // An error toast (writer offline, clipboard failure, port in use)
  // must read as red, not ride the neutral accent the way success /
  // refusal toasts do — otherwise a failure masquerades as a positive
  // confirmation. Pin the bar's background to `palette.error` and
  // prove a neutral toast still uses `palette.accent`.
  let palette = llamastash::theme::palette_for(ThemeName::Macchiato);

  let mut app = App::new(AppOptions::default());
  app.show_error_toast("clipboard unavailable: no backend");
  let err_colors = toast_bar_bg_colors(&mut app, "clipboard unavailable");
  assert!(
    err_colors.contains(&palette.error),
    "error toast must paint on palette.error, saw {err_colors:?}"
  );
  assert!(
    !err_colors.contains(&palette.accent),
    "error toast must not ride the accent slot, saw {err_colors:?}"
  );

  let mut app = App::new(AppOptions::default());
  app.show_toast("copied logs (12 lines)");
  let ok_colors = toast_bar_bg_colors(&mut app, "copied logs");
  assert!(
    ok_colors.contains(&palette.accent),
    "neutral toast must keep the accent slot, saw {ok_colors:?}"
  );
  assert!(
    !ok_colors.contains(&palette.error),
    "neutral toast must not paint red, saw {ok_colors:?}"
  );
}

#[test]
fn narrow_terminal_truncates_long_model_names_with_ellipsis() {
  // Plan edge case: a name wider than the list pane should render
  // with `…` rather than wrapping. We use a synthetic super-long
  // name so the truncation kicks in regardless of theme padding.
  let mut app = App::new(AppOptions::default());
  let very_long = "a-very-very-very-long-model-name-that-easily-overflows-sixty-columns.gguf";
  app.models = vec![fake_model(&format!("/m/{very_long}"), "/m")];
  let frame = render_to_string(&mut app, 60, 20);
  assert!(
    frame.contains('…'),
    "expected ellipsis truncation glyph in:\n{frame}"
  );
  // No line in the rendered frame should be wider than the
  // terminal width (60 + a couple of trailing spaces for borders);
  // assert a generous upper bound to catch line-wrap regressions.
  for line in frame.lines() {
    assert!(
      line.chars().count() <= 64,
      "rendered line {line:?} wider than 60-col terminal"
    );
  }
}

#[test]
fn alt_enter_opens_launch_name_dialog() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen3.gguf", "/m")];
  app.go_top();

  // Alt+Enter should open the launch-name dialog.
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::ALT));
  assert!(
    app.launch_name_dialog.is_some(),
    "Alt+Enter must open the launch-name dialog"
  );

  // Escape should close it.
  pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
  assert!(
    app.launch_name_dialog.is_none(),
    "Escape must close the launch-name dialog"
  );
}

#[test]
fn plain_enter_does_not_open_launch_name_dialog() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen3.gguf", "/m")];
  app.go_top();

  // Plain Enter (no Alt) should NOT open the launch-name dialog.
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert!(
    app.launch_name_dialog.is_none(),
    "Plain Enter must not open the launch-name dialog"
  );
}
