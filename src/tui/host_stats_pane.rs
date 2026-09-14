//! Top-left info-row pane: host CPU / MEM / GPU / VRAM bar gauges
//! plus a backend tag line.
//!
//! Layout (32 cols × 5 inner rows by default):
//!
//! ```text
//! CPU  ███████░░░ 58% 71°C
//! MEM  █████░░░░░ 11.4/32 G
//! GPU  ██████████ 84% 68°C
//! VRAM ███████░░░ 14.2/24 G
//! backend  NVIDIA · 1 GPU
//! ```
//!
//! `MEM` is system RAM; `MEM*` marks unified memory (Apple Silicon,
//! AMD/Intel UMA APUs) where the GPU draws from that same pool. On a
//! discrete card the `VRAM` row reads `used / total`; on unified memory
//! the denominator is the GPU's *reachable* ceiling given current RAM
//! pressure (`pool − non-GPU RAM use`), not the static GTT cap — see
//! `vram_denominator`.
//!
//! Backend-specific variants:
//! * Apple Silicon (unified memory): CPU + `MEM*` + a
//!   `GPU  unified` text row.
//! * Multi-GPU: one `GPU*` row with combined usage (mean) + the hottest
//!   temp across cards; the per-card breakdown stays in
//!   `status --json .host.gpu_devices`.
//! * `CpuOnly`: CPU + MEM only, GPU + VRAM rows omitted.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::theme::Palette;
use crate::tui::fmt::format_bytes_pair;

const LABEL_WIDTH: usize = 5;

/// Render the Host stats pane into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, host: &HostMetricsSnapshot, palette: &Palette) {
  let block = palette.panel_block(" Host ", true);
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
        Span::styled("GPU  ", palette.label_style()),
        Span::styled("unified", palette.text_style()),
      ]));
    }
    _ => {
      // One GPU summary row regardless of card count. The Host pane is a
      // glance view, not a per-device dashboard, so a multi-GPU host
      // collapses to combined usage + the hottest temp under a `GPU*`
      // label (the legend explains it; `status --json .host.gpu_devices`
      // keeps the per-card breakdown).
      lines.push(gpu_summary_row(host, bar_width, palette));
      // The `MEM*` marker on the memory row already flags that the GPU
      // shares that pool, so the GPU memory keeps its own `VRAM` gauge.
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
  // Values land in a left-aligned column after the bar so CPU /
  // RAM / GPU / VRAM all start at the same screen offset.
  let value = format!(" {:.0}%", host.cpu_pct);
  let mut spans = vec![
    Span::styled("CPU  ", palette.label_style()),
    bar,
    Span::styled(value, palette.text_style()),
  ];
  // CPU temperature renders next to the percent so the row reads
  // symmetrically with the GPU row, when sysinfo's component
  // sensor surfaced a reading. Same colour tiers as GPU temp.
  if let Some(temp) = host.cpu_temp_c {
    spans.push(Span::raw(" "));
    spans.extend(temp_spans(temp, palette));
  }
  Line::from(spans)
}

fn ram_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  // Always show the true system RAM total (the same number `init`
  // reports). On unified-memory machines the GPU's allocation lives in
  // this pool, but `sysinfo`'s used/total already account for it — the
  // earlier subtraction of `uma_shared_*` produced a wrong RAM value
  // (and, when DXGI mis-flagged discrete cards, a spurious `*`). The
  // `unified` flag below is what tells the user the pool is shared.
  let (pct, value) = if host.ram_total_bytes == 0 {
    let d = crate::tui::glyphs::active().placeholder();
    (0.0_f32, format!("{d}/{d}"))
  } else {
    let pct = (host.ram_used_bytes as f64 / host.ram_total_bytes as f64) as f32 * 100.0;
    (
      pct.clamp(0.0, 100.0),
      format_bytes_pair(host.ram_used_bytes, host.ram_total_bytes),
    )
  };
  // `MEM*` flags unified memory (Apple Silicon + AMD/Intel UMA APUs)
  // where the GPU draws from this same pool; `MEM` is plain system RAM.
  // Sourced from the one `GpuInfo::is_unified` helper init shares, so
  // the marker can't drift between the two render paths.
  let label = if host.unified { "MEM* " } else { "MEM  " };
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  Line::from(vec![
    Span::styled(label, palette.label_style()),
    bar,
    Span::styled(format!(" {value}"), palette.text_style()),
  ])
}

/// Single GPU utilization row. A multi-GPU host collapses to one `GPU*`
/// row: combined usage (mean across the cards that report it) and the
/// hottest temperature (max across cards). Single-GPU hosts read `GPU`
/// straight from the aggregate snapshot fields. Same left-aligned-column
/// treatment as the CPU row so values line up across CPU/MEM/GPU/VRAM.
fn gpu_summary_row<'a>(
  host: &HostMetricsSnapshot,
  bar_width: usize,
  palette: &'a Palette,
) -> Line<'a> {
  let devices = host.gpu_devices.as_deref().unwrap_or(&[]);
  let multi = devices.len() > 1;
  let (util, temp) = if multi {
    let utils: Vec<f32> = devices.iter().filter_map(|d| d.utilization_pct).collect();
    let util = (!utils.is_empty()).then(|| utils.iter().sum::<f32>() / utils.len() as f32);
    let hottest = devices
      .iter()
      .filter_map(|d| d.temperature_c)
      .fold(None, |hi, t| Some(hi.map_or(t, |h: f32| h.max(t))));
    (util, hottest)
  } else {
    (host.gpu_util_pct, host.gpu_temp_c)
  };
  let label = if multi { "GPU* " } else { "GPU  " };
  let pct = util.unwrap_or(0.0).clamp(0.0, 100.0);
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  let value = util
    .map(|p| format!(" {:.0}%", p))
    .unwrap_or_else(|| format!(" {}", crate::tui::glyphs::active().placeholder()));
  let mut spans = vec![
    Span::styled(label, palette.label_style()),
    bar,
    Span::styled(value, palette.text_style()),
  ];
  if let Some(temp) = temp {
    spans.push(Span::raw(" "));
    spans.extend(temp_spans(temp, palette));
  }
  Line::from(spans)
}

/// Effective denominator for the VRAM gauge.
///
/// UMA pools that physically share one pool with system RAM — Linux
/// `amdgpu` GTT (`CarveSignature`) and Windows DXGI (`ExplicitDxgiUma`) —
/// can only reach their pool budget while that much RAM is actually free.
/// So the reachable ceiling is:
///
/// ```text
/// min(pool_total, ram_total − non_gpu_ram_use)
/// ```
///
/// - Plenty of free RAM → the full pool budget (matches `doctor` and
///   `llama-server --list-devices`).
/// - RAM pressure climbs past the free-RAM headroom → drops toward
///   `ram_total − non_gpu_ram_use`.
///
/// `non_gpu_ram_use` adds the GPU's own shared bytes back before
/// subtracting, because sysinfo already folds the shared allocation into
/// `ram_used` (double-counting it would shrink the ceiling twice). On
/// Linux the pool ≈ total RAM, so the `min` reduces to the old
/// `pool − non_gpu_ram_use`; on Windows the pool is ~half RAM, so the
/// `min` keeps the gauge at the full pool until RAM genuinely fills.
/// Clamped to `>= used` so the gauge never reads over 100%.
///
/// Discrete cards keep `used / total`. Unified-memory hosts (Apple, AMD
/// GTT, Windows DXGI — gated on `HostMetricsSnapshot::unified`, the same
/// flag `launch::admission` uses) share one pool with the CPU, so the
/// reachable ceiling is `min(pool_total, ram_total − non-GPU RAM use)`:
/// the full pool when RAM is free, dropping toward free-RAM headroom under
/// pressure. Clamped `>= used` so the bar never exceeds 100%.
/// In an AMD Linux LXC container where container RAM limit is artificial,
/// the real GPU pool is preserved without clamping against container RAM.
fn vram_denominator(host: &HostMetricsSnapshot, used: u64, total: u64) -> u64 {
  vram_denominator_for_policy(
    host,
    used,
    total,
    crate::util::process::linux_lxc_container(),
  )
}

pub(crate) fn vram_denominator_for_policy(
  host: &HostMetricsSnapshot,
  used: u64,
  total: u64,
  in_lxc: bool,
) -> u64 {
  if !host.unified {
    return total;
  }
  // AMD UMA in a Linux LXC container: the container RAM limit (host.ram_total_bytes)
  // is artificial, but the GPU operates directly against the real host GTT pool (total).
  // Don't clamp against the container's RAM limit.
  if in_lxc
    && host.gpu_backend == HostMetricsSnapshot::BACKEND_AMD
    && host.uma_shared_total_bytes.is_some()
  {
    return total.max(used);
  }
  // Add the GPU's own shared bytes back before subtracting: sysinfo folds
  // them into ram_used, so otherwise they'd be discounted twice.
  let gpu_in_shared = host.uma_shared_used_bytes.unwrap_or(used);
  let other_ram = host.ram_used_bytes.saturating_sub(gpu_in_shared);
  let reachable = host.ram_total_bytes.saturating_sub(other_ram);
  total.min(reachable).max(used)
}

fn vram_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  let (pct, value) = match (host.gpu_mem_used_bytes, host.gpu_mem_total_bytes) {
    (Some(used), Some(total)) if total > 0 => {
      // On UMA the denominator is the GPU's reachable ceiling given
      // current RAM pressure, not the static GTT cap.
      let avail = vram_denominator(host, used, total);
      let pct = (used as f64 / avail as f64) as f32 * 100.0;
      (pct.clamp(0.0, 100.0), format_bytes_pair(used, avail))
    }
    _ => {
      let d = crate::tui::glyphs::active().placeholder();
      (0.0_f32, format!("{d}/{d}"))
    }
  };
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  Line::from(vec![
    Span::styled("VRAM ", palette.label_style()),
    bar,
    Span::styled(format!(" {value}"), palette.text_style()),
  ])
}

fn backend_row<'a>(host: &HostMetricsSnapshot, palette: &'a Palette) -> Line<'a> {
  use crate::daemon::host_metrics::GpuFlavor;
  use crate::init::detection::gpu_vendor_display;
  let mid = crate::tui::glyphs::active().middot();
  let label = match host.flavor() {
    // Name the vendor (AMD / NVIDIA / Apple) consistently with
    // `status`, `doctor`, and `init` — not the metrics tool (NVML /
    // ROCm), which conflated the vendor with the llama.cpp runtime.
    GpuFlavor::Nvidia | GpuFlavor::Amd => format!(
      "{} {mid} {}",
      gpu_vendor_display(&host.gpu_backend),
      pluralize_gpu(host.gpu_device_count)
    ),
    GpuFlavor::AppleMetal => format!("Apple {mid} 1 GPU"),
    GpuFlavor::CpuOnly => "CPU only".into(),
    GpuFlavor::Unsampled => "detecting".into(),
    GpuFlavor::Multi => {
      // Two+ backends found GPUs — show combined count.
      let mut nvidia_count = 0u32;
      let mut amd_count = 0u32;
      let mut unknown_count = 0u32;
      let mut metal_count = 0u32;
      if let Some(devices) = &host.gpu_devices {
        for dev in devices {
          match dev.selector.chars().next() {
            Some('N') => nvidia_count += 1,
            Some('A') => amd_count += 1,
            Some('V') => unknown_count += 1,
            Some('M') => metal_count += 1,
            _ => unknown_count += 1,
          }
        }
      }
      let parts: Vec<String> = vec![
        if nvidia_count > 0 {
          Some(format!("NVIDIA {mid} {}", pluralize_gpu(nvidia_count)))
        } else {
          None
        },
        if amd_count > 0 {
          Some(format!("AMD {mid} {}", pluralize_gpu(amd_count)))
        } else {
          None
        },
        if metal_count > 0 {
          Some(format!("Apple {mid} 1 GPU"))
        } else {
          None
        },
        if unknown_count > 0 {
          Some(format!(
            "GPU (Vulkan) {mid} {}",
            pluralize_gpu(unknown_count)
          ))
        } else {
          None
        },
      ]
      .into_iter()
      .flatten()
      .collect();
      parts.join(" + ")
    }
    // Pass the raw label through so a future backend label not yet
    // classified by `GpuFlavor` still surfaces a debuggable string
    // (rather than a generic "unknown") in the Host pane.
    GpuFlavor::Unknown => host.gpu_backend.clone(),
  };
  Line::from(vec![
    Span::styled("backend  ", palette.label_style()),
    Span::styled(label, palette.text_style()),
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
  // Leave space for label + percent/value/temp. The 11-cell reserve
  // covers the widest right-of-bar payload (" 100% ▲82°C" — temp rows
  // carry a 1-cell severity glyph in the warning/critical tiers so the
  // bar doesn't shift width as temps cross thresholds).
  let usable = budget.saturating_sub(LABEL_WIDTH + 11);
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
  let glyphs = crate::tui::glyphs::active();
  let mut s = String::with_capacity(width * 3);
  for _ in 0..filled {
    s.push(glyphs.gauge_fill());
  }
  for _ in 0..trough {
    s.push(glyphs.gauge_trough());
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

/// Severity glyph for a temperature reading. Returns `""` on the
/// green tier (no glyph), the warning marker on yellow, the critical
/// marker on red. Pairs with [`gpu_temp_color`] so themes that can't
/// carry colour information (Mono) still differentiate `92°C` from
/// `65°C` purely on glyph shape — the two markers stay distinct in both
/// the Unicode (`△`/`▲`) and the ASCII (`!`/`*`) glyph sets.
fn temp_severity_glyph(temp: f32) -> &'static str {
  let glyphs = crate::tui::glyphs::active();
  if temp >= 82.0 {
    glyphs.severity_critical()
  } else if temp >= 70.0 {
    glyphs.severity_warn()
  } else {
    ""
  }
}

/// Build the `glyph + value` spans for a temperature reading. The
/// glyph carries the severity tier so colour isn't load-bearing; both
/// glyph and value share the tier colour so colour-capable themes
/// double-encode the signal. Reserves zero cells on the green tier
/// (no glyph) so the common case still renders compactly.
fn temp_spans<'a>(temp: f32, palette: &'a Palette) -> Vec<Span<'a>> {
  let color = gpu_temp_color(temp, palette);
  let glyph = temp_severity_glyph(temp);
  let style = Style::default().fg(color);
  let mut spans: Vec<Span<'a>> = Vec::with_capacity(2);
  if !glyph.is_empty() {
    spans.push(Span::styled(glyph, style));
  }
  spans.push(Span::styled(
    format!("{temp:.0}{}C", crate::tui::glyphs::active().degree()),
    style,
  ));
  spans
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::ClassSource;
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
  fn temp_severity_glyph_tiers_match_color_tiers() {
    // No glyph on the green tier (compact), `△` on yellow, `▲` on red.
    assert_eq!(temp_severity_glyph(0.0), "");
    assert_eq!(temp_severity_glyph(69.9), "");
    assert_eq!(temp_severity_glyph(70.0), "△");
    assert_eq!(temp_severity_glyph(81.9), "△");
    assert_eq!(temp_severity_glyph(82.0), "▲");
    assert_eq!(temp_severity_glyph(105.0), "▲");
  }

  #[test]
  fn temp_glyph_double_encodes_severity_on_mono_palette() {
    // On Mono, `success` and `error` both collapse to `White` — colour
    // alone can't tell `92°C` apart from `65°C`. The leading severity
    // glyph carries the signal independently so the reading stays
    // legible without colour.
    let snap = HostMetricsSnapshot {
      cpu_pct: 30.0,
      cpu_temp_c: Some(92.0),
      ram_used_bytes: 1,
      ram_total_bytes: 100,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let app = {
      let mut a = App::new(AppOptions::default());
      a.options.theme = crate::theme::ThemeName::Mono;
      a
    };
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(40, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 40, 7), &snap, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut frame = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        frame.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      frame.push('\n');
    }
    assert!(
      frame.contains("▲92°C"),
      "critical CPU temp should carry the `▲` glyph on Mono: {frame}"
    );
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
    assert!(body.contains("MEM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
    assert!(
      rows
        .iter()
        .any(|r| r.contains("backend") && r.contains("CPU only")),
      "expected `backend  cpu only` row, got: {rows:#?}"
    );
  }

  #[test]
  fn apple_metal_collapses_gpu_to_unified_line() {
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
    assert!(
      rows
        .iter()
        .any(|r| r.contains("GPU") && r.contains("unified")),
      "GPU row should read `GPU  unified`, got: {rows:#?}"
    );
    assert!(!body.contains("VRAM"));
    assert!(rows.iter().any(|r| r.contains("Apple")));
  }

  #[test]
  fn nvidia_renders_all_four_gauges_plus_backend() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 58.0,
      cpu_temp_c: Some(52.0),
      ram_used_bytes: 11 * 1024 * 1024 * 1024,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(84.0),
      gpu_mem_used_bytes: Some(14 * 1024 * 1024 * 1024),
      gpu_mem_total_bytes: Some(24 * 1024 * 1024 * 1024),
      gpu_temp_c: Some(68.0),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("MEM"));
    assert!(body.contains("GPU"));
    assert!(body.contains("VRAM"));
    assert!(body.contains("NVIDIA"));
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

  fn device_row(util: f32, temp: f32) -> crate::daemon::host_metrics::DeviceRow {
    crate::daemon::host_metrics::DeviceRow {
      selector: "Nvidia0".into(),
      backend: "nvidia".into(),
      name: "RTX".into(),
      total_memory_bytes: 0,
      used_memory_bytes: None,
      utilization_pct: Some(util),
      temperature_c: Some(temp),
    }
  }

  #[test]
  fn multi_gpu_collapses_to_combined_star_row() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    let snap = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      gpu_device_count: 2,
      gpu_devices: Some(vec![device_row(80.0, 60.0), device_row(20.0, 75.0)]),
      ..Default::default()
    };
    let line = gpu_summary_row(&snap, 6, palette);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    // Combined usage = mean(80, 20) = 50; hottest temp = max(60, 75) = 75.
    assert!(
      text.contains("GPU*"),
      "expected combined star label: {text}"
    );
    assert!(text.contains("50%"), "expected mean utilization: {text}");
    assert!(text.contains("75°C"), "expected hottest temp: {text}");
    assert!(
      !text.contains("GPU0") && !text.contains("GPU1"),
      "must not render per-device rows: {text}"
    );
  }

  #[test]
  fn single_gpu_row_has_no_star() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    let snap = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(42.0),
      gpu_temp_c: Some(55.0),
      gpu_device_count: 1,
      ..Default::default()
    };
    let line = gpu_summary_row(&snap, 6, palette);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
      text.contains("GPU "),
      "single GPU keeps the plain label: {text}"
    );
    assert!(!text.contains("GPU*"), "single GPU has no star: {text}");
    assert!(text.contains("42%"));
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
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
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
  fn ram_row_shows_full_total_and_star_on_unified_memory() {
    // Strix Halo / UMA APU: sysinfo sees the full 121 GiB pool. The RAM
    // row must show that true total (the same number `init` reports) —
    // NOT the total minus the shared/GTT portion. `sysinfo` already
    // counts the GPU's pool usage in `used`, so subtracting produced a
    // wrong value. The `RAM*` marker (from the `unified` flag) is what
    // tells the user the pool is shared with the GPU.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 71 * GIB,
      ram_total_bytes: 121 * GIB,
      gpu_backend: "amd".into(),
      gpu_util_pct: Some(0.0),
      gpu_mem_used_bytes: Some(43 * GIB),
      gpu_mem_total_bytes: Some(65 * GIB),
      gpu_device_count: 1,
      uma_shared_total_bytes: Some(61 * GIB),
      uma_shared_used_bytes: Some(43 * GIB),
      unified: true,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    assert!(
      ram_row.contains("71/121G"),
      "RAM row must show the full sysinfo total, not minus UMA-shared, got: {ram_row:?}"
    );
    assert!(
      ram_row.contains("MEM*"),
      "RAM label should flag unified memory with `*`, got: {ram_row:?}"
    );
  }

  #[test]
  fn ram_row_keeps_raw_sysinfo_numbers_for_discrete_gpu() {
    // Discrete cards don't populate `uma_shared_*` — the RAM row must
    // pass sysinfo's numbers through untouched.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 16 * GIB,
      ram_total_bytes: 64 * GIB,
      gpu_backend: "nvidia".into(),
      gpu_mem_used_bytes: Some(5 * GIB),
      gpu_mem_total_bytes: Some(24 * GIB),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    assert!(ram_row.contains("16/64G"), "RAM row got: {ram_row:?}");
    assert!(
      !ram_row.contains("MEM*"),
      "discrete GPUs shouldn't carry the unified-memory star, got: {ram_row:?}"
    );
  }

  #[test]
  fn ram_and_vram_rows_render_unit_suffix_only_once() {
    // Regression: rows used to render `66G/121G` and `2.5G/4.0G` —
    // the `G` (or `M` / `K` / `B`) suffix appeared on both sides of
    // the slash. Pair formatter shares one trailing suffix.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 66 * GIB,
      ram_total_bytes: 121 * GIB,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(40.0),
      gpu_mem_used_bytes: Some(2_642_341_888),
      gpu_mem_total_bytes: Some(4 * GIB),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      ram_row.contains("66/121G") && !ram_row.contains("66G/121G"),
      "RAM row should share one `G` suffix, got: {ram_row:?}"
    );
    assert!(
      vram_row.contains("2.5/4.0G") && !vram_row.contains("2.5G/4.0G"),
      "VRAM row should share one `G` suffix, got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_gauge_uses_reachable_ceiling_on_unified_memory() {
    // UMA: the GPU shares one pool with the CPU, so the gauge
    // denominator is the reachable ceiling = pool − non-GPU RAM use,
    // NOT the static GTT cap and NOT the double-counting `pool − ram_used`.
    // pool 124, ram_used 71 (of which 43 is the GPU's GTT), so non-GPU
    // RAM = 28 and the GPU can reach 124 − 28 = 96.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 71 * GIB,
      ram_total_bytes: 124 * GIB,
      gpu_backend: "amd".into(),
      gpu_util_pct: Some(10.0),
      gpu_mem_used_bytes: Some(43 * GIB),
      gpu_mem_total_bytes: Some(124 * GIB),
      gpu_device_count: 1,
      uma_shared_total_bytes: Some(123 * GIB),
      uma_shared_used_bytes: Some(43 * GIB),
      unified: true,
      uma_class_source: Some(ClassSource::CarveSignature),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      vram_row.contains("43/96G"),
      "UMA VRAM gauge should read used / reachable-ceiling, got: {vram_row:?}"
    );
    assert!(
      !vram_row.contains("43/53G"),
      "must not double-count the GPU's own bytes (pool − ram_used), got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_gauge_shows_full_pool_on_windows_uma_when_ram_free() {
    // Windows DXGI UMA with ample free RAM: the GPU can reach its full
    // shared-memory budget, so the gauge reads the pool (64), matching
    // `doctor` / `--list-devices`. Regression for the host pane showing
    // `0.0/42G` (the old `pool − ram_used` subtraction) when 107 GiB is
    // free. min(64, 127−21) = 64.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 21 * GIB,
      ram_total_bytes: 127 * GIB,
      gpu_backend: "amd".into(),
      gpu_mem_used_bytes: Some(0),
      gpu_mem_total_bytes: Some(64 * GIB),
      gpu_device_count: 1,
      uma_shared_total_bytes: Some(63 * GIB),
      uma_shared_used_bytes: Some(0),
      unified: true,
      uma_class_source: Some(ClassSource::ExplicitDxgiUma),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      vram_row.contains("0.0/64G"),
      "Windows UMA gauge should show the full pool when RAM is free, got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_gauge_clamps_on_windows_uma_under_ram_pressure() {
    // Windows DXGI UMA when the system has eaten most of RAM: the shared
    // pool competes with that use, so the reachable ceiling drops below
    // the pool. 100 GiB used, 0 of it the GPU's → min(64, 127−100) = 27.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 100 * GIB,
      ram_total_bytes: 127 * GIB,
      gpu_backend: "amd".into(),
      gpu_mem_used_bytes: Some(0),
      gpu_mem_total_bytes: Some(64 * GIB),
      gpu_device_count: 1,
      uma_shared_total_bytes: Some(63 * GIB),
      uma_shared_used_bytes: Some(0),
      unified: true,
      uma_class_source: Some(ClassSource::ExplicitDxgiUma),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      vram_row.contains("0.0/27G"),
      "Windows UMA gauge should clamp to free-RAM headroom under pressure, got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_gauge_uses_raw_total_on_discrete() {
    // Discrete cards keep `used / total` — RAM lives in a separate pool,
    // so subtracting it would be wrong.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 50 * GIB,
      ram_total_bytes: 64 * GIB,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(10.0),
      gpu_mem_used_bytes: Some(14 * GIB),
      gpu_mem_total_bytes: Some(24 * GIB),
      gpu_device_count: 1,
      unified: false,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      vram_row.contains("14/24G"),
      "discrete VRAM gauge stays used / total, got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_denominator_clamps_apple_unified_by_free_ram() {
    // Apple shares one pool with the CPU, so the gauge clamps to the
    // reachable ceiling like AMD GTT / Windows DXGI (matching admission),
    // not the raw working-set total. used 8, total 48, 40 of 64 GiB RAM
    // used → min(48, (64−40)+8) = 32.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      ram_used_bytes: 40 * GIB,
      ram_total_bytes: 64 * GIB,
      gpu_backend: HostMetricsSnapshot::BACKEND_APPLE_METAL.into(),
      unified: true,
      uma_class_source: Some(ClassSource::AppleUnified),
      ..Default::default()
    };
    assert_eq!(vram_denominator(&snap, 8 * GIB, 48 * GIB), 32 * GIB);
  }

  #[test]
  fn vram_denominator_in_amd_lxc_uses_full_gtt_pool_without_container_ram_clamp() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      ram_used_bytes: 2 * GIB,
      ram_total_bytes: 8 * GIB,
      gpu_backend: HostMetricsSnapshot::BACKEND_AMD.into(),
      gpu_mem_total_bytes: Some(96 * GIB),
      gpu_mem_used_bytes: Some(26 * GIB),
      unified: true,
      uma_shared_total_bytes: Some(96 * GIB),
      uma_class_source: Some(ClassSource::CarveSignature),
      ..Default::default()
    };
    // Outside LXC: clamped by container RAM limit -> (8-0) = 8 GiB reachable ceiling, clamped to used (26 GiB)
    assert_eq!(
      vram_denominator_for_policy(&snap, 26 * GIB, 96 * GIB, false),
      26 * GIB
    );
    // Inside LXC: uses the full 96 GiB GTT pool
    assert_eq!(
      vram_denominator_for_policy(&snap, 26 * GIB, 96 * GIB, true),
      96 * GIB
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
    assert!(body.contains("MEM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
    assert!(
      rows.iter().any(|r| r.contains("detecting")),
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
    assert!(body.contains("MEM"));
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
