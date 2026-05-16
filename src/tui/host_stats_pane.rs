//! Top-left info-row pane: host CPU / RAM / GPU / VRAM bar gauges
//! plus a backend tag line.
//!
//! Layout (32 cols × 5 inner rows by default):
//!
//! ```text
//! CPU  ███████░░░  58%  71°C
//! RAM  █████░░░░░  11.4/32 G
//! GPU  ██████████  84%  68°C
//! VRAM ███████░░░  14.2/24 G
//! backend  NVML · 1 GPU
//! ```
//!
//! Backend-specific variants:
//! * Apple Silicon (unified memory): CPU + `RAM (unified)` + a
//!   `GPU  unified memory` text row.
//! * `CpuOnly`: CPU + RAM only, GPU + VRAM rows omitted.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::theme::Palette;
use crate::tui::fmt::format_bytes;

const LABEL_WIDTH: usize = 5;

/// Render the Host stats pane into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, host: &HostMetricsSnapshot, palette: &Palette) {
  let block = Block::default()
    .title(" Host ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let bar_width = bar_width_for(inner.width);
  let mut lines: Vec<Line<'_>> = Vec::with_capacity(5);

  // CPU row is always present.
  lines.push(cpu_row(host, bar_width, palette));
  // RAM row is always present (Apple Silicon labels it "(unified)").
  lines.push(ram_row(host, bar_width, palette));
  // GPU rows depend on backend. The empty-string and "unsampled"
  // values both mean "no readings yet" — render the same collapsed
  // layout as cpu_only so the pre-first-tick window doesn't show
  // four bars filled to 0%.
  match host.gpu_backend.as_str() {
    s if s == HostMetricsSnapshot::BACKEND_CPU_ONLY
      || s == HostMetricsSnapshot::UNINITIALIZED_BACKEND
      || s.is_empty() =>
    {
      // No GPU rows; one blank line preserves vertical rhythm so
      // the backend label lands on the same row across variants.
      lines.push(Line::from(""));
    }
    s if s == HostMetricsSnapshot::BACKEND_APPLE_METAL => {
      lines.push(Line::from(vec![
        Span::styled("GPU  ", Style::default().fg(palette.muted)),
        Span::styled("unified memory", Style::default().fg(palette.fg)),
      ]));
    }
    _ => {
      lines.push(gpu_util_row(host, bar_width, palette));
      lines.push(vram_row(host, bar_width, palette));
    }
  }

  // Pad to four content rows so the backend label always sits on row 5.
  while lines.len() < 4 {
    lines.push(Line::from(""));
  }
  lines.push(backend_row(host, palette));
  frame.render_widget(Paragraph::new(lines), inner);
}

fn cpu_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  let pct = host.cpu_pct.clamp(0.0, 100.0);
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  let value = format!(" {:>3.0}%", host.cpu_pct);
  // Host CPU temperature isn't sampled today (sysinfo Components
  // would need another refresh kind). The row leaves the temp column
  // blank rather than fabricating a value.
  Line::from(vec![
    Span::styled("CPU  ", Style::default().fg(palette.muted)),
    bar,
    Span::styled(value, Style::default().fg(palette.fg)),
  ])
}

fn ram_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  let (pct, value) = if host.ram_total_bytes == 0 {
    (0.0_f32, "—/—".to_string())
  } else {
    let pct = (host.ram_used_bytes as f64 / host.ram_total_bytes as f64) as f32 * 100.0;
    (
      pct.clamp(0.0, 100.0),
      format!(
        "{}/{}",
        format_bytes(host.ram_used_bytes),
        format_bytes(host.ram_total_bytes)
      ),
    )
  };
  let label = if host.gpu_backend == HostMetricsSnapshot::BACKEND_APPLE_METAL {
    "RAM* "
  } else {
    "RAM  "
  };
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  Line::from(vec![
    Span::styled(label, Style::default().fg(palette.muted)),
    bar,
    Span::styled(format!(" {value}"), Style::default().fg(palette.fg)),
  ])
}

fn gpu_util_row<'a>(
  host: &HostMetricsSnapshot,
  bar_width: usize,
  palette: &'a Palette,
) -> Line<'a> {
  let pct = host.gpu_util_pct.unwrap_or(0.0).clamp(0.0, 100.0);
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  let value = host
    .gpu_util_pct
    .map(|p| format!(" {:>3.0}%", p))
    .unwrap_or_else(|| "   —".into());
  let mut spans = vec![
    Span::styled("GPU  ", Style::default().fg(palette.muted)),
    bar,
    Span::styled(value, Style::default().fg(palette.fg)),
  ];
  if let Some(temp) = host.gpu_temp_c {
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
      format!("{temp:>2.0}°C"),
      Style::default().fg(gpu_temp_color(temp, palette)),
    ));
  }
  Line::from(spans)
}

fn vram_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  let (pct, value) = match (host.gpu_mem_used_bytes, host.gpu_mem_total_bytes) {
    (Some(used), Some(total)) if total > 0 => {
      let pct = (used as f64 / total as f64) as f32 * 100.0;
      (
        pct.clamp(0.0, 100.0),
        format!("{}/{}", format_bytes(used), format_bytes(total)),
      )
    }
    _ => (0.0_f32, "—/—".into()),
  };
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  Line::from(vec![
    Span::styled("VRAM ", Style::default().fg(palette.muted)),
    bar,
    Span::styled(format!(" {value}"), Style::default().fg(palette.fg)),
  ])
}

fn backend_row<'a>(host: &HostMetricsSnapshot, palette: &'a Palette) -> Line<'a> {
  let label = match host.gpu_backend.as_str() {
    s if s == HostMetricsSnapshot::BACKEND_NVIDIA => {
      format!("NVML · {}", pluralize_gpu(host.gpu_device_count))
    }
    s if s == HostMetricsSnapshot::BACKEND_AMD => {
      format!("ROCm · {}", pluralize_gpu(host.gpu_device_count))
    }
    s if s == HostMetricsSnapshot::BACKEND_APPLE_METAL => "apple metal".into(),
    s if s == HostMetricsSnapshot::BACKEND_CPU_ONLY => "cpu only".into(),
    s if s == HostMetricsSnapshot::UNINITIALIZED_BACKEND => "unsampled".into(),
    s if s == HostMetricsSnapshot::BACKEND_UNKNOWN => "unknown".into(),
    other => other.to_string(),
  };
  Line::from(vec![
    Span::styled("backend  ", Style::default().fg(palette.muted)),
    Span::styled(label, Style::default().fg(palette.fg)),
  ])
}

fn pluralize_gpu(n: u32) -> String {
  if n == 1 {
    "1 GPU".into()
  } else {
    format!("{n} GPUs")
  }
}

/// Compute the bar width — 60% of the inner area, clamped to a usable
/// range so the trailing percent / units column always has room.
fn bar_width_for(inner_width: u16) -> usize {
  let budget = inner_width as usize;
  // Leave space for label + percent/value/temp.
  let usable = budget.saturating_sub(LABEL_WIDTH + 14);
  usable.clamp(4, 14)
}

/// Render a single bar `[████░░░░]` of `width` cells. Fill chars are
/// `█`, trough chars `░`. We can't color two halves of one span
/// without splitting, so the fill color owns the whole string — the
/// trough chars naturally read as a dimmer shade of the fill because
/// `░` is a 25%-density glyph. (Matches kdash's visual.)
fn bar(pct: f32, width: usize, fill: Color) -> Span<'static> {
  if width == 0 {
    return Span::raw("");
  }
  let filled = ((pct / 100.0) * width as f32).round() as usize;
  let filled = filled.min(width);
  let trough = width - filled;
  let mut s = String::with_capacity(width * 3);
  for _ in 0..filled {
    s.push('█');
  }
  for _ in 0..trough {
    s.push('░');
  }
  Span::styled(s, Style::default().fg(fill))
}

/// Gauge color tier: green ≤60%, yellow 60–85%, red ≥85%.
fn gauge_color(pct: f32, palette: &Palette) -> Color {
  if pct >= 85.0 {
    palette.error
  } else if pct >= 60.0 {
    palette.warning
  } else {
    palette.success
  }
}

/// GPU temperature tier: green ≤70°C, yellow 70–82°C, red ≥82°C.
fn gpu_temp_color(temp: f32, palette: &Palette) -> Color {
  if temp >= 82.0 {
    palette.error
  } else if temp >= 70.0 {
    palette.warning
  } else {
    palette.success
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::{App, AppOptions};
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  fn render_lines(snap: HostMetricsSnapshot) -> Vec<String> {
    let app = App::new(AppOptions::default());
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(32, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 32, 7), &snap, palette))
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
  fn gauge_tier_thresholds_match_plan() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    assert_eq!(gauge_color(0.0, palette), palette.success);
    assert_eq!(gauge_color(59.9, palette), palette.success);
    assert_eq!(gauge_color(60.0, palette), palette.warning);
    assert_eq!(gauge_color(84.9, palette), palette.warning);
    assert_eq!(gauge_color(85.0, palette), palette.error);
    assert_eq!(gauge_color(100.0, palette), palette.error);
  }

  #[test]
  fn gpu_temp_tier_thresholds_match_plan() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    assert_eq!(gpu_temp_color(50.0, palette), palette.success);
    assert_eq!(gpu_temp_color(70.0, palette), palette.warning);
    assert_eq!(gpu_temp_color(82.0, palette), palette.error);
  }

  #[test]
  fn cpu_only_omits_gpu_rows() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 50.0,
      ram_used_bytes: 4 * 1024 * 1024 * 1024,
      ram_total_bytes: 16 * 1024 * 1024 * 1024,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("RAM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
    assert!(
      rows
        .iter()
        .any(|r| r.contains("backend") && r.contains("cpu only")),
      "expected `backend  cpu only` row, got: {rows:#?}"
    );
  }

  #[test]
  fn apple_metal_collapses_gpu_to_unified_memory_line() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 30.0,
      ram_used_bytes: 8 * 1024 * 1024 * 1024,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      gpu_backend: "apple_metal".into(),
      gpu_mem_total_bytes: Some(32 * 1024 * 1024 * 1024),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("GPU"));
    assert!(body.contains("unified memory"));
    assert!(!body.contains("VRAM"));
    assert!(rows.iter().any(|r| r.contains("apple metal")));
  }

  #[test]
  fn nvidia_renders_all_four_gauges_plus_backend() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 58.0,
      ram_used_bytes: 11 * 1024 * 1024 * 1024,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(84.0),
      gpu_mem_used_bytes: Some(14 * 1024 * 1024 * 1024),
      gpu_mem_total_bytes: Some(24 * 1024 * 1024 * 1024),
      gpu_temp_c: Some(68.0),
      gpu_device_count: 1,
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("RAM"));
    assert!(body.contains("GPU"));
    assert!(body.contains("VRAM"));
    assert!(body.contains("NVML"));
    assert!(body.contains("1 GPU"));
  }

  #[test]
  fn multi_gpu_pluralizes_backend_label() {
    let snap = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      gpu_device_count: 2,
      gpu_mem_total_bytes: Some(48 * 1024 * 1024 * 1024),
      ..Default::default()
    };
    let rows = render_lines(snap);
    assert!(rows.iter().any(|r| r.contains("2 GPUs")));
  }

  #[test]
  fn ram_total_zero_renders_em_dash_value() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 0,
      ram_total_bytes: 0,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("RAM")).unwrap();
    assert!(
      ram_row.contains("—/—"),
      "expected `—/—` placeholder when total is 0, got: {ram_row:?}"
    );
  }

  #[test]
  fn cpu_over_100_pct_clamps_bar_but_keeps_label() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 312.0,
      ram_used_bytes: 1,
      ram_total_bytes: 100,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let cpu_row = rows.iter().find(|r| r.contains("CPU")).unwrap();
    // Numeric label preserves the unclamped value so users see the
    // true multi-core sum.
    assert!(
      cpu_row.contains("312%"),
      "CPU row must keep the unclamped numeric label, got: {cpu_row:?}"
    );
  }

  #[test]
  fn bar_width_scales_with_panel_width() {
    assert_eq!(bar_width_for(32), bar_width_for(32));
    assert!(bar_width_for(20) < bar_width_for(40));
    // Pathologically narrow panels still produce a minimum-width bar.
    assert!(bar_width_for(8) >= 4);
  }

  #[test]
  fn unsampled_backend_collapses_gpu_rows_like_cpu_only() {
    // The pre-first-tick window emits `gpu_backend == "unsampled"`. The
    // host panel must not render gpu_util_row/vram_row in that window
    // (they would show bars filled to 0% / "—/—" placeholders that
    // misrepresent the actual GPU state).
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 1024 * 1024 * 1024,
      ram_total_bytes: 16 * 1024 * 1024 * 1024,
      gpu_backend: HostMetricsSnapshot::UNINITIALIZED_BACKEND.into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("RAM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
    assert!(
      rows.iter().any(|r| r.contains("unsampled")),
      "expected `backend  unsampled`, got: {rows:#?}"
    );
  }

  #[test]
  fn empty_backend_collapses_gpu_rows_like_cpu_only() {
    // A default-constructed `HostMetricsSnapshot` (which the IPC
    // emits as `host` when no sampler is attached) carries
    // `gpu_backend == ""`. The pane should not render GPU/VRAM rows
    // for that state.
    let snap = HostMetricsSnapshot {
      gpu_backend: String::new(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("RAM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
  }

  #[test]
  fn arbitrary_backend_string_falls_through_to_catch_all() {
    // An unrecognised label (e.g. a future backend not yet handled by
    // the explicit arms) should still render — just verbatim — so the
    // user gets a debuggable signal rather than a missing row.
    let snap = HostMetricsSnapshot {
      gpu_backend: "future-backend".into(),
      gpu_device_count: 1,
      gpu_mem_total_bytes: Some(8 * 1024 * 1024 * 1024),
      ..Default::default()
    };
    let rows = render_lines(snap);
    assert!(
      rows.iter().any(|r| r.contains("future-backend")),
      "expected verbatim catch-all label: {rows:#?}"
    );
  }

  #[test]
  fn unknown_backend_renders_via_constant_arm() {
    // The Vulkan fallback emits BACKEND_UNKNOWN. The backend row
    // should pick up the explicit "unknown" label rather than passing
    // through the catch-all (otherwise the wire string leaks into the
    // UI verbatim).
    let snap = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_UNKNOWN.into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    assert!(
      rows.iter().any(|r| r.contains("unknown")),
      "expected `backend  unknown`, got: {rows:#?}"
    );
  }
}
