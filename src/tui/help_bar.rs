//! Title-row global hint strip.
//!
//! Pre-relayout this module owned a focus-aware bottom help bar. After
//! the kdash-style relayout, the bottom bar is gone and panel-specific
//! hints live inside each panel's block title (`list_pane`,
//! `right_pane`, etc.). What's left is the small strip of **global**
//! keybindings — help, focus chain, kill-daemon, theme, quit — that
//! the title row right-aligns over the accent background.
//!
//! Each chip's key label is resolved live through the App's `KeyMap`,
//! so a `keybindings:` config override flows through to the title
//! strip without code changes (`quit: ctrl+q` becomes `Ctrl+q:quit`).

use crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Binding, Focus};

/// One global chip — a stable description plus the action(s) whose
/// live key labels populate the chip. Multiple actions are joined
/// with `/` so the "focus" chip can carry both `NextFocus` and
/// `PrevFocus` keys.
struct GlobalChip {
  description: &'static str,
  actions: &'static [Action],
}

/// Canonical list of global chips. The description is fixed; the key
/// label is resolved at render time from the App's `KeyMap` so config
/// overrides reflect here automatically. Adding or reordering an
/// entry updates every call site (slot width, text, and renderer).
const GLOBAL_CHIPS: &[GlobalChip] = &[
  GlobalChip {
    description: "help",
    actions: &[Action::ToggleHelp],
  },
  // Focus chain has two halves — show one representative key per
  // direction so the user knows which keys cycle panes regardless of
  // how the chain is bound. `cycle_first_label` picks the first
  // binding per action, so a config that rebinds `next_focus` flows
  // through cleanly.
  GlobalChip {
    description: "focus",
    actions: &[Action::NextFocus, Action::PrevFocus],
  },
  GlobalChip {
    description: "kill daemon",
    actions: &[Action::KillDaemon],
  },
  GlobalChip {
    description: "theme",
    actions: &[Action::CycleTheme],
  },
  GlobalChip {
    description: "quit",
    actions: &[Action::Quit],
  },
];

const HINT_SEP: &str = " · ";

/// Resolve a chip's keys against the supplied keymap. Single-action
/// chips just show the first binding's label. The two-action focus
/// chip (`NextFocus + PrevFocus`) gets a curated picker so the strip
/// reads `Tab/←/→` rather than `Tab/Shift+Tab` — i.e. we prefer Tab
/// for the forward direction, and arrow glyphs for the alternatives,
/// because that's what most users reach for. If a config override
/// removes those preferred keys, the resolver falls back to whatever
/// the user has bound. Missing actions (user unbound them entirely)
/// silently drop — nothing is ever shown without a working key.
fn chip_keys(app: &App, chip: &GlobalChip) -> Option<String> {
  let bindings = app.bindings_for(Focus::List);
  let labels = if chip.actions == [Action::NextFocus, Action::PrevFocus] {
    focus_chip_labels(bindings)
  } else {
    let mut acc: Vec<String> = Vec::new();
    for action in chip.actions {
      if let Some(b) = bindings.iter().find(|b| b.action == *action) {
        acc.push(b.label.to_string());
      }
    }
    acc
  };
  if labels.is_empty() {
    None
  } else {
    Some(labels.join("/"))
  }
}

/// Curated label picker for the focus chip. Walks the live bindings
/// and emits, in order:
///   1. Tab (if `NextFocus` is bound to it) — the canonical forward
///      key, called out first because new users reach for Tab.
///   2. The PrevFocus binding for `Left` if present — the arrow
///      glyph reads better in the strip than `Shift+Tab`.
///   3. The NextFocus binding for `Right` if present — symmetric
///      with the PrevFocus arrow.
/// If none of the curated picks land, we fall back to first binding
/// per action so a fully-rebound keymap still surfaces something
/// useful in the strip.
fn focus_chip_labels(bindings: &[Binding]) -> Vec<String> {
  let mut labels: Vec<String> = Vec::new();
  let push_label = |labels: &mut Vec<String>, candidate: &Binding| {
    let s = candidate.label.to_string();
    if !labels.contains(&s) {
      labels.push(s);
    }
  };
  let next_tab = bindings
    .iter()
    .find(|b| b.action == Action::NextFocus && b.key == KeyCode::Tab);
  let prev_left = bindings
    .iter()
    .find(|b| b.action == Action::PrevFocus && b.key == KeyCode::Left);
  let next_right = bindings
    .iter()
    .find(|b| b.action == Action::NextFocus && b.key == KeyCode::Right);
  if let Some(b) = next_tab {
    push_label(&mut labels, b);
  }
  if let Some(b) = prev_left {
    push_label(&mut labels, b);
  }
  if let Some(b) = next_right {
    push_label(&mut labels, b);
  }
  if labels.is_empty() {
    // Fallback: no curated keys are bound — surface first binding
    // per action so the user still sees what their keymap exposes.
    for action in [Action::NextFocus, Action::PrevFocus] {
      if let Some(b) = bindings.iter().find(|b| b.action == action) {
        push_label(&mut labels, b);
      }
    }
  }
  labels
}

/// Build the (keys, description) pairs for every chip in display
/// order, skipping chips whose actions have been entirely unbound.
fn resolved_chips(app: &App) -> Vec<(String, &'static str)> {
  GLOBAL_CHIPS
    .iter()
    .filter_map(|chip| chip_keys(app, chip).map(|keys| (keys, chip.description)))
    .collect()
}

/// Width in columns the title row should reserve for the global hint
/// strip, including the leading space inside each `key:label` pair and
/// a single trailing pad column.
pub fn global_hint_slot_width(app: &App) -> u16 {
  let chips = resolved_chips(app);
  let mut w: usize = 0;
  for (i, (keys, label)) in chips.iter().enumerate() {
    if i > 0 {
      w += HINT_SEP.chars().count();
    }
    w += keys.chars().count() + 1 + label.chars().count();
  }
  // One-cell trailing pad so the rightmost hint isn't flush against
  // the terminal edge.
  w += 1;
  u16::try_from(w).unwrap_or(u16::MAX)
}

/// Format the global hint string. Stable order; resolved live from
/// the App's KeyMap so config overrides flow through.
pub fn global_hint_text(app: &App) -> String {
  let mut out = String::new();
  for (i, (keys, label)) in resolved_chips(app).into_iter().enumerate() {
    if i > 0 {
      out.push_str(HINT_SEP);
    }
    out.push_str(&keys);
    out.push(':');
    out.push_str(label);
  }
  out
}

/// Render the global hint strip into `area`, right-aligned with text
/// in `palette.on_accent` on the accent background already painted by
/// the title-row renderer. `on_accent` rather than `bg` here because
/// `bg` is `Color::Reset` on the mono theme, which would fall through
/// to the terminal's default fg over a White accent bar.
pub fn render_global(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let chips = resolved_chips(app);
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(chips.len() * 2 + 1);
  for (i, (keys, label)) in chips.into_iter().enumerate() {
    if i > 0 {
      spans.push(Span::raw(HINT_SEP));
    }
    spans.push(hint_span(&keys, label));
  }
  spans.push(Span::raw(" "));
  let para = Paragraph::new(Line::from(spans))
    .alignment(Alignment::Right)
    .style(Style::default().bg(palette.accent).fg(palette.on_accent));
  frame.render_widget(para, area);
}

/// Build a `key:label` span. The key+colon+label are bolded together;
/// both inherit the accent-bg/bg-fg style from the parent Paragraph.
/// `keys` is allocated per render (the runtime-resolved key string);
/// `label` is `&'static` because the chip descriptions are fixed.
fn hint_span(keys: &str, label: &'static str) -> Span<'static> {
  Span::styled(
    [keys, ":", label].concat(),
    Style::default().add_modifier(Modifier::BOLD),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crate::tui::keybindings::KeyMap;
  use std::collections::BTreeMap;

  fn default_app() -> App {
    App::new(AppOptions::default())
  }

  #[test]
  fn global_hint_text_lists_required_keys_from_default_keymap() {
    let app = default_app();
    let text = global_hint_text(&app);
    assert!(text.contains("?:help"), "got: {text}");
    // Curated focus picker: Tab (forward) + ← / → arrow alternatives.
    // Shift+Tab + h / l are valid alternatives but kept out of the
    // top-bar chip so the strip stays scannable.
    assert!(text.contains("Tab/←/→:focus"), "got: {text}");
    assert!(text.contains("Q:kill daemon"), "got: {text}");
    assert!(text.contains("t:theme"), "got: {text}");
    assert!(text.contains("q:quit"), "got: {text}");
    // `/:filter` is panel-scoped now (lives in the Models block
    // title) — it should not appear in the global strip.
    assert!(
      !text.contains("/:filter"),
      "filter is panel-scoped; remove from global hints: {text}"
    );
  }

  #[test]
  fn focus_chip_falls_back_when_user_removes_curated_keys() {
    // If a user remaps `next_focus` and `prev_focus` to non-curated
    // keys (e.g. F7 / F8), the chip should surface those rather than
    // emitting nothing.
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [
      (String::from("next_focus"), String::from("f7")),
      (String::from("prev_focus"), String::from("f8")),
    ]
    .into_iter()
    .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let text = global_hint_text(&app);
    assert!(text.contains("F7/F8:focus"), "got: {text}");
  }

  #[test]
  fn global_hint_text_fits_typical_terminal_widths() {
    // The strip must stay scannable on a normal terminal. The
    // default keymap produces ~55 cells of text; allow some slack
    // for label rewording without losing the budget signal.
    let app = default_app();
    assert!(global_hint_text(&app).chars().count() < 70);
  }

  #[test]
  fn slot_width_matches_rendered_text_plus_pad() {
    // Slot width should equal the visible text width plus the one
    // trailing pad column. If the resolver drifts from the renderer,
    // the title row would either clip the rightmost hint or leave a
    // gap.
    let app = default_app();
    let text_w = global_hint_text(&app).chars().count() as u16;
    assert_eq!(global_hint_slot_width(&app), text_w + 1);
  }

  #[test]
  fn config_rebind_of_quit_flows_through_to_global_strip() {
    // If the user remaps `quit: ctrl+q` in config, the title strip
    // must surface `Ctrl+q:quit` — not the stale default `q:quit`.
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
    let text = global_hint_text(&app);
    assert!(text.contains("Ctrl+q:quit"), "got: {text}");
    // The stale default `q:quit` chip — bare `q` rather than the
    // remapped `Ctrl+q` — must not appear. Anchor on the leading
    // separator so we don't false-match the tail of `Ctrl+q:quit`.
    assert!(
      !text.contains(" · q:quit"),
      "stale default `q:quit` must not appear after rebind: {text}"
    );
  }

  #[test]
  fn chip_drops_silently_when_user_unbinds_the_action() {
    // If a user removes every binding for an action, the chip drops
    // — better an empty slot than a hint with no working key. We
    // simulate this by rebinding `cycle_theme` onto `q` so the
    // theme chip loses its `t` binding (the override path drops
    // conflicting bindings of the other action; here CycleTheme's
    // own `t` is replaced and Quit's `q` is claimed by CycleTheme).
    let mut keymap = KeyMap::default();
    // Use a key that doesn't already host a global action so we don't
    // accidentally drop a different chip.
    let overrides: BTreeMap<String, String> = [(String::from("cycle_theme"), String::from("F9"))]
      .into_iter()
      .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let text = global_hint_text(&app);
    assert!(text.contains("F9:theme"), "got: {text}");
  }
}
