//! Shared display helpers for the TUI panes.
//!
//! Centralizing these formatters avoids the silent drift that crept in
//! when three panes each defined their own `format_bytes` with subtly
//! different thresholds.

// `panel_title` moved to `Palette::title_style()` / `Palette::panel_block`
// during the Tier-B sweep — see `src/theme/palette.rs`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;
use crate::tui::launch_picker::INHERITED_LABEL;

/// Single caret span (`▏` painted in `palette.accent` + REVERSED).
/// Used by every single-line text input so the cursor reads
/// identically across the TUI (replaces three
/// open-coded `Span::styled("▏", …)` sites in `advanced_panel`,
/// `list_pane`'s filter chip, and `tabs/input_pane`).
pub(crate) fn caret(palette: &Palette) -> Span<'static> {
  Span::styled(
    crate::tui::glyphs::active().caret(),
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::REVERSED),
  )
}

/// A model-list column cell (Arch / Quant / Mode / …): `value` when it's a
/// real value, else `placeholder`. Treats an empty string and the `unknown`
/// sentinels (`Quant::Unknown` → `"Unknown"`, `ModeHint::Unknown` → `"unknown"`)
/// as missing, so a registry-served model with no GGUF header (Lemonade) reads
/// with the same placeholder as any other absent column. Shared by the TUI list
/// pane (`—`) and the CLI `list` table (`?`) so the two never drift.
pub(crate) fn list_cell(value: Option<&str>, placeholder: &str) -> String {
  match value {
    Some(v) if !v.is_empty() && !v.eq_ignore_ascii_case("unknown") => v.to_string(),
    _ => placeholder.to_string(),
  }
}

/// Format a token count for the Ctx column / launch picker:
/// `131072` → `128k`, `262144` → `256k`, `2_000_000` → `2.0M`.
/// Sub-1024 values render as raw integers (e.g., `512`).
pub(crate) fn format_tokens(n: u64) -> String {
  const K: u64 = 1024;
  const M: u64 = K * 1024;
  if n >= M {
    let m = n as f64 / M as f64;
    if m >= 10.0 {
      format!("{m:.0}M")
    } else {
      format!("{m:.1}M")
    }
  } else if n >= K {
    let k = n as f64 / K as f64;
    if k >= 10.0 {
      format!("{k:.0}k")
    } else {
      format!("{k:.1}k")
    }
  } else {
    n.to_string()
  }
}

/// Format a `used/total` byte pair using one shared unit suffix taken
/// from `total`, so `MEM` / `VRAM` rows render as `2.5/4.0G` rather
/// than `2.5G/4.0G`. Each value follows the same 1-decimal-under-10
/// rule as [`format_bytes`].
///
/// Units are **binary** and abbreviated for the width-constrained Host
/// pane: `K`/`M`/`G` mean KiB/MiB/GiB (÷1024), the same values the
/// spelled-out `GiB` surfaces (`doctor` / `status` / `init`) print.
pub(crate) fn format_bytes_pair(used: u64, total: u64) -> String {
  const KIB: u64 = 1024;
  const MIB: u64 = KIB * 1024;
  const GIB: u64 = MIB * 1024;
  let (div, suffix): (u64, &str) = if total >= GIB {
    (GIB, "G")
  } else if total >= MIB {
    (MIB, "M")
  } else if total >= KIB {
    (KIB, "K")
  } else {
    (1, "B")
  };
  let one = |b: u64| -> String {
    let v = b as f64 / div as f64;
    if v >= 10.0 {
      format!("{v:.0}")
    } else {
      format!("{v:.1}")
    }
  };
  format!("{}/{}{suffix}", one(used), one(total))
}

/// Format a byte count for compact display in panel headers and bars.
/// Rounds to a single decimal place between 1G and 10G (so `4.2G` is
/// distinguishable from `5.1G`), and drops the decimal at 10G+ to keep
/// the label inside ~4 characters.
pub(crate) fn format_bytes(bytes: u64) -> String {
  const KIB: f64 = 1024.0;
  const MIB: f64 = KIB * 1024.0;
  const GIB: f64 = MIB * 1024.0;
  let b = bytes as f64;
  if b >= GIB {
    let g = b / GIB;
    if g >= 10.0 {
      format!("{g:.0}G")
    } else {
      format!("{g:.1}G")
    }
  } else if b >= MIB {
    format!("{:.0}M", b / MIB)
  } else if b >= KIB {
    format!("{:.0}K", b / KIB)
  } else {
    format!("{bytes}B")
  }
}

/// Truncate `s` to `max` **characters**, replacing the dropped tail
/// with a single `…`. Char-count based (not display width) — used by
/// the fixed-column HF picker rows where every glyph is one cell.
/// Returns the original string when it already fits.
pub(crate) fn truncate_end(s: &str, max: usize) -> String {
  if s.chars().count() <= max {
    return s.to_string();
  }
  let ellipsis = crate::tui::glyphs::active().ellipsis();
  // Reserve room for the ellipsis width so the result still fits `max`
  // cells; the ASCII ellipsis (`...`) is three cells, not one.
  let ell_w = ellipsis.chars().count();
  let mut out: String = s.chars().take(max.saturating_sub(ell_w)).collect();
  out.push_str(ellipsis);
  out
}

/// Truncate `s` to fit `budget` terminal columns from the **left**,
/// prepending `…/` so the trailing component (the binary name, the
/// launch id) stays visible. Returns the original string unmodified
/// when it already fits.
///
/// Measures in unicode display width (`UnicodeWidthStr`), not char
/// count: a CJK character occupies two cells, so on a CJK install
/// prefix like `/Users/张伟/.../llama-server` char-count truncation
/// would overflow the reserved budget and push a trailing chunk
/// off-screen.
pub(crate) fn truncate_start(s: &str, budget: usize) -> String {
  if budget == 0 {
    return String::new();
  }
  if s.width() <= budget {
    return s.to_string();
  }
  let prefix = format!("{}/", crate::tui::glyphs::active().ellipsis());
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
pub(crate) fn take_tail_by_width(s: &str, budget: usize) -> String {
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

/// Label column width for every knob row, derived from the labels backends
/// actually register rather than hand-maintained.
///
/// Two hand-set copies of this used to exist, both `18`, both documenting the
/// same two 17-char labels, with nothing linking them — so the next backend to
/// register a longer label would need both bumped, and bumping one reproduced
/// the `Trust remote codeinherited` collision in whichever pane was missed.
pub fn kv_label_width() -> usize {
  static W: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
  *W.get_or_init(|| {
    crate::backend::Backends::all()
      .iter()
      .flat_map(|b| crate::backend::Backend::native_knobs(b).iter())
      .map(|d| d.label.chars().count())
      .chain(std::iter::once(MIN_KV_LABEL_W))
      .max()
      .unwrap_or(MIN_KV_LABEL_W)
      // One column of gap between label and value.
      + 1
  })
}

/// Floor, so the shared (non-native) rows keep their historical alignment even
/// on a build whose backends all register short labels.
const MIN_KV_LABEL_W: usize = 15;

/// Inherited / empty value sentinels rendered when no override exists.
/// Tracked in one place so `kv_row` / `kv_row_focused` agree on which
/// strings deserve the muted tone.
fn is_default_value(value: &str) -> bool {
  value == INHERITED_LABEL || value == "(none)"
}

/// Style to paint a settings value with — muted when the row falls
/// through to its layered default (`inherited`, `(none)`), normal text
/// when the value is overridden by the user or carries a real reading.
fn kv_value_style(value: &str, palette: &Palette) -> Style {
  if is_default_value(value) {
    palette.muted_style()
  } else {
    palette.text_style()
  }
}

/// `  label            value` row used by the Settings running-launch
/// view. Label padded to [`kv_label_width`] in the label tone; value muted
/// when it reads as a layered default.
pub(crate) fn kv_row(label: &str, value: String, palette: &Palette) -> Line<'static> {
  let style = kv_value_style(&value, palette);
  Line::from(vec![
    Span::styled(
      format!("  {label:<width$}", width = kv_label_width()),
      palette.label_style(),
    ),
    Span::styled(value, style),
  ])
}

/// Editable Settings form row. When focused and `cyclable`, the value
/// is wrapped in `◀ … ▶` so the user sees that Left/Right change it.
/// When `source_label` is `Some` and `show_source` is true, a
/// right-aligned `(<label>)` chip is appended.
#[allow(clippy::too_many_arguments)]
pub(crate) fn kv_row_focused(
  label: &str,
  value: String,
  source_label: Option<&str>,
  focused: bool,
  cyclable: bool,
  palette: &Palette,
  show_source: bool,
) -> Line<'static> {
  let marker = if focused {
    crate::tui::glyphs::active().focus_marker()
  } else {
    "  "
  };
  let label_style = if focused {
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD)
  } else {
    palette.label_style()
  };
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(6);
  spans.push(Span::styled(
    format!("{marker}{label:<width$}", width = kv_label_width()),
    label_style,
  ));
  let v_style = kv_value_style(&value, palette);
  if focused && cyclable {
    let glyphs = crate::tui::glyphs::active();
    spans.push(Span::styled(
      format!("{} ", glyphs.cycle_left()),
      palette.accent_style(),
    ));
    spans.push(Span::styled(value, v_style));
    spans.push(Span::styled(
      format!(" {}", glyphs.cycle_right()),
      palette.accent_style(),
    ));
  } else {
    spans.push(Span::styled(value, v_style));
  }
  if let (true, Some(src)) = (show_source, source_label) {
    spans.push(Span::styled(format!("  ({src})"), palette.muted_style()));
  }
  Line::from(spans)
}

/// Clip a styled line to `max_width` display columns, marking any cut
/// with a single muted `…`. Lines within budget pass through untouched,
/// styles preserved. The Settings views render with this (and without
/// `Wrap`) so an overlong `value  (server default)` row truncates on one
/// line instead of wrapping — wrapping shifts every row below it, which
/// makes preset cycling and live knob updates visibly jump.
pub(crate) fn clip_line(line: Line<'static>, max_width: usize, palette: &Palette) -> Line<'static> {
  let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
  if total <= max_width {
    return line;
  }
  let line_style = line.style;
  let line_alignment = line.alignment;
  let budget = max_width.saturating_sub(1); // reserve one column for the …
  let mut out: Vec<Span<'static>> = Vec::new();
  let mut used = 0usize;
  for span in line.spans {
    let w = span.content.width();
    if used + w <= budget {
      used += w;
      out.push(span);
      continue;
    }
    let remaining = budget - used;
    if remaining > 0 {
      let mut taken = String::new();
      let mut acc = 0usize;
      for ch in span.content.chars() {
        let cw = ch.to_string().width();
        if acc + cw > remaining {
          break;
        }
        taken.push(ch);
        acc += cw;
      }
      if !taken.is_empty() {
        out.push(Span::styled(taken, span.style));
      }
    }
    break;
  }
  out.push(Span::styled("…", palette.muted_style()));
  let mut clipped = Line::from(out);
  clipped.style = line_style;
  clipped.alignment = line_alignment;
  clipped
}

#[cfg(test)]
mod tests {

  /// The label column is a cross-module contract: the strings live in backend
  /// modules, the renderer is generic, and no golden exercises a long one — a
  /// backend registering an 18-char label used to produce
  /// `KV cache dtype sizeinherited` with every test still green.
  #[test]
  fn every_registered_knob_label_keeps_a_gap_before_its_value() {
    let width = kv_label_width();
    for b in crate::backend::Backends::all() {
      for d in crate::backend::Backend::native_knobs(&b) {
        let rendered = format!("{:<width$}", d.label, width = width);
        assert!(
          rendered.len() > d.label.chars().count(),
          "`{}` ({} chars) fills the {width}-wide column with no separating \
           space, so its value renders flush against it",
          d.label,
          d.label.chars().count()
        );
      }
    }
  }
  use super::*;

  fn demo_palette() -> &'static Palette {
    crate::theme::palette_for(crate::theme::ThemeName::Macchiato)
  }

  #[test]
  fn clip_line_truncates_overflow_with_ellipsis() {
    let p = demo_palette();
    let line = Line::from("ctx             inherited  (model default)".to_string());
    let clipped = clip_line(line, 20, p);
    let w: usize = clipped.spans.iter().map(|s| s.content.width()).sum();
    assert!(w <= 20, "clipped width {w} should fit 20 cols");
    let text: String = clipped.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.ends_with('…'), "truncation marked with …: {text:?}");
    assert!(!text.contains("default)"), "tail must be cut: {text:?}");
  }

  #[test]
  fn clip_line_leaves_fitting_lines_unchanged() {
    let p = demo_palette();
    let line = Line::from(vec![Span::raw("ctx  "), Span::raw("32768")]);
    let clipped = clip_line(line, 80, p);
    let text: String = clipped.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "ctx  32768");
    assert!(!text.contains('…'));
  }

  #[test]
  fn list_cell_maps_empty_and_unknown_sentinels_to_placeholder() {
    // Real values pass through.
    assert_eq!(list_cell(Some("Q4_K"), "—"), "Q4_K");
    assert_eq!(list_cell(Some("llama"), "?"), "llama");
    // Empty, None, and the unknown sentinels (either case) → the placeholder.
    assert_eq!(list_cell(Some(""), "—"), "—");
    assert_eq!(list_cell(None, "—"), "—");
    assert_eq!(list_cell(Some("Unknown"), "?"), "?"); // Quant::Unknown label
    assert_eq!(list_cell(Some("unknown"), "?"), "?"); // ModeHint::Unknown label
  }

  #[test]
  fn format_tokens_basic_ranges() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(512), "512");
    assert_eq!(format_tokens(1024), "1.0k");
    assert_eq!(format_tokens(2048), "2.0k");
    assert_eq!(format_tokens(8192), "8.0k");
    assert_eq!(format_tokens(32_768), "32k");
    assert_eq!(format_tokens(131_072), "128k");
    assert_eq!(format_tokens(262_144), "256k");
    assert_eq!(format_tokens(1_048_576), "1.0M");
    assert_eq!(format_tokens(2_097_152), "2.0M");
    assert_eq!(format_tokens(10_485_760), "10M");
  }

  #[test]
  fn under_kib_renders_raw_bytes() {
    assert_eq!(format_bytes(0), "0B");
    assert_eq!(format_bytes(512), "512B");
    assert_eq!(format_bytes(1023), "1023B");
  }

  #[test]
  fn kib_and_mib_drop_decimals() {
    assert_eq!(format_bytes(1024), "1K");
    assert_eq!(format_bytes(1024 * 1024), "1M");
  }

  #[test]
  fn gib_below_ten_keeps_one_decimal() {
    assert_eq!(format_bytes(4_500_000_000), "4.2G");
    assert_eq!(format_bytes(9_000_000_000), "8.4G");
  }

  #[test]
  fn gib_at_or_above_ten_drops_decimal() {
    assert_eq!(format_bytes(11_000_000_000), "10G");
    assert_eq!(format_bytes(24_000_000_000), "22G");
    assert_eq!(format_bytes(100_000_000_000), "93G");
  }

  #[test]
  fn format_bytes_pair_shares_unit_suffix() {
    // Regression: RAM / VRAM rows used to render `66G/121G` and
    // `2.5G/4.0G` — unit repeated on both sides. Both values now share
    // one trailing suffix taken from `total`.
    const GIB: u64 = 1024 * 1024 * 1024;
    assert_eq!(format_bytes_pair(66 * GIB, 121 * GIB), "66/121G");
    assert_eq!(
      format_bytes_pair(2_642_341_888, 4 * GIB),
      "2.5/4.0G",
      "VRAM at <10G should keep one decimal on both sides"
    );
  }

  #[test]
  fn format_bytes_pair_scales_used_to_total_unit() {
    // `500M/2.0G` would mix units; pair scales `used` down into the
    // larger unit (`0.2/2.0G`) so the suffix stays consistent.
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    assert_eq!(format_bytes_pair(500 * MIB, 2 * GIB), "0.5/2.0G");
  }

  #[test]
  fn format_bytes_pair_at_or_above_ten_drops_decimal() {
    const GIB: u64 = 1024 * 1024 * 1024;
    assert_eq!(format_bytes_pair(14 * GIB, 32 * GIB), "14/32G");
  }

  #[test]
  fn format_bytes_pair_falls_back_to_bytes_for_tiny_totals() {
    assert_eq!(format_bytes_pair(0, 512), "0.0/512B");
  }

  #[test]
  fn truncate_end_inserts_ellipsis_for_long_strings() {
    let out = truncate_end("supercalifragilisticexpialidocious", 10);
    assert_eq!(out.chars().count(), 10);
    assert!(out.ends_with('…'));
  }

  #[test]
  fn truncate_end_leaves_short_strings_unchanged() {
    assert_eq!(truncate_end("ok", 10), "ok");
    assert_eq!(truncate_end("", 10), "");
  }

  #[test]
  fn truncate_start_pads_short_strings_unchanged() {
    assert_eq!(truncate_start("ok", 10), "ok");
    assert_eq!(truncate_start("", 10), "");
  }

  #[test]
  fn truncate_start_left_truncates_long_paths() {
    let truncated = truncate_start("/usr/local/lib/llama-cpp-cuda/bin/llama-server", 20);
    assert!(truncated.starts_with("…/"));
    assert!(truncated.ends_with("llama-server"));
    assert!(truncated.width() <= 20);
  }

  #[test]
  fn truncate_start_measures_in_display_columns_not_char_count() {
    // CJK characters occupy two terminal cells each. A char-count
    // implementation would let `/张伟/llama-server` (15 chars, 17
    // cells) "fit" inside a 16-column budget; ratatui would then
    // clip the flavor chunk that the caller appends. Width-based
    // measurement keeps the truncated string inside the requested
    // budget so the trailing chunk renders intact.
    let s = "/usr/local/张伟/bin/llama-server";
    let out = truncate_start(s, 20);
    assert!(
      out.width() <= 20,
      "expected width <= 20, got {} for {out:?}",
      out.width()
    );
    assert!(out.ends_with("llama-server"));
  }
}
