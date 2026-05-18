//! Headless TUI smoke tests against `ratatui::backend::TestBackend`.
//!
//! Each test renders one frame in a known state and asserts on
//! either the visible glyph layout or the App's post-render state.
//! No real terminal, no daemon — proves the render pipeline + key
//! handling stay coherent when the surrounding process changes.

use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use llamadash::discovery::{DiscoveredModel, ModelSource};
use llamadash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamadash::theme::ThemeName;
use llamadash::tui::app::{App, AppOptions};
use llamadash::tui::events::pump_input;
use llamadash::tui::keybindings::KeyMap;
use llamadash::tui::render::render;
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
fn empty_app_renders_title_info_and_empty_state() {
  let mut app = App::new(AppOptions::default());
  // Wider terminal so the full title row (brand + hint strip) fits.
  // The hint strip grew with the Q:kill daemon chip and now needs
  // ~120 cells to coexist with the connecting-daemon label.
  let frame = render_to_string(&mut app, 130, 20);
  assert!(
    frame.contains("LlamaDash"),
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
  assert!(frame.contains("/m/x"), "directory group header missing");
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
fn enter_on_model_opens_inline_launch_picker_in_settings_tab() {
  // The launch picker no longer pops a centred modal — pressing
  // Enter on a Models row parks focus on the right pane's Settings
  // tab and renders the picker form inline (kdash-style). The
  // surrounding right pane (with the picker form) shares one frame
  // with the Models list, so assertions check the inline form
  // fields rather than a `Launch · qwen` modal title.
  use llamadash::tui::keybindings::Focus;
  use llamadash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert_eq!(app.focus, Focus::RightPane);
  assert_eq!(app.right_tab, RightTab::Settings);
  assert!(app.launch_picker.is_some(), "picker state must materialise");
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Launch settings"),
    "Settings tab heading missing: {frame}"
  );
  assert!(
    frame.contains("ctx") && frame.contains("reasoning") && frame.contains("advanced"),
    "inline picker fields missing: {frame}"
  );
}

#[test]
fn arrows_in_settings_tab_cycle_fields_and_values() {
  // Round-7 navigation model: ↑/↓ in the Settings tab cycle the
  // form's fields (ctx → reasoning → advanced), and ←/→ cycle
  // the focused field's value. Tab cycles panes universally.
  use llamadash::tui::keybindings::Focus;
  use llamadash::tui::launch_picker::PickerField;
  use llamadash::tui::launch_picker::CTX_PRESETS;
  use llamadash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  assert_eq!(app.focus, Focus::RightPane);
  assert_eq!(app.right_tab, RightTab::Settings);
  let picker = app.launch_picker.as_ref().expect("picker");
  assert_eq!(picker.field, PickerField::Ctx);
  assert_eq!(picker.ctx, None, "ctx defaults to native");
  // → advances the focused field's value.
  pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
  assert_eq!(
    app.launch_picker.as_ref().unwrap().ctx,
    Some(CTX_PRESETS[0])
  );
  // ← walks it back to native.
  pump_input(&mut app, key(KeyCode::Left, KeyModifiers::NONE));
  assert_eq!(app.launch_picker.as_ref().unwrap().ctx, None);
  // ↓ moves the cursor to the next field.
  pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
  assert_eq!(
    app.launch_picker.as_ref().unwrap().field,
    PickerField::Reasoning
  );
  // → walks the reasoning tri-state forward (ModelDefault → On).
  pump_input(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
  use llamadash::tui::launch_picker::ReasoningSetting;
  assert_eq!(
    app.launch_picker.as_ref().unwrap().reasoning,
    ReasoningSetting::On
  );
}

#[test]
fn arrow_on_settings_auto_stages_picker_when_none() {
  // Landing on Settings via `Shift+S` doesn't open the picker,
  // but any field-cycle or value-cycle keystroke immediately
  // should — lets the user start editing without first reaching
  // for Enter.
  use llamadash::tui::keybindings::Focus;
  use llamadash::tui::RightTab;
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
  // Cursor lands on Ctx (the first field) by default.
  let frame = render_to_string(&mut app, 120, 24);
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
  });
  pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
  assert_ne!(app.options.theme, ThemeName::Macchiato);
  // Render still produces a coherent frame with the new theme.
  let frame = render_to_string(&mut app, 80, 12);
  assert!(frame.contains("LlamaDash"));
}

#[test]
fn right_pane_shows_settings_only_for_unlaunched_model() {
  // Pre-launch, the right pane has nothing live to surface for the
  // model — Logs / Chat / Embed / Rerank all need a running
  // supervisor. The tab set collapses to just Settings so the user
  // can configure and dispatch the launch from the same pane.
  use llamadash::tui::RightTab;
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
  use llamadash::tui::app::ManagedRow;
  use llamadash::tui::status_icons::SurfaceState;
  use llamadash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
    rss_bytes: None,
    cpu_pct: None,
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
  use llamadash::tui::status_icons::SurfaceState;
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
  // Round-8: persisted `reasoning: true` lands on the explicit
  // `On` tri-state, not on the `ModelDefault` neutral slot.
  use llamadash::tui::launch_picker::ReasoningSetting;
  assert_eq!(
    picker.reasoning,
    ReasoningSetting::On,
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
  // Submitting the picker on a model that already has a running
  // launch is allowed (v1 supports duplicate launches on fresh
  // ports), but the Settings tab must surface a heads-up so the
  // user isn't surprised. Running paths drop out of Favorites /
  // folder groups, so the only row for this model is the Running
  // entry itself — that's where the cursor lands.
  use llamadash::tui::app::ManagedRow;
  use llamadash::tui::status_icons::SurfaceState;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41101,
    state: SurfaceState::Ready,
    rss_bytes: None,
    cpu_pct: None,
  }];
  // Row layout: [TableHeader, Header(󰑐 Running), Model(L1)].
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
fn typing_into_chat_input_extends_prompt_buffer() {
  // Tab/Shift+Tab now cycle form fields rather than pane focus
  // (see `kdash-style polish round 5`), so this test drives the
  // pane chain with → / e instead: → moves Models → RightPane on
  // the Chat tab, then `e` enters edit mode → ChatInput.
  use llamadash::tui::app::ManagedRow;
  use llamadash::tui::keybindings::Focus;
  use llamadash::tui::status_icons::SurfaceState;
  use llamadash::tui::RightTab;
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/qwen.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
    rss_bytes: None,
    cpu_pct: None,
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
  assert_eq!(app.chat.prompt, "hello");
}

#[test]
fn ctrl_r_in_chat_input_toggles_think_collapse() {
  use llamadash::tui::keybindings::Focus;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::ChatInput;
  assert!(!app.chat.collapse_thinks);
  pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::CONTROL));
  assert!(app.chat.collapse_thinks);
  pump_input(&mut app, key(KeyCode::Char('r'), KeyModifiers::CONTROL));
  assert!(!app.chat.collapse_thinks);
}

#[test]
fn s_in_right_pane_toggles_logs_auto_scroll() {
  use llamadash::tui::keybindings::Focus;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RightPane;
  assert!(app.logs_state.auto_scroll);
  pump_input(&mut app, key(KeyCode::Char('s'), KeyModifiers::NONE));
  assert!(!app.logs_state.auto_scroll);
}

#[test]
fn rerank_input_cycles_field_with_down_and_stages_via_equals() {
  // Round-7 navigation: ↓ cycles between the Rerank query and
  // candidate fields, and `=` (no Shift) stages the candidate
  // onto the list. Tab is now the universal pane-cycle.
  use llamadash::tui::keybindings::Focus;
  use llamadash::tui::tabs::rerank::RerankField;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RerankInput;
  // Type a query then ↓ to candidate field.
  for ch in "what?".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  pump_input(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
  assert_eq!(app.rerank.field, RerankField::Candidate);
  // Type a candidate then `=` (the no-shift alias for `+`) to stage.
  for ch in "doc one".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  pump_input(&mut app, key(KeyCode::Char('='), KeyModifiers::NONE));
  assert_eq!(app.rerank.query, "what?");
  assert_eq!(app.rerank.candidates, vec!["doc one".to_string()]);
  assert!(app.rerank.candidate_buffer.is_empty());
}

#[test]
fn rerank_plus_with_shift_also_stages_candidate() {
  // `+` mirrors `=` for users who naturally hold Shift. Both
  // alias to `StageRerankCandidate`.
  use llamadash::tui::keybindings::Focus;
  use llamadash::tui::tabs::rerank::RerankField;
  let mut app = App::new(AppOptions::default());
  app.focus = Focus::RerankInput;
  app.rerank.field = RerankField::Candidate;
  for ch in "doc two".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  pump_input(&mut app, key(KeyCode::Char('+'), KeyModifiers::SHIFT));
  assert_eq!(app.rerank.candidates, vec!["doc two".to_string()]);
}

#[test]
fn narrow_terminal_does_not_crash_render() {
  // Plan edge case: terminal width 60 cols → renderer must
  // tolerate the constraint without panicking.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/very-long-model-name-2.5b-Q4_K_M.gguf", "/m")];
  let frame = render_to_string(&mut app, 60, 12);
  assert!(frame.contains("LlamaDash"));
}

#[test]
fn narrow_terminal_truncates_long_model_names_with_ellipsis() {
  // Plan edge case: a name wider than the list pane should render
  // with `…` rather than wrapping. We use a synthetic super-long
  // name so the truncation kicks in regardless of theme padding.
  let mut app = App::new(AppOptions::default());
  let very_long = "a-very-very-very-long-model-name-that-easily-overflows-sixty-columns.gguf";
  app.models = vec![fake_model(&format!("/m/{very_long}"), "/m")];
  let frame = render_to_string(&mut app, 60, 12);
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
