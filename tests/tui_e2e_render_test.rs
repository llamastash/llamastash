//! End-to-end TUI render test (KDash-style).
//!
//! Renders the full dashboard into a fixed-size `TestBackend`, with a
//! seeded App that exercises every visible region — title bar, host
//! stats, daemon info, logo pane, models list with sections, right
//! pane with tab strip — and snapshots the resulting buffer to a
//! golden text file. The fixture lives at
//! `tests/golden/dashboard-overview.txt`; refresh it with the env var
//! `UPDATE_GOLDEN=1` after intentional UI changes.
//!
//! Modeled on KDash's `test_draw_overview_full_screen_fixture` in
//! `kdash/src/ui/mod.rs` — same approach: feed a deterministic state,
//! draw once, line-by-line compare against an embedded fixture.

use std::path::PathBuf;

use llamastash::daemon::host_metrics::HostMetricsSnapshot;
use llamastash::discovery::{DiscoveredModel, ModelSource};
use llamastash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamastash::init::hf_api::{HfGgufMeta, HfSearchResult};
use llamastash::theme::ThemeName;
use llamastash::tui::app::{App, AppOptions, DaemonInfo, ManagedRow};
use llamastash::tui::hf_dialog::{
  HardwareFitContext, HfDialogState, HfStage, PickerLoad, PickerRow,
};
use llamastash::tui::input_field::InputField;
use llamastash::tui::keybindings::{Focus, KeyMap};
use llamastash::tui::render::render;
use llamastash::tui::status_icons::SurfaceState;
use llamastash::tui::tabs::RightTab;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 30;
const GOLDEN_PATH: &str = "tests/golden/dashboard-overview.txt";

fn fake_model(path: &str, parent: &str, arch: &str, ctx: u64, weights: u64) -> DiscoveredModel {
  DiscoveredModel {
    path: PathBuf::from(path),
    parent: PathBuf::from(parent),
    source: ModelSource::UserPath,
    metadata: Some(ModelMetadata {
      arch: Some(arch.into()),
      total_parameters: Some(7_000_000_000),
      parameter_label: Some("7B".into()),
      quant: Quant::Q4_K,
      quant_label: None,
      native_ctx: Some(ctx),
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: false,
      mode_hint: ModeHint::Chat,
      weights_bytes: Some(weights),
    }),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: None,
    multimodal: None,
    supported_backends: Vec::new(),
  }
}

/// Build a fully-populated App fixture for the golden render.
fn seeded_dashboard_app() -> App {
  let mut app = App::new(AppOptions {
    theme: ThemeName::Macchiato,
    custom_palette: None,
    keymap: KeyMap::default(),
    ..Default::default()
  });
  app.daemon_connected = true;
  app.daemon_info = DaemonInfo {
    pid: Some(4242),
    uptime_seconds: Some(3 * 3600 + 12 * 60 + 45),
    build: Some("0.1.0".into()),
    server_path: Some("/usr/local/bin/llama-server".into()),
    ipc_url: Some("http://127.0.0.1:48134".into()),
    proxy: None,
    backend_binaries: Vec::new(),
  };
  app.host_metrics = HostMetricsSnapshot {
    cpu_pct: 47.5,
    cpu_temp_c: Some(54.0),
    ram_used_bytes: 11 * 1024 * 1024 * 1024,
    ram_total_bytes: 32 * 1024 * 1024 * 1024,
    gpu_util_pct: Some(84.0),
    gpu_mem_used_bytes: Some(14 * 1024 * 1024 * 1024),
    gpu_mem_total_bytes: Some(24 * 1024 * 1024 * 1024),
    gpu_temp_c: Some(68.0),
    gpu_backend: HostMetricsSnapshot::BACKEND_NVIDIA.into(),
    gpu_device_count: 1,
    ..Default::default()
  };
  app.models = vec![
    fake_model("/m/x/qwen-7b.gguf", "/m/x", "qwen3", 32_768, 4_500_000_000),
    fake_model(
      "/m/x/mistral-7b.gguf",
      "/m/x",
      "llama",
      8_192,
      4_300_000_000,
    ),
    fake_model("/m/y/phi-3.gguf", "/m/y", "phi", 8_192, 4_200_000_000),
  ];
  // Two favourites: qwen-7b is currently running (so it lives in
  // the Running group, not Favorites — running paths drop out of
  // the catalog groupings entirely), mistral-7b is not running so
  // it shows up in Favorites **and** in its `/m/x` folder group
  // (favorited paths stay in their original folder; the Favorites
  // section is an extra shortcut, not a relocation). The golden
  // therefore exercises Running + Favorites + Divider + folder
  // groups in one frame.
  app.favorites = vec![
    PathBuf::from("/m/x/qwen-7b.gguf"),
    PathBuf::from("/m/x/mistral-7b.gguf"),
  ];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/x/qwen-7b.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
    device: None,
    rss_bytes: Some(4_500_000_000),
    cpu_pct: Some(312.0),
    ..Default::default()
  }];
  // Park the cursor on the Running launch row so the right pane
  // header carries live launch metadata (port / state / RAM / CPU).
  // Row 0 is the table header, row 1 is the `▶ Running` group,
  // row 2 is the running qwen-7b launch.
  app.list_cursor = 2;
  app
}

fn render_to_lines(app: &mut App) -> Vec<String> {
  let backend = TestBackend::new(WIDTH, HEIGHT);
  let mut terminal = Terminal::new(backend).expect("test terminal");
  terminal.draw(|f| render(f, app)).expect("render");
  let buf = terminal.backend().buffer();
  let mut rows: Vec<String> = Vec::with_capacity(HEIGHT as usize);
  for y in 0..buf.area.height {
    let mut row = String::with_capacity(WIDTH as usize);
    for x in 0..buf.area.width {
      row.push_str(buf[(x, y)].symbol());
    }
    rows.push(row.trim_end().to_string());
  }
  rows
}

// Row 0 is the global title + hint strip. It carries two things that
// churn independently of the structural layout this golden test is here
// to defend: the live `CARGO_PKG_VERSION` (changes every release and
// pre-release) and the help_bar hint glyph set (gets rebalanced when
// any visible action is renamed or rebound). The `dashboard_render_
// carries_key_landmarks` sibling test already asserts the structural
// contract for that row — brand, daemon-connected dot, `?:help`,
// `q:quit`. So we deliberately exclude it from the golden compare to
// keep the test stable across version bumps and hint-strip tweaks.
const SKIP_ROWS: &[usize] = &[0];

/// Render `app`, then compare it line-by-line against the golden at
/// `rel_path` (or rewrite the fixture when `UPDATE_GOLDEN=1`). Row 0
/// (`SKIP_ROWS`) is excluded — it carries the live `CARGO_PKG_VERSION`
/// and the platform-specific key glyphs, both of which churn
/// independently of the structural layout these goldens defend.
fn assert_golden(app: &mut App, rel_path: &str) {
  let rendered = render_to_lines(app).join("\n") + "\n";
  let manifest = env!("CARGO_MANIFEST_DIR");
  let fixture_path = std::path::Path::new(manifest).join(rel_path);

  if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
    if let Some(parent) = fixture_path.parent() {
      std::fs::create_dir_all(parent).expect("create golden dir");
    }
    std::fs::write(&fixture_path, &rendered).expect("write golden");
    eprintln!("UPDATE_GOLDEN=1: wrote {}", fixture_path.display());
    return;
  }

  let expected = std::fs::read_to_string(&fixture_path).unwrap_or_else(|_| {
    panic!(
      "golden fixture missing at {} — run `UPDATE_GOLDEN=1 cargo test \
       --test tui_e2e_render_test` to create it",
      fixture_path.display()
    )
  });

  // Diff line-by-line so the first mismatch points at the row.
  let actual_lines: Vec<&str> = rendered.lines().collect();
  let expected_lines: Vec<&str> = expected.lines().collect();
  assert_eq!(
    actual_lines.len(),
    expected_lines.len(),
    "row count diverged for {rel_path}: actual={} expected={}\n--- actual ---\n{}\n--- expected ---\n{}",
    actual_lines.len(),
    expected_lines.len(),
    rendered,
    expected
  );
  for (i, (a, e)) in actual_lines.iter().zip(expected_lines.iter()).enumerate() {
    if SKIP_ROWS.contains(&i) {
      continue;
    }
    assert_eq!(
      a, e,
      "row {i} diverged in {rel_path}\n  actual:   {a:?}\n  expected: {e:?}\nFull frame:\n{rendered}"
    );
  }
}

// Every golden below is captured on Linux and pins the PC-style key
// glyphs (`Ctrl+s`, `↹`, `⇧↹`, `⏎`). On macOS the renderer emits the
// native Apple glyphs with different display widths, so chips reflow
// and trailing `─` padding diverges. The structural sibling tests run
// on every platform; only the character-exact compares are skipped on
// macOS.
#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn dashboard_golden_render_matches_fixture() {
  assert_golden(&mut seeded_dashboard_app(), GOLDEN_PATH);
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn hf_search_golden_render_matches_fixture() {
  assert_golden(&mut seeded_hf_search_app(), "tests/golden/hf-search.txt");
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn hf_files_golden_render_matches_fixture() {
  assert_golden(&mut seeded_hf_files_app(), "tests/golden/hf-files.txt");
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn hf_confirm_golden_render_matches_fixture() {
  assert_golden(&mut seeded_hf_confirm_app(), "tests/golden/hf-confirm.txt");
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn logs_view_golden_render_matches_fixture() {
  assert_golden(&mut seeded_logs_view_app(), "tests/golden/logs-view.txt");
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn chat_view_golden_render_matches_fixture() {
  assert_golden(&mut seeded_chat_view_app(), "tests/golden/chat-view.txt");
}

/// A search result with deterministic counts/sizes so the rendered
/// `downloads` / `params` / `size` columns are stable.
fn fake_hf_result(repo_id: &str, downloads: u64, file_size: u64, params: u64) -> HfSearchResult {
  HfSearchResult {
    repo_id: repo_id.into(),
    downloads: Some(downloads),
    likes: Some(128),
    last_modified: Some("2026-04-18T12:00:00Z".into()),
    pipeline_tag: Some("text-generation".into()),
    tags: vec!["gguf".into()],
    gguf: Some(HfGgufMeta {
      total: Some(params),
      total_file_size: Some(file_size),
    }),
  }
}

/// Dashboard + HF dialog parked on the Search stage with a query and
/// two result rows.
fn seeded_hf_search_app() -> App {
  let mut app = seeded_dashboard_app();
  let mut state = HfDialogState::open(false, HardwareFitContext::default());
  state.input = InputField::with_text("qwen");
  state.results = vec![
    fake_hf_result(
      "Qwen/Qwen3-7B-GGUF",
      1_234_567,
      5_732_991_008,
      7_600_000_000,
    ),
    fake_hf_result(
      "bartowski/Qwen3-4B-GGUF",
      987_654,
      2_900_000_000,
      4_000_000_000,
    ),
  ];
  app.hf_dialog = Some(state);
  app
}

/// HF dialog on the File picker stage with two collapsed quant rows.
fn seeded_hf_files_app() -> App {
  let mut app = seeded_dashboard_app();
  let mut state = HfDialogState::open(false, HardwareFitContext::default());
  // The query that drilled into this repo persists (resting, not
  // editing) — otherwise the search line shows the empty-edit
  // placeholder, which misreads as an active search.
  state.input = InputField::with_text("qwen");
  state.stage = HfStage::FilePicker;
  state.picker_repo_id = Some("Qwen/Qwen3-7B-GGUF".into());
  state.picker_load = PickerLoad::Ready;
  state.picker_rows = vec![
    PickerRow::Single {
      filename: "qwen3-7b-q4_k_m.gguf".into(),
      size_bytes: Some(4_500_000_000),
    },
    PickerRow::Single {
      filename: "qwen3-7b-q8_0.gguf".into(),
      size_bytes: Some(8_000_000_000),
    },
  ];
  app.hf_dialog = Some(state);
  app
}

/// HF dialog on the Confirm stage with a chosen file.
fn seeded_hf_confirm_app() -> App {
  let mut app = seeded_dashboard_app();
  let mut state = HfDialogState::open(false, HardwareFitContext::default());
  // Carry the resting query through, same as the File picker fixture.
  state.input = InputField::with_text("qwen");
  state.stage = HfStage::Confirm;
  state.picker_repo_id = Some("Qwen/Qwen3-7B-GGUF".into());
  state.confirm_row = Some(PickerRow::Single {
    filename: "qwen3-7b-q4_k_m.gguf".into(),
    size_bytes: Some(4_500_000_000),
  });
  app.hf_dialog = Some(state);
  app
}

/// Dashboard with the right pane focused on the Logs tab of the
/// running launch, with a few log lines.
fn seeded_logs_view_app() -> App {
  let mut app = seeded_dashboard_app();
  app.focus = Focus::RightPane;
  app.right_tab = RightTab::Logs;
  app.logs_state.lines = vec![
    "llama_model_loader: loaded meta data with 26 key-value pairs".into(),
    "load_tensors: offloading 28 repeating layers to GPU".into(),
    "main: server listening on 127.0.0.1:41100".into(),
    "srv  update_slots: all slots are idle".into(),
  ];
  app
}

/// Dashboard with the right pane focused on the Chat tab, with a
/// prompt and a response.
fn seeded_chat_view_app() -> App {
  let mut app = seeded_dashboard_app();
  app.focus = Focus::RightPane;
  app.right_tab = RightTab::Chat;
  app.chat.prompt = InputField::with_text("Explain GGUF in one sentence");
  app.chat.response =
    "GGUF is a single-file container for quantized model weights plus metadata.".into();
  app
}

#[test]
fn dashboard_render_carries_key_landmarks() {
  // Independent of the golden snapshot: a few structural assertions
  // so accidental wholesale-fixture refreshes don't mask regressions.
  let mut app = seeded_dashboard_app();
  let lines = render_to_lines(&mut app);
  let frame = lines.join("\n");

  // Title row: brand + version + connected daemon dot.
  assert!(frame.contains("LlamaStash"), "brand missing: {frame}");
  assert!(
    frame.contains("daemon") && !frame.contains("daemon connecting"),
    "connected daemon label expected: {frame}"
  );
  // Global hint strip.
  assert!(frame.contains("?:help"));
  assert!(frame.contains("q:quit"));
  // Info row: Host pane shows CPU/RAM/GPU/VRAM + NVML backend tag.
  assert!(frame.contains("CPU"));
  assert!(frame.contains("RAM"));
  assert!(frame.contains("GPU"));
  assert!(frame.contains("VRAM"));
  assert!(frame.contains("NVIDIA"));
  // Daemon pane: server path + always-on proxy row. The build
  // version surfaces on the title bar (`render_title_left`) and is
  // no longer repeated on the info pane.
  assert!(frame.contains("llama-server"));
  assert!(
    frame.contains("proxy"),
    "proxy row missing from Daemon info pane: {frame}"
  );
  // Logo pane (visible at width 120).
  assert!(frame.contains("macchiato"));
  // Models pane: section headers + per-row badges. Running has a
  // launch for qwen-7b; mistral-7b appears in both Favorites and
  // the /m/x folder group (favorited paths stay in their folder);
  // phi-3 lands in its /m/y folder group.
  assert!(
    frame.contains("▶ Running"),
    "Running header missing: {frame}"
  );
  assert!(
    frame.contains("★ Favorites"),
    "Favorites header missing: {frame}"
  );
  assert!(frame.contains("qwen-7b"));
  assert!(frame.contains("mistral-7b"));
  assert!(frame.contains("phi-3"));
  // Folder headers reappear once Favorites no longer hides them.
  // `friendly_group_label` collapses the parent path to its trailing
  // segments, so `/m/x` renders as `m/x` in the section header.
  assert!(frame.contains("m/x"), "m/x folder header missing: {frame}");
  assert!(frame.contains("m/y"), "m/y folder header missing: {frame}");
  // The horizontal rule between Favorites and the folder sections
  // is painted with `─` (U+2500) — assert at least one full-width
  // run sits between the Favorites header and the m/x header.
  let fav_at = frame.find("★ Favorites").expect("Favorites header");
  // Search for the section header `m/x` after the Favorites position
  // — the right-pane path row also surfaces `m/x` as a substring, so
  // a bare `find` could resolve to that earlier match.
  let mx_at = fav_at
    + frame[fav_at..]
      .find("m/x")
      .expect("m/x header after Favorites");
  let between = &frame[fav_at..mx_at];
  assert!(
    between.contains("──────"),
    "Divider rule must sit between Favorites and folder groups: {between:?}"
  );
  // Right pane: focused-model header carries the launch metadata.
  assert!(frame.contains(":41100"));
  assert!(frame.contains("ready"));
  // Right pane stats column.
  assert!(frame.contains("RAM ·"));
  assert!(frame.contains("CPU"));
}
