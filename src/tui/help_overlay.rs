//! Modal `?` help overlay listing every keybinding grouped by
//! category. Layout is fully derived from `DEFAULT_BINDINGS` —
//! each binding row carries its own `categories` list, and the
//! renderer walks every [`Category`] in order, collecting rows
//! whose binding lands in that section.
//!
//! The overlay packs the keybinding sections across three balanced
//! columns (one when narrow) and renders the static glyph/marker legend
//! full width below them. The whole page is a single scroll surface,
//! and the title carries an `↑↓:scroll` hint.
//!
//! Bindings come from `App::bindings_for` so config overrides flow
//! through automatically.

use std::collections::BTreeMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Binding, Category, Focus};

/// Render the overlay. Caller is responsible for only invoking
/// this when `app.show_help` is true. Layout is static — the active
/// focus does not change which sections appear or where.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let rect = crate::tui::layout::centered_abs(
    area,
    area.width.saturating_sub(4).min(130),
    area.height.saturating_sub(4).max(20),
    2,
    2,
  );
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  // Inner geometry is independent of the title text, so derive it from a
  // bare panel first; the column packing and scroll math need it before
  // the titled block is built.
  let inner = palette
    .panel()
    .padding(Padding::horizontal(1))
    .build()
    .inner(rect);

  let sections = build_sections(app);
  let legend = legend_section();
  // Keybindings pack into balanced columns (single column when narrow —
  // three thin columns just truncate every description); the legend
  // renders full width one blank line below the tallest column. The
  // whole thing is one scroll surface — `j`/`k` slide columns and legend
  // together.
  let n_cols: usize = if inner.width >= 80 { 3 } else { 1 };
  let columns = balance_into_columns(&sections, n_cols);
  let tallest = columns
    .iter()
    .map(|col| column_height(col))
    .max()
    .unwrap_or(0);
  let legend_lines = render_legend(&legend, palette);
  // `column_height` already counts a trailing blank after the last
  // section, so the legend starts at `tallest` with one blank gap.
  let legend_top = tallest;
  let content_height = (legend_top + legend_lines.len()) as u16;
  // Clamp so an over-advanced `help_scroll` can't run past the content.
  let max_scroll = content_height.saturating_sub(inner.height);
  let scroll_y = app.help_scroll.min(max_scroll);

  // Title carries the close and scroll chips. The close chip lists both
  // dismiss keys: `Esc` (hardcoded modal-dismiss in `events::handle_key`,
  // not an Action, so always works) and the `toggle_help` live key
  // (default `?`). Scroll keys come from the keymap too, so config
  // overrides flow through.
  let toggle_key = app
    .bindings_for(Focus::List)
    .iter()
    .find(|b| b.action == Action::ToggleHelp)
    .map(|b| b.label.to_string())
    .unwrap_or_else(|| "?".to_string());
  let block = palette
    .panel()
    .title(Line::from(vec![
      Span::styled(
        " Help ",
        Style::default()
          .fg(palette.panel_title)
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(format!("· Esc/{toggle_key}:close "), palette.muted_style()),
      Span::styled(
        format!("· {}:scroll ", scroll_hint_label(app)),
        palette.muted_style(),
      ),
    ]))
    .padding(Padding::horizontal(1))
    .build();
  frame.render_widget(block, rect);

  // Keybinding columns fill the inner area, scrolled as one.
  let constraints: Vec<Constraint> = (0..n_cols)
    .map(|_| Constraint::Ratio(1, n_cols as u32))
    .collect();
  let cols = Layout::default()
    .direction(Direction::Horizontal)
    .constraints(constraints)
    .split(inner);
  for (idx, col_sections) in columns.iter().enumerate() {
    frame.render_widget(
      Paragraph::new(render_column(col_sections, palette))
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0)),
      cols[idx],
    );
  }

  // Legend flows below the columns, sharing the same scroll offset.
  // `legend_rel` is its top row relative to the viewport top: positive
  // means it sits below the fold, negative means it's partly scrolled
  // off the top (then the paragraph's own scroll trims the hidden rows).
  let legend_rel = legend_top as i32 - scroll_y as i32;
  if legend_rel < inner.height as i32 {
    let (y, inner_scroll, height) = if legend_rel >= 0 {
      (
        inner.y + legend_rel as u16,
        0u16,
        inner.height - legend_rel as u16,
      )
    } else {
      (inner.y, (-legend_rel) as u16, inner.height)
    };
    frame.render_widget(
      Paragraph::new(legend_lines)
        .wrap(Wrap { trim: false })
        .scroll((inner_scroll, 0)),
      Rect::new(inner.x, y, inner.width, height),
    );
  }
}

/// Scroll affordance label for the help title, derived from the live
/// `MoveUp`/`MoveDown` bindings (the overlay also accepts arrows / `j`
/// / `k` directly). Mirrors the top-bar `↑↓:scroll` chip's no-separator
/// join so the two read identically.
fn scroll_hint_label(app: &App) -> String {
  let binds = app.bindings_for(Focus::List);
  let label = |action: Action, fallback: &'static str| {
    binds
      .iter()
      .find(|b| b.action == action)
      .map(|b| b.label)
      .unwrap_or(fallback)
  };
  format!(
    "{}{}",
    label(Action::MoveUp, "↑"),
    label(Action::MoveDown, "↓")
  )
}

/// Total line count a column would render at: title + one line per
/// row + one blank trailer, summed across every section.
fn column_height(sections: &[&Section]) -> usize {
  sections.iter().map(|s| s.rows.len() + 2).sum()
}

/// One help-overlay section: a category title plus the resolved
/// rows (each a `(keys, description)` pair) drawn under it.
#[derive(Debug)]
struct Section {
  title: &'static str,
  rows: Vec<(String, String)>,
}

/// Walk `Category::ALL` in order; for each, collect every binding
/// whose `categories` contains that category. Bindings collapse to
/// one row only when they share BOTH the same action and the same
/// effective description — so `↑` and `k` merge into `↑,k → up/prev`
/// (same action `MoveUp`, same description), but `Esc` and `M` stay
/// separate (both `FocusList`, but Esc reads "back/cancel/clear/exit
/// edit" while M reads "models list"). Sections with no rows drop.
fn build_sections(app: &App) -> Vec<Section> {
  let mut sections: Vec<Section> = Vec::with_capacity(Category::ALL.len());
  let flat: Vec<&Binding> = collect_flat(app);

  for &category in Category::ALL {
    // Group bindings landing in this category by (action, effective
    // description), preserving first-seen order. Same-action bindings
    // with diverging descriptions land in separate rows.
    let mut row_order: Vec<(Action, String)> = Vec::new();
    let mut by_row: BTreeMap<usize, Vec<&Binding>> = BTreeMap::new();
    for b in &flat {
      if !b.categories.contains(&category) {
        continue;
      }
      let description = b
        .action
        .description_for(category)
        .map(|s| s.to_string())
        .unwrap_or_else(|| b.description().to_string());
      let key = (b.action, description);
      let pos = row_order.iter().position(|k| *k == key);
      let idx = match pos {
        Some(i) => i,
        None => {
          row_order.push(key);
          row_order.len() - 1
        }
      };
      by_row.entry(idx).or_default().push(b);
    }

    let mut rows: Vec<(String, String)> = Vec::with_capacity(row_order.len());
    for (idx, (_, description)) in row_order.iter().enumerate() {
      let group = &by_row[&idx];
      let keys = group.iter().map(|b| b.label).collect::<Vec<_>>().join(",");
      rows.push((keys, description.clone()));
    }
    if rows.is_empty() {
      continue;
    }
    sections.push(Section {
      title: category.label(),
      rows,
    });
  }
  sections
}

/// Static legend explaining glyphs used in panel labels: the `*`
/// suffix on the Host panel's `MEM` row (unified memory — Apple Metal /
/// AMD UMA APUs where the GPU draws from the same physical pool, so the
/// `VRAM` row is the GPU's view of that same memory, not an additional
/// pool), and the `◉`/`♪` modality glyphs the right-pane title carries
/// when a model has an auto-detected mmproj projector.
fn legend_section() -> Section {
  let mut rows = vec![
    (
      "MEM*".to_string(),
      "unified memory (VRAM is the GPU's view of this pool)".to_string(),
    ),
    (
      "GPU*".to_string(),
      "combined usage + hottest temp across multiple GPUs".to_string(),
    ),
    (
      format!(
        "RAM/CPU/port{}",
        crate::tui::right_pane::SHARED_UMBRELLA_MARKER
      ),
      "shared Lemonade umbrella (one process serves all Lemonade models)".to_string(),
    ),
  ];
  for (glyph, desc) in crate::discovery::Multimodal::LEGEND {
    rows.push((glyph.to_string(), desc.to_string()));
  }
  let (mtp_glyph, mtp_desc) = crate::discovery::MTP_LEGEND;
  rows.push((mtp_glyph.to_string(), mtp_desc.to_string()));
  Section {
    title: "Legend",
    rows,
  }
}

/// Snapshot the full keymap as a flat slice, deduplicating bindings
/// that appear under multiple focuses (the per-focus cache stores
/// the same binding once per focus).
fn collect_flat(app: &App) -> Vec<&Binding> {
  let mut seen: Vec<(
    crossterm::event::KeyCode,
    crossterm::event::KeyModifiers,
    Action,
  )> = Vec::new();
  let mut out: Vec<&Binding> = Vec::new();
  for focus in [
    Focus::List,
    Focus::Filter,
    Focus::RightPane,
    Focus::ChatInput,
    Focus::EmbedInput,
    Focus::RerankInput,
    Focus::ConfirmPopup,
    Focus::HfDialog,
  ] {
    for b in app.bindings_for(focus) {
      let key = (b.key, b.mods, b.action);
      if !seen.contains(&key) {
        seen.push(key);
        out.push(b);
      }
    }
  }
  out
}

/// Greedy column-packing: distribute `sections` into `n` columns so
/// the columns are roughly the same length (one line per row plus
/// title + blank trailer). Picks the shortest column at each step.
fn balance_into_columns(sections: &[Section], n: usize) -> Vec<Vec<&Section>> {
  let mut columns: Vec<Vec<&Section>> = vec![Vec::new(); n];
  let mut lengths: Vec<usize> = vec![0; n];
  for section in sections {
    let target = lengths
      .iter()
      .enumerate()
      .min_by_key(|(_, &l)| l)
      .map(|(i, _)| i)
      .unwrap_or(0);
    columns[target].push(section);
    // 1 title row + 1 row per binding + 1 trailing blank.
    lengths[target] += section.rows.len() + 2;
  }
  columns
}

fn render_column(sections: &[&Section], palette: &Palette) -> Vec<Line<'static>> {
  let mut out: Vec<Line<'static>> = Vec::new();
  for section in sections {
    out.push(Line::from(Span::styled(
      section.title.to_string(),
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    )));
    for (keys, description) in &section.rows {
      out.push(render_binding_line(keys, description, palette));
    }
    out.push(Line::default());
  }
  out
}

/// Render the legend for its dedicated bottom row: title plus one line
/// per entry, with no trailing blank so the row hugs the legend's exact
/// height.
fn render_legend(legend: &Section, palette: &Palette) -> Vec<Line<'static>> {
  let mut out: Vec<Line<'static>> = Vec::with_capacity(legend.rows.len() + 1);
  out.push(Line::from(Span::styled(
    legend.title.to_string(),
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD),
  )));
  for (keys, description) in &legend.rows {
    out.push(render_binding_line(keys, description, palette));
  }
  out
}

fn render_binding_line(keys: &str, description: &str, palette: &Palette) -> Line<'static> {
  Line::from(vec![
    Span::styled("  ".to_string(), Style::default()),
    Span::styled(format!("{keys:<12}"), Style::default().fg(palette.label)),
    Span::styled("  ".to_string(), Style::default()),
    Span::styled(description.to_string(), palette.text_style()),
  ])
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  fn render_to_string(width: u16, height: u16, app: &App) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
      .draw(|frame| {
        let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
        render(frame, frame.area(), app, palette);
      })
      .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut s = String::new();
    for y in 0..buffer.area.height {
      for x in 0..buffer.area.width {
        s.push_str(buffer[(x, y)].symbol());
      }
      s.push('\n');
    }
    s
  }

  #[test]
  fn overlay_renders_global_section() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("General"),
      "missing General section:\n{frame}"
    );
    assert!(frame.contains("quit"), "missing quit row:\n{frame}");
    assert!(frame.contains("help"), "missing help row:\n{frame}");
  }

  #[test]
  fn overlay_lists_motion_only_under_global() {
    // Motion (`↑/↓/k/j`) lives in Global as a single merged row —
    // category-specific overrides for Logs/Settings have been
    // retired. The Logs/Settings sections still exist with their
    // own non-motion chords.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("General"),
      "General section missing:\n{frame}"
    );
    assert!(
      frame.contains("up/prev") && frame.contains("down/next"),
      "Global motion descriptions missing:\n{frame}"
    );
    // Rerank's NextField/PrevField bindings still surface "prev field"
    // in the Rerank section — assert only the Logs override is gone.
    assert!(
      !frame.contains("scroll up") && !frame.contains("scroll down"),
      "Logs motion overrides should be gone:\n{frame}"
    );
  }

  #[test]
  fn overlay_merges_aliased_chords() {
    // `c` and `y` both bind to YankCurl with the same description; the
    // Models section should surface them on one row as `c,y` rather
    // than two separate rows.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("c,y"),
      "aliased yank chord missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_shows_mem_star_legend() {
    // The Host panel marks unified-memory machines with `MEM*` — the
    // help overlay's Legend section is the only place that explains
    // what the star means.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(frame.contains("Legend"), "Legend section missing:\n{frame}");
    assert!(
      frame.contains("MEM*") && frame.contains("unified memory"),
      "MEM* legend row missing:\n{frame}"
    );
    // The Host panel collapses multi-GPU hosts to a `GPU*` row; the
    // Legend is where the marker is explained.
    assert!(
      frame.contains("GPU*") && frame.contains("combined usage"),
      "GPU* legend row missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_shows_modality_glyph_legend() {
    // The right-pane title carries `◉`/`♪` when a model has an mmproj
    // projector; the Legend section explains the glyphs.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains('◉') && frame.contains("vision"),
      "vision glyph legend row missing:\n{frame}"
    );
    assert!(
      frame.contains('♪') && frame.contains("audio"),
      "audio glyph legend row missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_shows_hf_pull_under_global() {
    // Shift+P (`OpenHfDialog`) is categorised as Global since it
    // works from any non-input focus.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(frame.contains("pull"), "pull row missing:\n{frame}");
  }

  #[test]
  fn esc_and_shift_m_render_as_separate_rows() {
    // Esc (RIGHT_PANE → FocusList) and Shift+M (NAV → FocusList) share
    // the same action but carry different descriptions — the help
    // overlay must keep them on separate rows so the Esc line reads
    // "cancel/clear/back", not "models list".
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("cancel/clear/back"),
      "Esc consolidated description missing:\n{frame}"
    );
    assert!(
      frame.contains("models list"),
      "Shift+M models list row missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_merges_chat_embed_rerank_into_single_section() {
    // The three input tabs collapse into one `Chat/Embed/Rerank`
    // section. Enter reads "submit" (no per-tab override); the rerank
    // ↑/↓ field-cycle rows and the chat-specific `send chat` text
    // are hidden so the section stays a focused list of shared chords.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("Chat/Embed/Rerank"),
      "merged section title missing:\n{frame}"
    );
    assert!(
      !frame.contains("Chat tab") && !frame.contains("Embed tab") && !frame.contains("Rerank tab"),
      "old per-tab headings should be gone:\n{frame}"
    );
    assert!(
      !frame.contains("send chat") && !frame.contains("send embed") && !frame.contains("query/add"),
      "per-tab Enter overrides should be gone:\n{frame}"
    );
    assert!(
      !frame.contains("next field") && !frame.contains("prev field"),
      "rerank field-cycle rows should not surface here:\n{frame}"
    );
  }

  #[test]
  fn overlay_lists_hf_dialog_stage_chords() {
    // `o` (sort), `n` (next page), `p` (prev page) live in events.rs
    // but surface in the help overlay via display-only bindings.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("HF pull dialog"),
      "HF section missing:\n{frame}"
    );
    assert!(
      frame.contains("cycle sort order"),
      "sort row missing:\n{frame}"
    );
    assert!(
      frame.contains("next page"),
      "next-page row missing:\n{frame}"
    );
    assert!(
      frame.contains("prev page"),
      "prev-page row missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_renders_legend_below_keybinding_columns() {
    // The legend renders full width below the keybinding columns. On a
    // terminal tall enough to fit everything, the "Legend" line sits
    // strictly below each category title.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 60, &app);
    let lines: Vec<&str> = frame.lines().collect();
    let legend_y = lines
      .iter()
      .position(|l| l.contains("Legend"))
      .expect("Legend row present");
    for title in ["General", "Models", "Settings"] {
      let y = lines
        .iter()
        .position(|l| l.contains(title))
        .unwrap_or_else(|| panic!("{title} section present:\n{frame}"));
      assert!(
        y < legend_y,
        "{title} (y={y}) must render above the legend (y={legend_y}):\n{frame}"
      );
    }
  }

  #[test]
  fn overlay_title_carries_scroll_hint() {
    // The title advertises the scroll affordance next to the close chip,
    // with the key label derived from the keymap (default `↑↓`).
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("↑↓:scroll"),
      "help title must carry the scroll hint:\n{frame}"
    );
  }

  #[test]
  fn overlay_scroll_reveals_legend_on_short_terminal() {
    // On a terminal too short to show the legend at rest, scrolling the
    // page down brings the legend into view (it shares the columns'
    // scroll offset rather than pinning to the bottom).
    let mut app = App::new(AppOptions::default());
    let top = render_to_string(140, 16, &app);
    assert!(
      !top.contains("unified memory"),
      "legend should be below the fold at scroll=0:\n{top}"
    );
    app.help_scroll = 200;
    let bottom = render_to_string(140, 16, &app);
    assert!(
      bottom.contains("unified memory"),
      "scrolling down must reveal the legend:\n{bottom}"
    );
  }

  #[test]
  fn overlay_falls_back_to_single_column_on_narrow_widths() {
    // Below the 80-cell threshold the renderer collapses to a single
    // column so each row prints in full — three thin columns would
    // truncate every description.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(60, 36, &app);
    // General section still surfaces; the Esc row fits on one line
    // because the column is now full-width.
    assert!(frame.contains("General"), "General missing:\n{frame}");
    assert!(
      frame.contains("cancel/clear/back"),
      "Esc row missing under single-column:\n{frame}"
    );
  }

  #[test]
  fn overlay_merges_vim_aliases_into_their_semantic_rows() {
    // Vim aliases sit on the same row as the canonical chord they
    // alias, never in a separate "Vim" section. Tab/l/gt share the
    // NextFocus row in General; PgDn/Ctrl+F share the page-down row
    // in Models list; g/Home/0 share the GoTop row; G/End/$ share
    // the GoBottom row. The merge keeps muscle-memory users covered
    // without bloating the layout with a redundant section.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 60, &app);
    // Ctrl-letter labels diverge by platform: `ctrl_label!` emits
    // `⌃f`/`⌃b` on macOS and `Ctrl+f`/`Ctrl+b` elsewhere. Match the
    // live convention so this test stays green on every CI lane.
    // `e,i` covers the vim `i` alias for `EnterEdit` — must appear
    // as a merged row, not a separate one. `Ctrl+u` is intentionally
    // NOT asserted here. Vim's Ctrl+U is half-page-up, but llamastash
    // collapses it to full PgUp (same as Ctrl+B), so listing both in
    // help next to PgUp would just duplicate the row. It stays
    // dispatchable (muscle memory keeps working) but hidden via
    // NO_CAT in `keybindings.rs`.
    let ctrl_f = crate::ctrl_label!("f");
    let ctrl_b = crate::ctrl_label!("b");
    for needle in [ctrl_f, ctrl_b, "0", "$", "gt", "gT", "e,i"] {
      assert!(
        frame.contains(needle),
        "vim chord {needle:?} missing from help overlay:\n{frame}"
      );
    }
    assert!(
      !frame.contains("Vim aliases"),
      "vim chords must NOT live in a dedicated section — they belong on their semantic row:\n{frame}"
    );
  }

  #[test]
  fn overlay_scrolls_when_help_scroll_advanced() {
    // A short terminal can't fit every section. Advancing `help_scroll`
    // should slide the content up so later rows appear and earlier
    // rows leave the viewport.
    let mut app = App::new(AppOptions::default());
    let frame_top = render_to_string(80, 14, &app);
    assert!(
      frame_top.contains("General"),
      "General must be visible at scroll=0:\n{frame_top}"
    );
    app.help_scroll = 30;
    let frame_bottom = render_to_string(80, 14, &app);
    assert_ne!(
      frame_top, frame_bottom,
      "advancing help_scroll must change the rendered viewport"
    );
  }
}
