//! Right-pane tab dispatcher.
//!
//! Renders the block (with focused-model header in the title), the
//! tab strip (when more than one tab is reachable), and dispatches
//! to the active tab's renderer. The tab strip carries dynamic
//! per-tab key hints, and is suppressed entirely when the focused
//! model exposes only `Logs`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::fmt::format_bytes;
use crate::tui::status_icons::{glyph_for, label_for};
use crate::tui::tabs::{chat, embed, logs, rerank, RightTab};

/// Render the full right-pane area: block + (optional) tab strip +
/// the active tab.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let outer = Block::default()
    .title(right_pane_title(app))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent));
  let inner = outer.inner(area);
  frame.render_widget(outer, area);

  let tabs = app.available_right_tabs();
  let show_strip = tabs.len() > 1;
  let body_area = if show_strip {
    let layout = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Length(1), Constraint::Min(1)])
      .split(inner);
    render_tab_strip(frame, layout[0], app, &tabs, palette);
    layout[1]
  } else {
    inner
  };

  match app.right_tab {
    RightTab::Logs => {
      logs::render(frame, body_area, &app.logs_state, palette);
    }
    RightTab::Chat => chat::render(frame, body_area, &app.chat, palette),
    RightTab::Embed => embed::render(frame, body_area, &app.embed, palette),
    RightTab::Rerank => rerank::render(frame, body_area, &app.rerank, palette),
  }
}

fn right_pane_title(app: &App) -> String {
  use crate::util::paths::model_display_name;
  match app.focused_managed() {
    Some(m) => {
      let stats = format_per_model_stats(m);
      format!(
        " {} · :{} {} {}{} ",
        model_display_name(&m.path),
        m.port,
        glyph_for(m.state),
        label_for(m.state).to_ascii_lowercase(),
        stats,
      )
    }
    None => match app.focused_path() {
      Some(p) => format!(" {} · not launched ", model_display_name(&p)),
      None => " — ".into(),
    },
  }
}

/// Format the trailing `· 4.2G RAM · 312% CPU` portion of the right-
/// pane block title. Both fields are optional — `None` renders as
/// `—` so the user sees the column exists but hasn't been populated
/// yet (the per-PID sampler primes one tick after launch).
fn format_per_model_stats(m: &crate::tui::app::ManagedRow) -> String {
  // VRAM intentionally absent in v1 — per-PID attribution requires
  // NVML and is deferred to v2 (README).
  let rss = match m.rss_bytes {
    Some(b) => format_bytes(b),
    None => "—".into(),
  };
  let cpu = match m.cpu_pct {
    Some(p) => format!("{p:.0}%"),
    None => "—".into(),
  };
  format!(" · {rss} RAM · {cpu} CPU")
}

fn render_tab_strip(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  tabs: &[RightTab],
  palette: &Palette,
) {
  let mut spans: Vec<Span<'_>> = Vec::with_capacity(tabs.len() * 3 + 1);
  for (i, tab) in tabs.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" │ ", Style::default().fg(palette.muted)));
    }
    let style = if *tab == app.right_tab {
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
      Style::default().fg(palette.muted)
    };
    spans.push(Span::styled(tab.label(), style));
  }
  spans.push(Span::raw("   "));
  spans.push(Span::styled(
    per_tab_hints(app.right_tab),
    Style::default().fg(palette.muted),
  ));
  frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Per-tab dynamic key hints. Updates with the active tab so the user
/// always sees what the relevant keystrokes are.
pub(crate) fn per_tab_hints(tab: RightTab) -> &'static str {
  match tab {
    RightTab::Logs => "Tab:next  j/k:scroll  L:auto-scroll  Esc:back",
    RightTab::Chat => "Tab:next  Ctrl+Enter:send  r:reasoning  Esc:back",
    RightTab::Embed => "Tab:next  Enter:embed  Esc:back",
    RightTab::Rerank => "Tab:next  Enter:rerank  Esc:back",
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
    // The directory header sits at row 0; the model lands at row 1.
    app.list_cursor = 1;
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
    // The directory header sits at row 0; the model lands at row 1.
    app.list_cursor = 1;
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
    let app = App::new(AppOptions::default());
    assert_eq!(app.available_right_tabs(), vec![RightTab::Logs]);
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(50, 12)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 50, 12), &app, palette))
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
    // None of the multi-tab labels appear when the strip is suppressed.
    for label in ["Chat", "Embed", "Rerank"] {
      assert!(
        !body.contains(label),
        "expected `{label}` absent when only Logs is reachable: {body}"
      );
    }
    // And the per-tab hint strings stay hidden too.
    for hint in ["Ctrl+Enter:send", "Enter:embed", "Enter:rerank"] {
      assert!(
        !body.contains(hint),
        "expected hint `{hint}` absent when strip is suppressed: {body}"
      );
    }
  }
}
