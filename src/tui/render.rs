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
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::Focus;
use crate::tui::{
  advanced_panel, confirm_overlay, help_bar, help_overlay, host_stats_pane, info_pane, list_pane,
  logo_pane, right_pane,
};

const INFO_ROW_HEIGHT: u16 = 7;
const MIN_HEIGHT_FOR_INFO_ROW: u16 = 18;
const HOST_PANEL_WIDTH: u16 = 28;
/// Lower bound on what `render()` will paint a full dashboard into.
/// Matches the `--render-size` parser's minimum (40×10). Anything
/// smaller renders the placeholder instead so a sub-minimum terminal
/// doesn't silently clip every panel (audit §5 #9).
const MIN_RENDER_WIDTH: u16 = 40;
const MIN_RENDER_HEIGHT: u16 = 10;
// COMPACT_BANNER is 7 cells wide; +1 cell padding each side + 2
// border cells = 11. Drop the panel entirely on narrower terminals.
const LOGO_PANEL_WIDTH: u16 = 11;
const MIN_LOGO_INNER_WIDTH: u16 = 9;

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
  // amortise to a single build (audit §4.1 #1). Cleared at the
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

  // Vertical layout: title, [info,] body. The filter input renders
  // inline in the Models block title now, so the body owns the
  // bottom edge — no dedicated filter row.
  let mut constraints: Vec<Constraint> = Vec::with_capacity(3);
  constraints.push(Constraint::Length(1));
  if show_info_row {
    constraints.push(Constraint::Length(INFO_ROW_HEIGHT));
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
  render_body(frame, chunks[idx], app, &palette);

  // Overlays last. The launch picker no longer has a modal — the
  // form lives inline in the right pane's Settings tab. The
  // `launch_picker` module still owns the form state struct, but no
  // dedicated overlay is painted.
  if app.focus == Focus::AdvancedPanel {
    if let Some(state) = &app.advanced_panel {
      advanced_panel::render(frame, area, state, &palette);
    }
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
  // (brand + daemon dot) flexes into the rest. The slot width is
  // derived live from the App's `KeyMap` so a user-supplied
  // `keybindings:` override flows through to the visible hints. On
  // terminals too narrow to fit both the hint strip and a readable
  // brand we drop the hints — the user can still press `?` for the
  // full help overlay, so nothing is lost.
  let hint_slot = help_bar::global_hint_slot_width(app);
  let min_brand_w: u16 = 20;
  let show_hints = area.width >= hint_slot.saturating_add(min_brand_w);
  if show_hints {
    let split = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Min(min_brand_w), Constraint::Length(hint_slot)])
      .split(area);
    render_title_left(frame, split[0], app, palette);
    help_bar::render_global(frame, split[1], app, palette);
  } else {
    render_title_left(frame, area, app, palette);
  }
}

fn render_title_left(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let version = env!("CARGO_PKG_VERSION");
  let (dot_color, daemon_label) = if app.daemon_connected {
    (palette.success, "daemon")
  } else {
    (palette.warning, "daemon connecting…")
  };
  let theme_name = app.options.theme.short_name();
  // Text colour for content on the accent bar. Most themes route
  // this to `palette.bg`; mono pins it to Black because `palette.bg`
  // there is `Color::Reset` and would render as the terminal's
  // default fg (typically light on a dark terminal) over the White
  // accent bar — i.e. invisible.
  let on_accent = palette.on_accent;
  let line = Line::from(vec![
    Span::raw(" "),
    // Llama mascot glyph — picked from the BMP so it renders without
    // an emoji-capable font dependency.
    Span::styled("🦙 ", Style::default().fg(on_accent)),
    Span::styled(
      "LlamaStash",
      Style::default().fg(on_accent).add_modifier(Modifier::BOLD),
    ),
    Span::styled(format!(" v{version} · "), Style::default().fg(on_accent)),
    Span::styled("●", Style::default().fg(dot_color)),
    Span::raw(" "),
    Span::styled(daemon_label, Style::default().fg(on_accent)),
    Span::styled(" · ", Style::default().fg(on_accent)),
    // Half-filled circle glyph — visual cue for "theme" / light/dark.
    Span::styled("◐ ", Style::default().fg(on_accent)),
    Span::styled(theme_name, Style::default().fg(on_accent)),
  ]);
  let para = Paragraph::new(line).style(Style::default().bg(palette.accent).fg(on_accent));
  frame.render_widget(para, area);
}

/// Render the three-panel info row.
fn render_info_row(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // Reserve the Logo panel only when there's enough width for its
  // inner area to be readable. Otherwise the Daemon panel claims the
  // freed space.
  let show_logo = area
    .width
    .saturating_sub(HOST_PANEL_WIDTH)
    .saturating_sub(LOGO_PANEL_WIDTH)
    >= MIN_LOGO_INNER_WIDTH + 2;
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
  let show_right = app.right_pane_visible();
  let split = if show_right {
    Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
      .split(area)
  } else {
    // Hidden right pane → Models list owns the whole body width.
    Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Percentage(100)])
      .split(area)
  };
  let rows = app.rendered_rows();
  let title = build_models_title(app, split[0].width as usize, &rows);
  let filter_chip = models_filter_chip(app);
  let list_focused = list_is_focused(app.focus);
  let right_focused = right_is_focused(app.focus);
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
      },
    );
  }
  if show_right {
    right_pane::render(frame, split[1], app, palette, right_focused);
  }
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
  let filter_active = !(app.filter_buffer.is_empty() && app.focus != Focus::Filter);
  let filter = if filter_active {
    list_pane::FilterTitle::Active {
      buffer: app.filter_buffer.as_str(),
      focused: app.focus == Focus::Filter,
    }
  } else {
    list_pane::FilterTitle::Inactive
  };
  let on_running = focused_row_is_running(app, rows);
  let hints = build_models_hints(app, filter_active, on_running);
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
fn build_models_hints(app: &App, filter_active: bool, on_running: bool) -> Vec<String> {
  use crate::tui::keybindings::Action;
  let mut out: Vec<String> = Vec::with_capacity(7);
  if filter_active {
    // While the filter is being typed only the apply/clear keys are
    // useful — every row-action hint would just clutter the strip.
    if let Some(h) = app.hint(Focus::Filter, Action::Submit) {
      out.push(h);
    }
    if let Some(h) = app.hint(Focus::Filter, Action::ClearFilter) {
      out.push(h);
    }
  } else {
    // Audit §F5 #23: only surface `Enter:launch` when the cursor
    // sits on a launchable row. `open_launch_picker` is silently a
    // no-op on header rows (`★ Favorites`, `↺ Recent`, folder
    // group headings), so showing the chip there would teach a
    // binding that doesn't fire.
    if app.focused_name().is_some() {
      if let Some(h) = app.hint(Focus::List, Action::OpenLaunchPicker) {
        out.push(h);
      }
    }
    // When the cursor sits on a running row, `s:stop` is the most
    // valuable next keystroke — hoist it ahead of `f:fav` so it
    // doesn't get clipped first under width pressure and reads as
    // the headline action for that row.
    if on_running {
      if let Some(h) = app.hint(Focus::List, Action::StopModel) {
        out.push(h);
      }
    }
    // `favorite` is the canonical description; override here to
    // keep the chip terse without renaming the help-overlay entry.
    if let Some(h) = app.hint_with(Focus::List, Action::ToggleFavorite, "fav") {
      out.push(h);
    }
    if let Some(h) = app.hint(Focus::List, Action::YankPath) {
      out.push(h);
    }
    if on_running {
      if let Some(h) = app.hint(Focus::List, Action::YankUrl) {
        out.push(h);
      }
      if let Some(h) = app.hint(Focus::List, Action::YankCurl) {
        out.push(h);
      }
    }
  }
  out
}

/// Resolve the inactive-filter chip label (`/:filter`) against the
/// live keymap so a remap of `open_filter` flows through. Falls
/// back to a static `/` glyph if the user has unbound the action
/// (the chip still hints at the filter feature even without a key).
fn models_filter_chip(app: &App) -> String {
  use crate::tui::keybindings::Action;
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

fn render_empty_state(
  frame: &mut Frame<'_>,
  area: Rect,
  palette: &Palette,
  title: list_pane::TitleInputs<'_>,
  filter_chip: &str,
  focused: bool,
) {
  use ratatui::widgets::{Block, Borders};
  let title_line = list_pane::build_block_title(title, filter_chip, palette);
  let border_color = list_pane::border_color(palette, focused);
  let block = Block::default()
    .title(title_line)
    .borders(Borders::ALL)
    .border_style(Style::default().fg(border_color));
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
    // Audit §5 #9: sub-`MIN_RENDER_*` terminals used to paint the
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
    // A 16-row terminal is below MIN_HEIGHT_FOR_INFO_ROW; the info
    // row drops and only title + body render.
    let app = App::new(AppOptions::default());
    let rows = render_into(80, 16, app);
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
    // 45-col terminal: 28 (host) + 11 (logo) leaves only ~6 cells
    // for the daemon middle, which is below the
    // `MIN_LOGO_INNER_WIDTH + 2` threshold the renderer enforces.
    // The Logo panel drops and Daemon flexes to fill the rest. We
    // assert the banner glyph (the only thing the logo panel emits)
    // is absent rather than greping for the theme tag — that tag
    // now lives on the top header bar regardless of the logo panel.
    let app = App::new(AppOptions::default());
    let rows = render_into(45, 30, app);
    let body = rows.join("\n");
    assert!(body.contains("Host"));
    assert!(body.contains("Daemon"));
    assert!(
      !body.contains("██"),
      "logo panel should be hidden at width 45: {body}"
    );
  }

  #[test]
  fn wider_width_shows_logo_panel() {
    let app = App::new(AppOptions::default());
    let rows = render_into(100, 30, app);
    let body = rows.join("\n");
    // At 100 cols, Host(28) + Daemon(min 1) + Logo(11) easily fit.
    // The logo panel emits the COMPACT_BANNER glyphs (`██`) — assert
    // those rather than the theme tag, which now lives on the
    // top-row hint strip and may get clipped at narrower widths.
    assert!(
      body.contains("██"),
      "logo panel should render at width 100: {body}"
    );
  }

  #[test]
  fn filter_input_appears_inline_in_models_title_when_focused() {
    let mut app = App::new(AppOptions::default());
    app.focus = Focus::Filter;
    app.filter_buffer = "qwen".into();
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
      &app, /*filter_active=*/ false, /*on_running=*/ true,
    );
    let stop_at = hints
      .iter()
      .position(|h| h.contains("stop"))
      .expect("s:stop must appear when on_running");
    let fav_at = hints
      .iter()
      .position(|h| h.contains("fav"))
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
      &app, /*filter_active=*/ false, /*on_running=*/ false,
    );
    assert!(!hints.iter().any(|h| h.contains("stop")), "{hints:?}");
    assert!(!hints.iter().any(|h| h.contains(":url")), "{hints:?}");
    assert!(!hints.iter().any(|h| h.contains(":curl")), "{hints:?}");
    assert!(hints.iter().any(|h| h.contains("fav")), "{hints:?}");
  }
}
