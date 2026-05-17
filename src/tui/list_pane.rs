//! Model list rows for the left pane.
//!
//! Three row variants live here:
//!  1. [`ListRow::TableHeader`] — the column-label row pinned to the
//!     very top of the list. Always first, never selectable, kept in
//!     lock-step with the per-model row layout so the columns align.
//!  2. [`ListRow::Header`] — folder-group / `★ Favorites` section
//!     header that introduces the rows beneath it.
//!  3. [`ListRow::Model`] — one rendered row per discovered GGUF.
//!
//! Per-state colour: rows pick their foreground from the surface
//! state — `Ready` is rendered with `palette.success`, `Error` with
//! `palette.error`, everything else with `palette.fg`. The selected
//! row uses `Modifier::REVERSED` so fg/bg flip at the terminal layer,
//! which means a running row stays visibly "green" even when it's
//! the highlighted row (it just inverts).

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::discovery::DiscoveredModel;
use crate::theme::Palette;
use crate::tui::fmt::{format_bytes, format_tokens};
use crate::tui::status_icons::{colour_for, glyph_for, SurfaceState};

/// A row as it appears in the rendered list — table header, group
/// header, or model row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
  /// Always-on column-label row pinned to the top of the list.
  /// Renders the same fixed-width column grid as every model row so
  /// the column labels align with their values.
  TableHeader,
  /// Section header (Favorites, or one parent directory).
  Header { label: String },
  /// One model row. `state` is the surface-level lifecycle for any
  /// active launch of this model, or `NotLaunched` when no
  /// supervisor has touched it.
  Model {
    /// Canonical path — `Model` rows match against the daemon's
    /// `start_model` API which expects this same path.
    path: PathBuf,
    /// User-facing display name (file stem).
    name: String,
    /// Architecture badge (e.g. `llama`, `qwen3`); empty when
    /// metadata is unavailable.
    arch: String,
    /// Quantisation badge (e.g. `Q4_K`, `Q8_0`).
    quant: String,
    /// Native context length in tokens, when known.
    native_ctx: Option<u64>,
    /// Weights footprint in bytes (sum of tensor storage), when
    /// known.
    weights_bytes: Option<u64>,
    /// Mode hint surfaced at discovery time.
    mode_hint: String,
    /// Whether this row is favorited (drives the `★` glyph).
    favorite: bool,
    state: SurfaceState,
  },
}

impl ListRow {
  pub fn is_selectable(&self) -> bool {
    matches!(self, ListRow::Model { .. })
  }

  pub fn path(&self) -> Option<&std::path::Path> {
    match self {
      ListRow::Model { path, .. } => Some(path),
      ListRow::Header { .. } | ListRow::TableHeader => None,
    }
  }
}

/// Inputs to [`build_rows`]. `model_states` is a snapshot of every
/// `(model_path → surface_state)` pair the daemon reports as
/// supervised; rows for paths absent from the map land as
/// `NotLaunched`.
pub struct RowInputs<'a> {
  pub models: &'a [DiscoveredModel],
  pub favorites: &'a [PathBuf],
  pub model_states: &'a BTreeMap<PathBuf, SurfaceState>,
}

/// Group `models` into:
///  - [`ListRow::TableHeader`] (always row 0, even when there's
///    nothing else; the renderer makes the column labels themselves
///    a visible signal that the pane is loaded).
///  - `★ Favorites` (only when at least one favorited model is in
///    the list).
///  - One section per parent directory, alphabetical by parent.
///    Rows within a group sort by display name.
pub fn build_rows(inputs: RowInputs<'_>) -> Vec<ListRow> {
  // No discovered models → no rows at all. The renderer detects an
  // empty Vec and switches to `render_empty_state`, which paints the
  // "drop a .gguf …" hint. Returning `[TableHeader]` here would
  // shadow that hint with a lone column-label row.
  if inputs.models.is_empty() {
    return Vec::new();
  }
  let mut rows: Vec<ListRow> = Vec::with_capacity(inputs.models.len() + 4);
  rows.push(ListRow::TableHeader);

  let favorite_set: std::collections::BTreeSet<&PathBuf> = inputs.favorites.iter().collect();

  // Favorites section.
  let favorites: Vec<&DiscoveredModel> = inputs
    .models
    .iter()
    .filter(|m| favorite_set.contains(&m.path))
    .collect();
  if !favorites.is_empty() {
    rows.push(ListRow::Header {
      label: "★ Favorites".into(),
    });
    let mut sorted = favorites.clone();
    sorted.sort_by_key(|a| display_name(a));
    for m in sorted {
      rows.push(model_row(
        m,
        true,
        surface_state_for(m, inputs.model_states),
      ));
    }
  }

  // Group remaining (non-favorite) rows by parent directory.
  let mut grouped: BTreeMap<&PathBuf, Vec<&DiscoveredModel>> = BTreeMap::new();
  for m in inputs.models {
    if !favorite_set.contains(&m.path) {
      grouped.entry(&m.parent).or_default().push(m);
    }
  }
  for (parent, mut entries) in grouped {
    rows.push(ListRow::Header {
      label: parent.display().to_string(),
    });
    entries.sort_by_key(|a| display_name(a));
    for m in entries {
      let fav = favorite_set.contains(&m.path);
      rows.push(model_row(m, fav, surface_state_for(m, inputs.model_states)));
    }
  }

  rows
}

fn surface_state_for(
  m: &DiscoveredModel,
  states: &BTreeMap<PathBuf, SurfaceState>,
) -> SurfaceState {
  states
    .get(&m.path)
    .copied()
    .unwrap_or(SurfaceState::NotLaunched)
}

fn display_name(m: &DiscoveredModel) -> String {
  m.path
    .file_stem()
    .and_then(|s| s.to_str())
    .map(|s| s.to_string())
    .unwrap_or_else(|| m.path.display().to_string())
}

fn model_row(m: &DiscoveredModel, favorite: bool, state: SurfaceState) -> ListRow {
  let (arch, quant, native_ctx, mode_hint, weights_bytes) = match &m.metadata {
    Some(md) => (
      md.arch.clone().unwrap_or_default(),
      md.quant.label().to_string(),
      md.native_ctx,
      mode_hint_label(md.mode_hint),
      md.weights_bytes,
    ),
    None => (String::new(), String::new(), None, "unknown".into(), None),
  };
  ListRow::Model {
    path: m.path.clone(),
    name: display_name(m),
    arch,
    quant,
    native_ctx,
    weights_bytes,
    mode_hint,
    favorite,
    state,
  }
}

fn mode_hint_label(hint: crate::gguf::metadata::ModeHint) -> String {
  use crate::gguf::metadata::ModeHint;
  match hint {
    ModeHint::Chat => "chat".into(),
    ModeHint::Embedding => "embedding".into(),
    ModeHint::Rerank => "rerank".into(),
    ModeHint::Unknown => "unknown".into(),
  }
}

// ── Table column geometry ────────────────────────────────────────
//
// The pane is wrapped in a bordered Block, so ratatui reserves 1
// column on each side. Inside the borders the rendered widget then
// reserves a 2-cell highlight gutter (for the `> ` selection
// arrow). What remains is the budget the table grid lives in.
//
// All five right-side columns are fixed width so headers align with
// values. The `Name` column flexes to fill whatever's left and is
// ellipsised when the discovered name overflows it.

// No external highlight symbol/gutter — rows render flush to the
// inner edge. The selection cursor (`=>`) is painted INSIDE the
// favorite-marker slot instead so we don't burn a column on a
// near-always-empty gutter.
const HIGHLIGHT_SYMBOL: &str = "";
const HIGHLIGHT_GUTTER: usize = 0;
const STATUS_W: usize = 3; // " X " (glyph between two spaces)
const FAV_W: usize = 3; // 2-cell `=>` / `★ ` / spaces, plus 1 trailing space
const COL_ARCH_W: usize = 8;
const COL_QUANT_W: usize = 7;
const COL_CTX_W: usize = 7;
const COL_SIZE_W: usize = 6;
const COL_MODE_W: usize = 10;
const COL_SEP_W: usize = 1; // space between columns
const RIGHT_COLS_W: usize =
  COL_ARCH_W + COL_QUANT_W + COL_CTX_W + COL_SIZE_W + COL_MODE_W + COL_SEP_W * 5;
/// Minimum number of columns the Name column always reserves.
const MIN_NAME_W: usize = 8;

/// Filter-input state for the Models block title. When the filter
/// is active the `/:filter` chip is replaced by an inline input
/// containing the buffered query; `focused=true` adds the block
/// cursor (rendered via `Modifier::REVERSED`).
#[derive(Debug, Clone, Copy)]
pub enum FilterTitle<'a> {
  Inactive,
  Active { buffer: &'a str, focused: bool },
}

/// Inputs to [`build_block_title`]. Bundled so the title call site
/// in `render` and `render_empty_state` doesn't drift; adding a new
/// piece of context (e.g. a "stale catalog" badge) only touches
/// one place.
#[derive(Debug, Clone, Copy)]
pub struct TitleInputs<'a> {
  pub total: usize,
  pub area_width: usize,
  pub filter: FilterTitle<'a>,
  /// Whether the cursor sits on a model in a "running"-ish state
  /// (Ready / Loading / Launching) — the `s:stop` hint is only
  /// shown when stopping the focused launch is meaningful.
  pub show_stop: bool,
}

/// Yellow border when the pane has keyboard focus, otherwise the
/// theme's `accent`. Re-used by the empty-state path in
/// `render.rs` so both surfaces share one focus rule.
pub fn border_color(palette: &Palette, focused: bool) -> Color {
  if focused {
    Color::Yellow
  } else {
    palette.accent
  }
}

/// Compose the bottom-edge status legend that explains every
/// surface-state glyph used in the row list. Rendered into the
/// block's bottom title so it's always visible without spending a
/// content row. Each glyph carries its semantic palette colour;
/// the labels are muted so the strip reads as a hint, not data.
fn build_status_legend(palette: &Palette) -> Line<'static> {
  use crate::tui::status_icons::{colour_for, glyph_for, SurfaceState};
  let entries: &[(SurfaceState, &str)] = &[
    (SurfaceState::Launching, "launching"),
    (SurfaceState::Loading, "loading"),
    (SurfaceState::Ready, "ready"),
    (SurfaceState::Error, "error"),
    (SurfaceState::Stopped, "stopped"),
    (SurfaceState::External, "external"),
  ];
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(entries.len() * 4 + 2);
  spans.push(Span::raw(" "));
  for (i, (state, label)) in entries.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" · ", Style::default().fg(palette.muted)));
    }
    spans.push(Span::styled(
      glyph_for(*state).to_string(),
      Style::default().fg(colour_for(*state, palette)),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
      (*label).to_string(),
      Style::default().fg(palette.muted),
    ));
  }
  spans.push(Span::raw(" "));
  Line::from(spans)
}

/// Render `rows` into the supplied area using the active palette.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  rows: &[ListRow],
  selected: usize,
  title: TitleInputs<'_>,
  palette: &Palette,
  focused: bool,
) {
  // Width inside the borders is `area.width - 2`. Subtract the
  // highlight gutter ratatui reserves for the selection marker
  // (`HIGHLIGHT_GUTTER` cells on every row, even unselected ones,
  // so columns stay column-aligned).
  let inner_w = area.width.saturating_sub(2) as usize;
  let content_w = inner_w.saturating_sub(HIGHLIGHT_GUTTER);
  let name_w = column_name_budget(content_w);

  let safe_selected = if rows.is_empty() {
    None
  } else {
    Some(selected.min(rows.len().saturating_sub(1)))
  };
  let items: Vec<ListItem<'_>> = rows
    .iter()
    .enumerate()
    .map(|(i, r)| {
      let is_selected = Some(i) == safe_selected;
      render_row(r, palette, name_w, content_w, is_selected)
    })
    .collect();
  let title_line = build_block_title(title, palette);
  let legend = build_status_legend(palette);
  let border_color = border_color(palette, focused);
  let list = List::new(items)
    .block(
      Block::default()
        .title(title_line)
        .title_bottom(legend)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color)),
    )
    // KDash-style row highlight: paint the focused row with a
    // saturated `highlight` background (gold/amber per theme) and
    // flip the foreground to the panel `bg` so the row reads with
    // strong contrast against the muted body. Themes that opt out
    // (mono: `highlight = Color::Reset`) fall back to plain
    // `Modifier::REVERSED` so the inverted-block idiom still works
    // without colour. Each `ListItem` leaves its cell fg unset so
    // the override here applies uniformly across the row.
    .highlight_style(highlight_style(palette))
    .highlight_symbol(HIGHLIGHT_SYMBOL);
  let mut state = ListState::default();
  state.select(safe_selected);
  frame.render_stateful_widget(list, area, &mut state);
}

/// KDash-style selected-row paint: a pure `REVERSED` inversion of
/// the row's own foreground. This is the "inverse of the row colour"
/// effect — a Ready (green) row paints a green bar with the panel
/// `bg` showing through the text; an Error (red) row paints red;
/// normal rows paint with `palette.fg`. Works uniformly across every
/// theme (including mono) because the swap is purely a modifier and
/// doesn't depend on a `highlight` colour slot.
fn highlight_style(_palette: &Palette) -> Style {
  Style::default()
    .add_modifier(Modifier::REVERSED)
    .add_modifier(Modifier::BOLD)
}

/// Decide how many cells the flexible Name column gets. When the
/// pane is wide enough to fit every right-side column, Name takes
/// whatever is left. When the pane is too narrow even for that,
/// Name shrinks to `MIN_NAME_W` and the columns spill — they get
/// clipped at the right border, which is the same way a too-narrow
/// pane has always behaved. Returning a usize keeps callers simple.
fn column_name_budget(content_w: usize) -> usize {
  let chrome = STATUS_W + FAV_W;
  let reserved = chrome.saturating_add(RIGHT_COLS_W);
  if content_w > reserved + MIN_NAME_W {
    content_w - reserved
  } else {
    MIN_NAME_W
  }
}

/// Build the Models block title as a styled `Line`. Order
/// (left-to-right):
///   ` Models [N] ` · filter slot · Enter:launch · [s:stop] · f:fav
///   · y:yank ·
///
/// The filter slot is either the `/:filter` hint (filter inactive)
/// or an inline `/ <buffer>` input (filter active). The input is
/// suffixed with a block cursor styled with `Modifier::REVERSED`
/// when `focused=true`.
///
/// `s:stop` is shown only when `show_stop` is true (i.e., the
/// selected row's model has a running launch to stop). Hints drop
/// from the tail (y:yank → f:fav → s:stop) on overflow; the count,
/// the filter slot, and `Enter:launch` are never dropped.
pub(crate) fn build_block_title(input: TitleInputs<'_>, palette: &Palette) -> Line<'static> {
  // The full title strip including borders consumes the whole top
  // edge. ratatui leaves 1 cell on each side for the corner glyphs.
  // Reserve one cell each side for the leading/trailing space the
  // title carries inside the block edge.
  let budget = input.area_width.saturating_sub(4);

  // Decide which static hints we'd ideally include. Order matches
  // display order; we drop from the tail under budget pressure.
  let mut hints: Vec<&'static str> = Vec::with_capacity(4);
  hints.push("Enter:launch");
  if input.show_stop {
    hints.push("s:stop");
  }
  hints.push("f:fav");
  hints.push("y:yank");

  // Filter slot text width (no styling here — we just need the cell
  // count for the budget calculation).
  let filter_text_width = match input.filter {
    FilterTitle::Inactive => "/:filter".chars().count(),
    FilterTitle::Active { buffer, focused } => {
      // `/ buffer` plus the cursor block when focused.
      "/ ".chars().count() + buffer.chars().count() + if focused { 1 } else { 0 }
    }
  };

  let count = format!("Models [{}]", input.total);
  // Trim hints from the tail until the line fits. `Enter:launch`
  // (always hints[0]) is never dropped — agents and new users rely
  // on the launch chip to bootstrap the keyboard surface.
  // Hint separator is ` · ` (3 cells) instead of double-space.
  loop {
    let mut width = 1; // leading space
    width += count.chars().count();
    width += 3; // ` · ` before filter slot
    width += filter_text_width;
    for h in &hints {
      width += 3 + h.chars().count();
    }
    width += 1; // trailing space
    if width <= budget || hints.len() <= 1 {
      break;
    }
    hints.pop();
  }

  // Now build the actual Line with styled spans.
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
  spans.push(Span::raw(" "));
  spans.push(Span::styled(
    count,
    Style::default()
      .fg(palette.panel_title)
      .add_modifier(Modifier::BOLD),
  ));
  spans.push(Span::styled(
    " · ".to_string(),
    Style::default().fg(palette.muted),
  ));

  // Filter slot. Inactive chip uses the same muted style as the
  // other hints so the title reads as a uniform hint strip.
  match input.filter {
    FilterTitle::Inactive => {
      spans.push(Span::styled(
        "/:filter".to_string(),
        Style::default().fg(palette.muted),
      ));
    }
    FilterTitle::Active { buffer, focused } => {
      spans.push(Span::styled(
        "/ ".to_string(),
        Style::default()
          .fg(palette.accent)
          .add_modifier(Modifier::BOLD),
      ));
      spans.push(Span::styled(
        buffer.to_string(),
        Style::default().fg(palette.fg),
      ));
      if focused {
        spans.push(Span::styled(
          "▏".to_string(),
          Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::REVERSED),
        ));
      }
    }
  }

  // Hint chips, separated by ` · `.
  for h in &hints {
    spans.push(Span::styled(
      " · ".to_string(),
      Style::default().fg(palette.muted),
    ));
    spans.push(Span::styled(
      (*h).to_string(),
      Style::default().fg(palette.muted),
    ));
  }
  spans.push(Span::raw(" "));

  Line::from(spans)
}

/// Left-aligned pad/truncate to `w` display columns. Truncated
/// strings end with `…` so overflow is visible.
fn cell(s: &str, w: usize) -> String {
  if w == 0 {
    return String::new();
  }
  let count = s.chars().count();
  if count <= w {
    let mut out = String::with_capacity(w);
    out.push_str(s);
    for _ in count..w {
      out.push(' ');
    }
    out
  } else {
    let keep = w.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
  }
}

/// Foreground colour for a model row, encoding launch state into the
/// row colour. Running (`Ready`) → success, failed (`Error`) →
/// error, everything else → default `fg`. Loading / Stopped /
/// External keep the default fg so the eye is drawn to the active
/// states only.
fn row_fg(state: SurfaceState, palette: &Palette) -> ratatui::style::Color {
  match state {
    SurfaceState::Ready => palette.success,
    SurfaceState::Error => palette.error,
    _ => palette.fg,
  }
}

fn render_row<'a>(
  row: &'a ListRow,
  palette: &Palette,
  name_w: usize,
  content_w: usize,
  is_selected: bool,
) -> ListItem<'a> {
  match row {
    ListRow::TableHeader => {
      // Label cells line up with model-row value cells: same widths,
      // same separators, same status/favorite gutter (rendered as
      // blanks). Header is bolded and tinted with the accent colour
      // to set it apart from group headers and model rows.
      let mut line = String::with_capacity(content_w);
      line.push_str(&" ".repeat(STATUS_W));
      line.push_str(&" ".repeat(FAV_W));
      line.push_str(&cell("Name", name_w));
      line.push(' ');
      line.push_str(&cell("Arch", COL_ARCH_W));
      line.push(' ');
      line.push_str(&cell("Quant", COL_QUANT_W));
      line.push(' ');
      line.push_str(&cell("Ctx", COL_CTX_W));
      line.push(' ');
      line.push_str(&cell("Size", COL_SIZE_W));
      line.push(' ');
      line.push_str(&cell("Mode", COL_MODE_W));
      ListItem::new(Line::from(Span::styled(
        line,
        Style::default()
          .fg(palette.label)
          .add_modifier(Modifier::BOLD),
      )))
    }
    ListRow::Header { label } => {
      // Group header (favorites or folder path). Render across the
      // full content width so it visibly separates groups; the label
      // is ellipsised if the directory path is longer than the pane.
      // Painted with `palette.label` (blue per theme) so folder
      // names match the in-pane label convention rather than
      // disappearing into the muted hint tone.
      let shown = cell(label.as_str(), content_w);
      ListItem::new(Line::from(Span::styled(
        shown,
        Style::default()
          .fg(palette.label)
          .add_modifier(Modifier::BOLD),
      )))
    }
    ListRow::Model {
      name,
      arch,
      quant,
      native_ctx,
      weights_bytes,
      mode_hint,
      favorite,
      state,
      ..
    } => {
      // The whole row carries a single semantic foreground via
      // `ListItem::style`. Spans for name/columns/separators leave
      // their fg unset so they inherit the row's colour — that way
      // when the row gets `Modifier::REVERSED` on selection, every
      // cell flips with the same source colour and the row reads
      // as one inverted block instead of cell-by-cell splotches.
      // The status glyph keeps its semantic colour so the launch
      // state stays scannable even on unselected rows.
      let fg = row_fg(*state, palette);
      let glyph = glyph_for(*state);
      let mut spans: Vec<Span<'a>> = Vec::with_capacity(7);
      spans.push(Span::styled(
        format!(" {glyph} "),
        Style::default().fg(colour_for(*state, palette)),
      ));
      // FAV_W (=3 cells) hosts either the selection cursor `=> ` or
      // the favorite star `★  ` / a blank `   `. The selection
      // cursor wins over the favorite mark — the row's REVERSED
      // selection style still flips colours so the row is
      // unambiguously selected; the favorite info is recoverable
      // from the `★ Favorites` section grouping.
      let (marker, marker_style) = if is_selected {
        // No explicit fg here so the marker inherits the row's
        // semantic colour from `ListItem::style().fg(fg)` below.
        // That way `REVERSED` flips the whole row (marker + name +
        // columns) with the same source colour, instead of the
        // marker drifting toward the accent palette.
        (
          "=> ".to_string(),
          Style::default().add_modifier(Modifier::BOLD),
        )
      } else if *favorite {
        ("★  ".to_string(), Style::default().fg(palette.warning))
      } else {
        ("   ".to_string(), Style::default())
      };
      spans.push(Span::styled(marker, marker_style));
      spans.push(Span::raw(cell(name.as_str(), name_w)));
      spans.push(Span::raw(" "));
      spans.push(Span::raw(cell(arch.as_str(), COL_ARCH_W)));
      spans.push(Span::raw(" "));
      spans.push(Span::raw(cell(quant.as_str(), COL_QUANT_W)));
      spans.push(Span::raw(" "));
      let ctx = native_ctx.map(format_tokens).unwrap_or_else(|| "—".into());
      spans.push(Span::raw(cell(&ctx, COL_CTX_W)));
      spans.push(Span::raw(" "));
      let size = weights_bytes
        .map(format_bytes)
        .unwrap_or_else(|| "—".into());
      spans.push(Span::raw(cell(&size, COL_SIZE_W)));
      spans.push(Span::raw(" "));
      spans.push(Span::raw(cell(mode_hint.as_str(), COL_MODE_W)));
      // `ListItem::style` is the canonical place to apply
      // `Modifier::REVERSED` to the *whole* row uniformly. Setting
      // fg here makes every unset-fg span (name + columns +
      // separator spaces) render in `fg`, so REVERSED flips the
      // whole strip in one go.
      ListItem::new(Line::from(spans)).style(Style::default().fg(fg))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

  fn fake(path: &str, parent: &str) -> DiscoveredModel {
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

  #[test]
  fn build_rows_starts_with_a_table_header_when_models_present() {
    let states = BTreeMap::new();
    let m = fake("/m/a.gguf", "/m");
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
    });
    assert_eq!(rows.first(), Some(&ListRow::TableHeader));
  }

  #[test]
  fn build_rows_empty_input_yields_no_rows_so_empty_state_can_render() {
    // The renderer in `render.rs` flips to `render_empty_state` when
    // `rendered_rows()` is empty; returning `[TableHeader]` here
    // would mask that hint with a stray column-label row.
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[],
      favorites: &[],
      model_states: &states,
    });
    assert!(rows.is_empty(), "no models → no rows at all");
  }

  #[test]
  fn table_header_is_not_selectable_and_carries_no_path() {
    let h = ListRow::TableHeader;
    assert!(!h.is_selectable(), "table header is a label, not a target");
    assert!(h.path().is_none());
  }

  #[test]
  fn favorites_section_only_appears_when_favorites_present() {
    let m = fake("/m/a.gguf", "/m");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
    });
    assert!(
      !rows
        .iter()
        .any(|r| matches!(r, ListRow::Header { label } if label.contains("Favorites"))),
      "no favorites = no favorites header"
    );
  }

  #[test]
  fn favorites_appear_first_below_table_header() {
    let a = fake("/m/a.gguf", "/m");
    let b = fake("/m/b.gguf", "/m");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[a.clone(), b.clone()],
      favorites: std::slice::from_ref(&a.path),
      model_states: &states,
    });
    // Row 0 is the table header; row 1 should be the favorites group.
    assert_eq!(rows[0], ListRow::TableHeader);
    let first_group = match &rows[1] {
      ListRow::Header { label } => label.clone(),
      _ => panic!("expected group header at row 1, got {:?}", rows[1]),
    };
    assert!(
      first_group.contains("Favorites"),
      "favorites header sits directly below the table header, got: {first_group}"
    );
    // The favorited model is row 2.
    let first_model = match &rows[2] {
      ListRow::Model { path, .. } => path.clone(),
      _ => panic!("expected model row at index 2"),
    };
    assert_eq!(first_model, a.path);
  }

  #[test]
  fn directory_groups_render_sorted_by_parent() {
    let a = fake("/m/x/a.gguf", "/m/x");
    let b = fake("/m/y/b.gguf", "/m/y");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[a, b],
      favorites: &[],
      model_states: &states,
    });
    let headers: Vec<String> = rows
      .iter()
      .filter_map(|r| match r {
        ListRow::Header { label } => Some(label.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(headers, vec!["/m/x".to_string(), "/m/y".to_string()]);
  }

  fn title_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
  }

  fn macchiato() -> &'static crate::theme::Palette {
    crate::theme::palette_for(crate::theme::ThemeName::Macchiato)
  }

  #[test]
  fn title_filter_hint_renders_before_other_hints() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Inactive,
        show_stop: true,
      },
      macchiato(),
    );
    let text = title_text(&title);
    let filter_at = text.find("/:filter").expect("filter chip");
    let enter_at = text.find("Enter:launch").expect("enter chip");
    assert!(
      filter_at < enter_at,
      "/:filter must come before Enter:launch: {text:?}"
    );
  }

  #[test]
  fn title_includes_count_and_full_hint_strip_when_filter_inactive() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Inactive,
        show_stop: true,
      },
      macchiato(),
    );
    let text = title_text(&title);
    assert!(text.contains("Models [127]"));
    assert!(text.contains("Enter:launch"));
    assert!(text.contains("/:filter"));
    assert!(text.contains("s:stop"));
    assert!(text.contains("f:fav"));
    assert!(text.contains("y:yank"));
  }

  #[test]
  fn title_renders_inline_filter_input_when_filter_active() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Active {
          buffer: "qwen",
          focused: true,
        },
        show_stop: true,
      },
      macchiato(),
    );
    let text = title_text(&title);
    assert!(
      text.contains("/ qwen"),
      "expected inline `/ qwen` input, got: {text:?}"
    );
    assert!(
      !text.contains("/:filter"),
      "inline input replaces the `/:filter` hint: {text:?}"
    );
    // The `Enter:launch` hint still shows.
    assert!(text.contains("Enter:launch"));
  }

  #[test]
  fn title_inline_input_carries_cursor_block_when_focused() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Active {
          buffer: "q",
          focused: true,
        },
        show_stop: true,
      },
      macchiato(),
    );
    let text = title_text(&title);
    // The cursor span is a single block glyph appended after the
    // buffer; the exact char is `▏` (U+258F) — assert any non-ASCII
    // block char follows the buffer.
    let after = text.split("/ q").nth(1).expect("split after buffer");
    assert!(
      after.chars().next().map(|c| !c.is_ascii()).unwrap_or(false),
      "expected a cursor glyph after the buffer, got: {after:?}"
    );
  }

  #[test]
  fn title_omits_s_stop_when_show_stop_false() {
    let title = build_block_title(
      TitleInputs {
        total: 3,
        area_width: 120,
        filter: FilterTitle::Inactive,
        show_stop: false,
      },
      macchiato(),
    );
    let text = title_text(&title);
    assert!(
      !text.contains("s:stop"),
      "s:stop must hide when no running launch is selected: {text:?}"
    );
    // The other hints still render.
    assert!(text.contains("Enter:launch"));
    assert!(text.contains("f:fav"));
  }

  #[test]
  fn title_drops_hints_right_to_left_under_pressure() {
    // A 40-col area can't fit the whole strip; the title builder
    // should drop hints from the tail (`y:yank` first, then `f:fav`).
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 40,
        filter: FilterTitle::Inactive,
        show_stop: true,
      },
      macchiato(),
    );
    let text = title_text(&title);
    assert!(
      text.contains("Enter:launch"),
      "must never drop launch chip: {text:?}"
    );
    assert!(
      text.contains("Models [127]"),
      "must never drop the count: {text:?}"
    );
    // `y:yank` should be dropped first.
    assert!(
      !text.contains("y:yank"),
      "expected y:yank dropped at 40 cols: {text:?}"
    );
  }

  #[test]
  fn surface_state_overlay_picks_up_running_supervisor_state() {
    let m = fake("/m/a.gguf", "/m");
    let mut states = BTreeMap::new();
    states.insert(m.path.clone(), SurfaceState::Ready);
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
    });
    let model_row = rows
      .iter()
      .find_map(|r| match r {
        ListRow::Model { state, .. } => Some(*state),
        _ => None,
      })
      .expect("model row");
    assert_eq!(model_row, SurfaceState::Ready);
  }

  #[test]
  fn cell_pads_short_values_to_width() {
    assert_eq!(cell("hi", 5), "hi   ");
  }

  #[test]
  fn cell_truncates_with_ellipsis_when_value_overflows() {
    // 5 cells: "abcde" → "abcd…"
    assert_eq!(cell("abcdef", 5), "abcd…");
  }

  #[test]
  fn cell_width_zero_yields_empty_string() {
    assert_eq!(cell("anything", 0), "");
  }

  #[test]
  fn row_fg_maps_ready_to_success_and_error_to_error() {
    use crate::theme::{palette_for, ThemeName};
    let p = palette_for(ThemeName::Macchiato);
    assert_eq!(row_fg(SurfaceState::Ready, p), p.success);
    assert_eq!(row_fg(SurfaceState::Error, p), p.error);
    // Default fg for the rest so colour stays semantic.
    assert_eq!(row_fg(SurfaceState::NotLaunched, p), p.fg);
    assert_eq!(row_fg(SurfaceState::Loading, p), p.fg);
    assert_eq!(row_fg(SurfaceState::Launching, p), p.fg);
    assert_eq!(row_fg(SurfaceState::Stopped, p), p.fg);
    assert_eq!(row_fg(SurfaceState::External, p), p.fg);
  }

  #[test]
  fn name_budget_reserves_columns_when_pane_is_wide() {
    // Pane wide enough for every right column plus generous Name.
    let big = column_name_budget(200);
    let chrome = STATUS_W + FAV_W;
    assert_eq!(big, 200 - chrome - RIGHT_COLS_W);
  }

  #[test]
  fn name_budget_falls_back_to_min_name_when_pane_is_narrow() {
    // 30-col content area is far below chrome + RIGHT_COLS_W + MIN.
    // Name floors at MIN_NAME_W; the right columns spill into the
    // border-clipped overflow (same behaviour as v0).
    assert_eq!(column_name_budget(30), MIN_NAME_W);
  }

  #[test]
  fn highlight_style_uses_reversed_so_selection_inverts_row_fg() {
    // KDash-style: the highlight is a pure REVERSED modifier, so a
    // Ready (green) row paints a green bar on selection, an Error
    // (red) row paints red, and normal rows paint with `palette.fg`.
    // No concrete bg/fg colours are pinned — that's what makes the
    // selection adopt the row's semantic colour uniformly.
    use crate::theme::{palette_for, ThemeName};
    for theme in [ThemeName::Macchiato, ThemeName::Latte, ThemeName::Mono] {
      let p = palette_for(theme);
      let style = highlight_style(p);
      assert_eq!(style.bg, None, "{theme:?} must not pin a bg colour");
      assert_eq!(style.fg, None, "{theme:?} must not pin a fg colour");
      assert!(style.add_modifier.contains(Modifier::REVERSED));
      assert!(style.add_modifier.contains(Modifier::BOLD));
    }
  }
}
