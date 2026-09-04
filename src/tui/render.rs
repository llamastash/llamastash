//! Single-frame renderer (kdash-style dashboard layout).
//!
//! Vertical:
//! 1. **Title row** (1 line) — `LlamaStash v0.1.0 · ● daemon` left,
//!    global hint strip (`?:help  t:theme  /:filter  q:quit`) right.
//!    Both styled with `palette.accent` background and `palette.bg`
//!    foreground.
//! 2. **Info row** (7 lines) — `Host` (fixed 32 cols), `Daemon` (flex
//!    middle), `Logo` (fixed ~25 cols when there's room). Skipped
//!    entirely when `area.height < 18`.
//! 3. **Body** — Models pane (60%) + right pane with tab strip (40%).
//! 4. **Filter input** (1 line) — only rendered when
//!    `Focus::Filter`. Sits above the body's last row.
//!
//! No bottom help bar — panel-specific hints live in each panel's
//! block title. The global hint strip is on row 1.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::{App, ToastKind};
use crate::tui::keybindings::{Action, Focus};
use crate::tui::{
  confirm_overlay, help_bar, help_overlay, host_stats_pane, info_pane, list_pane, logo_pane,
  right_pane,
};

const INFO_ROW_HEIGHT: u16 = 7;
/// Threshold below which the info row (Host + Daemon + Logo) hides so
/// the Models pane gets every row. Sits above [`MIN_RENDER_HEIGHT`]
/// so the gradient is `< 20 → placeholder`, `20–23 → title + body`,
/// `≥ 24 → title + info row + body`.
const MIN_HEIGHT_FOR_INFO_ROW: u16 = 24;
const HOST_PANEL_WIDTH: u16 = 25;
/// Lower bound on what `render()` will paint a full dashboard into.
/// Matches the `--render-size` parser's minimum (60×20). Anything
/// smaller renders the placeholder instead so a sub-minimum terminal
/// doesn't silently clip every panel. 60 cells is the
/// "compact" floor — below 100 the right pane hides by default and
/// the left list runs the marker column + a generous Name column
/// (data columns drop by rank — see `list_pane::layout_columns`).
const MIN_RENDER_WIDTH: u16 = 60;
const MIN_RENDER_HEIGHT: u16 = 20;
// COMPACT_BANNER is 8 cells wide; +1 cell padding each side + 2
// border cells = 12.
const LOGO_PANEL_WIDTH: u16 = 12;
// Hide the logo panel on terminals narrower than this many cells —
// at sub-120 widths the Daemon middle pane gets squeezed and the
// logo competes for cells the info readouts need. The banner still
// surfaces on the top header bar regardless.
const LOGO_MIN_TOTAL_WIDTH: u16 = 120;

/// Paint `palette.bg` over `area` so subsequent foreground-only
/// widgets inherit the theme surface rather than the terminal
/// default. Mono opts out via `Color::Reset` so its modals still let
/// the terminal palette show through. Used both by the root layout
/// and by every overlay so a light-theme popup actually looks light.
pub(crate) fn paint_theme_bg(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
  if let Some(style) = palette.popup_bg_style() {
    frame.render_widget(Paragraph::new("").style(style), area);
  }
}

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
  app.expire_toast();
  app.ensure_right_tab_reachable();
  // Prime the per-frame `rendered_rows` memo so the 12+ in-frame
  // calls (focused_path / focused_managed / right_pane / settings)
  // amortise to a single build. Cleared at the
  // bottom so event-handler invocations between frames recompute.
  // `Palette` is `Copy`, so take a snapshot up front and free
  // the borrow on `app` for the `prime_rows_cache` / mutable cache
  // dance at the frame boundary.
  let palette: Palette = *app.palette();
  app.prime_rows_cache();
  let area = frame.area();

  // Sub-minimum terminals: paint a clear "too small" message
  // instead of silently clipping every panel. The placeholder
  // still updates with the current size so the user knows when
  // they've grown the terminal enough.
  if area.width < MIN_RENDER_WIDTH || area.height < MIN_RENDER_HEIGHT {
    render_too_small(frame, area, &palette);
    app.clear_rows_cache();
    return;
  }

  // Paint the root area with the theme's background first so light
  // palettes (Latte) actually show on a light surface — without this,
  // the terminal's native background bleeds through every gap between
  // bordered Blocks. `Color::Reset` opts out (used by `mono`) so the
  // terminal default keeps winning for that theme.
  paint_theme_bg(frame, area, &palette);

  let show_info_row = area.height >= MIN_HEIGHT_FOR_INFO_ROW;

  // Vertical layout: title, [info,] [download strip,] body. The
  // download strip is reserved only when active so the body keeps
  // every available row when no pull is in flight.
  let show_strip = app.download_strip_active();
  let mut constraints: Vec<Constraint> = Vec::with_capacity(4);
  constraints.push(Constraint::Length(1));
  if show_info_row {
    constraints.push(Constraint::Length(INFO_ROW_HEIGHT));
  }
  if show_strip {
    // 1 row for the strip itself + 1 row of vertical margin below
    // it so the body's panel border doesn't sit flush against the
    // progress text.
    constraints.push(Constraint::Length(2));
  }
  constraints.push(Constraint::Min(1));
  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints)
    .split(area);

  let mut idx = 0;
  render_title_row(frame, chunks[idx], app, &palette);
  idx += 1;
  if show_info_row {
    render_info_row(frame, chunks[idx], app, &palette);
    idx += 1;
  }
  if show_strip {
    // Strip renders into the top row of its 2-row slot; the bottom
    // row stays blank (theme-painted background) as a visual gutter.
    let strip_area = ratatui::layout::Rect {
      height: 1,
      ..chunks[idx]
    };
    // Surface the cancel-download chip only while a pull is actually
    // active — the lingering-error / queue-promoting interstitials
    // can't be cancelled because they're not consuming bytes.
    let cancel_hint = if app.download_strip.active.is_some() {
      app.hint(Focus::List, Action::CancelDownload)
    } else {
      None
    };
    super::download_strip::render(
      frame,
      strip_area,
      &app.download_strip,
      cancel_hint.as_deref(),
      &palette,
    );
    idx += 1;
  }
  render_body(frame, chunks[idx], app, &palette);

  // Transient toast bar — paints on top of the body so copy / theme /
  // refusal confirmations are actually visible. The kdash refactor
  // (commit 5005b4c) removed the bottom help-bar slot that used to
  // display the toast; this
  // restores a single-line floating bar above the bottom edge.
  // Drawn before the modal overlays so a confirm/help popup still
  // wins focus while it is open.
  render_toast(frame, area, app, &palette);

  // Overlays last. The launch picker no longer has a modal — the
  // form lives inline in the right pane's Settings tab. The
  // `launch_picker` module still owns the form state struct, but no
  // dedicated overlay is painted.
  if app.hf_dialog.is_some() {
    super::hf_dialog::render(frame, area, app, &palette);
  }
  if let Some(dialog) = app.save_preset_dialog.as_ref() {
    super::save_preset_dialog::render(frame, area, app, dialog, &palette);
  }
  if let Some(dialog) = app.launch_name_dialog.as_ref() {
    super::launch_name_dialog::render(frame, area, app, dialog, &palette);
  }
  if app.show_help {
    help_overlay::render(frame, area, app, &palette);
  }
  // Confirmation overlay paints last so it sits on top of every
  // other modal — by design: a destructive action wins focus
  // unconditionally until the user resolves it.
  if let Some(action) = app.confirm_dialog.as_ref() {
    confirm_overlay::render(frame, area, app, action, &palette);
  }
  app.clear_rows_cache();
}

/// Draw a single-line floating toast near the bottom of `area`,
/// centered horizontally. No-op when no toast is set. The line is
/// painted on the accent background (or `palette.error` for an error
/// toast) so it pops over whatever panel it lands on; `Clear` wipes
/// the underlying cells first so the panel borders don't bleed
/// through.
fn render_toast(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let Some(msg) = app.toast_message() else {
    return;
  };
  let kind = app.toast_kind().unwrap_or_default();
  // Truncate aggressively rather than wrap — a multi-line toast
  // would push the body content visibly upward. Reserve 4 cells of
  // margin so the bar never butts against the edges.
  let max_inner = area.width.saturating_sub(4) as usize;
  if max_inner < 8 {
    return;
  }
  let body: String = if msg.chars().count() > max_inner {
    let ellipsis = crate::tui::glyphs::active().ellipsis();
    let mut truncated: String = msg
      .chars()
      .take(max_inner.saturating_sub(ellipsis.chars().count()))
      .collect();
    truncated.push_str(ellipsis);
    truncated
  } else {
    msg.to_string()
  };
  let text = format!(" {body} ");
  let w = text.chars().count() as u16;
  let x = area.x + area.width.saturating_sub(w) / 2;
  let y = area.y + area.height.saturating_sub(2);
  let rect = Rect::new(x, y, w, 1);
  frame.render_widget(Clear, rect);
  let bg = match kind {
    ToastKind::Error => palette.error,
    ToastKind::Info => palette.accent,
  };
  let style = Style::default()
    .bg(bg)
    .fg(palette.on_accent)
    .add_modifier(Modifier::BOLD);
  frame.render_widget(Paragraph::new(text).style(style), rect);
}

/// Placeholder shown when the terminal is below the `MIN_RENDER_*`
/// floor. A multi-panel dashboard at sub-40×10 paints garbled or
/// truncated borders, so we replace it with a single centred line
/// that tells the user what to do. The text updates with the live
/// size so growing the terminal shows the change before the full
/// dashboard kicks back in.
fn render_too_small(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
  paint_theme_bg(frame, area, palette);
  let msg = Paragraph::new(vec![
    Line::from(Span::styled(
      "Terminal too small".to_string(),
      palette.title_style(),
    )),
    Line::from(Span::styled(
      format!(
        "have {}×{}, need at least {}×{}",
        area.width, area.height, MIN_RENDER_WIDTH, MIN_RENDER_HEIGHT
      ),
      palette.muted_style(),
    )),
  ])
  .alignment(ratatui::layout::Alignment::Center);
  let centred = Rect {
    x: area.x,
    // Centre vertically: 2-line message, so reserve area.height/2 - 1 rows above.
    y: area.y + area.height.saturating_sub(2) / 2,
    width: area.width,
    height: area.height.min(2),
  };
  frame.render_widget(msg, centred);
}

/// Render the accent-bg title row: brand + daemon dot on the left,
/// global hints on the right.
fn render_title_row(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // Paint the whole row with the accent background first; the
  // sub-renderers below overlay text but inherit the bg via the
  // Paragraph's base style.
  let bg = Paragraph::new("").style(Style::default().bg(palette.accent));
  frame.render_widget(bg, area);

  // Reserve the right slot for the global hint strip; the left slot
  // (brand + daemon dot) flexes into the rest. Chips resolve live from
  // the App's `KeyMap` (so a `keybindings:` override flows through) and
  // drop one-by-one by rank as the terminal narrows, keeping the brand
  // at least `min_brand_w` cells. The user can always press `?` for the
  // full help overlay, so a dropped chip loses nothing.
  let min_brand_w: u16 = 20;
  let budget = area.width.saturating_sub(min_brand_w) as usize;
  let chips = help_bar::fit_global_hints(app, budget);
  if chips.is_empty() {
    render_title_left(frame, area, app, palette);
  } else {
    let hint_slot = help_bar::hints_render_width(&chips);
    let split = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Min(min_brand_w), Constraint::Length(hint_slot)])
      .split(area);
    render_title_left(frame, split[0], app, palette);
    help_bar::render_global(frame, split[1], palette, &chips);
  }
}

fn render_title_left(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // Text colour for content on the accent bar. Most themes route
  // this to `palette.bg`; mono pins it to Black because `palette.bg`
  // there is `Color::Reset` and would render as the terminal's
  // default fg (typically light on a dark terminal) over the White
  // accent bar — i.e. invisible.
  let on_accent = palette.on_accent;
  let spans = title_left_spans(area.width, app, palette);
  let para =
    Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.accent).fg(on_accent));
  frame.render_widget(para, area);
}

/// Build the brand spans for the title bar's left slot, dropping
/// trailing segments to fit `width` rather than letting the Paragraph
/// hard-clip a glyph mid-word against the hint strip.
///
/// `LlamaStash` always stays. The optional trailing segments drop in
/// priority order — theme first, then daemon, then version — and the
/// kept set always leaves a 1-cell gap before `width` so the brand
/// never sits flush against the hint slot. When even the bare name
/// won't fit, the renderer falls back to the name alone and lets the
/// Paragraph clip it (no width on earth is narrower than this path
/// reaches in practice).
fn title_left_spans(width: u16, app: &App, palette: &Palette) -> Vec<Span<'static>> {
  use unicode_width::UnicodeWidthStr;

  let version = env!("CARGO_PKG_VERSION");
  let (dot_color, daemon_label) = if app.daemon_connected {
    (palette.success, "daemon".to_string())
  } else {
    (
      palette.warning,
      format!(
        "daemon connecting{}",
        crate::tui::glyphs::active().ellipsis()
      ),
    )
  };
  let theme_name = app.options.theme.short_name();
  let on_accent = palette.on_accent;

  // Leading space + always-present brand name.
  let lead: Vec<Span<'static>> = vec![
    Span::raw(" "),
    Span::styled(
      "LlamaStash",
      Style::default().fg(on_accent).add_modifier(Modifier::BOLD),
    ),
  ];
  let lead_w = 1 + "LlamaStash".width();

  // Trailing segments in render order, each tagged with the plain text
  // it occupies (for width accounting). Each carries its own leading
  // separator so a dropped segment never strands a dangling ` · `.
  // Drop order is the reverse of this list: theme is dropped first,
  // then daemon, then version.
  let version_text = format!(" v{version}");
  let version_w = version_text.width();
  let version_seg = vec![Span::styled(version_text, Style::default().fg(on_accent))];

  let glyphs = crate::tui::glyphs::active();
  let sep = format!(" {} ", glyphs.middot());
  let daemon_dot = glyphs.status_icon(crate::tui::status_icons::SurfaceState::Ready);
  let theme_dot = glyphs.status_icon(crate::tui::status_icons::SurfaceState::Loading);
  let daemon_seg = vec![
    Span::styled(sep.clone(), Style::default().fg(on_accent)),
    Span::styled(daemon_dot.to_string(), Style::default().fg(dot_color)),
    Span::raw(" "),
    Span::styled(daemon_label.to_string(), Style::default().fg(on_accent)),
  ];
  let daemon_w = sep.width() + daemon_dot.to_string().width() + 1 + daemon_label.width();

  let theme_seg = vec![
    Span::styled(sep.clone(), Style::default().fg(on_accent)),
    // Half-filled circle glyph — visual cue for "theme" / light/dark.
    Span::styled(format!("{theme_dot} "), Style::default().fg(on_accent)),
    Span::styled(theme_name.to_string(), Style::default().fg(on_accent)),
  ];
  let theme_w = sep.width() + theme_dot.to_string().width() + 1 + theme_name.width();

  // Each segment carries its own leading separator, so daemon only
  // reads right after version and theme after daemon. Nesting the
  // fit checks enforces the priority-drop order (theme → daemon →
  // version).
  let budget = width as usize;
  // Reserve a 1-cell gap before the hint slot so the rightmost kept
  // segment never butts up against it.
  let gap = 1usize;

  let mut spans = lead;
  let mut used = lead_w;
  if used + version_w + gap <= budget {
    spans.extend(version_seg);
    used += version_w;
    if used + daemon_w + gap <= budget {
      spans.extend(daemon_seg);
      used += daemon_w;
      if used + theme_w + gap <= budget {
        spans.extend(theme_seg);
      }
    }
  }
  spans
}

/// Render the three-panel info row.
fn render_info_row(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // Reserve the Logo panel only when the terminal is wide enough that
  // the Daemon middle pane won't get squeezed by it. Otherwise the
  // Daemon panel claims the freed space.
  let show_logo = area.width >= LOGO_MIN_TOTAL_WIDTH;
  let constraints = if show_logo {
    vec![
      Constraint::Length(HOST_PANEL_WIDTH),
      Constraint::Min(1),
      Constraint::Length(LOGO_PANEL_WIDTH),
    ]
  } else {
    vec![Constraint::Length(HOST_PANEL_WIDTH), Constraint::Min(1)]
  };
  let split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints(constraints)
    .split(area);
  host_stats_pane::render(frame, split[0], &app.host_metrics, palette);
  info_pane::render(frame, split[1], app, palette);
  if show_logo {
    logo_pane::render(frame, split[2], app, palette);
  }
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let show_right = app.right_pane_visible_at(area.width);
  let split = body_split(area, show_right, app);
  let rows = app.rendered_rows();
  let title = build_models_title(app, split[0].width as usize, &rows);
  let filter_chip = models_filter_chip(app);
  let list_focused = list_is_focused(app.focus);
  let right_focused = right_is_focused(app.focus);
  // A `left_pane_ratios` slot of `100` (list-full) or `0` (right-full)
  // collapses one pane to zero width. Skip rendering a zero-width pane —
  // widgets subtract border cells and would underflow — and zero its
  // hit-test rect so a stray click can't match a collapsed footprint.
  let show_list = split[0].width > 0;
  let show_right_pane = show_right && split.get(1).is_some_and(|r| r.width > 0);
  // Refresh the body-level hit-test rects for the mouse-focus
  // dispatch. The list-pane rect is always populated; the right-pane
  // rect is zeroed when the right pane is hidden so a stray click in
  // that screen region doesn't match a stale frame's footprint.
  {
    let mut hits = app.hit_rects.borrow_mut();
    hits.list_pane = if show_list { split[0] } else { Rect::default() };
    hits.right_pane = if show_right_pane {
      split[1]
    } else {
      Rect::default()
    };
    if !show_right_pane {
      hits.right_tabs.clear();
    }
  }
  if show_list {
    if rows.is_empty() {
      render_empty_state(frame, split[0], palette, title, &filter_chip, list_focused);
    } else {
      list_pane::render(
        frame,
        split[0],
        palette,
        list_pane::RenderInputs {
          rows: &rows,
          selected: app.list_cursor,
          title,
          filter_chip_label: &filter_chip,
          focused: list_focused,
          show_device: app.multi_device(),
          show_backend: app.multi_backend(),
        },
      );
    }
  }
  if show_right_pane {
    right_pane::render(frame, split[1], app, palette, right_focused);
  }
}

/// Decide the body's horizontal split based on width and whether
/// the right pane is currently showing.
///
/// - **Right hidden** → list owns the whole body, regardless of
///   width. Covers both `models.is_empty()` and compact-mode with
///   focus on the list.
/// - **Wide mode** (`area.width ≥ COMPACT_WIDTH_THRESHOLD`) → the
///   user's `Alt+L` cycle slot (`App::left_pane_ratio`, default
///   `65`), so the left/right split is `<slot> / <100 - slot>`.
///   `100` collapses the right pane; `0` collapses the list — the
///   caller skips rendering a zero-width pane.
/// - **Compact mode, drilled in** → `35 / 65`, unchanged. The list
///   collapses to its marker + Name column (the ranked-column picker
///   drops everything else as the budget shrinks), and the right
///   pane gets the larger slice for chat / settings / logs content.
///   The ratio override is wide-mode only so it can't fight the
///   compact drill-in.
fn body_split(area: Rect, show_right: bool, app: &App) -> std::rc::Rc<[Rect]> {
  if !show_right {
    return Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Percentage(100)])
      .split(area);
  }
  let constraints = if area.width >= crate::tui::app::COMPACT_WIDTH_THRESHOLD {
    let left = app.left_pane_ratio();
    [
      Constraint::Percentage(left),
      Constraint::Percentage(100 - left),
    ]
  } else {
    [Constraint::Percentage(35), Constraint::Percentage(65)]
  };
  Layout::default()
    .direction(Direction::Horizontal)
    .constraints(constraints)
    .split(area)
}

/// True when the Models pane currently owns keyboard focus. The
/// filter input lives inside the Models block title, so capturing
/// `Focus::Filter` here keeps the border yellow while the user is
/// typing a filter.
fn list_is_focused(focus: Focus) -> bool {
  matches!(focus, Focus::List | Focus::Filter)
}

/// True when the right pane (any tab) owns keyboard focus. Every
/// tab-specific input focus counts; only Models / Filter and the
/// modal overlays leave the right pane unfocused.
fn right_is_focused(focus: Focus) -> bool {
  matches!(
    focus,
    Focus::RightPane | Focus::ChatInput | Focus::EmbedInput | Focus::RerankInput
  )
}

/// Compose the Models block title from current app state. Pulled
/// out so the empty-state path and the populated-list path share
/// the same title bar (a /:filter chip when inactive, the inline
/// `/ buf` input when active).
fn build_models_title<'a>(
  app: &'a App,
  area_width: usize,
  rows: &[list_pane::ListRow],
) -> list_pane::TitleInputs<'a> {
  let filter_active = !(app.filter_input.is_empty() && app.focus != Focus::Filter);
  let filter = if filter_active {
    list_pane::FilterTitle::Active {
      buffer: app.filter_input.buffer(),
      focused: app.focus == Focus::Filter,
    }
  } else {
    list_pane::FilterTitle::Inactive
  };
  let on_running = focused_row_is_running(app, rows);
  let deletable = focused_row_is_deletable(app, rows);
  // Mode the chip strip resolves against. `filter_active` collapses
  // three independent signals (focus, edit state, buffer-non-empty)
  // into one explicit enum so build_models_hints reads as a single
  // match instead of three nested `if`s. See `FilterChipMode`.
  let filter_chip_mode = filter_chip_mode(app, filter_active);
  let hints = build_models_hints(app, filter_chip_mode, on_running, deletable);
  list_pane::TitleInputs {
    total: app.models.len(),
    area_width,
    filter,
    hints,
  }
}

/// Build the Models title chip strip, resolved live against the
/// keymap so a `keybindings:` config override flows through to the
/// title bar. Order matters: the first chip is never dropped under
/// budget pressure, so put the most important keystroke first
/// (`Enter:apply` while filtering, `Enter:launch` otherwise).
/// Filter-chip rendering mode. The chip strip differentiates three
/// states so the InputField's modal contract (`e:edit / Esc:stop /
/// 2nd-Esc:clear`) reads as a sequence of chips the user can follow
/// without leaving the filter pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterChipMode {
  /// No filter — render the normal row-action chips.
  Inactive,
  /// Filter input has focus and is in edit mode (typing). Surface
  /// the in-edit chord — `Esc:stop edit · Enter:apply` — instead of
  /// the row actions so the user can see how to exit edit.
  Editing,
  /// Filter has focus but is resting (Esc was pressed once, buffer
  /// kept). Surface `e:edit · Esc:clear · Enter:apply` so the user
  /// can either re-enter edit, walk back one more step (clear), or
  /// apply the predicate they already typed.
  Resting,
}

/// Decide which filter-chip mode the title strip should render.
/// Pure projection of the three relevant signals (`filter_active`,
/// the focus, the InputField edit state). The
/// `Editing` / `Resting` arms only fire when focus *is* the filter
/// — once the user steps focus back to the list (filter buffer
/// retained), the chip strip falls back to the normal model row
/// chips and the filter-chip slot drops out so the model nav chord
/// is the headline.
fn filter_chip_mode(app: &App, filter_active: bool) -> FilterChipMode {
  if !filter_active || app.focus != Focus::Filter {
    return FilterChipMode::Inactive;
  }
  if app.filter_input.is_editing() {
    FilterChipMode::Editing
  } else {
    FilterChipMode::Resting
  }
}

fn build_models_hints(
  app: &App,
  filter_mode: FilterChipMode,
  on_running: bool,
  deletable: bool,
) -> Vec<crate::tui::hint_picker::RankedChip> {
  use crate::tui::hint_picker::RankedChip;
  let mut out: Vec<RankedChip> = Vec::with_capacity(7);
  // `push_ranked` is the only Vec-shaping helper here so adding a
  // new chip means picking a rank (lower = stickier under width
  // pressure) — no risk of accidentally giving a hint silent
  // priority through source order alone.
  let mut push_ranked = |rank: u8, text: Option<String>| {
    if let Some(t) = text {
      out.push(RankedChip::new(rank, t));
    }
  };
  // Filter is a live predicate (applies on every keystroke), so
  // `Submit` carries `Enter:launch` semantics: drill into the
  // focused result. Override the binding's description to read as
  // the actual user-facing action.
  let enter_launch = || {
    Some(
      app
        .hint_with(Focus::Filter, Action::Submit, "launch")
        .unwrap_or_else(|| format!("{}:launch", crate::tui::keybindings::ENTER_LABEL)),
    )
  };
  if filter_mode == FilterChipMode::Editing {
    // While editing only the in-edit chord is useful — `Esc:stop
    // edit` exits to resting (buffer kept). The InputField's static
    // binding for Esc inside Focus::Filter is `ClearFilter`, but in
    // edit mode the field intercepts Esc first and exits edit; we
    // surface the actual observed behavior here.
    push_ranked(10, enter_launch());
    push_ranked(20, Some("Esc:stop edit".to_string()));
    return out;
  }
  if filter_mode == FilterChipMode::Resting {
    // Resting: the InputField is in its post-first-Esc state. `e`
    // re-enters edit, `Esc` clears the buffer, `Enter` launches the
    // focused row. ↑/↓ scroll the filtered results without leaving
    // the filter focus.
    push_ranked(10, Some("e:edit".to_string()));
    push_ranked(20, app.hint(Focus::Filter, Action::ClearFilter));
    push_ranked(30, enter_launch());
    push_ranked(40, Some("↑/↓:nav".to_string()));
    return out;
  }
  // only surface `Enter:launch` when the cursor
  // sits on a launchable row. `open_launch_picker` is silently a
  // no-op on header rows (`★ Favorites`, `↺ Recent`, folder
  // group headings), so showing the chip there would teach a
  // binding that doesn't fire.
  if app.focused_name().is_some() {
    push_ranked(10, app.hint(Focus::List, Action::OpenLaunchPicker));
  }
  // `s:stop` is the most valuable next keystroke when the cursor
  // sits on a running row — rank 20 so it survives ahead of `fav`.
  if on_running {
    push_ranked(20, app.hint(Focus::List, Action::StopModel));
  }
  // `favorite` is the canonical description; override here to keep
  // the chip terse without renaming the help-overlay entry.
  push_ranked(
    30,
    app.hint_with(Focus::List, Action::ToggleFavorite, "fav"),
  );
  push_ranked(40, app.hint(Focus::List, Action::YankPath));
  if on_running {
    push_ranked(50, app.hint(Focus::List, Action::YankUrl));
    push_ranked(60, app.hint(Focus::List, Action::YankCurl));
  }
  // Delete-model chip is gated to *idle* rows only — see
  // [`focused_row_is_deletable`] for the exact rule. Running
  // (Ready / Loading / Launching), Error, and External rows hide
  // the chip so we don't tempt the user toward a refusal or a
  // delete that would crash an out-of-process llama-server. The
  // keybinding still fires on those rows (it toasts the reason it
  // refused) so muscle memory works — the chip is purely the
  // discovery surface.
  if app.focused_name().is_some() && deletable {
    push_ranked(70, app.hint(Focus::List, Action::DeleteModel));
  }
  out
}

/// Resolve the inactive-filter chip label (`/:filter`) against the
/// live keymap so a remap of `open_filter` flows through. Falls
/// back to a static `/` glyph if the user has unbound the action
/// (the chip still hints at the filter feature even without a key).
fn models_filter_chip(app: &App) -> String {
  app
    .hint(Focus::List, Action::OpenFilter)
    .unwrap_or_else(|| "/:filter".to_string())
}

/// True when the cursor row points at a model whose launch state
/// is one of the "running"-ish slots (Ready / Loading / Launching).
/// The `s:stop` hint should hide when stopping makes no sense.
fn focused_row_is_running(app: &App, rows: &[list_pane::ListRow]) -> bool {
  use crate::tui::status_icons::SurfaceState;
  match rows.get(app.list_cursor) {
    Some(list_pane::ListRow::Model { state, .. }) => matches!(
      state,
      SurfaceState::Ready | SurfaceState::Loading | SurfaceState::Launching
    ),
    _ => false,
  }
}

/// True when the focused row points at a model that's safe to delete
/// from disk — i.e. nothing is currently reading the file. Idle
/// states are `NotLaunched` (never launched in this session) and
/// `Stopped` (gracefully terminated). Everything else (Launching,
/// Loading, Ready, Error, External) keeps the file pinned by a
/// process we'd otherwise crash, so the `Ctrl+D` hint chip hides
/// and the keybinding refuses with a toast.
///
/// `Error` is included in the blocked set because the user's reading
/// of "non-error" is: a failed-to-launch row still has a managed
/// entry that may hold a file lock, and surfacing delete on the same
/// row as "Error" reads as "retry by deleting" — wrong UX shape for
/// a v1 surface.
fn focused_row_is_deletable(app: &App, rows: &[list_pane::ListRow]) -> bool {
  use crate::tui::status_icons::SurfaceState;
  match rows.get(app.list_cursor) {
    Some(list_pane::ListRow::Model { state, path, .. }) => {
      // Lemonade registry models have no local file to unlink — never deletable
      // (mirrors `events::delete_refusal_reason`).
      crate::backend::lemonade::registry_name_from_path(path).is_none()
        && matches!(state, SurfaceState::NotLaunched | SurfaceState::Stopped)
    }
    _ => false,
  }
}

fn render_empty_state(
  frame: &mut Frame<'_>,
  area: Rect,
  palette: &Palette,
  title: list_pane::TitleInputs<'_>,
  filter_chip: &str,
  focused: bool,
) {
  let title_line = list_pane::build_block_title(title, filter_chip, palette, focused);
  let border_color = list_pane::border_color(palette, focused);
  let block = palette
    .panel()
    .title(title_line)
    .border(border_color)
    .build();
  let inner = block.inner(area);
  frame.render_widget(block, area);
  let lines = vec![
    Line::from(Span::styled("No GGUFs surfaced yet.", palette.text_style())),
    Line::from(Span::styled(
      "Drop a `.gguf` into a watched directory or run `llamastash --model-path <DIR>`.",
      palette.muted_style(),
    )),
  ];
  frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use ratatui::backend::TestBackend;
  use ratatui::style::Color;
  use ratatui::Terminal;

  #[test]
  fn body_split_wide_follows_the_left_pane_ratio_slot() {
    let mut app = App::new(AppOptions {
      left_pane_ratios: vec![65, 100, 0],
      ..Default::default()
    });
    let area = Rect::new(0, 0, 200, 40); // safely wide (> COMPACT_WIDTH_THRESHOLD)

    // Slot 0 (65) → 65 / 35 of 200.
    let slot0 = body_split(area, true, &app);
    assert_eq!((slot0[0].width, slot0[1].width), (130, 70));

    // Slot 1 (100) → list full, right pane collapses to zero width.
    app.cycle_left_pane_ratio();
    let slot1 = body_split(area, true, &app);
    assert_eq!((slot1[0].width, slot1[1].width), (200, 0));

    // Slot 2 (0) → list collapses, right pane full.
    app.cycle_left_pane_ratio();
    let slot2 = body_split(area, true, &app);
    assert_eq!((slot2[0].width, slot2[1].width), (0, 200));
  }

  #[test]
  fn body_split_ignores_ratio_in_compact_mode() {
    let app = App::new(AppOptions {
      left_pane_ratios: vec![90],
      ..Default::default()
    });
    // Narrow area (< COMPACT_WIDTH_THRESHOLD) keeps the adaptive 35 / 65
    // drill-in regardless of the configured slot.
    let area = Rect::new(0, 0, 80, 40);
    let s = body_split(area, true, &app);
    assert_eq!((s[0].width, s[1].width), (28, 52)); // 35% / 65% of 80
  }

  fn render_into(width: u16, height: u16, mut app: App) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| render(f, &mut app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::with_capacity(buf.area.height as usize);
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
  fn sub_minimum_size_renders_too_small_placeholder() {
    // sub-`MIN_RENDER_*` terminals used to paint the
    // full dashboard with clipped borders. The placeholder now
    // surfaces "too small" instead so the user understands why
    // the dashboard isn't drawing.
    let app = App::new(AppOptions::default());
    let rows = render_into(30, 8, app);
    let body = rows.join("\n");
    assert!(
      body.contains("Terminal too small"),
      "placeholder missing: {body}"
    );
    assert!(
      body.contains("30×8"),
      "placeholder must surface current size: {body}"
    );
    // No panels should have been drawn.
    assert!(
      !body.contains("LlamaStash"),
      "panels must not render: {body}"
    );
  }

  #[test]
  fn full_size_renders_title_info_and_body() {
    let app = App::new(AppOptions::default());
    let rows = render_into(100, 30, app);
    let body = rows.join("\n");
    assert!(
      body.contains("LlamaStash"),
      "title row missing brand: {body}"
    );
    assert!(body.contains("?:help"), "title row missing global hints");
    assert!(body.contains("Host"), "info row missing Host block");
    assert!(body.contains("Daemon"), "info row missing Daemon block");
    assert!(body.contains("Models"), "body missing Models block");
  }

  #[test]
  fn root_bg_is_painted_with_palette_bg_for_light_theme() {
    // Latte is the only built-in light theme; without an explicit
    // root paint, gaps between bordered Blocks would expose the
    // terminal's default (typically dark) background and the panel
    // would look broken on a light terminal. Cells inside a Block's
    // body — pure background between text — should carry Latte's
    // off-white bg.
    use crate::theme::{palette_for, ThemeName};
    use crate::tui::keybindings::KeyMap;
    let mut app = App::new(AppOptions {
      theme: ThemeName::Latte,
      custom_palette: None,
      keymap: KeyMap::default(),
      ..Default::default()
    });
    // Force the Models pane into its populated path so the body cell
    // we probe is inside a real list area, not the empty-state hint.
    app.daemon_connected = true;
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| render(f, &mut app)).unwrap();
    let buf = term.backend().buffer().clone();
    let palette = palette_for(ThemeName::Latte);
    // Cell (1, 1) sits just inside the outer body area — between the
    // info row's bordered Blocks. Without the root paint this cell is
    // a `Color::Reset` bg, leaking the terminal default.
    let cell = buf.cell((1, 1)).unwrap();
    assert_eq!(
      cell.bg, palette.bg,
      "root paint should make Latte show on its own bg, got bg={:?}",
      cell.bg
    );
  }

  #[test]
  fn root_bg_paints_dark_themes_too_so_panel_gaps_match() {
    // Same property for macchiato: cells between bordered Blocks pick
    // up the theme bg, not the terminal default. This is what keeps
    // the dashboard looking like one solid surface across terminals
    // with non-matching bg (e.g. white terminals showing the dark
    // theme).
    use crate::theme::{palette_for, ThemeName};
    use crate::tui::keybindings::KeyMap;
    let app = App::new(AppOptions {
      theme: ThemeName::Macchiato,
      custom_palette: None,
      keymap: KeyMap::default(),
      ..Default::default()
    });
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut app_mut = app;
    term.draw(|f| render(f, &mut app_mut)).unwrap();
    let buf = term.backend().buffer().clone();
    let palette = palette_for(ThemeName::Macchiato);
    let cell = buf.cell((1, 1)).unwrap();
    assert_eq!(cell.bg, palette.bg);
  }

  #[test]
  fn mono_theme_skips_root_bg_paint() {
    // Mono opts out (`palette.bg == Color::Reset`) so the terminal's
    // own bg shows through — that's the whole point of the mono
    // theme. Verify by confirming a body cell still has `Reset` bg.
    use crate::theme::ThemeName;
    use crate::tui::keybindings::KeyMap;
    let app = App::new(AppOptions {
      theme: ThemeName::Mono,
      custom_palette: None,
      keymap: KeyMap::default(),
      ..Default::default()
    });
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut app_mut = app;
    term.draw(|f| render(f, &mut app_mut)).unwrap();
    let buf = term.backend().buffer().clone();
    let cell = buf.cell((1, 1)).unwrap();
    assert_eq!(cell.bg, Color::Reset);
  }

  #[test]
  fn help_overlay_paints_theme_bg_on_light_palette() {
    // Regression: pre-fix, `frame.render_widget(Clear, rect)` reset
    // the overlay cells to `Color::Reset`, so the help dialog on
    // Latte (light theme) rendered as a dark patch where the
    // terminal's default bg bled through. The overlay now follows
    // `Clear` with `paint_theme_bg`, so cells inside the dialog
    // must carry `palette.bg`.
    use crate::theme::{palette_for, ThemeName};
    use crate::tui::keybindings::KeyMap;
    let mut app = App::new(AppOptions {
      theme: ThemeName::Latte,
      custom_palette: None,
      keymap: KeyMap::default(),
      ..Default::default()
    });
    app.show_help = true;
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
    term.draw(|f| render(f, &mut app)).unwrap();
    let buf = term.backend().buffer().clone();
    let palette = palette_for(ThemeName::Latte);
    // Pick an interior cell well inside the centred overlay — the
    // rect math (`area.width.saturating_sub(4).min(130)` centred in
    // 140 cols) puts the overlay's centre near (70, 20).
    let cell = buf.cell((70, 20)).unwrap();
    assert_eq!(
      cell.bg, palette.bg,
      "help overlay interior must paint palette.bg, got {:?}",
      cell.bg
    );
  }

  #[test]
  fn mono_help_overlay_keeps_terminal_default_bg() {
    // Mono explicitly opts out of bg painting; the overlay must
    // honour the same opt-out so the user's terminal palette still
    // shows underneath the dialog body.
    use crate::theme::ThemeName;
    use crate::tui::keybindings::KeyMap;
    let mut app = App::new(AppOptions {
      theme: ThemeName::Mono,
      custom_palette: None,
      keymap: KeyMap::default(),
      ..Default::default()
    });
    app.show_help = true;
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
    term.draw(|f| render(f, &mut app)).unwrap();
    let buf = term.backend().buffer().clone();
    let cell = buf.cell((70, 20)).unwrap();
    assert_eq!(cell.bg, Color::Reset);
  }

  #[test]
  fn narrow_height_collapses_info_row() {
    // A 22-row terminal is above MIN_RENDER_HEIGHT (20) but below
    // MIN_HEIGHT_FOR_INFO_ROW (24); the info row drops and only
    // title + body render.
    let app = App::new(AppOptions::default());
    let rows = render_into(80, 22, app);
    let body = rows.join("\n");
    assert!(body.contains("LlamaStash"), "title still renders");
    assert!(body.contains("Models"), "body still renders");
    // The Host pane block title is rendered as discrete cells, so the
    // joined frame won't contain a contiguous "─ Host ─" token even
    // when the pane is visible — assert the literal word `Host` is
    // absent instead, and that no Host block border row appears
    // between the title row and the body.
    assert!(!body.contains("Host"), "info row should be hidden: {body}");
    assert!(
      !body.contains("Daemon"),
      "info row should be hidden: {body}"
    );
  }

  #[test]
  fn narrow_width_hides_logo_panel() {
    // Anything below the 120-col threshold drops the logo so the
    // Daemon middle pane keeps its readouts uncramped. We assert the
    // banner glyph (the only thing the logo panel emits) is absent
    // rather than greping for the theme tag — that tag now lives on
    // the top header bar regardless of the logo panel.
    let app = App::new(AppOptions::default());
    let rows = render_into(119, 30, app);
    let body = rows.join("\n");
    assert!(body.contains("Host"));
    assert!(body.contains("Daemon"));
    assert!(
      !body.contains("██"),
      "logo panel should be hidden below 120 cols: {body}"
    );
  }

  #[test]
  fn wider_width_shows_logo_panel() {
    let app = App::new(AppOptions::default());
    let rows = render_into(140, 30, app);
    let body = rows.join("\n");
    // At ≥120 cols the logo panel renders. It emits the COMPACT_BANNER
    // glyphs (`██`) — assert those rather than the theme tag, which
    // lives on the top-row hint strip and may get clipped at narrower
    // widths.
    assert!(
      body.contains("██"),
      "logo panel should render at width 140: {body}"
    );
  }

  #[test]
  fn filter_input_appears_inline_in_models_title_when_focused() {
    let mut app = App::new(AppOptions::default());
    app.focus = Focus::Filter;
    app.filter_input.set_text("qwen");
    let rows = render_into(100, 30, app);
    let frame = rows.join("\n");
    // The inline input renders inside the Models block title strip.
    assert!(
      frame.contains("/ qwen"),
      "expected inline `/ qwen` in Models title: {frame}"
    );
    // The dedicated bottom filter row no longer exists.
    assert!(
      !rows.iter().any(|r| r.starts_with("/ ")),
      "no separate filter row should render: {rows:#?}"
    );
  }

  #[test]
  fn running_row_hint_order_puts_stop_before_fav() {
    // When the focused row is running, `s:stop` is the most valuable
    // next action. It must appear *before* `f:fav` in the chip strip
    // so a narrow Models pane drops the lower-value chips first and
    // keeps the headline action visible.
    let app = App::new(AppOptions::default());
    let hints = build_models_hints(
      &app,
      FilterChipMode::Inactive,
      /*on_running=*/ true,
      /*deletable=*/ false,
    );
    let stop_at = hints
      .iter()
      .position(|h| h.text.contains("stop"))
      .expect("s:stop must appear when on_running");
    let fav_at = hints
      .iter()
      .position(|h| h.text.contains("fav"))
      .expect("f:fav must appear");
    assert!(
      stop_at < fav_at,
      "s:stop must come before f:fav, got {hints:?}"
    );
  }

  #[test]
  fn non_running_row_hint_strip_omits_stop_and_yank_chips() {
    // When the cursor sits on a not-launched row, the stop/url/curl
    // chips drop entirely — only the always-applicable launch / fav /
    // path keys remain so the strip stays uncluttered.
    let app = App::new(AppOptions::default());
    let hints = build_models_hints(
      &app,
      FilterChipMode::Inactive,
      /*on_running=*/ false,
      /*deletable=*/ true,
    );
    assert!(!hints.iter().any(|h| h.text.contains("stop")), "{hints:?}");
    assert!(!hints.iter().any(|h| h.text.contains(":url")), "{hints:?}");
    assert!(!hints.iter().any(|h| h.text.contains(":curl")), "{hints:?}");
    assert!(hints.iter().any(|h| h.text.contains("fav")), "{hints:?}");
  }

  #[test]
  fn delete_chip_appears_only_when_focused_row_is_deletable() {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use std::path::PathBuf;
    // No focused row → chip hidden.
    let empty_app = App::new(AppOptions::default());
    let empty_hints = build_models_hints(&empty_app, FilterChipMode::Inactive, false, false);
    assert!(
      !empty_hints.iter().any(|h| h.text.contains("delete")),
      "no focused name = no delete chip: {empty_hints:?}"
    );

    // Focused row + deletable=true (NotLaunched / Stopped) → chip
    // appears.
    let mut focused_app = App::new(AppOptions::default());
    focused_app.models = vec![DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }];
    focused_app.go_top();
    let deletable_hints = build_models_hints(&focused_app, FilterChipMode::Inactive, false, true);
    assert!(
      deletable_hints.iter().any(|h| h.text.contains("delete")),
      "deletable focused row must surface delete chip: {deletable_hints:?}"
    );

    // Same focused row but deletable=false → chip hidden.
    let non_deletable = build_models_hints(&focused_app, FilterChipMode::Inactive, false, false);
    assert!(
      !non_deletable.iter().any(|h| h.text.contains("delete")),
      "non-deletable row must hide delete chip: {non_deletable:?}"
    );
  }

  #[test]
  fn filter_chip_strip_switches_between_editing_and_resting() {
    // Edit mode: chip strip surfaces `⏎:launch` (filter is a
    // live predicate, so Enter drills into the focused row) plus
    // the in-edit chord (`Esc:stop edit`).
    use crate::tui::keybindings::ENTER_LABEL;
    let enter_launch = format!("{ENTER_LABEL}:launch");
    let mut app = App::new(AppOptions::default());
    app.open_filter();
    let editing = build_models_hints(&app, FilterChipMode::Editing, false, false);
    assert!(
      editing.iter().any(|h| h.text == enter_launch),
      "editing mode must surface {enter_launch} (filter is live, no apply): {editing:?}"
    );
    assert!(
      editing.iter().any(|h| h.text == "Esc:stop edit"),
      "editing mode must surface stop-edit chord: {editing:?}"
    );
    assert!(
      !editing.iter().any(|h| h.text.contains("apply")),
      "editing mode must NOT surface apply (filter applies live): {editing:?}"
    );
    assert!(
      !editing.iter().any(|h| h.text.contains("clear")),
      "editing mode must NOT surface clear: {editing:?}"
    );
    // Resting mode: `e:edit`, `Esc:clear`, `⏎:launch`, plus a
    // navigation hint so the user knows arrows still work.
    let resting = build_models_hints(&app, FilterChipMode::Resting, false, false);
    assert!(
      resting.iter().any(|h| h.text == "e:edit"),
      "resting mode must surface enter-edit chord: {resting:?}"
    );
    assert!(
      resting.iter().any(|h| h.text.contains("clear")),
      "resting mode must surface clear chord: {resting:?}"
    );
    assert!(
      resting.iter().any(|h| h.text == enter_launch),
      "resting mode must surface {enter_launch}: {resting:?}"
    );
    assert!(
      !resting.iter().any(|h| h.text.contains("apply")),
      "resting mode must NOT surface apply: {resting:?}"
    );
    assert!(
      resting.iter().any(|h| h.text.contains("↑/↓")),
      "resting mode must hint that arrows still navigate: {resting:?}"
    );
  }

  #[test]
  fn filter_chip_mode_projects_focus_and_edit_state() {
    let mut app = App::new(AppOptions::default());
    // No filter — Inactive.
    assert_eq!(filter_chip_mode(&app, false), FilterChipMode::Inactive);
    // Open filter (auto-enters edit) — Editing.
    app.open_filter();
    assert_eq!(filter_chip_mode(&app, true), FilterChipMode::Editing);
    // Exit edit — Resting (focus still on filter).
    app.filter_input.exit_edit();
    assert_eq!(filter_chip_mode(&app, true), FilterChipMode::Resting);
    // Focus moves back to List but buffer still has content — the
    // chip strip stops claiming the filter slot and falls back to
    // the model navigation chips. Filter remains visible in the
    // pane title, just not in the chip strip.
    app.filter_input.set_text("qwen");
    app.focus = Focus::List;
    assert_eq!(filter_chip_mode(&app, true), FilterChipMode::Inactive);
  }

  #[test]
  fn focused_row_is_deletable_matches_idle_states_only() {
    use crate::tui::list_pane::ListRow;
    use crate::tui::status_icons::SurfaceState;
    use std::path::PathBuf;
    fn model_row(state: SurfaceState) -> ListRow {
      ListRow::Model {
        path: PathBuf::from("/m/qwen.gguf"),
        name: "qwen".into(),
        arch: String::new(),
        params: String::new(),
        quant: String::new(),
        native_ctx: None,
        weights_bytes: None,
        mode_hint: String::new(),
        backend: String::new(),
        favorite: false,
        state,
        port: None,
        device: None,
        launch_id: None,
        launch_name: None,
      }
    }
    let app = App::new(AppOptions::default());
    // Idle states allow delete.
    for s in [SurfaceState::NotLaunched, SurfaceState::Stopped] {
      let rows = vec![model_row(s)];
      assert!(
        focused_row_is_deletable(&app, &rows),
        "{s:?} should be deletable"
      );
    }
    // In-use states refuse delete.
    for s in [
      SurfaceState::Launching,
      SurfaceState::Loading,
      SurfaceState::Ready,
      SurfaceState::Error,
      SurfaceState::External,
    ] {
      let rows = vec![model_row(s)];
      assert!(
        !focused_row_is_deletable(&app, &rows),
        "{s:?} must block delete"
      );
    }
    // Lemonade registry models are never deletable, even when idle — there's no
    // local file to unlink.
    let mut lemon = model_row(SurfaceState::NotLaunched);
    if let ListRow::Model { path, .. } = &mut lemon {
      *path = PathBuf::from("lemonade://Llama-3.1-8B");
    }
    assert!(
      !focused_row_is_deletable(&app, &[lemon]),
      "Lemonade registry row must never be deletable"
    );
  }

  fn spans_plain(spans: &[Span<'static>]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
  }

  #[test]
  fn title_left_drops_brand_segments_in_priority_order() {
    use crate::theme::palette_for;
    use unicode_width::UnicodeWidthStr;
    let mut app = App::new(AppOptions::default());
    // Connected → the short "daemon" label, so the segment widths are
    // deterministic for the breakpoint assertions below.
    app.daemon_connected = true;
    let palette = palette_for(app.options.theme);

    // Wide: every segment present.
    let wide = spans_plain(&title_left_spans(120, &app, palette));
    assert!(wide.contains("LlamaStash"));
    assert!(wide.contains("v"), "version kept when wide: {wide:?}");
    assert!(wide.contains("daemon"), "daemon kept when wide: {wide:?}");
    assert!(wide.contains("macchiato"), "theme kept when wide: {wide:?}");

    // At the 80-col floor the line still fits everything (the brand is
    // short), so the regression we guard against is a mid-word clip —
    // assert the rendered width leaves the reserved 1-cell gap.
    let at80 = spans_plain(&title_left_spans(80, &app, palette));
    assert!(
      at80.width() < 80,
      "80-col brand must leave a gap, got width {} for {at80:?}",
      at80.width()
    );

    // Squeeze each segment out one at a time and confirm the drop
    // order: theme first, then daemon, then version, never the name.
    let theme_dropped = spans_plain(&title_left_spans(34, &app, palette));
    assert!(theme_dropped.contains("daemon"));
    assert!(
      !theme_dropped.contains("macchiato"),
      "theme drops first: {theme_dropped:?}"
    );

    let daemon_dropped = spans_plain(&title_left_spans(20, &app, palette));
    assert!(
      daemon_dropped.contains("v0"),
      "version kept: {daemon_dropped:?}"
    );
    assert!(
      !daemon_dropped.contains("daemon"),
      "daemon drops second: {daemon_dropped:?}"
    );

    let only_name = spans_plain(&title_left_spans(12, &app, palette));
    assert!(only_name.contains("LlamaStash"));
    assert!(
      !only_name.contains('v') || only_name.trim() == "LlamaStash",
      "version drops last, name always stays: {only_name:?}"
    );
  }

  #[test]
  fn title_left_never_strands_a_trailing_separator() {
    use crate::theme::palette_for;
    let app = App::new(AppOptions::default());
    let palette = palette_for(app.options.theme);
    // At a width that keeps version but drops daemon/theme, the line
    // must not end in a dangling ` · ` separator.
    let s = spans_plain(&title_left_spans(20, &app, palette));
    assert!(!s.trim_end().ends_with('·'), "no stranded separator: {s:?}");
  }
}
