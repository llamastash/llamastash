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
    backend_servers: vec![llamastash::tui::app::BackendServers {
      id: "llamacpp".into(),
      labels: vec!["cuda".into()],
    }],
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
  // The opening tab of a launch is Requests (pinned by the `tui::app`
  // tests); this frame pins the read-only launch-details layout, which
  // is what `named-launch` and the Settings hint ride on.
  app.right_tab = RightTab::Settings;
  app
}

fn render_to_lines(app: &mut App) -> Vec<String> {
  render_to_lines_at(app, WIDTH, HEIGHT)
}

fn render_to_lines_at(app: &mut App, width: u16, height: u16) -> Vec<String> {
  let backend = TestBackend::new(width, height);
  let mut terminal = Terminal::new(backend).expect("test terminal");
  terminal.draw(|f| render(f, app)).expect("render");
  let buf = terminal.backend().buffer();
  let mut rows: Vec<String> = Vec::with_capacity(height as usize);
  for y in 0..buf.area.height {
    let mut row = String::with_capacity(width as usize);
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
  assert_golden_text(render_to_lines(app).join("\n") + "\n", rel_path);
}

/// [`assert_golden`] for a frame that is already rendered.
fn assert_golden_text(rendered: String, rel_path: &str) {
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
fn hf_search_empty_golden_render_matches_fixture() {
  assert_golden(
    &mut seeded_hf_empty_search_app(),
    "tests/golden/hf-search-empty.txt",
  );
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn hf_search_empty_narrow_golden_render_matches_fixture() {
  // The narrowest frame that renders at all, pinned so the `~` explanation in
  // the note above the list cannot grow past what two wrapped lines hold.
  let rendered = render_to_lines_at(&mut seeded_hf_empty_search_app(), 60, HEIGHT).join("\n");
  assert_golden_text(rendered + "\n", "tests/golden/hf-search-empty-narrow.txt");
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

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn requests_view_golden_render_matches_fixture() {
  // The Time column is local wall-clock time, which depends on the
  // machine's zone, so the fixture holds `HH:MM:SS` in its place.
  let rendered = mask_clock_times(&render_to_lines(&mut seeded_requests_view_app()).join("\n"));
  assert_golden_text(rendered + "\n", "tests/golden/requests-view.txt");
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn requests_view_wide_golden_render_matches_fixture() {
  // The wide frame is the only golden that shows `Request for`, `Client` and
  // `Note`, and the only one where a long note is cut to its first words with
  // an ellipsis instead of being dropped with the column.
  let mut app = seeded_requests_view_app();
  let rendered = mask_clock_times(&render_to_lines_at(&mut app, 400, 30).join("\n"));
  assert_golden_text(rendered + "\n", "tests/golden/requests-view-wide.txt");
}

/// `text` with every `dd:dd:dd` clock time replaced by `HH:MM:SS`.
fn mask_clock_times(text: &str) -> String {
  const MASK: &str = "HH:MM:SS";
  let mut chars: Vec<char> = text.chars().collect();
  let is_time = |w: &[char]| {
    w.iter().zip(MASK.chars()).all(|(c, m)| match m {
      ':' => *c == ':',
      _ => c.is_ascii_digit(),
    })
  };
  let mut i = 0;
  while i + MASK.len() <= chars.len() {
    if is_time(&chars[i..i + MASK.len()]) {
      chars.splice(i..i + MASK.len(), MASK.chars());
      i += MASK.len();
    } else {
      i += 1;
    }
  }
  chars.into_iter().collect()
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn named_launch_golden_render_matches_fixture() {
  assert_golden(
    &mut seeded_named_launch_app(),
    "tests/golden/named-launch.txt",
  );
}

#[cfg_attr(target_os = "macos", ignore = "fixture uses Linux key glyphs")]
#[test]
fn launch_picker_golden_render_matches_fixture() {
  assert_golden(
    &mut seeded_launch_picker_app(),
    "tests/golden/launch-picker.txt",
  );
}

/// The running launch carries a name, so the list row reads
/// `qwen-7b.gguf@coder` and the Settings running view carries the `name` row —
/// the two places the address shows up in the TUI.
fn seeded_named_launch_app() -> App {
  let mut app = seeded_dashboard_app();
  app.managed[0].name = Some("coder".into());
  app.right_tab = RightTab::Settings;
  app
}

/// An idle model with the launch picker staged: the surface that owns
/// "launch as…", so the hint chip has to be in this footer and nowhere else.
fn seeded_launch_picker_app() -> App {
  let mut app = seeded_dashboard_app();
  // Row 4 is the idle mistral-7b under Favorites (row 3 is the group header).
  app.list_cursor = 4;
  app.drill_into_focused_model();
  app
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

/// A four-row catalog written into the test, so the fixture pins the row
/// rendering rather than the shipped catalog's contents: a snapshot regen must
/// not fail this golden. The rows cover the shapes a recommendation row takes —
/// one that fits the GPU, one that runs tight, a mixture-of-experts pick whose
/// experts sit in system RAM, and one whose publisher is `synthetic`, whose size
/// is an estimate the File picker can contradict.
fn recommendation_fixture_snapshot() -> llamastash::init::benchmark::BenchmarkSnapshot {
  let catalog = r#"{
    "schema_version": 1,
    "bundle_date": "2026-01-01",
    "min_version": "0.0.1",
    "recommender_weights": {
      "benchmark": 0.6,
      "tok_per_second": 0.15,
      "param_quality": 0.15,
      "recency": 0.1,
      "overhead_band_bytes": { "cuda": 1073741824 }
    },
    "models": [
      { "id": "fixture-small-q4-k-m", "repo": "fixture/Tiny-Instruct-GGUF",
        "file": "tiny-q4.gguf", "architecture": "llama", "quant": "Q4_K_M",
        "params": 7000000000, "weights_bytes": 4294967296,
        "task_hints": ["general"],
        "benchmark_score": { "value": 61.5, "source": "fixturebench" },
        "tok_s_factor": 2.4, "recency": 0.9, "source_hf_id": "fixture/Tiny-Instruct",
        "gguf_publisher": "fixture" },
      { "id": "fixture-roomy-q6-k", "repo": "fixture/Mid-Instruct-GGUF",
        "file": "mid-q6.gguf", "architecture": "llama", "quant": "Q6_K",
        "params": 27000000000, "weights_bytes": 17179869184,
        "task_hints": ["general"],
        "benchmark_score": { "value": 72.25, "source": "fixturebench" },
        "tok_s_factor": 1.1, "recency": 1.0, "source_hf_id": "fixture/Mid-Instruct",
        "gguf_publisher": "fixture" },
      { "id": "fixture-sparse-q3-k-m", "repo": "fixture/Vast-Sparse-Instruct-GGUF",
        "file": "vast-q3.gguf", "architecture": "qwen3next", "quant": "Q3_K_M",
        "params": 80000000000, "weights_bytes": 36507222016,
        "task_hints": ["general"],
        "benchmark_score": { "value": 68.0, "source": "fixturebench" },
        "tok_s_factor": 1.8, "recency": 1.0, "source_hf_id": "fixture/Vast-Sparse",
        "params_active": 3000000000, "is_moe": true, "gguf_publisher": "fixture" },
      { "id": "fixture-synthetic-q4-k-m", "repo": "fixture/Upstream-Instruct",
        "file": "upstream-q4.gguf", "architecture": "llama", "quant": "Q4_K_M",
        "params": 4000000000, "weights_bytes": 2254857830,
        "task_hints": ["general"],
        "benchmark_score": { "value": 55.0, "source": "fixturebench" },
        "tok_s_factor": 3.0, "recency": 0.8, "source_hf_id": "fixture/Upstream-Instruct",
        "gguf_publisher": "synthetic" }
    ]
  }"#;
  serde_json::from_str(catalog).expect("the golden fixture's catalog parses")
}

/// Dashboard + HF dialog parked on the Search stage with an empty query: the
/// recommender's ranked picks for a fixed 24 GiB CUDA host out of the catalog
/// above, so the fixture pins the list and the note over it without depending
/// on the host it runs on or on the shipped catalog.
fn seeded_hf_empty_search_app() -> App {
  let mut app = seeded_dashboard_app();
  let ctx = HardwareFitContext {
    backend: "cuda".into(),
    vram_bytes: Some(24 * 1024 * 1024 * 1024),
    ram_total_bytes: 32 * 1024 * 1024 * 1024,
    ctx_tokens: llamastash::init::recommender::DEFAULT_CTX,
    ..Default::default()
  };
  let snapshot = recommendation_fixture_snapshot();
  let mut state = HfDialogState::open(false, ctx.clone());
  state.recs = state.hardware_fit_ctx.recommendations(&snapshot);
  assert_eq!(
    state.recs.len(),
    4,
    "the fixture catalog must rank all four rows, or the golden pins nothing",
  );
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

/// Dashboard with the right pane focused on the Requests tab of the
/// running launch: a served request, one that auto-started the model and
/// unloaded another for it, a refused auto-start, a client that hung up,
/// a fallback that answered for another model, and two still streaming
/// from two peers of one host.
fn seeded_requests_view_app() -> App {
  use llamastash::proxy::request_log::{RequestRow, RequestState, RequestSummary};
  use llamastash::tui::tabs::requests::RequestsScope;
  let mut app = seeded_dashboard_app();
  app.focus = Focus::RightPane;
  app.right_tab = RightTab::Requests;
  let row = |seq: u64| RequestRow {
    seq,
    // 2023-11-14T22:13:20Z plus one minute per row.
    started_at_ms: 1_700_000_000_000 + seq * 60_000,
    client: Some("127.0.0.1:53412".into()),
    route: "/v1/chat/completions".into(),
    requested_model: Some("qwen-7b".into()),
    model: Some("qwen-7b".into()),
    model_path: Some("/m/x/qwen-7b.gguf".into()),
    launch_id: Some("L1".into()),
    state: RequestState::Done,
    status: Some(200),
    ttfb_ms: Some(182),
    duration_ms: Some(4_310),
    prompt_tokens: Some(18_432),
    completion_tokens: Some(164),
    tokens_per_second: Some(41.27),
    prefill_tokens: Some(4_096),
    prefill_tokens_per_second: Some(612.4),
    cached_tokens: Some(14_336),
    cache_hit_ratio: Some(14_336.0 / 18_432.0),
    ..RequestRow::default()
  };
  let rows = vec![
    RequestRow {
      // The engine sent no prefill timing here, so the row carries the proxy's
      // own estimate.
      prefill_estimated: true,
      prefill_tokens_per_second: Some(96.5),
      fallback: Some("launch_failed".into()),
      requested_model: Some("qwen-70b".into()),
      client: Some("127.0.0.1:53477".into()),
      ..row(7)
    },
    RequestRow {
      state: RequestState::InFlight,
      duration_ms: None,
      prompt_tokens: None,
      completion_tokens: None,
      tokens_per_second: None,
      prefill_tokens: None,
      prefill_tokens_per_second: None,
      cached_tokens: None,
      cache_hit_ratio: None,
      ..row(5)
    },
    RequestRow {
      state: RequestState::InFlight,
      client: Some("127.0.0.1:53498".into()),
      duration_ms: None,
      prompt_tokens: None,
      completion_tokens: None,
      tokens_per_second: None,
      prefill_tokens: None,
      prefill_tokens_per_second: None,
      cached_tokens: None,
      cache_hit_ratio: None,
      ..row(6)
    },
    RequestRow {
      state: RequestState::ClientClosed,
      prompt_tokens: None,
      completion_tokens: None,
      tokens_per_second: None,
      prefill_tokens: None,
      prefill_tokens_per_second: None,
      cached_tokens: None,
      cache_hit_ratio: None,
      route: "/v1/messages".into(),
      ..row(4)
    },
    RequestRow {
      client: Some("192.168.1.42:51820".into()),
      status: Some(503),
      launch_id: None,
      auto_start: true,
      error: Some("launch_failed".into()),
      cause: Some(
        "compose_and_spawn: not enough memory to map the KV cache at this \
         context; unload a model or lower --ctx-size"
          .into(),
      ),
      ttfb_ms: None,
      duration_ms: Some(96),
      prompt_tokens: None,
      completion_tokens: None,
      tokens_per_second: None,
      prefill_tokens: None,
      prefill_tokens_per_second: None,
      cached_tokens: None,
      cache_hit_ratio: None,
      ..row(3)
    },
    RequestRow {
      auto_start: true,
      evicted: vec!["L7".into()],
      ttfb_ms: Some(31_870),
      duration_ms: Some(36_020),
      ..row(2)
    },
    row(1),
  ];
  app.requests.set(
    RequestsScope::new("L1"),
    RequestSummary {
      requests: 6,
      errors: 1,
      // Two hosts, four peers; both live requests share one host but not
      // one peer, so the live pair prints as `1/2`.
      hosts: Some(2),
      connections: Some(4),
      live_hosts: Some(1),
      live_connections: Some(2),
      avg_duration_ms: Some(20_165),
      avg_ttfb_ms: Some(16_026),
      tokens_per_second_avg: Some(41.27),
      // 328 tokens over 6.3s of generation: the two streams overlapped, so
      // the box ran faster than any one request.
      tokens_per_second_aggregate: Some(52.4),
      busy_ms: Some(6_259),
      tokens_per_second_session: Some(48.9),
      tokens_per_second_last: Some(41.27),
      prompt_tokens: 36_864,
      prefill_tokens_per_second: Some(1042.6),
      // Two prompts computed at once: the box prefilled faster than either.
      prefill_tokens_per_second_aggregate: Some(1_800.0),
      prefill_busy_ms: Some(3_700),
      cached_tokens: 33_472,
      cache_hit_ratio: Some(0.908),
      completion_tokens: 328,
      auto_starts: 2,
      evictions: 1,
    },
    rows,
  );
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
  // Daemon pane: server row + always-on proxy row. The build
  // version surfaces on the title bar (`render_title_left`) and is
  // no longer repeated on the info pane.
  assert!(frame.contains("llamacpp (cuda)"));
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
