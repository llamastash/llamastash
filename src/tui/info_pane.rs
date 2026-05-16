//! Top-middle info-row pane: daemon endpoint, build, llama-server,
//! discovery counters, and a one-line running summary.
//!
//! Five label-prefixed rows. Long paths left-truncate with `…/`. The
//! `running` line collapses to `—` when nothing is supervised. Width
//! is flexible — this panel takes whatever's between Host (fixed 32)
//! and Logo (fixed 25 when present).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

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
  let mut lines: Vec<Line<'_>> = Vec::with_capacity(5);
  lines.push(socket_row(app, row_budget, palette));
  lines.push(uptime_build_row(app, palette));
  lines.push(server_row(app, row_budget, palette));
  lines.push(counts_row(app, palette));
  lines.push(running_row(app, row_budget, palette));
  frame.render_widget(Paragraph::new(lines), inner);
}

fn socket_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  let socket = app
    .daemon_info
    .pid
    .map(|pid| format!("pid {pid}"))
    .unwrap_or_else(|| "—".into());
  // We don't propagate the socket path through the status response
  // (the TUI already knows it), so the row leads with the pid the
  // daemon reports and leaves room for the path to be derived from
  // the TUI's connection state in a future pass.
  let value = ellipsise(&socket, budget);
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
  let path = app.daemon_info.server_path.as_deref().unwrap_or("—");
  // Backend label (cuda / amd / apple_metal / cpu_only) comes from the
  // host_metrics snapshot — the daemon's GPU probe is the source of
  // truth either way.
  let flavor = flavor_label(app);
  let flavor_chunk = match flavor {
    Some(f) => format!(" ({f})"),
    None => String::new(),
  };
  // Reserve room for the parenthesised flavor; truncate the path
  // first so the flavor stays visible.
  let path_budget = budget.saturating_sub(flavor_chunk.chars().count() + 1);
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
  // Truncate the head to make room for the suffix.
  let head_budget = budget.saturating_sub(suffix.chars().count());
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

/// Truncate `s` to fit `budget` columns from the **left**, prepending
/// `…/` so the trailing component (the binary name, the launch id)
/// stays visible. Returns the original string unmodified when it
/// already fits.
fn ellipsise(s: &str, budget: usize) -> String {
  let count = s.chars().count();
  if budget == 0 {
    return String::new();
  }
  if count <= budget {
    return s.to_string();
  }
  // Reserve space for the `…/` prefix.
  let prefix = "…/";
  let prefix_len = prefix.chars().count();
  if budget <= prefix_len {
    return s.chars().rev().take(budget).collect::<String>().chars().rev().collect();
  }
  let keep = budget - prefix_len;
  let tail: String = s.chars().skip(count - keep).collect();
  format!("{prefix}{tail}")
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
    assert!(truncated.chars().count() <= 20);
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
}
