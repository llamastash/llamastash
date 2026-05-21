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
/// live key labels populate the chip. `focus` is which binding table
/// the resolver should consult (most chips share the same key across
/// focuses, but `fields` is only registered in `Focus::RightPane`).
/// Multiple actions are joined with `/` so the `panes` chip can
/// carry both `NextFocus` and `PrevFocus` keys.
struct GlobalChip {
  description: &'static str,
  focus: Focus,
  actions: &'static [Action],
}

/// Canonical list of global chips. The description is fixed; the key
/// label is resolved at render time from the App's `KeyMap` so config
/// overrides reflect here automatically. Adding or reordering an
/// entry updates every call site (slot width, text, and renderer).
const GLOBAL_CHIPS: &[GlobalChip] = &[
  GlobalChip {
    description: "help",
    focus: Focus::List,
    actions: &[Action::ToggleHelp],
  },
  // Tab cycles panes everywhere now. `panes` surfaces both
  // directions; the picker prefers `Tab` for forward and
  // `Shift+Tab` for backward (the canonical surface across every
  // GUI/TUI).
  GlobalChip {
    description: "panes",
    focus: Focus::List,
    actions: &[Action::NextFocus, Action::PrevFocus],
  },
  // R104: surface the HF pull dialog opener right after `panes` so
  // first-time users see the affordance without having to open the
  // help overlay. List focus only — the binding only fires from the
  // model list (the dialog overlays once open).
  GlobalChip {
    description: "pull",
    focus: Focus::List,
    actions: &[Action::OpenHfDialog],
  },
  // RestartDaemon (Ctrl+R) and KillDaemon (Ctrl+Q) intentionally
  // do NOT appear in the global hint strip. Both are confirmation-
  // gated destructive actions; surfacing them in the always-on chip
  // row encourages muscle-memory misuse and crowds the title bar.
  // They remain discoverable through the `?` help overlay, which
  // walks every binding in the active KeyMap.
  GlobalChip {
    description: "theme",
    focus: Focus::List,
    actions: &[Action::CycleTheme],
  },
  GlobalChip {
    description: "quit",
    focus: Focus::List,
    actions: &[Action::Quit],
  },
];

const HINT_SEP: &str = " · ";

/// Resolve a chip's keys against the supplied keymap. Single-action
/// chips just show the first binding's label. The `panes` chip
/// (`NextFocus + PrevFocus`) gets a curated picker so the strip
/// reads `Tab/Shift+Tab` — that's the canonical pane-cycle surface.
/// If a config override removes those preferred keys, the resolver
/// falls back to whatever the user has bound. Missing actions
/// (user unbound them entirely) silently drop — nothing is ever
/// shown without a working key.
fn chip_keys(app: &App, chip: &GlobalChip) -> Option<String> {
  let bindings = app.bindings_for(chip.focus);
  let labels = if chip.actions == [Action::NextFocus, Action::PrevFocus] {
    pane_chip_labels(bindings)
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

/// Curated label picker for the `panes` chip. Walks the live
/// bindings and emits, in order:
///
/// 1. The `NextFocus` binding on `Tab` if present — the canonical
///    forward pane-cycle key across every GUI/TUI.
/// 2. The `PrevFocus` binding on `BackTab` (Shift+Tab) if present
///    — symmetric reverse.
///
/// Arrow keys are deliberately not picked here: round-7 reassigned
/// ←/→ to value cycling in the Settings tab. Surfacing arrows in
/// the `panes` chip would teach the wrong mental model.
///
/// If neither Tab nor Shift+Tab is bound, fall back to first
/// binding per action so a fully-rebound keymap still surfaces
/// something useful in the strip.
fn pane_chip_labels(bindings: &[Binding]) -> Vec<String> {
  let mut acc: Vec<String> = Vec::new();
  let push_label = |dst: &mut Vec<String>, candidate: &Binding| {
    let s = candidate.label.to_string();
    if !dst.contains(&s) {
      dst.push(s);
    }
  };
  let next_tab = bindings
    .iter()
    .find(|b| b.action == Action::NextFocus && b.key == KeyCode::Tab);
  let prev_back_tab = bindings
    .iter()
    .find(|b| b.action == Action::PrevFocus && b.key == KeyCode::BackTab);
  if let Some(b) = next_tab {
    push_label(&mut acc, b);
  }
  if let Some(b) = prev_back_tab {
    push_label(&mut acc, b);
  }
  if acc.is_empty() {
    // Fallback: Tab pair isn't bound — surface first binding per
    // action so the user still sees what their keymap exposes.
    for action in [Action::NextFocus, Action::PrevFocus] {
      if let Some(b) = bindings.iter().find(|b| b.action == action) {
        push_label(&mut acc, b);
      }
    }
  }
  acc
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
    // Tab/⇧+Tab is the canonical pane-cycle surface. The ⇧ glyph
    // is the Nerd Font rendering of the Shift modifier (item 1).
    // ←/→ are no longer pane-cycle keys (they cycle values in
    // Settings now), so the chip carries only the Tab pair.
    assert!(text.contains("Tab/⇧+Tab:panes"), "got: {text}");
    // The legacy `Shift+` text must never appear — keymap rendering
    // routes every modifier through `format_key_label`, which
    // surfaces the glyph form.
    assert!(
      !text.contains("Shift+"),
      "Shift+ text must not appear: {text}"
    );
    // Pre-round-7 surfaces must be gone.
    assert!(
      !text.contains(":fields"),
      "fields chip removed in round-7 (↑/↓ cycle fields now): {text}"
    );
    assert!(
      !text.contains("←/→:panes"),
      "arrows are not pane-cycle keys any more: {text}"
    );
    assert!(
      !text.contains(":focus"),
      "stale `focus` chip must not reappear: {text}"
    );
    // Restart-daemon and kill-daemon chips were intentionally removed
    // from the global hint strip — both are confirmation-gated
    // destructive actions and stay discoverable via the `?` help
    // overlay. Pin the absence here so a future regression that
    // re-adds them to the chip row fails loudly.
    assert!(
      !text.contains(":restart"),
      "restart-daemon chip must not appear in the global strip: {text}"
    );
    assert!(
      !text.contains(":kill daemon"),
      "kill-daemon chip must not appear in the global strip: {text}"
    );
    assert!(text.contains("t:theme"), "got: {text}");
    assert!(text.contains("q:quit"), "got: {text}");
    // R104: the HF pull dialog chip sits immediately after `panes`
    // so the affordance is discoverable from the top row.
    assert!(text.contains("d:pull"), "got: {text}");
    let panes_pos = text.find(":panes").expect("panes chip present");
    let pull_pos = text.find(":pull").expect("pull chip present");
    let theme_pos = text.find(":theme").expect("theme chip present");
    assert!(
      panes_pos < pull_pos && pull_pos < theme_pos,
      "expected order: panes → pull → theme, got: {text}"
    );
    // `/:filter` is panel-scoped now (lives in the Models block
    // title) — it should not appear in the global strip.
    assert!(
      !text.contains("/:filter"),
      "filter is panel-scoped; remove from global hints: {text}"
    );
  }

  #[test]
  fn panes_chip_falls_back_when_user_removes_curated_keys() {
    // If a user remaps `next_focus` and `prev_focus` away from the
    // curated Tab pair, the chip should surface whatever they
    // bound rather than emitting nothing.
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
    assert!(text.contains("F7/F8:panes"), "got: {text}");
  }

  #[test]
  fn global_hint_text_fits_typical_terminal_widths() {
    // The strip must stay scannable on a normal terminal. Restart /
    // kill chips are out, the HF `D:pull` chip (R104) is in; the
    // default keymap now produces ~50 cells. Keep the budget under
    // 70 so a small label tweak still catches accidental blowups.
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
