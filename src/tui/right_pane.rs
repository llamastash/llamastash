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
  let bottom_chips = bottom_hint_chips(app);
  let border_color = if focused {
    Color::Yellow
  } else {
    palette.accent
  };

  let mut outer = Block::default()
    .title(title_line)
    .borders(Borders::ALL)
    .border_style(Style::default().fg(border_color));
  // All right-pane key hints live on the bottom border now —
  // contextual to the active tab and the current focus. Keeps the
  // top reserved for the tab strip alone (cleaner mnemonic
  // underlines) and gives the user one stable place to scan for
  // active keys.
  if !bottom_chips.is_empty() {
    outer = outer.title_bottom(bottom_hint_line(&bottom_chips, palette));
  }
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
  let para = Paragraph::new(Line::from(Span::styled(line, palette.muted_style())));
  frame.render_widget(para, area);
}

/// Contextual hint chips that ride on the right pane's *bottom*
/// border. The strip resolves live against the App's `KeyMap` so a
/// `keybindings:` config override flows through automatically.
///
/// Each (focus, tab) combination picks its own set so the strip
/// stays scannable. Settings has two distinct contexts — the
/// read-only running view (focused model has a managed launch and
/// no picker is open) gets the yank + stop chips, while the
/// editable launch form gets cycle / advanced / Enter chips.
/// `c` / `u` are intentionally absent from the editable form: the
/// running URL belongs to the live instance, not to whatever
/// duplicate the user is staging.
pub(crate) fn bottom_hint_chips(app: &App) -> Vec<String> {
  use crate::tui::keybindings::{Action, Focus};
  let mut chips: Vec<String> = Vec::with_capacity(6);
  let push = |c: &mut Vec<String>, h: Option<String>| {
    if let Some(h) = h {
      c.push(h);
    }
  };
  match (app.focus, app.right_tab) {
    // Edit-mode focuses surface the keys live inside the buffer.
    // Override descriptions to keep the chips short without
    // muddying the help-overlay rows.
    (Focus::ChatInput, _) => {
      push(
        &mut chips,
        app.hint_with(Focus::ChatInput, Action::ExitEdit, "clear"),
      );
      push(&mut chips, app.hint(Focus::ChatInput, Action::SendChat));
      push(
        &mut chips,
        app.hint_with(Focus::ChatInput, Action::ToggleThinkCollapse, "think"),
      );
    }
    (Focus::EmbedInput, _) => {
      push(
        &mut chips,
        app.hint_with(Focus::EmbedInput, Action::ExitEdit, "clear"),
      );
      push(&mut chips, app.hint(Focus::EmbedInput, Action::Submit));
    }
    (Focus::RerankInput, _) => {
      push(
        &mut chips,
        app.hint_with(Focus::RerankInput, Action::ExitEdit, "clear"),
      );
      push(
        &mut chips,
        app.hint_with(Focus::RerankInput, Action::Submit, "rerank"),
      );
      push(
        &mut chips,
        app.hint(Focus::RerankInput, Action::StageRerankCandidate),
      );
    }
    // Navigation focuses surface the entry-point keystroke per tab.
    (_, RightTab::Logs) => {
      push(
        &mut chips,
        app.hint(Focus::RightPane, Action::ToggleAutoScroll),
      );
    }
    (_, RightTab::Chat | RightTab::Embed | RightTab::Rerank) => {
      push(&mut chips, app.hint(Focus::RightPane, Action::EnterEdit));
    }
    (_, RightTab::Settings) => {
      let running_readonly = app.launch_picker.is_none() && app.focused_managed().is_some();
      if running_readonly {
        // Read-only running view — `c` (curl) / `u` (url) target
        // the live instance, so they belong here, not on the
        // editable form. `s` doubles as `stop` when the dispatcher
        // sees it on Settings.
        push(
          &mut chips,
          app.hint_with(Focus::RightPane, Action::ToggleAutoScroll, "stop"),
        );
        push(&mut chips, app.hint(Focus::RightPane, Action::YankPath));
        push(&mut chips, app.hint(Focus::RightPane, Action::YankUrl));
        push(&mut chips, app.hint(Focus::RightPane, Action::YankCurl));
      } else if app.focused_path().is_some() {
        // Editable launch form — surface launch + the field/value
        // cycle pairs + `a:advanced` + `p:path`. No `u`/`c` here
        // because the user is editing settings, not addressing a
        // running instance.
        push(
          &mut chips,
          app.hint_with(Focus::RightPane, Action::Submit, "launch"),
        );
        push(
          &mut chips,
          app.hint(Focus::RightPane, Action::OpenAdvancedPanel),
        );
        if let (Some(down), Some(up)) = (
          app.hint_with(Focus::RightPane, Action::MoveDown, "cycle fields"),
          app.hint_with(Focus::RightPane, Action::MoveUp, "cycle fields"),
        ) {
          chips.push(bidirectional_chip(&up, &down, "cycle fields"));
        }
        if let (Some(next), Some(prev)) = (
          app.hint_with(Focus::RightPane, Action::CycleValueNext, "cycle value"),
          app.hint_with(Focus::RightPane, Action::CycleValuePrev, "cycle value"),
        ) {
          chips.push(bidirectional_chip(&prev, &next, "cycle value"));
        }
        push(&mut chips, app.hint(Focus::RightPane, Action::YankPath));
      }
    }
  }
  chips
}

/// Collapse a (reverse, forward) `key:description` pair into a
/// single chip like `↑↓:cycle fields`. Falls back to the forward
/// chip alone if the keys match (the binding collapsed to one
/// chord).
fn bidirectional_chip(reverse: &str, forward: &str, description: &str) -> String {
  let key = |chip: &str| -> Option<String> { chip.split(':').next().map(str::to_string) };
  match (key(reverse), key(forward)) {
    (Some(r), Some(f)) if r != f => format!("{r}{f}:{description}"),
    _ => forward.to_string(),
  }
}

/// Render the bottom-border hint strip as a styled line. Chips are
/// muted and separated by ` · `, matching the in-block status row
/// chips so the visual cadence carries across panes.
fn bottom_hint_line(chips: &[String], palette: &Palette) -> Line<'static> {
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(chips.len() * 2 + 2);
  spans.push(Span::raw(" "));
  for (i, chip) in chips.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" · ", palette.muted_style()));
    }
    spans.push(Span::styled(chip.clone(), palette.muted_style()));
  }
  spans.push(Span::raw(" "));
  Line::from(spans)
}

/// Compose the block title as a styled line: ` Settings │ Logs │
/// Chat `. The active tab is highlighted; all key hints live on
/// the *bottom* border now (see [`bottom_hint_chips`]) so the
/// top stays a clean tab strip.
fn block_title_line(app: &App, tabs: &[RightTab], palette: &Palette) -> Line<'static> {
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 3 + 4);
  spans.push(Span::raw(" "));
  for (i, tab) in tabs.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" │ ", palette.muted_style()));
    }
    // Active tab gets `panel_title` + bold so it reads like the
    // panel's heading text (matches Host/Daemon/Models titles).
    // Inactive tabs stay muted so the heading carries clear focus.
    // The mnemonic underline (first letter) is applied separately
    // by [`mnemonic_spans`].
    let active = *tab == app.right_tab;
    spans.extend(mnemonic_spans(tab.label(), active, palette));
  }
  spans.push(Span::raw(" "));
  Line::from(spans)
}

/// Split a tab label into spans that underline the first character
/// when it should serve as a quick-jump mnemonic. The selected tab
/// drops the underline (its panel_title style already calls focus
/// to it; doubling up with an underline reads as noise).
fn mnemonic_spans(label: &str, active: bool, palette: &Palette) -> Vec<Span<'static>> {
  let base_style = if active {
    palette.title_style()
  } else {
    palette.muted_style()
  };
  let mut chars = label.chars();
  let first = match chars.next() {
    Some(c) => c.to_string(),
    None => return vec![Span::styled(label.to_string(), base_style)],
  };
  let rest: String = chars.collect();
  let first_style = if active {
    base_style
  } else {
    base_style.add_modifier(Modifier::UNDERLINED)
  };
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(2);
  spans.push(Span::styled(first, first_style));
  if !rest.is_empty() {
    spans.push(Span::styled(rest, base_style));
  }
  spans
}

/// Render line 1 of the header: the model's display name in bold
/// blue (`panel_title` slot — same hue as the `Host` / `Daemon` /
/// `Models` panel headings so the right pane reads as a peer panel).
/// Falls back to `—` when nothing is focused.
fn render_header_name(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  use crate::util::paths::model_display_name;
  let name_style = palette.title_style();
  let name_line = match app.right_pane_focus() {
    Some(m) => Line::from(Span::styled(model_display_name(&m.path), name_style)),
    None => match app.focused_path() {
      Some(p) => Line::from(Span::styled(model_display_name(&p), name_style)),
      None => Line::from(Span::styled("—", palette.muted_style())),
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
      let label_style = palette.label_style();
      let value_style = palette.text_style();
      Line::from(vec![
        Span::styled(format!(":{}  ", m.port), palette.muted_style()),
        Span::styled(
          format!("{} ", glyph_for(m.state)),
          Style::default().fg(crate::tui::status_icons::colour_for(m.state, palette)),
        ),
        Span::styled(
          label_for(m.state).to_ascii_lowercase(),
          palette.text_style(),
        ),
        Span::styled("  ", Style::default()),
        // Split stats into label/value spans so `RAM` and `CPU` read
        // as blue labels matching the in-pane convention (Host /
        // Daemon panes) instead of disappearing into the same muted
        // tone as the value digits.
        Span::styled(rss, value_style),
        Span::styled(" RAM", label_style),
        Span::styled(" · ", palette.muted_style()),
        Span::styled(cpu, value_style),
        Span::styled(" CPU", label_style),
      ])
    }
    None => match app.focused_path() {
      Some(_) => Line::from(Span::styled("not launched", palette.muted_style())),
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
  fn bottom_hint_chips_match_each_focus_tab_combo() {
    use crate::tui::keybindings::Focus;
    // Navigation focuses surface the entry-point keystroke per tab.
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::RightPane, RightTab::Logs)),
      vec!["s:auto-scroll".to_string()]
    );
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::RightPane, RightTab::Chat)),
      vec!["e:edit".to_string()]
    );
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::RightPane, RightTab::Embed)),
      vec!["e:edit".to_string()]
    );
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::RightPane, RightTab::Rerank)),
      vec!["e:edit".to_string()]
    );
    // Settings on an unfocused selection has no model to act on,
    // so no chips fire — the bottom border stays bare instead of
    // teaching keys that no-op.
    assert!(bottom_hint_chips(&app_with_focus(Focus::RightPane, RightTab::Settings)).is_empty());
    // Edit-mode focuses surface the in-buffer keystrokes.
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::ChatInput, RightTab::Chat)),
      vec![
        "Esc:clear".to_string(),
        "Enter:send".to_string(),
        "Ctrl+r:think".to_string(),
      ]
    );
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::EmbedInput, RightTab::Embed)),
      vec!["Esc:clear".to_string(), "Enter:embed".to_string()]
    );
    assert_eq!(
      bottom_hint_chips(&app_with_focus(Focus::RerankInput, RightTab::Rerank)),
      vec![
        "Esc:clear".to_string(),
        "Enter:rerank".to_string(),
        "+:stage candidate".to_string(),
      ]
    );
  }

  fn fake_model() -> crate::discovery::DiscoveredModel {
    crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/qwen.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }
  }

  #[test]
  fn settings_bottom_chips_split_running_readonly_vs_launch_form() {
    // Read-only running view (managed launch present, no picker
    // staged) carries the live-instance verbs: s:stop, p/u/c.
    // The editable launch form carries Enter:launch +
    // advanced + cycle + path — no u/c, since the URL belongs to
    // the running instance, not whatever duplicate the user is
    // staging.
    use crate::tui::keybindings::Focus;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    // Read-only view: no picker open, managed launch present.
    assert_eq!(
      bottom_hint_chips(&app),
      vec![
        "s:stop".to_string(),
        "p:path".to_string(),
        "u:url".to_string(),
        "c:curl".to_string(),
      ]
    );
    // Open the picker — the user is now editing a staged launch.
    // Chips switch to launch+cycle+advanced. u/c are intentionally
    // omitted on the editable form.
    app.open_launch_picker();
    let chips = bottom_hint_chips(&app);
    assert!(chips.contains(&"Enter:launch".to_string()));
    assert!(chips.contains(&"a:advanced".to_string()));
    assert!(chips.contains(&"↑↓:cycle fields".to_string()));
    assert!(chips.contains(&"←→:cycle value".to_string()));
    assert!(chips.contains(&"p:path".to_string()));
    assert!(!chips.iter().any(|c| c.contains("u:url")));
    assert!(!chips.iter().any(|c| c.contains("c:curl")));
  }

  #[test]
  fn settings_bottom_chips_for_unlaunched_focus_show_launch_form() {
    // Unlaunched selection: the form is the only context, so the
    // chips read launch + cycle + path.
    use crate::tui::keybindings::Focus;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    let chips = bottom_hint_chips(&app);
    assert!(chips.contains(&"Enter:launch".to_string()));
    assert!(chips.contains(&"a:advanced".to_string()));
    assert!(chips.contains(&"p:path".to_string()));
    assert!(!chips.iter().any(|c| c.contains("u:url")));
  }

  #[test]
  fn bottom_hint_chips_pick_up_config_keybinding_overrides() {
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
      bottom_hint_chips(&app),
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
  fn block_title_strip_carries_only_tab_labels() {
    // Round-9: hints moved off the top title to the bottom border.
    // The top stays a clean tab strip so the mnemonic underlines
    // read clearly.
    use crate::tui::keybindings::Focus;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.right_tab = RightTab::Logs;
    app.focus = Focus::RightPane;
    let palette = app.palette();
    let tabs = app.available_right_tabs();
    let line = block_title_line(&app, &tabs, palette);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("Logs"));
    assert!(text.contains("Settings"));
    assert!(text.contains("Chat"));
    assert!(
      !text.contains("auto-scroll"),
      "top title must not carry hints: {text:?}"
    );
    assert!(
      !text.contains("Enter:"),
      "top title must not carry hints: {text:?}"
    );
  }

  #[test]
  fn block_title_underlines_mnemonic_letter_for_inactive_tabs() {
    // The first letter of each *inactive* tab label carries the
    // UNDERLINED modifier so it reads as a press-this-letter
    // shortcut hint. The active tab drops the underline so it
    // doesn't double up with the bold focus styling.
    use crate::tui::keybindings::Focus;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.right_tab = RightTab::Settings; // active
    app.focus = Focus::RightPane;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(80, 18)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 80, 18), &app, palette, false))
      .unwrap();
    let buf = term.backend().buffer().clone();
    // Iterate cells on the top border row tracking column (x)
    // directly — `row.find(ch)` returns byte offsets and the `│`
    // separators are multi-byte in UTF-8, throwing the column
    // alignment off if we go through a String first.
    let mut settings_x: Option<u16> = None;
    let mut logs_x: Option<u16> = None;
    for x in 0..buf.area.width {
      let sym = buf.cell((x, 0)).unwrap().symbol();
      if settings_x.is_none() && sym == "S" {
        settings_x = Some(x);
      } else if logs_x.is_none() && sym == "L" {
        logs_x = Some(x);
      }
    }
    let s_cell = buf
      .cell((settings_x.expect("S of Settings on top row"), 0))
      .unwrap();
    let l_cell = buf
      .cell((logs_x.expect("L of Logs on top row"), 0))
      .unwrap();
    assert!(
      !s_cell.modifier.contains(Modifier::UNDERLINED),
      "active Settings tab's first letter must NOT be underlined"
    );
    assert!(
      l_cell.modifier.contains(Modifier::UNDERLINED),
      "inactive Logs tab's first letter must be underlined as a mnemonic"
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
