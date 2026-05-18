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

use llamadash::daemon::host_metrics::HostMetricsSnapshot;
use llamadash::discovery::{DiscoveredModel, ModelSource};
use llamadash::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamadash::theme::ThemeName;
use llamadash::tui::app::{App, AppOptions, DaemonInfo, ManagedRow};
use llamadash::tui::keybindings::KeyMap;
use llamadash::tui::render::render;
use llamadash::tui::status_icons::SurfaceState;
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
      native_ctx: Some(ctx),
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: None,
      mode_hint: ModeHint::Chat,
      weights_bytes: Some(weights),
    }),
    parse_error: None,
    split_siblings: Vec::new(),
  }
}

/// Build a fully-populated App fixture for the golden render.
fn seeded_dashboard_app() -> App {
  let mut app = App::new(AppOptions {
    theme: ThemeName::Macchiato,
    custom_palette: None,
    keymap: KeyMap::default(),
  });
  app.daemon_connected = true;
  app.daemon_info = DaemonInfo {
    pid: Some(4242),
    uptime_seconds: Some(3 * 3600 + 12 * 60 + 45),
    build: Some("0.1.0".into()),
    server_path: Some("/usr/local/bin/llama-server".into()),
    socket_path: Some("/run/user/1000/llamadash/daemon.sock".into()),
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
  // the catalog groupings), mistral-7b is not running so it shows
  // up in Favorites. The golden therefore exercises Running +
  // Favorites + folder groups in one frame.
  app.favorites = vec![
    PathBuf::from("/m/x/qwen-7b.gguf"),
    PathBuf::from("/m/x/mistral-7b.gguf"),
  ];
  app.managed = vec![ManagedRow {
    launch_id: "L1".into(),
    path: PathBuf::from("/m/x/qwen-7b.gguf"),
    port: 41100,
    state: SurfaceState::Ready,
    rss_bytes: Some(4_500_000_000),
    cpu_pct: Some(312.0),
  }];
  // Park the cursor on the Running launch row so the right pane
  // header carries live launch metadata (port / state / RAM / CPU).
  // Row 0 is the table header, row 1 is the `󰑐 Running` group,
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

#[test]
fn dashboard_golden_render_matches_fixture() {
  let mut app = seeded_dashboard_app();
  let lines = render_to_lines(&mut app);
  let rendered = lines.join("\n") + "\n";

  let manifest = env!("CARGO_MANIFEST_DIR");
  let fixture_path = std::path::Path::new(manifest).join(GOLDEN_PATH);

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
    "row count diverged: actual={} expected={}\n--- actual ---\n{}\n--- expected ---\n{}",
    actual_lines.len(),
    expected_lines.len(),
    rendered,
    expected
  );
  for (i, (a, e)) in actual_lines.iter().zip(expected_lines.iter()).enumerate() {
    assert_eq!(
      a, e,
      "row {i} diverged\n  actual:   {a:?}\n  expected: {e:?}\nFull frame:\n{rendered}"
    );
  }
}

#[test]
fn dashboard_render_carries_key_landmarks() {
  // Independent of the golden snapshot: a few structural assertions
  // so accidental wholesale-fixture refreshes don't mask regressions.
  let mut app = seeded_dashboard_app();
  let lines = render_to_lines(&mut app);
  let frame = lines.join("\n");

  // Title row: brand + version + connected daemon dot.
  assert!(frame.contains("LlamaDash"), "brand missing: {frame}");
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
  assert!(frame.contains("NVML"));
  // Daemon pane: build + server path.
  assert!(frame.contains("v0.1.0"));
  assert!(frame.contains("llama-server"));
  // Logo pane (visible at width 120).
  assert!(frame.contains("macchiato"));
  // Models pane: section headers + per-row badges. Running has a
  // launch for qwen-7b; mistral-7b is the only Favorite left after
  // running paths drop out; phi-3 lands in its folder group.
  assert!(
    frame.contains("󰑐 Running"),
    "Running header missing: {frame}"
  );
  assert!(
    frame.contains("★ Favorites"),
    "Favorites header missing: {frame}"
  );
  assert!(frame.contains("qwen-7b"));
  assert!(frame.contains("mistral-7b"));
  assert!(frame.contains("phi-3"));
  // Right pane: focused-model header carries the launch metadata.
  assert!(frame.contains(":41100"));
  assert!(frame.contains("ready"));
  // Right pane stats column.
  assert!(frame.contains("RAM ·"));
  assert!(frame.contains("CPU"));
}
