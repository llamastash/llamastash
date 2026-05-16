//! Model list rows for the left pane.
//!
//! Two responsibilities live here:
//! 1. Convert the daemon-side `DiscoveredModel` + favorites + active
//!    launches into a flat list of [`ListRow`]s grouped by section
//!    (favorites first, then by parent directory).
//! 2. Render the rows into a ratatui `List` widget with theme-aware
//!    glyphs and colours.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::discovery::DiscoveredModel;
use crate::theme::Palette;
use crate::tui::status_icons::{colour_for, glyph_for, SurfaceState};

/// A row as it appears in the rendered list — section header or
/// model row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
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
    /// known. Rendered as a `· 4.2G` badge alongside the quant /
    /// native-ctx columns so users can size models at a glance
    /// (plan: "est-mem badge"). KV-aware variants land at launch
    /// picker time, not in the always-on list row.
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
      ListRow::Header { .. } => None,
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

/// Group `models` into the section ordering the plan calls for:
/// `★ Favorites` first (only when at least one favorite is in the
/// list), then alphabetical-by-parent groups. Within each group
/// rows are sorted by display name so the layout is stable across
/// rescans.
pub fn build_rows(inputs: RowInputs<'_>) -> Vec<ListRow> {
  let mut rows: Vec<ListRow> = Vec::with_capacity(inputs.models.len() + 4);
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

/// Render a weights footprint in the most compact human-friendly
/// unit (GiB-or-MiB). Aims for the same readout the plan's `· 4.2G`
/// example shows — single-letter unit suffix, one fractional digit
/// for sub-10 G values so 4.2G doesn't collapse to 4G.
fn format_bytes(bytes: u64) -> String {
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

fn mode_hint_label(hint: crate::gguf::metadata::ModeHint) -> String {
  use crate::gguf::metadata::ModeHint;
  match hint {
    ModeHint::Chat => "chat".into(),
    ModeHint::Embedding => "embedding".into(),
    ModeHint::Rerank => "rerank".into(),
    ModeHint::Unknown => "unknown".into(),
  }
}

/// Inputs to the Models pane block title.
pub struct TitleInputs<'a> {
  /// Total models discovered (not "matched after filter").
  pub total: usize,
  /// Active filter query, if any. When `Some`, the `[/query]` chip
  /// replaces the `/:filter` hint chip in the strip.
  pub filter: Option<&'a str>,
}

/// Render `rows` into the supplied area using the active palette.
/// `selected` is the index in `rows` (NOT in the model list) the
/// user is currently focused on. `title` carries the dynamic content
/// (count + filter chip + hint strip).
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  rows: &[ListRow],
  selected: usize,
  title: TitleInputs<'_>,
  palette: &Palette,
) {
  // Reserve columns for borders (2), the highlight gutter (2), the
  // status glyph (3), and the favorite mark (2). What remains is the
  // budget the name + inline badges share; we use it to ellipsise
  // overlong model names so they don't get silently clipped by
  // ratatui without a visible signal.
  const ROW_CHROME: u16 = 2 + 2 + 3 + 2;
  let name_budget = (area.width.saturating_sub(ROW_CHROME)) as usize;
  let items: Vec<ListItem<'_>> = rows
    .iter()
    .map(|r| render_row(r, palette, name_budget))
    .collect();
  let title_str = build_block_title(&title, area.width as usize);
  let list = List::new(items)
    .block(
      Block::default()
        .title(title_str)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent)),
    )
    .highlight_style(
      Style::default()
        .bg(palette.selection)
        .fg(palette.fg)
        .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("> ");
  let mut state = ListState::default();
  let safe = if rows.is_empty() {
    None
  } else {
    Some(selected.min(rows.len().saturating_sub(1)))
  };
  state.select(safe);
  frame.render_stateful_widget(list, area, &mut state);
}

/// Build the dynamic Models block title:
///
/// * filter inactive: ` Models [127]  Enter:launch  /:filter  s:stop  f:fav  y:yank `
/// * filter active:   ` Models [127]  [/qwen]  Enter:launch  s:stop  f:fav  y:yank `
///
/// On overflow, drop hint chips right-to-left in this priority:
/// `y:yank` → `f:fav` → `s:stop` → `/:filter`. `Enter:launch`, the
/// count, and the filter chip are never dropped.
pub(crate) fn build_block_title(inputs: &TitleInputs<'_>, area_width: usize) -> String {
  // The full title strip including borders consumes the whole top
  // edge. ratatui leaves 1 cell on each side for the corner glyphs,
  // so the usable budget is `area_width - 2`. Subtract another 2 for
  // the leading/trailing space inside the title string.
  let budget = area_width.saturating_sub(4);

  let count = format!("Models [{}]", inputs.total);
  let filter_chip = inputs.filter.map(|q| format!("[/{}]", q));

  // Hints in display order. `/:filter` is suppressed when the filter
  // is already active (the `[/query]` chip takes its slot).
  let mut hints: Vec<&'static str> = Vec::with_capacity(5);
  hints.push("Enter:launch");
  if filter_chip.is_none() {
    hints.push("/:filter");
  }
  hints.push("s:stop");
  hints.push("f:fav");
  hints.push("y:yank");

  // Truncate right-to-left: drop hints from the tail until the
  // assembled title fits.
  loop {
    let candidate = format_title(&count, filter_chip.as_deref(), &hints);
    if candidate.chars().count() <= budget || hints.len() <= 1 {
      return format!(" {candidate} ");
    }
    hints.pop();
  }
}

fn format_title(count: &str, filter_chip: Option<&str>, hints: &[&str]) -> String {
  let mut out = String::with_capacity(count.len() + 64);
  out.push_str(count);
  if let Some(chip) = filter_chip {
    out.push_str("  ");
    out.push_str(chip);
  }
  for hint in hints {
    out.push_str("  ");
    out.push_str(hint);
  }
  out
}

/// Truncate `s` to fit `budget` columns, appending `…` if anything was
/// dropped. Returns the original string unmodified when it already fits.
fn ellipsise(s: &str, budget: usize) -> std::borrow::Cow<'_, str> {
  if budget == 0 || s.chars().count() <= budget {
    return std::borrow::Cow::Borrowed(s);
  }
  let keep = budget.saturating_sub(1);
  let mut out = String::with_capacity(keep + 3);
  out.extend(s.chars().take(keep));
  out.push('…');
  std::borrow::Cow::Owned(out)
}

fn render_row<'a>(row: &'a ListRow, palette: &Palette, name_budget: usize) -> ListItem<'a> {
  match row {
    ListRow::Header { label } => ListItem::new(Line::from(vec![Span::styled(
      label.as_str(),
      Style::default()
        .fg(palette.muted)
        .add_modifier(Modifier::BOLD),
    )]))
    .style(Style::default().fg(palette.muted)),
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
      let mut spans: Vec<Span<'a>> = Vec::with_capacity(8);
      // Status glyph + favorite mark (always two slots so columns
      // line up across rows).
      let glyph = glyph_for(*state);
      spans.push(Span::styled(
        format!(" {glyph} "),
        Style::default().fg(colour_for(*state, palette)),
      ));
      spans.push(Span::styled(
        if *favorite { "★ " } else { "  " },
        Style::default().fg(palette.warning),
      ));
      // Display name. Ellipsise on overflow so a narrow pane gives a
      // visible signal instead of silently clipping at the border.
      let shown = ellipsise(name.as_str(), name_budget);
      spans.push(Span::styled(
        shown.into_owned(),
        Style::default().fg(palette.fg),
      ));
      // Arch badge.
      if !arch.is_empty() {
        spans.push(Span::styled(
          format!("  {arch}"),
          Style::default().fg(palette.accent),
        ));
      }
      // Quant badge.
      if !quant.is_empty() {
        spans.push(Span::styled(
          format!(" · {quant}"),
          Style::default().fg(palette.muted),
        ));
      }
      // Native ctx.
      if let Some(ctx) = native_ctx {
        spans.push(Span::styled(
          format!(" · {ctx}"),
          Style::default().fg(palette.muted),
        ));
      }
      // Est-mem badge (weights footprint). KV cache is launch-time
      // and shows up in the launch picker, not here.
      if let Some(bytes) = weights_bytes {
        spans.push(Span::styled(
          format!(" · {}", format_bytes(*bytes)),
          Style::default().fg(palette.muted),
        ));
      }
      // Mode hint.
      spans.push(Span::styled(
        format!(" · {mode_hint}"),
        Style::default().fg(palette.muted),
      ));
      ListItem::new(Line::from(spans))
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
  fn empty_input_produces_no_rows() {
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[],
      favorites: &[],
      model_states: &states,
    });
    assert!(rows.is_empty());
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
  fn favorites_appear_first() {
    let a = fake("/m/a.gguf", "/m");
    let b = fake("/m/b.gguf", "/m");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[a.clone(), b.clone()],
      favorites: std::slice::from_ref(&a.path),
      model_states: &states,
    });
    let first_header = rows
      .iter()
      .find_map(|r| match r {
        ListRow::Header { label } => Some(label.clone()),
        _ => None,
      })
      .expect("at least one header");
    assert!(
      first_header.contains("Favorites"),
      "favorites header must come first, got: {first_header}"
    );
    let first_model = rows
      .iter()
      .find_map(|r| match r {
        ListRow::Model { path, .. } => Some(path.clone()),
        _ => None,
      })
      .expect("at least one model");
    assert_eq!(first_model, a.path, "favorited model rendered first");
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

  #[test]
  fn title_includes_count_and_full_hint_strip_when_filter_inactive() {
    let title = build_block_title(
      &TitleInputs {
        total: 127,
        filter: None,
      },
      120,
    );
    assert!(title.contains("Models [127]"));
    assert!(title.contains("Enter:launch"));
    assert!(title.contains("/:filter"));
    assert!(title.contains("s:stop"));
    assert!(title.contains("f:fav"));
    assert!(title.contains("y:yank"));
  }

  #[test]
  fn title_swaps_filter_hint_for_chip_when_filter_active() {
    let title = build_block_title(
      &TitleInputs {
        total: 127,
        filter: Some("qwen"),
      },
      120,
    );
    assert!(title.contains("[/qwen]"));
    assert!(
      !title.contains("/:filter"),
      "filter chip and `/:filter` hint must not coexist: {title:?}"
    );
    // Other hints still present.
    assert!(title.contains("Enter:launch"));
    assert!(title.contains("s:stop"));
  }

  #[test]
  fn title_drops_hints_right_to_left_under_pressure() {
    // A 40-col area can't fit the whole strip; the title builder
    // should drop hints from the tail (`y:yank` first, then `f:fav`).
    let title = build_block_title(
      &TitleInputs {
        total: 127,
        filter: None,
      },
      40,
    );
    assert!(
      title.contains("Enter:launch"),
      "must never drop launch chip: {title:?}"
    );
    assert!(
      title.contains("Models [127]"),
      "must never drop the count: {title:?}"
    );
    // `y:yank` should be dropped first.
    assert!(
      !title.contains("y:yank"),
      "expected y:yank dropped at 40 cols: {title:?}"
    );
  }

  #[test]
  fn title_never_drops_filter_chip() {
    // Even at very narrow widths the `[/query]` chip stays — it
    // shares a logical slot with the `/:filter` hint, and dropping
    // the chip would lose the active filter signal.
    let title = build_block_title(
      &TitleInputs {
        total: 5,
        filter: Some("qwen"),
      },
      28,
    );
    assert!(
      title.contains("[/qwen]"),
      "filter chip must survive any width: {title:?}"
    );
    assert!(
      title.contains("Enter:launch"),
      "launch chip must survive: {title:?}"
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
}
