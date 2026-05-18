//! Modal `?` help overlay listing every keybinding grouped by
//! purpose. Rendered as three fixed columns of category sections so
//! a single screen can carry the full keymap. Centred over the
//! dashboard with a translucent border; Esc or `?` closes it.
//!
//! Layout and groupings are static — the overlay does **not** shift
//! based on which pane is focused. Every binding shown is resolved
//! live through [`App::bindings_for`] so config-driven overrides
//! (`keybindings:` in `config.yaml`) reflect here automatically.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus};

/// One displayed help row.
///
/// - `Single` resolves a single `(focus, action)` lookup and renders
///   the live key + the binding's own description.
/// - `Multi` covers several `(focus, action)` pairs that share a
///   key (e.g. all three right-pane submit actions on `Enter`) and
///   collapses them into a single row with an editorial joined
///   description. If a config override breaks the shared-key
///   invariant, the renderer falls back to one line per part so
///   nothing is hidden.
enum Row {
  Single {
    focus: Focus,
    action: Action,
  },
  Multi {
    parts: &'static [(Focus, Action)],
    description: &'static str,
  },
}

/// A vertical block in one column: a bold title followed by its
/// rows.
struct Group {
  title: &'static str,
  rows: &'static [Row],
}

// ─── Category contents ────────────────────────────────────────────
//
// Each row resolves to one line in the overlay. The grouping is
// editorial — actions are deliberately listed under the pane where
// they're most useful even if the keymap technically registers them
// in a different `Focus`. The live binding is always pulled from
// the `Focus` named in the row so a config override flows through.

const GENERAL: &[Row] = &[
  Row::Single {
    focus: Focus::List,
    action: Action::ToggleHelp,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::Quit,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::KillDaemon,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::CycleTheme,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::NextFocus,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::PrevFocus,
  },
  // Shift-letter pane jumps. Bound from either Models or the right
  // pane so they're TUI-wide rather than focus-specific.
  Row::Single {
    focus: Focus::List,
    action: Action::FocusList,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::FocusLogsTab,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::FocusChatTab,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::FocusSettingsTab,
  },
];

/// Models pane's `Enter` collapse: applies the live filter buffer
/// when the user is inside the inline filter input, otherwise opens
/// the launch picker for the focused model.
const MODELS_ENTER: &[(Focus, Action)] = &[
  (Focus::Filter, Action::Submit),
  (Focus::List, Action::OpenLaunchPicker),
];

const MODELS: &[Row] = &[
  Row::Single {
    focus: Focus::List,
    action: Action::MoveUp,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::MoveDown,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::PageUp,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::PageDown,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::GoTop,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::GoBottom,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::OpenFilter,
  },
  Row::Multi {
    parts: MODELS_ENTER,
    description: "apply filter/launch",
  },
  Row::Single {
    focus: Focus::Filter,
    action: Action::ClearFilter,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::ToggleFavorite,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::OpenAdvancedPanel,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::YankUrl,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::YankCurl,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::YankPath,
  },
  Row::Single {
    focus: Focus::List,
    action: Action::StopModel,
  },
];

const LOGS: &[Row] = &[
  Row::Single {
    focus: Focus::RightPane,
    action: Action::ToggleAutoScroll,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::MoveUp,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::MoveDown,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::FocusList,
  },
];

/// The three submit actions across the right-pane inputs collapse
/// into one row at the default `Enter` binding.
const SUBMIT_TRIPLET: &[(Focus, Action)] = &[
  (Focus::ChatInput, Action::SendChat),
  (Focus::EmbedInput, Action::Submit),
  (Focus::RerankInput, Action::Submit),
];

const CHAT_EMBED_RERANK: &[Row] = &[
  Row::Single {
    focus: Focus::RightPane,
    action: Action::EnterEdit,
  },
  Row::Single {
    focus: Focus::ChatInput,
    action: Action::ExitEdit,
  },
  Row::Multi {
    parts: SUBMIT_TRIPLET,
    description: "send/embed/rerank",
  },
  Row::Single {
    focus: Focus::ChatInput,
    action: Action::ToggleThinkCollapse,
  },
  Row::Single {
    focus: Focus::RerankInput,
    action: Action::StageRerankCandidate,
  },
  // Shift+Tab in rerank cycles back to the query field. Surfaced
  // here so the help overlay teaches the reverse direction.
  Row::Single {
    focus: Focus::RerankInput,
    action: Action::PrevField,
  },
];

/// Three different Enter destinations across the Settings flow —
/// open the picker from the Settings tab, launch the model from
/// inside the picker, save & close from the Advanced flags panel.
/// User-facing summary collapses them as `launch/save`.
const SETTINGS_ENTER: &[(Focus, Action)] = &[
  (Focus::RightPane, Action::Submit),
  (Focus::LaunchPicker, Action::Submit),
  (Focus::AdvancedPanel, Action::Submit),
];

/// Picker dismiss vs. advanced-panel dismiss share `Esc` but mean
/// slightly different things (cancel the launch vs. discard flag
/// edits and step back). Both collapse under one row as
/// `cancel/back`.
const SETTINGS_ESC: &[(Focus, Action)] = &[
  (Focus::LaunchPicker, Action::Cancel),
  (Focus::AdvancedPanel, Action::Cancel),
];

const SETTINGS: &[Row] = &[
  Row::Multi {
    parts: SETTINGS_ENTER,
    description: "launch/save",
  },
  Row::Single {
    focus: Focus::LaunchPicker,
    action: Action::MoveDown,
  },
  // Tab/Shift+Tab cycle the form fields (ctx / reasoning / advanced)
  // inside the right pane's Settings tab. Surfaced from
  // `Focus::RightPane` because that's the focus the user occupies
  // when reading the form inline.
  Row::Single {
    focus: Focus::RightPane,
    action: Action::NextField,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::PrevField,
  },
  Row::Multi {
    parts: SETTINGS_ESC,
    description: "cancel/back",
  },
];

// ─── Column assignment ────────────────────────────────────────────
//
// Fixed left-to-right packing. Tuned so the three columns balance
// roughly in height — `Models` is the biggest group so it owns its
// own column; the smaller groups pair up on either side.

const COLUMN_1: &[Group] = &[
  Group {
    title: "General",
    rows: GENERAL,
  },
  Group {
    title: "Logs",
    rows: LOGS,
  },
];

const COLUMN_2: &[Group] = &[Group {
  title: "Models",
  rows: MODELS,
}];

const COLUMN_3: &[Group] = &[
  Group {
    title: "Chat / Embed / Rerank",
    rows: CHAT_EMBED_RERANK,
  },
  Group {
    title: "Settings",
    rows: SETTINGS,
  },
];

/// Render the overlay. Caller is responsible for only invoking
/// this when `app.show_help` is true. Layout is static — the active
/// focus does not change which groups appear or where.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let rect = centred(
    area,
    area.width.saturating_sub(4).min(130),
    area.height.saturating_sub(4).max(20),
  );
  frame.render_widget(Clear, rect);

  // The close chip is keymap-driven — whichever key the user has
  // bound to `toggle_help` is what we surface in the title. Esc
  // also closes the modal but is hardcoded modal-dismiss (not an
  // Action), so we don't claim it here; the user can rely on the
  // bound `?` (or its override) without having to guess.
  let close_chip = app
    .hint_with(Focus::List, Action::ToggleHelp, "close")
    .unwrap_or_else(|| "?:close".to_string());
  let block = Block::default()
    .title(Line::from(vec![
      Span::styled(
        " Help ",
        Style::default()
          .fg(palette.panel_title)
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(
        format!("· {close_chip} "),
        Style::default().fg(palette.muted),
      ),
    ]))
    .borders(Borders::ALL)
    .border_style(Style::default().fg(palette.accent))
    // Breathing room inside the border so column titles and key
    // labels don't kiss the frame on either side.
    .padding(Padding::new(2, 2, 1, 1));
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let cols = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
      Constraint::Ratio(1, 3),
      Constraint::Ratio(1, 3),
      Constraint::Ratio(1, 3),
    ])
    .split(inner);

  for (idx, groups) in [COLUMN_1, COLUMN_2, COLUMN_3].iter().enumerate() {
    let lines = render_column(groups, app, palette);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), cols[idx]);
  }
}

/// Build the line list for one column. Each `Group` becomes a bold
/// title line followed by one row per [`Row`] and a trailing blank
/// for vertical separation between groups.
fn render_column(groups: &[Group], app: &App, palette: &Palette) -> Vec<Line<'static>> {
  let mut out: Vec<Line<'static>> = Vec::new();
  for group in groups {
    out.push(Line::from(Span::styled(
      group.title.to_string(),
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    )));
    for row in group.rows {
      append_row(&mut out, row, app, palette);
    }
    out.push(Line::default());
  }
  out
}

fn append_row(out: &mut Vec<Line<'static>>, row: &Row, app: &App, palette: &Palette) {
  match row {
    Row::Single { focus, action } => {
      if let Some((keys, description)) = resolve_one(app, *focus, *action) {
        out.push(render_binding_line(&keys, &description, palette));
      }
    }
    Row::Multi { parts, description } => {
      let resolved: Vec<(String, String)> = parts
        .iter()
        .filter_map(|(f, a)| resolve_one(app, *f, *a))
        .collect();
      if resolved.is_empty() {
        return;
      }
      let first_key = resolved[0].0.clone();
      let same_key = resolved.iter().all(|(k, _)| *k == first_key);
      if same_key {
        // Collapsed row: shared key + the editorial joined
        // description from the row definition.
        out.push(render_binding_line(&first_key, description, palette));
      } else {
        // A config override broke the shared-key invariant. Fall
        // back to one line per part so the user still sees every
        // remapped key with its own per-binding description.
        for (keys, per_part_desc) in resolved {
          out.push(render_binding_line(&keys, &per_part_desc, palette));
        }
      }
    }
  }
}

fn render_binding_line(keys: &str, description: &str, palette: &Palette) -> Line<'static> {
  Line::from(vec![
    Span::styled(
      format!("  {keys:<14}"),
      Style::default()
        .fg(palette.label)
        .add_modifier(Modifier::BOLD),
    ),
    Span::styled(description.to_string(), Style::default().fg(palette.fg)),
  ])
}

/// Look up every live binding for `action` in `focus` and assemble
/// the display strings. Returns `None` when the user has unbound
/// the action entirely (so it's never silently shown without a key).
fn resolve_one(app: &App, focus: Focus, action: Action) -> Option<(String, String)> {
  let bindings = app.bindings_for(focus);
  let matches: Vec<_> = bindings.iter().filter(|b| b.action == action).collect();
  if matches.is_empty() {
    return None;
  }
  let keys = matches
    .iter()
    .map(|b| b.label)
    .collect::<Vec<_>>()
    .join(",");
  let description = matches[0].description.to_string();
  Some((keys, description))
}

/// Centre a `w × h` rect within `area`, clamping to the available
/// space so a narrow terminal still sees the overlay (just snug).
fn centred(area: Rect, w: u16, h: u16) -> Rect {
  let w = w.min(area.width.saturating_sub(2));
  let h = h.min(area.height.saturating_sub(2));
  let x = area.x + (area.width.saturating_sub(w)) / 2;
  let y = area.y + (area.height.saturating_sub(h)) / 2;
  Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crate::tui::keybindings::KeyMap;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;
  use std::collections::BTreeMap;

  fn render_to_string(width: u16, height: u16, app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
      .draw(|frame| {
        let area = frame.area();
        render(frame, area, app, app.palette());
      })
      .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut lines: Vec<String> = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
      let mut row = String::new();
      for x in 0..buf.area.width {
        row.push_str(buf[(x, y)].symbol());
      }
      lines.push(row.trim_end().to_string());
    }
    lines.join("\n")
  }

  #[test]
  fn overlay_shows_all_five_group_titles() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    for title in [
      "General",
      "Models",
      "Logs",
      "Chat / Embed / Rerank",
      "Settings",
    ] {
      assert!(
        frame.contains(title),
        "missing group `{title}` in:\n{frame}"
      );
    }
  }

  #[test]
  fn logs_esc_uses_models_list_description() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("models list"),
      "Logs Esc description should say `models list`:\n{frame}"
    );
  }

  #[test]
  fn models_enter_collapses_filter_and_launch() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("apply filter/launch"),
      "Models Enter row should be the collapsed `apply filter/launch`:\n{frame}"
    );
    let occurrences = frame.matches("apply filter/launch").count();
    assert_eq!(
      occurrences, 1,
      "expected exactly one collapsed row:\n{frame}"
    );
  }

  #[test]
  fn submit_triplet_collapses_under_one_ctrl_enter_row() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("send/embed/rerank"),
      "right-pane submit row should be `send/embed/rerank`:\n{frame}"
    );
    let occurrences = frame.matches("send/embed/rerank").count();
    assert_eq!(occurrences, 1, "expected a single collapsed row:\n{frame}");
  }

  #[test]
  fn settings_collapses_enter_and_esc() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("launch/save"),
      "Settings Enter row should be `launch/save`:\n{frame}"
    );
    assert!(
      frame.contains("cancel/back"),
      "Settings Esc row should be `cancel/back`:\n{frame}"
    );
  }

  #[test]
  fn overlay_reflects_config_keybinding_overrides() {
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("quit"), String::from("ctrl+q"))]
      .into_iter()
      .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");

    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("Ctrl+q"),
      "remapped quit key missing: {frame}"
    );
    assert!(
      !frame.contains("q,Ctrl+C"),
      "stale default quit aliases still rendered: {frame}"
    );
  }

  #[test]
  fn submit_row_falls_back_to_per_part_lines_when_override_diverges_keys() {
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("send_chat"), String::from("f12"))]
      .into_iter()
      .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");

    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let frame = render_to_string(140, 36, &app);
    // Collapsed text vanishes; per-part bindings still surface.
    assert!(
      !frame.contains("send/embed/rerank"),
      "collapsed row should split after override:\n{frame}"
    );
    assert!(frame.contains("F12"), "F12 send binding missing:\n{frame}");
    // Embed/Rerank still on plain Enter so their per-part rows show
    // up with each binding's own description.
    assert!(
      frame.contains("Enter") && frame.contains("embed"),
      "embed row should remain visible after the chat-only override:\n{frame}"
    );
    assert!(
      frame.contains("rank"),
      "rerank row should remain visible after the chat-only override:\n{frame}"
    );
  }

  #[test]
  fn overlay_layout_is_static_across_focuses() {
    let baseline = App::new(AppOptions::default());
    let mut shifted = App::new(AppOptions::default());
    shifted.focus = Focus::RightPane;
    assert_eq!(
      render_to_string(140, 36, &baseline),
      render_to_string(140, 36, &shifted)
    );
  }

  #[test]
  fn centred_clamps_to_area() {
    let area = Rect::new(0, 0, 40, 10);
    let r = centred(area, 80, 30);
    assert!(r.width <= 38);
    assert!(r.height <= 8);
  }
}
