//! Shared CLI rendering helpers for padded/colored tables, key/value
//! blocks, and section headers.
//!
//! Visual identity mirrors the init wizard (cliclack-inspired): bold
//! headers, dim secondary text, semantic value colors from
//! `cli::colors`. The global `console::colors_enabled()` flag set in
//! `cli::colors::init` doubles as the predicate that gates *both*
//! ANSI styling and the padded-vs-TSV layout choice — so `--no-colors`,
//! `NO_COLOR`, and a non-TTY stdout each silence color *and* fall back
//! to today's pipe-friendly TSV rows.
//!
//! JSON output paths must never call these helpers — `--json` is a
//! machine contract and stays byte-stable.
//!
//! Module is `pub(crate)`; nothing outside the binary consumes it.

use console::measure_text_width;

/// Column separator in the padded (TTY) layout. Two spaces gives
/// enough visual breathing room without doubling the line length on
/// narrow tables.
const COL_GAP: &str = "  ";

/// Render a table from a header row and per-row cell values.
///
/// When colors are disabled (the three off-conditions documented in
/// `cli::colors::init`), output is byte-equivalent to today's
/// `header.join("\t")` + per-row `cells.join("\t")` shape, so
/// `awk -F\t` and `column -t` pipelines keep working.
///
/// When colors are enabled, the helper:
/// - measures each column's display width (unicode-aware via
///   `console::measure_text_width`, which strips ANSI before
///   counting),
/// - pads every cell to its column's max width,
/// - emits a bold header row,
/// - emits a dim `─` rule under the header,
/// - emits each data row left-justified.
///
/// Caller-supplied cells may already carry ANSI styling (e.g. a state
/// cell wrapped by `colors::state(...)`); the measurement function
/// strips ANSI before counting cells, so alignment stays correct.
///
/// Cells containing newlines or tabs corrupt the layout — callers must
/// normalise those before passing them in. We don't panic, just trust
/// callers; every current call site builds cells from primitives
/// (model name, port, state, etc.) that never carry literal newlines.
pub(crate) fn table(header: &[&str], rows: &[Vec<String>]) -> String {
  if !console::colors_enabled() {
    return render_tsv(header, rows);
  }
  if header.is_empty() && rows.is_empty() {
    return String::new();
  }
  let cols = header
    .len()
    .max(rows.iter().map(Vec::len).max().unwrap_or(0));
  if cols == 0 {
    return String::new();
  }
  let mut widths: Vec<usize> = vec![0; cols];
  for (i, h) in header.iter().enumerate() {
    widths[i] = widths[i].max(measure_text_width(h));
  }
  for row in rows {
    for (i, cell) in row.iter().enumerate() {
      if i < cols {
        widths[i] = widths[i].max(measure_text_width(cell));
      }
    }
  }
  let mut out = String::new();
  if !header.is_empty() {
    let header_cells: Vec<String> = (0..cols)
      .map(|i| pad_cell(header.get(i).copied().unwrap_or(""), widths[i]))
      .collect();
    out.push_str(
      &console::style(header_cells.join(COL_GAP))
        .bold()
        .to_string(),
    );
    out.push('\n');
    let rule_width: usize =
      widths.iter().sum::<usize>() + COL_GAP.len() * widths.len().saturating_sub(1);
    let rule: String = "─".repeat(rule_width);
    out.push_str(&console::style(rule).dim().to_string());
    out.push('\n');
  }
  for row in rows {
    let cells: Vec<String> = (0..cols)
      .map(|i| pad_cell(row.get(i).map(String::as_str).unwrap_or(""), widths[i]))
      .collect();
    out.push_str(&cells.join(COL_GAP));
    out.push('\n');
  }
  out
}

/// Pipe-friendly TSV form. Strips any ANSI a caller may have wrapped
/// cells in (defence in depth — the colors-disabled global already
/// makes `console::style(...)` a no-op, but caller code that builds
/// styled cells via raw escapes would otherwise leak).
///
/// Header cells get the same strip pass: today every caller passes
/// literal `&str` slices, but `header: &[&str]` doesn't constrain that
/// — a future styled-header caller would otherwise leak ANSI into the
/// machine-readable header line.
fn render_tsv(header: &[&str], rows: &[Vec<String>]) -> String {
  let mut out = String::new();
  if !header.is_empty() {
    let stripped_header: Vec<String> = header
      .iter()
      .map(|h| console::strip_ansi_codes(h).into_owned())
      .collect();
    out.push_str(&stripped_header.join("\t"));
    out.push('\n');
  }
  for row in rows {
    let stripped: Vec<String> = row
      .iter()
      .map(|c| console::strip_ansi_codes(c).into_owned())
      .collect();
    out.push_str(&stripped.join("\t"));
    out.push('\n');
  }
  out
}

fn pad_cell(cell: &str, width: usize) -> String {
  let display = measure_text_width(cell);
  if display >= width {
    return cell.to_string();
  }
  let pad = " ".repeat(width - display);
  format!("{cell}{pad}")
}

/// Render a vertical key/value block. Keys are right-aligned (matching
/// the init `intro` panel's visual identity), separated from values
/// by a two-space gap, with a leading two-space indent on each line.
///
/// Keys are wrapped through `console::style(k).bold()`; values are
/// emitted verbatim so callers can pre-style them with the appropriate
/// helper from `cli::colors`. Non-TTY / colors-disabled output drops
/// the bold styling transparently, leaving the literal
/// `"  key  value"` shape.
///
/// **TTY-only surface.** The output is space-aligned for humans, not
/// tab-separated for machines — pipelines that grep / awk by column
/// position will misread it. Always gate calls on
/// `console::colors_enabled()` (or use the JSON branch) so piped
/// consumers see TSV or JSON, never this layout.
pub(crate) fn kv_block(items: &[(&str, String)]) -> String {
  if items.is_empty() {
    return String::new();
  }
  let key_width = items
    .iter()
    .map(|(k, _)| measure_text_width(k))
    .max()
    .unwrap_or(0);
  let mut out = String::new();
  for (k, v) in items {
    let pad = " ".repeat(key_width.saturating_sub(measure_text_width(k)));
    let styled_key = console::style(*k).bold().to_string();
    out.push_str(&format!("  {pad}{styled_key}  {v}\n"));
  }
  out
}

/// Render a section title with an optional dim count suffix.
///
/// Examples:
/// - `section_header("list", Some((3, "models")))` →
///   `"list (3 models)\n"` (bold title, dim count) when colors are
///   enabled; plain `"list (3 models)\n"` when disabled.
/// - `section_header("daemon", None)` → `"daemon\n"`.
///
/// Like [`kv_block`], the output is human-facing — gate piped output
/// to JSON / TSV branches that don't call this helper.
pub(crate) fn section_header(title: &str, count: Option<(usize, &str)>) -> String {
  let mut head = console::style(title).bold().to_string();
  if let Some((n, noun)) = count {
    let suffix = console::style(format!(" ({n} {noun})")).dim().to_string();
    head.push_str(&suffix);
  }
  head.push('\n');
  head
}

/// Render seconds as `1d 2h 3m 4s`, eliding zero higher-order parts.
/// `0` seconds renders as `"0s"` so we never print an empty string.
///
/// Shared by `cli::daemon::render_daemon_status` and `cli::output::status_human`
/// so the two surfaces never drift in how the same uptime is shown.
pub(crate) fn format_uptime(seconds: u64) -> String {
  let days = seconds / 86_400;
  let hours = (seconds % 86_400) / 3_600;
  let mins = (seconds % 3_600) / 60;
  let secs = seconds % 60;
  let mut parts: Vec<String> = Vec::with_capacity(4);
  if days > 0 {
    parts.push(format!("{days}d"));
  }
  if hours > 0 || !parts.is_empty() {
    parts.push(format!("{hours}h"));
  }
  if mins > 0 || !parts.is_empty() {
    parts.push(format!("{mins}m"));
  }
  parts.push(format!("{secs}s"));
  parts.join(" ")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::test_lock::serialize;
  use std::sync::MutexGuard;

  struct ColorGuard {
    _lock: MutexGuard<'static, ()>,
    prior: bool,
  }

  impl ColorGuard {
    fn set(enabled: bool) -> Self {
      let g = Self {
        _lock: serialize(),
        prior: console::colors_enabled(),
      };
      console::set_colors_enabled(enabled);
      g
    }
  }

  impl Drop for ColorGuard {
    fn drop(&mut self) {
      console::set_colors_enabled(self.prior);
    }
  }

  fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|s| s.to_string()).collect()
  }

  #[test]
  fn table_no_color_returns_byte_exact_tsv() {
    let _g = ColorGuard::set(false);
    let s = table(
      &["NAME", "ARCH", "CTX"],
      &[
        row(&["qwen", "qwen2", "8192"]),
        row(&["phi", "phi3", "4096"]),
      ],
    );
    assert_eq!(s, "NAME\tARCH\tCTX\nqwen\tqwen2\t8192\nphi\tphi3\t4096\n");
  }

  #[test]
  fn table_no_color_strips_ansi_from_caller_cells() {
    // Defence in depth: even if a caller wraps a cell in console::style
    // before the colors-disabled global takes effect, the TSV path
    // strips the escapes so pipelines see plain bytes.
    let _g = ColorGuard::set(false);
    let styled = format!(
      "{}",
      console::Style::new()
        .green()
        .force_styling(true)
        .apply_to("ok")
    );
    let s = table(&["STATE"], &[row(&[&styled])]);
    assert_eq!(s, "STATE\nok\n");
  }

  #[test]
  fn table_with_color_pads_columns_to_widest_cell() {
    let _g = ColorGuard::set(true);
    let s = table(
      &["NAME", "CTX"],
      &[row(&["a", "4096"]), row(&["bbb", "8192"])],
    );
    let plain = console::strip_ansi_codes(&s);
    let lines: Vec<&str> = plain.lines().collect();
    // Header + rule + 2 data lines.
    assert_eq!(lines.len(), 4, "expected 4 lines, got {lines:?}");
    assert_eq!(lines[0], "NAME  CTX ", "header: {:?}", lines[0]);
    // Rule line: 4 (NAME col width, max of "NAME"/"a"/"bbb") + 2 (gap)
    //          + 4 (CTX col width, max of "CTX"/"4096"/"8192") = 10 ─ runes.
    assert_eq!(lines[1].chars().count(), 10);
    assert!(lines[1].chars().all(|c| c == '─'));
    assert_eq!(lines[2], "a     4096");
    assert_eq!(lines[3], "bbb   8192");
  }

  #[test]
  fn table_with_color_emits_ansi_on_header_and_rule() {
    let _g = ColorGuard::set(true);
    let s = table(&["A"], &[row(&["x"])]);
    // Bold ESC is `\x1b[1m`; dim ESC is `\x1b[2m`. Cheap regex-free check.
    assert!(s.contains("\x1b[1m"), "expected bold escape in: {s:?}");
    assert!(s.contains("\x1b[2m"), "expected dim escape in: {s:?}");
  }

  #[test]
  fn table_with_color_aligns_cjk_by_display_width() {
    // Two display cells per CJK char; padding must respect that or
    // the rows misalign. Without the unicode-width measurement this
    // would fall back to byte length and the second row would jut
    // out by ~3 bytes per CJK glyph.
    let _g = ColorGuard::set(true);
    let s = table(
      &["NAME", "CTX"],
      &[row(&["日本語", "8192"]), row(&["a", "4096"])],
    );
    let plain = console::strip_ansi_codes(&s);
    let lines: Vec<&str> = plain.lines().collect();
    // "日本語" is 6 cells; "NAME" is 4 → column width = 6.
    // Row 0: "日本語  8192" (6 + 2-gap + 4).
    // Row 1: "a       4096" (1 + 5-pad + 2-gap + 4).
    assert_eq!(measure_text_width(lines[2]), measure_text_width(lines[3]));
  }

  #[test]
  fn table_empty_rows_with_color_emits_header_and_rule_only() {
    let _g = ColorGuard::set(true);
    let s = table(&["NAME", "CTX"], &[]);
    let plain = console::strip_ansi_codes(&s);
    let lines: Vec<&str> = plain.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "NAME  CTX");
    assert!(lines[1].chars().all(|c| c == '─'));
  }

  #[test]
  fn table_empty_rows_no_color_emits_header_line_only() {
    let _g = ColorGuard::set(false);
    let s = table(&["NAME", "CTX"], &[]);
    assert_eq!(s, "NAME\tCTX\n");
  }

  #[test]
  fn table_no_header_with_rows_still_emits_padded_rows_on_tty() {
    let _g = ColorGuard::set(true);
    let s = table(&[], &[row(&["a", "1"]), row(&["bb", "22"])]);
    let plain = console::strip_ansi_codes(&s);
    let lines: Vec<&str> = plain.lines().collect();
    assert_eq!(lines.len(), 2, "no header → no rule line: {lines:?}");
    assert_eq!(lines[0], "a   1 ");
    assert_eq!(lines[1], "bb  22");
  }

  #[test]
  fn table_short_row_pads_missing_cells_to_empty_strings() {
    // A row shorter than the header must not panic; missing cells
    // render as blank padded to their column width.
    let _g = ColorGuard::set(true);
    let s = table(&["A", "B", "C"], &[row(&["x", "y"])]);
    let plain = console::strip_ansi_codes(&s);
    let lines: Vec<&str> = plain.lines().collect();
    // 1-wide cells (max("A","x")=1, max("B","y")=1, max("C","")=1) joined
    // with the 2-space gap: "x" + "  " + "y" + "  " + " " (empty cell
    // padded to width 1) = "x  y   ".
    assert_eq!(lines[2], "x  y   ");
  }

  #[test]
  fn kv_block_right_aligns_keys_and_two_space_separates_value() {
    // Colors disabled so the test compares plain bytes; the bold
    // styling is a no-op in this mode.
    let _g = ColorGuard::set(false);
    let s = kv_block(&[("build", "0.0.1".to_string()), ("pid", "4242".to_string())]);
    assert_eq!(s, "  build  0.0.1\n    pid  4242\n");
  }

  #[test]
  fn kv_block_with_color_bolds_keys() {
    let _g = ColorGuard::set(true);
    let s = kv_block(&[("a", "1".to_string())]);
    assert!(s.contains("\x1b[1m"), "expected bold escape: {s:?}");
    let plain = console::strip_ansi_codes(&s);
    assert_eq!(plain, "  a  1\n");
  }

  #[test]
  fn kv_block_empty_returns_empty_string() {
    let _g = ColorGuard::set(true);
    assert_eq!(kv_block(&[]), "");
  }

  #[test]
  fn section_header_with_count_renders_dim_suffix() {
    let _g = ColorGuard::set(false);
    assert_eq!(
      section_header("list", Some((3, "models"))),
      "list (3 models)\n"
    );
    assert_eq!(section_header("daemon", None), "daemon\n");
  }

  #[test]
  fn section_header_with_color_emits_bold_and_dim() {
    let _g = ColorGuard::set(true);
    let s = section_header("list", Some((2, "items")));
    assert!(s.contains("\x1b[1m"), "expected bold escape: {s:?}");
    assert!(s.contains("\x1b[2m"), "expected dim escape: {s:?}");
    assert_eq!(console::strip_ansi_codes(&s), "list (2 items)\n");
  }

  #[test]
  fn format_uptime_elides_zero_higher_order_parts() {
    assert_eq!(format_uptime(0), "0s");
    assert_eq!(format_uptime(42), "42s");
    // Exact unit boundaries: 60s → 1m (mins=1, secs=0) preserves both
    // segments per the elision rule.
    assert_eq!(format_uptime(60), "1m 0s");
    assert_eq!(format_uptime(90), "1m 30s");
    // 3600s → 1h flips hours into the "any higher-order present →
    // include all lower" branch, so we get 1h 0m 0s.
    assert_eq!(format_uptime(3_600), "1h 0m 0s");
    assert_eq!(format_uptime(3_700), "1h 1m 40s");
    assert_eq!(format_uptime(86_400), "1d 0h 0m 0s");
    assert_eq!(format_uptime(90_061), "1d 1h 1m 1s");
  }
}
