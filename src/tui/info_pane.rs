//! Top-middle info-row pane: daemon endpoint, build, llama-server,
//! discovery counters, and a one-line running summary.
//!
//! Five label-prefixed rows. Long paths left-truncate with `…/`. The
//! `running` line collapses to `—` when nothing is supervised. Width
//! is flexible — this panel takes whatever's between Host (fixed 32)
//! and Logo (fixed 25 when present).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::status_icons::label_for;
use crate::util::paths::model_display_name;

const LABEL_WIDTH: usize = 8;
const LABEL_SOCKET: &str = "socket  ";
const LABEL_UPTIME: &str = "uptime  ";
const LABEL_SERVER: &str = "server  ";
const LABEL_COUNTS: &str = "counts  ";
const LABEL_RUNNING: &str = "running ";

/// Render the Daemon info panel into `area`. The block title is
/// `Daemon`; inner content is five label-prefixed rows.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let block = Block::default()
    .title(" Daemon ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let row_budget = inner.width.saturating_sub(LABEL_WIDTH as u16) as usize;
  let lines: Vec<Line<'_>> = vec![
    socket_row(app, row_budget, palette),
    uptime_build_row(app, palette),
    server_row(app, row_budget, palette),
    counts_row(app, palette),
    running_row(app, row_budget, palette),
  ];
  frame.render_widget(Paragraph::new(lines), inner);
}

fn socket_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  // Layout: `socket  …/daemon.sock  pid 1234`. The pid is fixed-width
  // (small int + label), so allocate its width first and let the
  // socket path consume the rest with `…/` left-truncation. When the
  // daemon doesn't surface a socket path (older builds), fall back
  // to `pid` alone so the row still carries useful identity.
  let pid_chunk = app
    .daemon_info
    .pid
    .map(|pid| format!("pid {pid}"))
    .unwrap_or_else(|| "—".into());
  let value = match app.daemon_info.socket_path.as_deref() {
    Some(path) => {
      // Reserve `  pid 1234` worth of width (two-space separator +
      // pid chunk); truncate the path with the remainder. When the
      // remaining path budget is too small to render a recognisable
      // socket path (less than ~12 cols — enough for `…/daemon.sock`),
      // drop the path entirely and just show the pid. A one- or
      // two-character socket prefix is worse than no path at all.
      const MIN_PATH_BUDGET: usize = 12;
      let separator = "  ";
      let reserved = pid_chunk.width() + separator.width();
      let path_budget = budget.saturating_sub(reserved);
      if path_budget < MIN_PATH_BUDGET {
        ellipsise(&pid_chunk, budget)
      } else {
        let path_truncated = ellipsise(path, path_budget);
        format!("{path_truncated}{separator}{pid_chunk}")
      }
    }
    None => ellipsise(&pid_chunk, budget),
  };
  Line::from(vec![
    Span::styled(LABEL_SOCKET, Style::default().fg(palette.muted)),
    Span::styled(value, Style::default().fg(palette.fg)),
  ])
}

fn uptime_build_row<'a>(app: &'a App, palette: &'a Palette) -> Line<'a> {
  let uptime = match (app.daemon_connected, app.daemon_info.uptime_seconds) {
    (true, Some(secs)) => format_uptime(secs),
    _ => "—".into(),
  };
  let build = app
    .daemon_info
    .build
    .clone()
    .map(|v| format!("v{v}"))
    .unwrap_or_else(|| "—".into());
  Line::from(vec![
    Span::styled(LABEL_UPTIME, Style::default().fg(palette.muted)),
    Span::styled(uptime, Style::default().fg(palette.fg)),
    Span::styled("   build  ", Style::default().fg(palette.muted)),
    Span::styled(build, Style::default().fg(palette.fg)),
  ])
}

fn server_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  // When the daemon hasn't resolved a `llama-server` binary, show
  // `—` (matching the uptime row's missing-data convention) and
  // drop the backend-flavor tag entirely — `— (rocm)` reads as a
  // bug rather than a missing-binary signal. The flavor only makes
  // sense paired with an actual binary path.
  let server_path = app.daemon_info.server_path.as_deref();
  let (path, flavor_chunk) = match server_path {
    Some(p) => {
      let flavor = flavor_label(app);
      let chunk = match flavor {
        Some(f) => format!(" ({f})"),
        None => String::new(),
      };
      (p, chunk)
    }
    None => ("—", String::new()),
  };
  // Reserve room for the parenthesised flavor; truncate the path
  // first so the flavor stays visible. The flavor's column width is
  // measured (not char-counted) so multi-byte glyphs don't desync
  // the budget from the rendered cell count.
  let path_budget = if flavor_chunk.is_empty() {
    budget
  } else {
    budget.saturating_sub(flavor_chunk.width())
  };
  let path_truncated = ellipsise(path, path_budget);
  Line::from(vec![
    Span::styled(LABEL_SERVER, Style::default().fg(palette.muted)),
    Span::styled(path_truncated, Style::default().fg(palette.fg)),
    Span::styled(flavor_chunk, Style::default().fg(palette.muted)),
  ])
}

fn counts_row<'a>(app: &'a App, palette: &'a Palette) -> Line<'a> {
  let total = app.models.len();
  if total == 0 {
    return Line::from(vec![
      Span::styled(LABEL_COUNTS, Style::default().fg(palette.muted)),
      Span::styled("no models found", Style::default().fg(palette.muted)),
    ]);
  }
  let ready = app
    .managed
    .iter()
    .filter(|m| matches!(m.state, crate::tui::status_icons::SurfaceState::Ready))
    .count();
  let favorites = app.favorites.len();
  Line::from(vec![
    Span::styled(LABEL_COUNTS, Style::default().fg(palette.muted)),
    Span::styled(format!("{total} found"), Style::default().fg(palette.fg)),
    Span::styled(" · ", Style::default().fg(palette.muted)),
    Span::styled(format!("{ready} ready"), Style::default().fg(palette.fg)),
    Span::styled(" · ", Style::default().fg(palette.muted)),
    Span::styled(
      format!("{favorites} ★"),
      Style::default().fg(palette.warning),
    ),
  ])
}

fn running_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  if app.managed.is_empty() {
    return Line::from(vec![
      Span::styled(LABEL_RUNNING, Style::default().fg(palette.muted)),
      Span::styled("—", Style::default().fg(palette.muted)),
    ]);
  }
  let first = &app.managed[0];
  let name = model_display_name(&first.path);
  let head = format!(
    "{} :{} {}",
    name,
    first.port,
    label_for(first.state).to_ascii_lowercase()
  );
  let suffix = if app.managed.len() > 1 {
    format!("  +{} more", app.managed.len() - 1)
  } else {
    String::new()
  };
  // Truncate the head to make room for the suffix. Width-based so
  // CJK model names don't push `+N more` off-screen.
  let head_budget = budget.saturating_sub(suffix.width());
  let head_truncated = ellipsise(&head, head_budget);
  Line::from(vec![
    Span::styled(LABEL_RUNNING, Style::default().fg(palette.muted)),
    Span::styled(head_truncated, Style::default().fg(palette.fg)),
    Span::styled(suffix, Style::default().fg(palette.muted)),
  ])
}

/// Backend → human-readable flavor tag used in the `server` row.
fn flavor_label(app: &App) -> Option<&'static str> {
  match app.host_metrics.gpu_backend.as_str() {
    "nvidia" => Some("cuda"),
    "amd" => Some("rocm"),
    "apple_metal" => Some("metal"),
    "cpu_only" => Some("cpu"),
    _ => None,
  }
}

/// Truncate `s` to fit `budget` terminal columns from the **left**,
/// prepending `…/` so the trailing component (the binary name, the
/// launch id) stays visible. Returns the original string unmodified
/// when it already fits.
///
/// Measures in unicode display width (`UnicodeWidthStr`), not char
/// count: a CJK character occupies two cells, so on a CJK install
/// prefix like `/Users/张伟/.../llama-server` char-count truncation
/// would overflow the reserved budget and push the trailing flavor
/// chunk off-screen.
fn ellipsise(s: &str, budget: usize) -> String {
  if budget == 0 {
    return String::new();
  }
  if s.width() <= budget {
    return s.to_string();
  }
  let prefix = "…/";
  let prefix_w = prefix.width();
  if budget <= prefix_w {
    return take_tail_by_width(s, budget);
  }
  let keep_budget = budget - prefix_w;
  let tail = take_tail_by_width(s, keep_budget);
  format!("{prefix}{tail}")
}

/// Take the longest suffix of `s` whose display width is `<= budget`.
/// Iterates chars in reverse so a wide character that wouldn't fit is
/// dropped rather than splitting it.
fn take_tail_by_width(s: &str, budget: usize) -> String {
  let mut acc_w = 0usize;
  let mut start = s.len();
  for (idx, ch) in s.char_indices().rev() {
    let w = ch.to_string().width();
    if acc_w + w > budget {
      break;
    }
    acc_w += w;
    start = idx;
  }
  s[start..].to_string()
}

fn format_uptime(secs: u64) -> String {
  let hours = secs / 3600;
  let minutes = (secs % 3600) / 60;
  if hours > 0 {
    format!("{hours}h{minutes:02}m")
  } else {
    format!("{minutes}m")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::daemon::host_metrics::HostMetricsSnapshot;
  use crate::discovery::{DiscoveredModel, ModelSource};
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
  use crate::tui::app::{App, AppOptions, DaemonInfo, ManagedRow};
  use crate::tui::status_icons::SurfaceState;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;
  use std::path::PathBuf;

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

  fn fake_managed(path: &str, port: u16, state: SurfaceState) -> ManagedRow {
    ManagedRow {
      launch_id: "L".into(),
      path: PathBuf::from(path),
      port,
      state,
      rss_bytes: Some(4_200_000_000),
      cpu_pct: Some(312.0),
    }
  }

  fn render_lines(app: &App) -> Vec<String> {
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(50, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 50, 7), app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area.height {
      let mut row = String::new();
      for x in 0..buf.area.width {
        row.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(row.trim_end().to_string());
    }
    rows
  }

  #[test]
  fn ellipsise_pads_short_strings_unchanged() {
    assert_eq!(ellipsise("ok", 10), "ok");
    assert_eq!(ellipsise("", 10), "");
  }

  #[test]
  fn ellipsise_left_truncates_long_paths() {
    let truncated = ellipsise("/usr/local/lib/llama-cpp-cuda/bin/llama-server", 20);
    assert!(truncated.starts_with("…/"));
    assert!(truncated.ends_with("llama-server"));
    assert!(truncated.width() <= 20);
  }

  #[test]
  fn ellipsise_measures_in_display_columns_not_char_count() {
    // CJK characters occupy two terminal cells each. A char-count
    // implementation would let `/张伟/llama-server` (15 chars, 17
    // cells) "fit" inside a 16-column budget; ratatui would then
    // clip the flavor chunk that the caller appends. Width-based
    // measurement keeps the truncated string inside the requested
    // budget so the trailing chunk renders intact.
    let s = "/usr/local/张伟/bin/llama-server";
    let out = ellipsise(s, 20);
    assert!(
      out.width() <= 20,
      "expected width <= 20, got {} for {out:?}",
      out.width()
    );
    assert!(out.ends_with("llama-server"));
  }

  #[test]
  fn format_uptime_omits_hours_when_zero() {
    assert_eq!(format_uptime(0), "0m");
    assert_eq!(format_uptime(45), "0m");
    assert_eq!(format_uptime(60), "1m");
    assert_eq!(format_uptime(3600), "1h00m");
    assert_eq!(format_uptime(3 * 3600 + 12 * 60), "3h12m");
  }

  #[test]
  fn counts_row_renders_no_models_message_when_empty() {
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    assert!(rows.iter().any(|r| r.contains("no models found")));
  }

  #[test]
  fn counts_row_renders_total_ready_favorites() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.managed = vec![fake_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.favorites = vec![PathBuf::from("/m/a.gguf")];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("1 found")),
      "counts row missing `1 found`: {rows:#?}"
    );
    assert!(rows.iter().any(|r| r.contains("1 ready")));
    assert!(rows.iter().any(|r| r.contains("1 ★")));
  }

  #[test]
  fn uptime_row_shows_em_dash_when_daemon_disconnected() {
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = false;
    app.daemon_info = DaemonInfo {
      uptime_seconds: Some(120),
      ..Default::default()
    };
    let rows = render_lines(&app);
    // The uptime field should be `—`, not `2m`, because the
    // connection went down — the cached value is stale.
    let uptime_row = rows.iter().find(|r| r.contains("uptime")).unwrap();
    assert!(
      uptime_row.contains("—"),
      "expected em-dash on uptime row when daemon disconnected, got: {uptime_row:?}"
    );
  }

  #[test]
  fn uptime_row_shows_uptime_when_daemon_connected() {
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_info = DaemonInfo {
      uptime_seconds: Some(3 * 3600 + 12 * 60),
      build: Some("0.1.0".into()),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let uptime_row = rows.iter().find(|r| r.contains("uptime")).unwrap();
    assert!(uptime_row.contains("3h12m"));
    assert!(uptime_row.contains("v0.1.0"));
  }

  #[test]
  fn running_row_renders_dash_when_zero_managed() {
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("—"),
      "expected em-dash on running row when none managed, got: {running_row:?}"
    );
  }

  #[test]
  fn running_row_renders_plus_n_more_for_extra_managed() {
    let mut app = App::new(AppOptions::default());
    app.managed = vec![
      fake_managed("/m/qwen.gguf", 41100, SurfaceState::Ready),
      fake_managed("/m/gemma.gguf", 41101, SurfaceState::Loading),
      fake_managed("/m/phi.gguf", 41102, SurfaceState::Ready),
    ];
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(running_row.contains("qwen"));
    assert!(running_row.contains(":41100"));
    assert!(running_row.contains("+2 more"));
  }

  #[test]
  fn server_row_emits_flavor_from_host_metrics_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("(cuda)"),
      "expected `(cuda)` flavor on nvidia backend, got: {server_row:?}"
    );
  }

  #[test]
  fn server_row_picks_rocm_flavor_for_amd_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "amd".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(server_row.contains("(rocm)"), "{server_row:?}");
  }

  #[test]
  fn server_row_picks_metal_flavor_for_apple_metal_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "apple_metal".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(server_row.contains("(metal)"), "{server_row:?}");
  }

  #[test]
  fn server_row_omits_flavor_chunk_for_unsampled_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "unsampled".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // No parenthesised flavor chunk should appear yet.
    assert!(
      !server_row.contains('('),
      "unsampled backend should suppress the flavor chunk: {server_row:?}"
    );
  }

  #[test]
  fn socket_row_renders_pid_when_daemon_info_carries_one() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let socket_row = rows.iter().find(|r| r.contains("socket")).unwrap();
    assert!(
      socket_row.contains("pid 1234"),
      "expected `pid 1234`, got: {socket_row:?}"
    );
  }

  #[test]
  fn socket_row_renders_path_alongside_pid_when_available() {
    // After wiring `daemon.socket_path` through the IPC contract,
    // the row leads with the absolute socket path so the user can
    // see which daemon they're talking to without an extra command.
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      pid: Some(4242),
      socket_path: Some("/run/user/1000/llamadash/daemon.sock".into()),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let socket_row = rows.iter().find(|r| r.contains("socket")).unwrap();
    assert!(
      socket_row.contains("daemon.sock"),
      "expected socket basename, got: {socket_row:?}"
    );
    assert!(
      socket_row.contains("pid 4242"),
      "pid must remain on the row alongside the path: {socket_row:?}"
    );
  }

  #[test]
  fn server_row_omits_flavor_when_binary_unresolved() {
    // Previously `— (rocm)` rendered when the daemon hadn't located
    // a `llama-server` binary, which read as a bug. The flavor tag
    // only makes sense paired with an actual binary path, so an
    // unresolved binary collapses the row to `—`.
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: None,
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "amd".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("—"),
      "expected em-dash placeholder when no binary, got: {server_row:?}"
    );
    assert!(
      !server_row.contains('('),
      "flavor must be suppressed when binary is unresolved: {server_row:?}"
    );
  }
}
