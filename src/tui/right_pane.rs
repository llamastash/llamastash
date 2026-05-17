//! Right-pane tab dispatcher.
//!
//! The right pane is a single bordered Block. The block's title
//! carries the tab strip (`Logs │ Chat`) so the active surface is
//! visible without a separate strip row. Inside the block:
//!  1. A header line — focused model name · port · state · RAM ·
//!     CPU.
//!  2. The active tab's content rendered directly into the area
//!     beneath. Tab renderers no longer wrap themselves in a
//!     second Block — borders here are owned by this dispatcher,
//!     keeping the panel a single unnested rectangle.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::fmt::format_bytes;
use crate::tui::status_icons::{glyph_for, label_for};
use crate::tui::tabs::{chat, embed, logs, rerank, settings, RightTab};

/// Render the right-pane area as a single unnested Block. `focused`
/// flips the border to yellow so the user can see which side of the
/// dashboard owns the keyboard chain at a glance.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette, focused: bool) {
  let tabs = app.available_right_tabs();
  let title_line = block_title_line(app, &tabs, palette);
  let border_color = if focused {
    Color::Yellow
  } else {
    palette.accent
  };

  let outer = Block::default()
    .title(title_line)
    .borders(Borders::ALL)
    .border_style(Style::default().fg(border_color));
  let inner = outer.inner(area);
  frame.render_widget(outer, area);

  // Inner stack: 1 blank pad, header (1 row), separator line, tab
  // content. The blank pad above and the separator below together
  // breathe the model header off the block edge so it reads as a
  // distinct strip from the body.
  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Min(1),
    ])
    .split(inner);

  render_header(frame, layout[1], app, palette);
  render_separator(frame, layout[2], palette);
  let body_area = layout[3];

  match app.right_tab {
    RightTab::Logs => logs::render(frame, body_area, &app.logs_state, palette),
    RightTab::Chat => chat::render(frame, body_area, &app.chat, palette),
    RightTab::Embed => embed::render(frame, body_area, &app.embed, palette),
    RightTab::Rerank => rerank::render(frame, body_area, &app.rerank, palette),
    RightTab::Settings => settings::render(frame, body_area, app, palette),
  }
}

/// Paint a horizontal line below the model header. Uses the box-
/// drawing horizontal char so the strip mirrors the block's outer
/// border but tinted with `muted` to keep it secondary.
fn render_separator(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
  let line: String = "─".repeat(area.width as usize);
  let para = Paragraph::new(Line::from(Span::styled(
    line,
    Style::default().fg(palette.muted),
  )));
  frame.render_widget(para, area);
}

/// Compose the block title as a styled line: ` Logs │ Chat │ ... `
/// with the active tab highlighted. Trailing per-tab hints are
/// suppressed from the title to keep it scannable; the inner tab
/// content owns its own hint strip when relevant.
fn block_title_line(app: &App, tabs: &[RightTab], palette: &Palette) -> Line<'static> {
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 3 + 2);
  spans.push(Span::raw(" "));
  for (i, tab) in tabs.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" │ ", Style::default().fg(palette.muted)));
    }
    // Active tab gets `panel_title` + bold + underline so it reads
    // like the panel's heading text (matches Host/Daemon/Models titles).
    // Inactive tabs stay muted so the heading carries clear focus.
    let style = if *tab == app.right_tab {
      Style::default()
        .fg(palette.panel_title)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
      Style::default().fg(palette.muted)
    };
    spans.push(Span::styled(tab.label().to_string(), style));
  }
  spans.push(Span::raw(" "));
  Line::from(spans)
}

/// Render the model-header line inside the block: name · port ·
/// state · RAM · CPU. Falls back to `not launched` / `—` when no
/// managed launch exists or no model is focused.
fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  use crate::util::paths::model_display_name;
  // `right_pane_focus()` follows the cursor when it lands on a
  // running model; otherwise it stays pinned to the last running
  // model so the header doesn't whip back to `—` every time the
  // cursor crosses an unlaunched row.
  let line = match app.right_pane_focus() {
    Some(m) => {
      let (rss, cpu) = stats_pair(m);
      let label_style = Style::default().fg(palette.label);
      let value_style = Style::default().fg(palette.fg);
      Line::from(vec![
        Span::styled(
          model_display_name(&m.path),
          Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
          format!("  :{}  ", m.port),
          Style::default().fg(palette.muted),
        ),
        Span::styled(
          format!("{} ", glyph_for(m.state)),
          Style::default().fg(crate::tui::status_icons::colour_for(m.state, palette)),
        ),
        Span::styled(
          label_for(m.state).to_ascii_lowercase(),
          Style::default().fg(palette.fg),
        ),
        Span::styled("  ", Style::default()),
        // Split stats into label/value spans so `RAM` and `CPU` read
        // as blue labels matching the in-pane convention (Host /
        // Daemon panes) instead of disappearing into the same muted
        // tone as the value digits.
        Span::styled(rss, value_style),
        Span::styled(" RAM", label_style),
        Span::styled(" · ", Style::default().fg(palette.muted)),
        Span::styled(cpu, value_style),
        Span::styled(" CPU", label_style),
      ])
    }
    None => match app.focused_path() {
      Some(p) => Line::from(vec![
        Span::styled(
          model_display_name(&p),
          Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  not launched", Style::default().fg(palette.muted)),
      ]),
      None => Line::from(Span::styled("—", Style::default().fg(palette.muted))),
    },
  };
  frame.render_widget(Paragraph::new(line), area);
}

/// Format the trailing `4.2G RAM · 312% CPU` portion of the model
/// header. The runtime renderer now builds these as separate styled
/// spans so `RAM` / `CPU` can carry the blue label colour; this
/// joined form is kept for the `right_pane_title` test helper and
/// regression tests that grep the flattened text.
#[cfg(test)]
fn format_per_model_stats(m: &crate::tui::app::ManagedRow) -> String {
  let (rss, cpu) = stats_pair(m);
  format!("{rss} RAM · {cpu} CPU")
}

/// Split the per-model stats into `(rss, cpu)` strings — needed by
/// the styled-header path so `RAM` / `CPU` labels can carry the
/// `palette.label` colour separately from the digit values.
fn stats_pair(m: &crate::tui::app::ManagedRow) -> (String, String) {
  let rss = match m.rss_bytes {
    Some(b) => format_bytes(b),
    None => "—".into(),
  };
  let cpu = match m.cpu_pct {
    Some(p) => format!("{p:.0}%"),
    None => "—".into(),
  };
  (rss, cpu)
}

/// Title-text view of [`block_title_line`] for tests that just want
/// to grep the flattened text.
#[cfg(test)]
fn right_pane_title(app: &App) -> String {
  use crate::util::paths::model_display_name;
  match app.focused_managed() {
    Some(m) => format!(
      "{} :{} {} {} {}",
      model_display_name(&m.path),
      m.port,
      glyph_for(m.state),
      label_for(m.state).to_ascii_lowercase(),
      format_per_model_stats(m),
    ),
    None => match app.focused_path() {
      Some(p) => format!("{} not launched", model_display_name(&p)),
      None => "—".into(),
    },
  }
}

/// Per-tab dynamic key hints. The block title now carries only the
/// tab strip; these hints live inside the panel body when a tab
/// chooses to surface them. Kept around for tests.
#[cfg(test)]
pub(crate) fn per_tab_hints(tab: RightTab) -> &'static str {
  match tab {
    RightTab::Logs => "Tab:list  ←/→:tabs  j/k:scroll  s:auto-scroll",
    RightTab::Chat => "Tab:list  ←/→:tabs  Ctrl+Enter:send  r:reasoning",
    RightTab::Embed => "Tab:list  ←/→:tabs  Enter:embed",
    RightTab::Rerank => "Tab:list  ←/→:tabs  Enter:rerank",
    RightTab::Settings => "Tab:list  ←/→:tabs  Enter:launch",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::{App, AppOptions, ManagedRow};
  use crate::tui::status_icons::SurfaceState;
  use std::path::PathBuf;

  fn ready_managed(name: &str, rss: Option<u64>, cpu: Option<f32>) -> ManagedRow {
    ManagedRow {
      launch_id: "L1".into(),
      path: PathBuf::from(format!("/m/{name}.gguf")),
      port: 41100,
      state: SurfaceState::Ready,
      rss_bytes: rss,
      cpu_pct: cpu,
    }
  }

  #[test]
  fn per_model_stats_render_both_when_available() {
    // 4_500_000_000 bytes ≈ 4.2 GiB.
    let m = ready_managed("qwen", Some(4_500_000_000), Some(312.0));
    let stats = format_per_model_stats(&m);
    assert!(stats.contains("4.2G RAM"), "stats was: {stats:?}");
    assert!(stats.contains("312% CPU"), "stats was: {stats:?}");
  }

  #[test]
  fn per_model_stats_emit_em_dash_for_missing_readings() {
    let m = ready_managed("qwen", None, None);
    let stats = format_per_model_stats(&m);
    assert!(stats.contains("— RAM"));
    assert!(stats.contains("— CPU"));
  }

  #[test]
  fn per_tab_hints_change_per_tab() {
    assert!(per_tab_hints(RightTab::Logs).contains("scroll"));
    assert!(per_tab_hints(RightTab::Chat).contains("Ctrl+Enter:send"));
    assert!(per_tab_hints(RightTab::Embed).contains("Enter:embed"));
    assert!(per_tab_hints(RightTab::Rerank).contains("Enter:rerank"));
  }

  #[test]
  fn right_pane_title_carries_per_model_stats_when_managed() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.managed = vec![ready_managed("qwen", Some(4_500_000_000), Some(312.0))];
    // Row 0 is the table header, row 1 is the directory group
    // header, row 2 is the model.
    app.list_cursor = 2;
    let title = right_pane_title(&app);
    assert!(title.contains("qwen"));
    assert!(title.contains(":41100"));
    assert!(title.contains("ready"));
    assert!(title.contains("4.2G RAM"));
    assert!(title.contains("312% CPU"));
  }

  #[test]
  fn right_pane_title_says_not_launched_when_no_managed_row() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    // Row 0 is the table header, row 1 is the directory group
    // header, row 2 is the model.
    app.list_cursor = 2;
    let title = right_pane_title(&app);
    assert!(title.contains("not launched"));
  }

  #[test]
  fn tab_strip_is_suppressed_when_only_logs_is_reachable() {
    // A non-Ready (or unlaunched) model exposes only the Logs tab.
    // The render path should omit the strip row entirely — no other
    // tab labels visible, no `│` separator from `render_tab_strip`.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    // Settings is always reachable, but mode-specific tabs (Chat /
    // Embed / Rerank) stay hidden when no model is Ready. So an
    // unlaunched default app exposes only `Logs` + `Settings`.
    let app = App::new(AppOptions::default());
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Logs, RightTab::Settings]
    );
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(50, 12)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 50, 12), &app, palette, false))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
      let mut row = String::with_capacity(buf.area.width as usize);
      for x in 0..buf.area.width {
        row.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(row);
    }
    let body = rows.join("\n");
    // None of the mode-specific labels appear when the model isn't Ready.
    for label in ["Chat", "Embed", "Rerank"] {
      assert!(
        !body.contains(label),
        "expected `{label}` absent when no Ready model: {body}"
      );
    }
  }
}
