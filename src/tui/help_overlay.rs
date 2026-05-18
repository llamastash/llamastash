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

/// `Enter` while filtering — applies the buffer, returns focus to the
/// list. Surfaces as its own help row so users learn the two
/// distinct contexts (filter vs row action) rather than collapsing
/// them into one ambiguous `apply filter/launch` cell (§5 #6).
const MODELS_ENTER_FILTER: &[(Focus, Action)] = &[(Focus::Filter, Action::Submit)];
/// `Enter` on a list row — opens the inline launch form.
const MODELS_ENTER_LAUNCH: &[(Focus, Action)] = &[(Focus::List, Action::OpenLaunchPicker)];

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
  // Split the two Enter contexts so users learn that the same key
  // applies the filter while typing and opens the launch form
  // otherwise — instead of seeing one ambiguous collapsed row.
  Row::Multi {
    parts: MODELS_ENTER_FILTER,
    description: "apply filter",
  },
  Row::Multi {
    parts: MODELS_ENTER_LAUNCH,
    description: "launch focused model",
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
  // ↑ in rerank cycles back to the previous field (round-7 moved
  // the reverse-field motion from Shift+Tab to ↑ so Tab / Shift+Tab
  // stay on the pane-cycle axis).
  Row::Single {
    focus: Focus::RerankInput,
    action: Action::PrevField,
  },
  // Round-8 yank affordances reachable from the right pane —
  // Chat/Embed/Rerank inherit `p` / `u` / `c` from RIGHT_PANE_BINDINGS,
  // so surface them here too so the user can discover the keys
  // without bouncing back to the Models list.
  Row::Single {
    focus: Focus::RightPane,
    action: Action::YankPath,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::YankUrl,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::YankCurl,
  },
];

/// Two Enter destinations across the Settings flow — launch the
/// model from the inline form, save & close from the Advanced flags
/// panel. User-facing summary collapses them as `launch/save`.
const SETTINGS_ENTER: &[(Focus, Action)] = &[
  (Focus::RightPane, Action::Submit),
  (Focus::AdvancedPanel, Action::Submit),
];

/// `Action::MoveDown` / `MoveUp` on `Focus::RightPane` carry the
/// description `scroll down/up` because that's their meaning on the
/// Logs tab. In the Settings tab the same keys cycle the form's
/// fields (ctx → reasoning → advanced), so the help row needs a
/// context-appropriate override. Wrapping the action in a
/// single-part `Row::Multi` is the renderer's mechanism for that.
const SETTINGS_FIELD_NEXT: &[(Focus, Action)] = &[(Focus::RightPane, Action::MoveDown)];
const SETTINGS_FIELD_PREV: &[(Focus, Action)] = &[(Focus::RightPane, Action::MoveUp)];

/// On the Settings tab `s` routes through `apply_stop_model` (round-8)
/// rather than toggling Logs auto-scroll. The default
/// `Action::ToggleAutoScroll` description (`"auto-scroll"`) doesn't
/// reflect that, so we override the row description.
const SETTINGS_STOP: &[(Focus, Action)] = &[(Focus::RightPane, Action::ToggleAutoScroll)];

const SETTINGS: &[Row] = &[
  Row::Multi {
    parts: SETTINGS_ENTER,
    description: "launch/save",
  },
  // `s` on Settings stops the focused managed launch when one
  // exists; toasts otherwise. Override the default description so
  // the overlay teaches the correct meaning (the same binding on
  // Logs toggles auto-scroll).
  Row::Multi {
    parts: SETTINGS_STOP,
    description: "stop focused launch",
  },
  // ↑/↓ cycle the form's fields. Descriptions overridden because
  // the same bindings mean `scroll up/down` on the Logs tab.
  Row::Multi {
    parts: SETTINGS_FIELD_NEXT,
    description: "next field",
  },
  Row::Multi {
    parts: SETTINGS_FIELD_PREV,
    description: "prev field",
  },
  // ←/→ change the focused field's value (ctx preset, reasoning
  // toggle). Round-7 introduced these dedicated keys so values
  // and fields don't share the same axis.
  Row::Single {
    focus: Focus::RightPane,
    action: Action::CycleValueNext,
  },
  Row::Single {
    focus: Focus::RightPane,
    action: Action::CycleValuePrev,
  },
  // Esc on the Advanced flags panel discards edits and steps back
  // to the Settings form.
  Row::Single {
    focus: Focus::AdvancedPanel,
    action: Action::Cancel,
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
  // `Clear` resets every cell to the terminal default, so paint
  // `palette.bg` back over the rect before rendering text. Without
  // this the overlay reads as transparent on light themes (Latte)
  // and as a coloured-text-on-terminal-bg patch on dark themes that
  // tint their root surface.
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  // Close chip carries both keys that dismiss the overlay:
  //  - `Esc` is hardcoded modal-dismiss in `events::handle_key`
  //    (not an Action), so it always works regardless of the
  //    `toggle_help` binding; we hardcode it here too.
  //  - The `toggle_help` action's live key (default `?`) is
  //    surfaced second so config overrides flow through.
  let toggle_key = app
    .bindings_for(Focus::List)
    .iter()
    .find(|b| b.action == Action::ToggleHelp)
    .map(|b| b.label.to_string())
    .unwrap_or_else(|| "?".to_string());
  let close_chip = format!("Esc/{toggle_key}:close");
  let block = Block::default()
    .title(Line::from(vec![
      Span::styled(
        " Help ",
        Style::default()
          .fg(palette.panel_title)
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(format!("· {close_chip} "), palette.muted_style()),
    ]))
    .borders(Borders::ALL)
    .border_style(palette.accent_style())
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
    Span::styled(description.to_string(), palette.text_style()),
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
  fn models_enter_renders_two_contextual_rows() {
    // Audit §5 #6: the previous single `apply filter/launch` row
    // collapsed two distinct meanings into one cell. Render them
    // separately so users can tell the two contexts apart.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("apply filter"),
      "filter-context Enter row missing:\n{frame}"
    );
    assert!(
      frame.contains("launch focused model"),
      "launch-context Enter row missing:\n{frame}"
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
  fn settings_group_lists_canonical_rows() {
    // The Settings group must teach the four canonical edit
    // surfaces: launch/save, cycle value (Up/Down), cycle fields
    // (Tab/Shift+Tab), and Esc → models list.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("launch/save"),
      "Settings Enter row should be `launch/save`:\n{frame}"
    );
    // Round-7 navigation: ↑/↓ cycle form fields, ←/→ cycle the
    // focused field's value.
    assert!(
      frame.contains("next field"),
      "Settings must surface the ↓ next-field row:\n{frame}"
    );
    assert!(
      frame.contains("cycle value"),
      "Settings must surface the ←/→ cycle-value rows:\n{frame}"
    );
    // The `next field` row must render exactly once (regression
    // guard: pre-round-7 the dead `Focus::LaunchPicker` binding
    // and the live `RightPane.NextField` description both said
    // `next field`, producing a visible duplicate). The renderer
    // now sources the row from `Focus::RightPane.MoveDown` via a
    // Multi override, so any future drift fails this loudly.
    let contexts: Vec<&str> = frame
      .match_indices("next field")
      .map(|(i, _)| {
        let start = i.saturating_sub(40);
        let end = (i + 30).min(frame.len());
        &frame[start..end]
      })
      .collect();
    assert_eq!(
      contexts.len(),
      1,
      "`next field` row must render exactly once. Contexts: {contexts:#?}"
    );
  }

  #[test]
  fn overlay_title_close_chip_surfaces_esc_and_toggle_help_key() {
    // Default keymap: `?` toggles the overlay; Esc always closes
    // it. Both keys must appear in the title so the user can
    // discover either path.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("Esc/?:close"),
      "title close chip must list both Esc and the toggle_help key: {frame}"
    );
  }

  #[test]
  fn overlay_title_close_chip_follows_toggle_help_rebind() {
    // Remap `toggle_help` to F1 — the close chip should now read
    // `Esc/F1:close` (Esc stays because it's the hardcoded modal
    // dismiss).
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("toggle_help"), String::from("f1"))]
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
      frame.contains("Esc/F1:close"),
      "remapped toggle_help must flow through to the close chip: {frame}"
    );
    assert!(
      !frame.contains("Esc/?:close"),
      "stale default `?` chip must drop when toggle_help is rebound: {frame}"
    );
  }

  #[test]
  fn general_chat_jump_row_describes_chat_embed_rerank() {
    // Shift+C dispatches `Action::FocusChatTab`, which picks the
    // first available of Chat / Embed / Rerank for the focused
    // model. The help row must mirror that — `chat` alone would
    // mislead users on embedding-only or reranker models.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("chat/embed/rerank"),
      "Shift+C row must describe all three mode targets: {frame}"
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
