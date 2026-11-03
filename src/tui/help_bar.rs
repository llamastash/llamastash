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
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::hint_picker::RankedChip;
use crate::tui::keybindings::{Action, Binding, Focus};

/// One global chip — a stable description plus the action(s) whose
/// live key labels populate the chip. `focus` is which binding table
/// the resolver should consult (most chips share the same key across
/// focuses, but `fields` is only registered in `Focus::RightPane`).
/// Multiple actions are joined with `/` so the `panes` chip can
/// carry both `NextFocus` and `PrevFocus` keys. `rank` is the
/// drop priority under width pressure — lower wins (kept longest),
/// independent of the left-to-right display order below.
struct GlobalChip {
  description: &'static str,
  focus: Focus,
  actions: &'static [Action],
  rank: u8,
}

/// Canonical list of global chips, in left-to-right **display** order.
/// The description is fixed; the key label is resolved at render time
/// from the App's `KeyMap` so config overrides reflect here
/// automatically. Under width pressure chips drop by `rank` (not
/// display order): `pull` survives longest, then `help`, `quit`,
/// `panes`, `theme`, and `scroll` drops first.
const GLOBAL_CHIPS: &[GlobalChip] = &[
  GlobalChip {
    description: "memory",
    focus: Focus::List,
    actions: &[Action::OpenMemoryReserve],
    rank: 45,
  },
  // Surface the HF pull dialog opener first so first-time users see
  // the affordance without opening the help overlay. `OpenHfDialog`
  // is a global (`FocusSet::NAV`) binding — it fires from the model
  // list and the right pane alike; `focus: Focus::List` here just
  // picks which binding table resolves the key label.
  GlobalChip {
    description: "pull",
    focus: Focus::List,
    actions: &[Action::OpenHfDialog],
    rank: 10,
  },
  GlobalChip {
    description: "help",
    focus: Focus::List,
    actions: &[Action::ToggleHelp],
    rank: 20,
  },
  // Tab cycles panes everywhere now. `panes` surfaces both
  // directions; the picker prefers `Tab` for forward and
  // `Shift+Tab` for backward (the canonical surface across every
  // GUI/TUI).
  GlobalChip {
    description: "panes",
    focus: Focus::List,
    actions: &[Action::NextFocus, Action::PrevFocus],
    rank: 40,
  },
  // `↑↓:scroll` mirrors the per-pane bottom-border scroll chip
  // (`right_pane::bidirectional_chip`) but surfaces it always-on so
  // the scroll affordance is discoverable before the user focuses a
  // scrollable pane. Resolved from MoveUp/MoveDown so a rebind flows
  // through; joined without a `/` so it reads as one affordance.
  GlobalChip {
    description: "scroll",
    focus: Focus::List,
    actions: &[Action::MoveUp, Action::MoveDown],
    rank: 60,
  },
  // RestartDaemon (Ctrl+R) and KillDaemon (Ctrl+K) intentionally
  // do NOT appear in the global hint strip. Both are confirmation-
  // gated destructive actions; surfacing them in the always-on chip
  // row encourages muscle-memory misuse and crowds the title bar.
  // They remain discoverable through the `?` help overlay, which
  // walks every binding in the active KeyMap.
  GlobalChip {
    description: "theme",
    focus: Focus::List,
    actions: &[Action::CycleTheme],
    rank: 50,
  },
  GlobalChip {
    description: "quit",
    focus: Focus::List,
    actions: &[Action::Quit],
    rank: 30,
  },
];

fn hint_sep() -> &'static str {
  crate::tui::glyphs::active().middot_sep()
}

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
    return None;
  }
  // The scroll chip reads as one `↑↓` affordance (no separator); every
  // other multi-key chip joins its labels with `/`.
  let sep = if chip.actions == [Action::MoveUp, Action::MoveDown] {
    ""
  } else {
    "/"
  };
  Some(labels.join(sep))
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
    // Skip display-only entries (`KeyCode::Null`, used by the gt/gT
    // help rows) since they're not real single-press chords the chip
    // could advertise.
    for action in [Action::NextFocus, Action::PrevFocus] {
      if let Some(b) = bindings
        .iter()
        .find(|b| b.action == action && b.key != KeyCode::Null)
      {
        push_label(&mut acc, b);
      }
    }
  }
  acc
}

/// Resolve every chip to a [`RankedChip`] (`key:label` text + drop
/// rank) in display order, skipping chips whose actions are entirely
/// unbound.
fn resolved_chips(app: &App) -> Vec<RankedChip> {
  GLOBAL_CHIPS
    .iter()
    .filter(|chip| {
      chip.actions != [Action::OpenMemoryReserve]
        || app
          .memory_budget
          .as_ref()
          .is_some_and(|budget| budget.supports_editing())
    })
    .filter_map(|chip| {
      chip_keys(app, chip)
        .map(|keys| RankedChip::new(chip.rank, format!("{keys}:{}", chip.description)))
    })
    .collect()
}

/// The chip strings (`key:label`) that fit in `budget` cells, dropping
/// the lowest-priority chips (highest `rank`) first. Survivors stay in
/// display order. An empty result means even the top-ranked chip can't
/// fit, in which case the caller drops the strip entirely.
pub fn fit_global_hints(app: &App, budget: usize) -> Vec<String> {
  crate::tui::hint_picker::pick(resolved_chips(app), budget, hint_sep())
}

/// Render width (columns) for an already-fitted set of chip strings:
/// the chips, their `·` separators, and a single trailing pad column so
/// the rightmost hint isn't flush against the terminal edge. `0` for an
/// empty set.
pub fn hints_render_width(chips: &[String]) -> u16 {
  if chips.is_empty() {
    return 0;
  }
  let sep_w = hint_sep().chars().count();
  let body = chips.iter().map(|c| c.chars().count()).sum::<usize>() + sep_w * (chips.len() - 1);
  u16::try_from(body + 1).unwrap_or(u16::MAX)
}

/// Format the full global hint string — every chip, display order, no
/// budget pressure. Used by tests and as the canonical "everything
/// fits" representation.
pub fn global_hint_text(app: &App) -> String {
  resolved_chips(app)
    .into_iter()
    .map(|c| c.text)
    .collect::<Vec<_>>()
    .join(hint_sep())
}

/// Render the supplied (already-fitted) chip strings into `area`,
/// right-aligned and non-bold, in `palette.on_accent` over the accent
/// background the title-row renderer already painted. `on_accent`
/// rather than `bg` because `bg` is `Color::Reset` on the mono theme,
/// which would fall through to the terminal's default fg over a White
/// accent bar.
pub fn render_global(frame: &mut Frame<'_>, area: Rect, palette: &Palette, chips: &[String]) {
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(chips.len() * 2 + 1);
  for (i, chip) in chips.iter().enumerate() {
    if i > 0 {
      spans.push(Span::raw(hint_sep()));
    }
    spans.push(Span::raw(chip.clone()));
  }
  spans.push(Span::raw(" "));
  let para = Paragraph::new(Line::from(spans))
    .alignment(Alignment::Right)
    .style(Style::default().bg(palette.accent).fg(palette.on_accent));
  frame.render_widget(para, area);
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
    // Tab/⇧+Tab is the canonical pane-cycle surface. Labels come
    // from the shared key-label consts so the test stays correct on
    // both PC (`↹` / `⇧↹`) and macOS (`⇥` / `⇧⇥`).
    use crate::tui::keybindings::{SHIFT_TAB_LABEL, TAB_LABEL};
    let expected_panes = format!("{TAB_LABEL}/{SHIFT_TAB_LABEL}:panes");
    assert!(text.contains(&expected_panes), "got: {text}");
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
    // The HF pull dialog chip leads the strip so the affordance is the
    // first thing discoverable from the top row.
    assert!(text.contains("P:pull"), "got: {text}");
    // The scroll chip reads `↑↓:scroll` (no slash).
    assert!(text.contains("↑↓:scroll"), "got: {text}");
    // Display order (left to right): pull → help → panes → scroll →
    // theme → quit. (Drop priority under width pressure is separate —
    // see `global_hints_drop_lowest_rank_first_under_pressure`.)
    let pull_pos = text.find(":pull").expect("pull chip present");
    let help_pos = text.find(":help").expect("help chip present");
    let panes_pos = text.find(":panes").expect("panes chip present");
    let scroll_pos = text.find(":scroll").expect("scroll chip present");
    let theme_pos = text.find(":theme").expect("theme chip present");
    let quit_pos = text.find(":quit").expect("quit chip present");
    assert!(
      pull_pos < help_pos
        && help_pos < panes_pos
        && panes_pos < scroll_pos
        && scroll_pos < theme_pos
        && theme_pos < quit_pos,
      "expected display order pull → help → panes → scroll → theme → quit, got: {text}"
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
    // kill chips are out; the HF `P:pull` and `↑↓:scroll` chips are
    // in; the default keymap now produces ~60 cells. Keep the budget
    // under 70 so a small label tweak still catches accidental blowups.
    let app = default_app();
    assert!(global_hint_text(&app).chars().count() < 70);
  }

  #[test]
  fn slot_width_matches_rendered_text_plus_pad() {
    // With a generous budget every chip survives, so the rendered slot
    // width equals the full visible text width plus the one trailing
    // pad column. If the width helper drifts from the renderer, the
    // title row would clip the rightmost hint or leave a gap.
    let app = default_app();
    let text_w = global_hint_text(&app).chars().count() as u16;
    let all = fit_global_hints(&app, 1000);
    assert_eq!(hints_render_width(&all), text_w + 1);
  }

  #[test]
  fn global_hints_drop_lowest_rank_first_under_pressure() {
    // Under width pressure chips drop by rank, not display order:
    // `pull` (10) survives longest, `scroll` (60) drops first. Budget
    // the three top-ranked chips (pull, help, quit) plus two seps and
    // assert the rest dropped while survivors keep display order.
    let app = default_app();
    let sep = hint_sep().chars().count();
    let chips = resolved_chips(&app);
    let width_of = |needle: &str| {
      chips
        .iter()
        .find(|c| c.text.contains(needle))
        .unwrap_or_else(|| panic!("{needle} chip present"))
        .text
        .chars()
        .count()
    };
    let budget = width_of(":pull") + width_of(":help") + width_of(":quit") + 2 * sep;
    let got = fit_global_hints(&app, budget);
    for keep in [":pull", ":help", ":quit"] {
      assert!(
        got.iter().any(|c| c.contains(keep)),
        "{keep} (top rank) must survive: {got:?}"
      );
    }
    for drop in [":scroll", ":theme", ":panes"] {
      assert!(
        !got.iter().any(|c| c.contains(drop)),
        "{drop} (lower rank) must drop first: {got:?}"
      );
    }
    // Survivors keep left-to-right display order: pull → help → quit.
    let joined = got.join(" ");
    let p = joined.find(":pull").unwrap();
    let h = joined.find(":help").unwrap();
    let q = joined.find(":quit").unwrap();
    assert!(p < h && h < q, "display order not preserved: {joined}");
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
    // Format depends on platform: `Ctrl+q` on PC, `⌃q` on macOS —
    // pull the prefix from the same const the runtime uses.
    use crate::tui::keybindings::CTRL_PREFIX;
    let expected = format!("{CTRL_PREFIX}q:quit");
    assert!(text.contains(&expected), "got: {text}");
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
