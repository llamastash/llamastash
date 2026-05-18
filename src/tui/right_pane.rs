//! Right-pane tab dispatcher.
//!
//! The right pane is a single bordered Block. The block's title
//! carries the tab strip (`Logs │ Chat`) so the active surface is
//! visible without a separate strip row. Inside the block:
//!  1. A model-name line — bold, full width so long filenames have
//!     somewhere to breathe.
//!  2. A stats line — `:port  state  RAM  CPU`.
//!  3. A muted separator rule.
//!  4. The active tab's content rendered directly into the area
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

  // Inner stack: 1 blank pad, name (1 row), 1 blank gap, stats
  // (1 row), separator line, tab content. The blank gap below the
  // name lets the bold-blue model heading breathe before the dense
  // `:port  state  RAM  CPU` line — matching kdash's panel header
  // rhythm. The contextual hint chips ride alongside the tab strip
  // in the block title.
  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Min(1),
    ])
    .split(inner);

  render_header_name(frame, layout[1], app, palette);
  render_header_stats(frame, layout[3], app, palette);
  render_separator(frame, layout[4], palette);
  let body_area = layout[5];

  match app.right_tab {
    RightTab::Logs => logs::render(frame, body_area, &app.logs_state, palette),
    RightTab::Chat => chat::render(frame, body_area, app, palette),
    RightTab::Embed => embed::render(frame, body_area, app, palette),
    RightTab::Rerank => rerank::render(frame, body_area, app, palette),
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

/// Contextual hint chips that ride alongside the tab strip in the
/// block title. Each chip is resolved live against the App's
/// `KeyMap` so a `keybindings:` config override flows through
/// automatically. Empty vec means "no chips for this combination"
/// — the strip stays scannable.
///
/// Some descriptions use overrides (e.g. `clear` on the input
/// `Esc` rather than the binding's `exit edit` description) to
/// keep the chip terse without making the help-overlay row
/// ambiguous.
fn contextual_hints(app: &App) -> Vec<String> {
  use crate::tui::keybindings::{Action, Focus};
  let mut out: Vec<String> = Vec::with_capacity(3);
  let push = |chips: &mut Vec<String>, h: Option<String>| {
    if let Some(h) = h {
      chips.push(h);
    }
  };
  match (app.focus, app.right_tab) {
    // Edit-mode focuses surface the keys the user needs while their
    // cursor is in the prompt buffer. `Esc:clear` matches kdash's
    // filter-active idiom — Esc unwinds the edit. Override the
    // ExitEdit binding's `exit edit` description with `clear` so
    // the chip stays short.
    (Focus::ChatInput, _) => {
      push(
        &mut out,
        app.hint_with(Focus::ChatInput, Action::ExitEdit, "clear"),
      );
      push(&mut out, app.hint(Focus::ChatInput, Action::SendChat));
      // `collapse think` is descriptive in the help overlay but
      // wordy in a chip — override with `think`.
      push(
        &mut out,
        app.hint_with(Focus::ChatInput, Action::ToggleThinkCollapse, "think"),
      );
    }
    (Focus::EmbedInput, _) => {
      push(
        &mut out,
        app.hint_with(Focus::EmbedInput, Action::ExitEdit, "clear"),
      );
      push(&mut out, app.hint(Focus::EmbedInput, Action::Submit));
    }
    (Focus::RerankInput, _) => {
      push(
        &mut out,
        app.hint_with(Focus::RerankInput, Action::ExitEdit, "clear"),
      );
      // The RerankInput Submit description is `rank` in the help
      // overlay (kept terse to align with the Chat/Embed triplet
      // collapse). The chip would rather show the full surface
      // name — override with `rerank`.
      push(
        &mut out,
        app.hint_with(Focus::RerankInput, Action::Submit, "rerank"),
      );
    }
    // Navigation focuses surface the entry-point keystroke per tab.
    (_, RightTab::Logs) => {
      push(
        &mut out,
        app.hint(Focus::RightPane, Action::ToggleAutoScroll),
      );
    }
    (_, RightTab::Chat | RightTab::Embed | RightTab::Rerank) => {
      push(&mut out, app.hint(Focus::RightPane, Action::EnterEdit));
    }
    (_, RightTab::Settings) => {
      // Round-8: keep the title strip minimal — Enter:launch and
      // s:stop (when a managed launch exists). The rest of the
      // edit affordances (advanced, cycle fields, cycle value,
      // p / u / c yanks) ride inside the pane body, beside the
      // "Press Enter to launch with these settings" hint.
      push(
        &mut out,
        app.hint_with(Focus::RightPane, Action::Submit, "launch"),
      );
      if app.focused_managed().is_some() {
        // `s` resolves to ToggleAutoScroll in the binding table,
        // but the dispatcher reroutes it to StopModel on the
        // Settings tab. Override the chip description so the user
        // sees the tab-appropriate verb.
        push(
          &mut out,
          app.hint_with(Focus::RightPane, Action::ToggleAutoScroll, "stop"),
        );
      }
    }
  }
  out
}

/// Compose the block title as a styled line: ` Logs │ Chat │ ... ·
/// hint · hint · ... `. The active tab is highlighted and the
/// contextual key hints sit alongside it so the user doesn't have
/// to scan past the model header to see which keys are live.
fn block_title_line(app: &App, tabs: &[RightTab], palette: &Palette) -> Line<'static> {
  let hints = contextual_hints(app);
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 3 + hints.len() * 2 + 4);
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
  for hint in hints {
    spans.push(Span::styled(
      " · ".to_string(),
      Style::default().fg(palette.muted),
    ));
    spans.push(Span::styled(hint, Style::default().fg(palette.muted)));
  }
  spans.push(Span::raw(" "));
  Line::from(spans)
}

/// Render line 1 of the header: the model's display name in bold
/// blue (`panel_title` slot — same hue as the `Host` / `Daemon` /
/// `Models` panel headings so the right pane reads as a peer panel).
/// Falls back to `—` when nothing is focused.
fn render_header_name(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  use crate::util::paths::model_display_name;
  let name_style = Style::default()
    .fg(palette.panel_title)
    .add_modifier(Modifier::BOLD);
  let name_line = match app.right_pane_focus() {
    Some(m) => Line::from(Span::styled(model_display_name(&m.path), name_style)),
    None => match app.focused_path() {
      Some(p) => Line::from(Span::styled(model_display_name(&p), name_style)),
      None => Line::from(Span::styled("—", Style::default().fg(palette.muted))),
    },
  };
  frame.render_widget(Paragraph::new(name_line), area);
}

/// Render line 2 of the header: `:port  state  RAM  CPU` for a
/// running model, `not launched` when the focused model has no
/// supervisor row, blank when nothing is focused.
fn render_header_stats(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let stats_line = match app.right_pane_focus() {
    Some(m) => {
      let (rss, cpu) = stats_pair(m);
      let label_style = Style::default().fg(palette.label);
      let value_style = Style::default().fg(palette.fg);
      Line::from(vec![
        Span::styled(format!(":{}  ", m.port), Style::default().fg(palette.muted)),
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
      Some(_) => Line::from(Span::styled(
        "not launched",
        Style::default().fg(palette.muted),
      )),
      None => Line::from(Span::raw("")),
    },
  };
  frame.render_widget(Paragraph::new(stats_line), area);
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

  fn app_with_focus(focus: crate::tui::keybindings::Focus, tab: RightTab) -> App {
    let mut app = App::new(AppOptions::default());
    app.focus = focus;
    app.right_tab = tab;
    app
  }

  #[test]
  fn contextual_hints_match_each_focus_tab_combo() {
    use crate::tui::keybindings::Focus;
    // Navigation focuses surface the entry-point keystroke per tab.
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::RightPane, RightTab::Logs)),
      vec!["s:auto-scroll".to_string()]
    );
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::RightPane, RightTab::Chat)),
      vec!["e:edit".to_string()]
    );
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::RightPane, RightTab::Embed)),
      vec!["e:edit".to_string()]
    );
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::RightPane, RightTab::Rerank)),
      vec!["e:edit".to_string()]
    );
    // Round-8: Settings title strip keeps only Enter:launch when
    // no launch is running. Advanced / cycle / yank chips moved
    // into the pane body beside the "Press Enter to launch" hint.
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::RightPane, RightTab::Settings)),
      vec!["Enter:launch".to_string()]
    );
    // Edit-mode focuses surface the in-buffer keystrokes.
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::ChatInput, RightTab::Chat)),
      vec![
        "Esc:clear".to_string(),
        "Enter:send".to_string(),
        "Ctrl+r:think".to_string(),
      ]
    );
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::EmbedInput, RightTab::Embed)),
      vec!["Esc:clear".to_string(), "Enter:embed".to_string()]
    );
    assert_eq!(
      contextual_hints(&app_with_focus(Focus::RerankInput, RightTab::Rerank)),
      vec!["Esc:clear".to_string(), "Enter:rerank".to_string()]
    );
  }

  #[test]
  fn settings_header_chip_surfaces_stop_when_managed_launch_present() {
    // Round-8: the Settings title chip strip adds `s:stop` only
    // when a managed launch is focused — for an unlaunched
    // selection the chip would be misleading (nothing to stop).
    use crate::tui::keybindings::Focus;
    let mut app = App::new(AppOptions::default());
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    assert_eq!(
      contextual_hints(&app),
      vec!["Enter:launch".to_string(), "s:stop".to_string()]
    );
  }

  #[test]
  fn contextual_hints_pick_up_config_keybinding_overrides() {
    use crate::tui::keybindings::{Action, KeyMap};
    use std::collections::BTreeMap;
    // Rebind enter_edit to F4 — the Chat tab's nav-mode chip must
    // surface `F4:edit`, not the stale default `e:edit`.
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("enter_edit"), String::from("f4"))]
      .into_iter()
      .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    app.focus = crate::tui::keybindings::Focus::RightPane;
    app.right_tab = RightTab::Chat;
    assert_eq!(
      contextual_hints(&app),
      vec!["F4:edit".to_string()],
      "remapped enter_edit must flow into the chip"
    );
    // Sanity: looking up the action directly through the App also
    // resolves to F4 (this is the path the chip uses internally).
    assert!(app
      .hint(crate::tui::keybindings::Focus::RightPane, Action::EnterEdit)
      .unwrap()
      .starts_with("F4:"));
  }

  #[test]
  fn block_title_includes_contextual_hints_alongside_tab_strip() {
    use crate::tui::keybindings::Focus;
    let mut app = App::new(AppOptions::default());
    // Force a focused running model so Chat tab is reachable.
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.right_tab = RightTab::Logs;
    app.focus = Focus::RightPane;
    let palette = app.palette();
    let tabs = app.available_right_tabs();
    let line = block_title_line(&app, &tabs, palette);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("Logs"));
    assert!(
      text.contains("s:auto-scroll"),
      "Logs tab must surface s:auto-scroll in title: {text:?}"
    );
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
  fn unlaunched_selection_shows_settings_only() {
    // The right pane follows the cursor. When the cursor sits on a
    // model with no managed launch (or no model at all), only the
    // Settings tab is reachable — Logs, Chat, Embed, Rerank stay
    // hidden until the model is running.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let app = App::new(AppOptions::default());
    assert_eq!(app.available_right_tabs(), vec![RightTab::Settings]);
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
    for label in ["Logs", "Chat", "Embed", "Rerank"] {
      assert!(
        !body.contains(label),
        "expected `{label}` absent for an unlaunched selection: {body}"
      );
    }
    assert!(body.contains("Settings"), "Settings must remain visible");
  }

  #[test]
  fn header_splits_name_and_stats_across_two_lines() {
    // The model name belongs on its own row (so long filenames stop
    // crowding `:port  state  RAM  CPU`); the stats sit on the row
    // immediately below.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
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
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 18)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 18), &app, palette, false))
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
    let name_row = rows.iter().position(|r| r.contains("qwen")).unwrap();
    assert!(
      !rows[name_row].contains(":41100"),
      "stats must not share the name row: {:?}",
      rows[name_row]
    );
    let stats_row = rows.iter().position(|r| r.contains(":41100")).unwrap();
    assert!(
      stats_row > name_row,
      "stats row {stats_row} should sit below name row {name_row}"
    );
    // Round-8: the name and stats lines are separated by exactly
    // one blank row so the bold-blue heading breathes.
    assert_eq!(
      stats_row,
      name_row + 2,
      "stats row should sit one blank line below the name row"
    );
    let gap_row = name_row + 1;
    let gap_inner = rows[gap_row].trim_matches(|c| c == '│' || c == ' ');
    assert!(
      gap_inner.is_empty(),
      "expected blank gap row between name and stats, got: {:?}",
      rows[gap_row]
    );
    assert!(
      rows[stats_row].contains("4.2G RAM") && rows[stats_row].contains("312% CPU"),
      "stats row missing RAM/CPU: {:?}",
      rows[stats_row]
    );
  }

  #[test]
  fn header_name_renders_in_panel_title_blue_and_bold() {
    // The model heading shares the `panel_title` hue with the
    // Host/Daemon/Models block titles so the right pane reads as a
    // peer panel. Asserting the styled cell colour pins both the
    // colour swap and the BOLD modifier introduced in round-8.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 18)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 18), &app, palette, false))
      .unwrap();
    let buf = term.backend().buffer().clone();
    // Locate the `q` of `qwen` and inspect its cell style.
    let mut found = false;
    for y in 0..buf.area.height {
      for x in 0..buf.area.width.saturating_sub(3) {
        let cell = buf.cell((x, y)).unwrap();
        if cell.symbol() == "q"
          && buf.cell((x + 1, y)).unwrap().symbol() == "w"
          && buf.cell((x + 2, y)).unwrap().symbol() == "e"
          && buf.cell((x + 3, y)).unwrap().symbol() == "n"
        {
          assert_eq!(
            cell.fg, palette.panel_title,
            "model name must be painted in panel_title (blue) hue"
          );
          assert!(
            cell.modifier.contains(Modifier::BOLD),
            "model name must be bold"
          );
          found = true;
        }
      }
      if found {
        break;
      }
    }
    assert!(found, "did not locate `qwen` in the header line");
  }
}
