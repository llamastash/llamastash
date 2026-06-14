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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::fmt::format_bytes;
use crate::tui::status_icons::{glyph_for, label_for};
use crate::tui::tabs::{chat, embed, logs, rerank, settings, RightTab};

/// Render the right-pane area as a single unnested Block. `focused`
/// flips the border to the theme's focus tone (`palette.highlight`
/// with `accent` fallback) so the user can see which side of the
/// dashboard owns the keyboard chain at a glance.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette, focused: bool) {
  let tabs = app.available_right_tabs();
  let (title_line, tab_rects) = block_title_with_rects(app, &tabs, palette, area);
  // Stash the per-tab rects on App so the mouse-focus click handler
  // can resolve `(column, row)` → `RightTab`. The list/right-pane
  // rects were written by `render_body`; we only touch `right_tabs`
  // here.
  app.hit_rects.borrow_mut().right_tabs = tab_rects;
  let bottom_chips_ranked = bottom_hint_chips(app);
  // Pane border eats 2 cells; `bottom_hint_line` itself pads the
  // strip with a leading + trailing space (2 more) so the picker
  // budget is `area.width - 4`. Chips drop in rank order when the
  // pane is too narrow for the full strip.
  let bottom_chips = crate::tui::hint_picker::pick(
    bottom_chips_ranked,
    (area.width as usize).saturating_sub(4),
    " · ",
  );
  let border_color = palette.focus_border(focused);

  let mut outer = Block::default()
    .title(title_line)
    .borders(Borders::ALL)
    .border_style(Style::default().fg(border_color))
    .padding(Padding::horizontal(1));
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

  // Inner stack: blank pad, name (bold), path (muted, wraps to as
  // many lines as needed up to 3), blank gap, stats
  // (`:port  state  RAM  CPU`), separator, tab content. Wrapping the
  // path means narrow panes still surface the full filesystem
  // location instead of a left-truncated stub. Capped at 3 lines so
  // a pathological path can't push the tab body off-screen.
  let path_lines = focused_path_line_count(app, inner.width);
  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(path_lines),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Min(1),
    ])
    .split(inner);

  render_header_name(frame, layout[1], app, palette);
  render_header_path(frame, layout[2], app, palette);
  render_header_stats(frame, layout[4], app, palette);
  render_separator(frame, layout[5], palette);
  let body_area = layout[6];

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
pub(crate) fn bottom_hint_chips(app: &App) -> Vec<crate::tui::hint_picker::RankedChip> {
  use crate::tui::hint_picker::RankedChip;
  use crate::tui::keybindings::{Action, Focus};
  let mut chips: Vec<RankedChip> = Vec::with_capacity(6);
  let push = |c: &mut Vec<RankedChip>, rank: u8, h: Option<String>| {
    if let Some(h) = h {
      c.push(RankedChip::new(rank, h));
    }
  };
  /// Surface the `InputField` modal-contract chips for one of the
  /// tab inputs (chat / embed / rerank / candidate). Mirrors the
  /// rule documented in `src/tui/input_field.rs`. The modal-
  /// contract chord is rank 10 — never drops — because it's the
  /// escape hatch from the captured-input mode; losing it strands
  /// the user mid-edit.
  fn push_input_field_chips(c: &mut Vec<RankedChip>, editing: bool, empty: bool) {
    if editing {
      c.push(RankedChip::new(10, "Esc:stop edit"));
      return;
    }
    c.push(RankedChip::new(10, "e:edit"));
    if !empty {
      c.push(RankedChip::new(20, "Esc:clear"));
    }
  }
  match (app.focus, app.right_tab) {
    // Edit-mode focuses surface the InputField's modal-contract
    // chord (rank 10) plus the action-layer chips (Send / Embed /
    // Rerank / ToggleThink) at higher ranks.
    (Focus::ChatInput, _) => {
      let editing = app.chat.prompt.is_editing();
      push_input_field_chips(&mut chips, editing, app.chat.prompt.is_empty());
      push(&mut chips, 30, app.hint(Focus::ChatInput, Action::SendChat));
      // `r:think` and `↑↓:scroll` only fire when the field is resting
      // (editing mode captures the char). Hiding these chips during edit
      // keeps the hint truthful instead of teaching keys that won't fire.
      if !editing {
        push(
          &mut chips,
          40,
          app.hint_with(Focus::ChatInput, Action::ToggleThinkCollapse, "think"),
        );
        // Scroll hint surfaces once there is a response to navigate.
        if !app.chat.response.is_empty() {
          if let (Some(down), Some(up)) = (
            app.hint_with(Focus::ChatInput, Action::MoveDown, "scroll"),
            app.hint_with(Focus::ChatInput, Action::MoveUp, "scroll"),
          ) {
            chips.push(RankedChip::new(
              50,
              bidirectional_chip(&up, &down, "scroll"),
            ));
          }
        }
      }
    }
    (Focus::EmbedInput, _) => {
      let editing = app.embed.input.is_editing();
      push_input_field_chips(&mut chips, editing, app.embed.input.is_empty());
      push(&mut chips, 30, app.hint(Focus::EmbedInput, Action::Submit));
      // Scroll hint when embed output is present and field is resting.
      if !editing && app.embed.dim.is_some() {
        if let (Some(down), Some(up)) = (
          app.hint_with(Focus::EmbedInput, Action::MoveDown, "scroll"),
          app.hint_with(Focus::EmbedInput, Action::MoveUp, "scroll"),
        ) {
          chips.push(RankedChip::new(
            40,
            bidirectional_chip(&up, &down, "scroll"),
          ));
        }
      }
    }
    (Focus::RerankInput, _) => {
      // Rerank has two `InputField`s (query / candidate); the
      // chip surfaces whichever one currently has focus.
      let (editing, empty) = match app.rerank.field {
        crate::tui::tabs::rerank::RerankField::Query => {
          (app.rerank.query.is_editing(), app.rerank.query.is_empty())
        }
        crate::tui::tabs::rerank::RerankField::Candidate => (
          app.rerank.candidate_buffer.is_editing(),
          app.rerank.candidate_buffer.is_empty(),
        ),
      };
      push_input_field_chips(&mut chips, editing, empty);
      // Submit is dual-duty: in the query field it dispatches
      // `/v1/rerank`; in the candidate field it stages the buffer
      // onto the candidate list. The chip description reflects the
      // currently focused field so it doesn't lie.
      let submit_desc = if app.rerank.field == crate::tui::tabs::rerank::RerankField::Candidate {
        "add candidate"
      } else {
        "rerank"
      };
      push(
        &mut chips,
        30,
        app.hint_with(Focus::RerankInput, Action::Submit, submit_desc),
      );
    }
    // Navigation focuses surface the entry-point keystroke per tab.
    (_, RightTab::Logs) => {
      push(
        &mut chips,
        10,
        app.hint(Focus::RightPane, Action::ToggleAutoScroll),
      );
      // `c` is tab-aware: copies the full log buffer when the Logs
      // tab is up, otherwise yanks the curl one-liner. Surface a
      // `c:copy` chip so the binding is discoverable here.
      push(
        &mut chips,
        20,
        app.hint_with(Focus::RightPane, Action::YankCurl, "copy"),
      );
    }
    (_, RightTab::Chat | RightTab::Embed | RightTab::Rerank) => {
      push(
        &mut chips,
        10,
        app.hint(Focus::RightPane, Action::EnterEdit),
      );
      // `r` toggles `<think>` collapse on the Chat tab only; surface
      // it as a chip so the binding is discoverable from the browsing
      // focus.
      if app.right_tab == RightTab::Chat {
        push(
          &mut chips,
          20,
          app.hint_with(Focus::RightPane, Action::ToggleThinkCollapse, "think"),
        );
      }
      // Scroll hint when there is output to navigate (browsing focus
      // already has ↑/↓ bound via MoveUp/MoveDown on RIGHT_PANE).
      let has_output = match app.right_tab {
        RightTab::Chat => !app.chat.response.is_empty(),
        RightTab::Embed => app.embed.dim.is_some(),
        RightTab::Rerank => !app.rerank.ranked.is_empty(),
        _ => false,
      };
      if has_output {
        if let (Some(down), Some(up)) = (
          app.hint_with(Focus::RightPane, Action::MoveDown, "scroll"),
          app.hint_with(Focus::RightPane, Action::MoveUp, "scroll"),
        ) {
          chips.push(RankedChip::new(
            30,
            bidirectional_chip(&up, &down, "scroll"),
          ));
        }
      }
    }
    (_, RightTab::Settings) => {
      let running_readonly = app.launch_picker.is_none() && app.focused_managed().is_some();
      if running_readonly {
        // Read-only running view — `e` stages the launch-edit
        // picker, `c` (curl) / `u` (url) target the live instance,
        // `s` doubles as `stop` when the dispatcher sees it on
        // Settings. `e` (rank 10) leads because the user's primary
        // mutation here is "edit for next launch"; the yank trio
        // drops first under width pressure.
        push(
          &mut chips,
          10,
          app.hint_with(Focus::RightPane, Action::EnterEdit, "edit for launch"),
        );
        push(
          &mut chips,
          20,
          app.hint_with(Focus::RightPane, Action::ToggleAutoScroll, "stop"),
        );
        // ↑/↓ scroll the read-only running-launch view (~17 rows).
        // Surface the chip so users discover the affordance — without
        // it, arrows look like a no-op.
        if let (Some(down), Some(up)) = (
          app.hint_with(Focus::RightPane, Action::MoveDown, "scroll"),
          app.hint_with(Focus::RightPane, Action::MoveUp, "scroll"),
        ) {
          chips.push(RankedChip::new(
            30,
            bidirectional_chip(&up, &down, "scroll"),
          ));
        }
        push(&mut chips, 40, app.hint(Focus::RightPane, Action::YankPath));
        push(&mut chips, 50, app.hint(Focus::RightPane, Action::YankUrl));
        push(&mut chips, 60, app.hint(Focus::RightPane, Action::YankCurl));
      } else if app.focused_path().is_some() {
        // Editable launch form — surface launch + the field/value
        // cycle pairs + `a:advanced` + `p:path`. No `u`/`c` here
        // because the user is editing settings, not addressing a
        // running instance.
        push(
          &mut chips,
          10,
          app.hint_with(Focus::RightPane, Action::Submit, "launch"),
        );
        // When the picker was staged via `e` over a running launch
        // (edit-for-next-launch mode), surface `Esc:discard` so the
        // user can step back to the read-only running view without
        // committing the edits. The keycap comes from the live
        // FocusList binding (default `Esc`) — that's the action
        // `apply_focus_list` intercepts to close the picker when a
        // managed launch is present, rather than dropping focus to
        // the Models list.
        if app.launch_picker.is_some() && app.focused_managed().is_some() {
          if let Some(chip) = app.hint_with(Focus::RightPane, Action::FocusList, "discard") {
            chips.push(RankedChip::new(20, chip));
          }
        }
        // Surface `e:edit` so the extras row (and numeric / enum
        // knobs) is discoverable. Without this chip, `e` looked like
        // a no-op on the editable form because nothing in the hint
        // strip pointed at it. Leads the cycle chips because edit is
        // the primary mutation verb on this form. While a row's
        // inline edit buffer is open (numeric/enum typing or the
        // extras free-text field), the chip flips to the bound
        // `exit_edit` key (default `Esc`) so the escape hatch from
        // the captured-input mode is visible. The lookup rides on
        // `Focus::ChatInput` because `handle_settings_inline_edit`
        // resolves the cancel key through the same focus — keep the
        // chip and the handler in lockstep so rebinds flow through.
        //
        // The chip only appears when the focused row is *actually*
        // editable. Boolean rows (reasoning, flash_attn, mlock,
        // no_mmap) are cycled with ←/→ and `e` is a no-op there —
        // showing `e:edit` on those rows would be a lying affordance.
        // `PickerField::is_editable` is the shared rule with
        // `open_focused_inline_edit`.
        let picker_ref = app.launch_picker.as_ref();
        let inline_editing = picker_ref
          .map(|p| p.inline_edit.is_open() || p.extras_input.is_editing())
          .unwrap_or(false);
        let focused_editable = picker_ref.map(|p| p.field.is_editable()).unwrap_or(false);
        if inline_editing {
          push(
            &mut chips,
            30,
            app.hint_with(Focus::ChatInput, Action::ExitEdit, "clear"),
          );
        } else if focused_editable {
          push(
            &mut chips,
            30,
            app.hint_with(Focus::RightPane, Action::EnterEdit, "edit"),
          );
        }
        if let (Some(down), Some(up)) = (
          app.hint_with(Focus::RightPane, Action::MoveDown, "cycle fields"),
          app.hint_with(Focus::RightPane, Action::MoveUp, "cycle fields"),
        ) {
          chips.push(RankedChip::new(
            40,
            bidirectional_chip(&up, &down, "cycle fields"),
          ));
        }
        if let (Some(next), Some(prev)) = (
          app.hint_with(Focus::RightPane, Action::CycleValueNext, "cycle value"),
          app.hint_with(Focus::RightPane, Action::CycleValuePrev, "cycle value"),
        ) {
          chips.push(RankedChip::new(
            50,
            bidirectional_chip(&prev, &next, "cycle value"),
          ));
        }
        push(&mut chips, 60, app.hint(Focus::RightPane, Action::YankPath));
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
///
/// Also returns the per-tab on-screen rectangles, computed in
/// lockstep with the span sequence so the mouse-focus click handler
/// in [`crate::tui::events`] can hit-test labels without re-deriving
/// the layout. Each rect spans one row (`y == area.y` — ratatui
/// paints titles on the top border) and the exact label width in
/// columns. Tabs whose label would extend past the visible width are
/// dropped from the rect list — the title is clipped on screen so
/// hit-testing them would land on truncated glyphs.
fn block_title_with_rects(
  app: &App,
  tabs: &[RightTab],
  palette: &Palette,
  area: Rect,
) -> (Line<'static>, Vec<(RightTab, Rect)>) {
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 3 + 4);
  let mut rects: Vec<(RightTab, Rect)> = Vec::with_capacity(tabs.len());
  spans.push(Span::raw(" "));
  // Ratatui paints `Block::title` starting at `area.x + 1`
  // (skipping the top-left corner glyph). The leading " " span
  // pushes the first label one cell further right.
  let mut col: u16 = area.x.saturating_add(2);
  let last_col: u16 = area.x.saturating_add(area.width);
  for (i, tab) in tabs.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" │ ", palette.muted_style()));
      col = col.saturating_add(3);
    }
    let label = tab.label();
    let label_width = label.chars().count() as u16;
    if col.saturating_add(label_width) <= last_col {
      rects.push((
        *tab,
        Rect {
          x: col,
          y: area.y,
          width: label_width,
          height: 1,
        },
      ));
    }
    // Active tab gets `panel_title` + bold so it reads like the
    // panel's heading text (matches Host/Daemon/Models titles).
    // Inactive tabs stay muted so the heading carries clear focus.
    // The mnemonic underline (first letter) is applied separately
    // by [`mnemonic_spans`].
    let active = *tab == app.right_tab;
    spans.extend(mnemonic_spans(label, active, palette));
    col = col.saturating_add(label_width);
  }
  spans.push(Span::raw(" "));
  (Line::from(spans), rects)
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
  // Prefer the discovery-supplied friendly label (Ollama's
  // `<name>:<tag>`) over the path's file_stem so the header doesn't
  // render the `sha256-<hex>` blob name.
  let label_for = |path: &std::path::Path| {
    app
      .display_label_for(path)
      .unwrap_or_else(|| model_display_name(path))
  };
  // Resolve the focused path (running launch first, then the list
  // selection) so the title and its modality glyphs describe one model.
  let focused = app
    .right_pane_focus()
    .map(|m| m.path.clone())
    .or_else(|| app.focused_path());
  let name_line = match focused {
    Some(path) => {
      let mut spans = vec![Span::styled(label_for(&path), name_style)];
      // `◉` vision / `♪` audio after the title when discovery found an
      // mmproj projector — single-cell glyphs in the accent tone, see
      // `Multimodal::LEGEND` (also surfaced in the help overlay).
      let glyphs: Vec<String> = app
        .multimodal_for(&path)
        .map(|mm| mm.glyphs())
        .unwrap_or_default()
        .iter()
        .map(|g| g.to_string())
        .collect();
      if !glyphs.is_empty() {
        spans.push(Span::styled(
          format!("  {}", glyphs.join(" ")),
          palette.accent_style(),
        ));
      }
      Line::from(spans)
    }
    None => Line::from(Span::styled("—", palette.muted_style())),
  };
  frame.render_widget(Paragraph::new(name_line), area);
}

/// Render the muted path row sitting under the model name. Shows the
/// focused model's full file path with `$HOME` collapsed to `~`,
/// hard-wrapped into chunks that fit `area.width` so the full path is
/// always visible (paths have no whitespace, so ratatui's default
/// word-wrap would just truncate them). Blank when nothing is focused.
fn render_header_path(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let Some(path) = focused_path(app) else {
    return;
  };
  let abbreviated = crate::util::paths::abbreviate_with_home(&path);
  let width = area.width as usize;
  let style = palette.muted_style();
  let lines: Vec<Line<'_>> = wrap_path_chunks(&abbreviated, width, area.height as usize)
    .into_iter()
    .map(|chunk| Line::from(Span::styled(chunk, style)))
    .collect();
  frame.render_widget(Paragraph::new(lines), area);
}

/// Resolve the path the right pane is currently focused on — running
/// launch first, falling back to the list-pane selection.
fn focused_path(app: &App) -> Option<std::path::PathBuf> {
  app
    .right_pane_focus()
    .map(|m| m.path.clone())
    .or_else(|| app.focused_path())
}

/// Number of vertical rows the focused path needs at `inner_width`,
/// clamped to `[1, 3]`. Used by the right-pane layout to reserve a
/// variable-height slot for the path row so the path wraps cleanly
/// without pushing tab content off-screen on pathological paths.
fn focused_path_line_count(app: &App, inner_width: u16) -> u16 {
  let Some(path) = focused_path(app) else {
    return 1;
  };
  let abbreviated = crate::util::paths::abbreviate_with_home(&path);
  wrap_path_chunks(&abbreviated, inner_width as usize, 3).len() as u16
}

/// Hard-wrap `s` into `max_lines` chunks of at most `width` chars
/// each. Path strings have no whitespace, so ratatui's word-wrap
/// truncates them; this function slices at character boundaries
/// instead. The last chunk is left-truncated with a leading `…` when
/// it overflows so the meaningful filename tail stays visible.
fn wrap_path_chunks(s: &str, width: usize, max_lines: usize) -> Vec<String> {
  if width == 0 || max_lines == 0 {
    return vec![s.to_string()];
  }
  let chars: Vec<char> = s.chars().collect();
  if chars.len() <= width {
    return vec![s.to_string()];
  }
  let mut out: Vec<String> = Vec::with_capacity(max_lines);
  let mut i = 0;
  while i < chars.len() && out.len() < max_lines {
    let end = (i + width).min(chars.len());
    let chunk: String = chars[i..end].iter().collect();
    out.push(chunk);
    i = end;
  }
  // Overflow: the path didn't fit in `max_lines`. Replace the last
  // chunk with a `…`-prefixed slice that keeps the path tail visible
  // instead of cleaving off the filename.
  if i < chars.len() {
    if let Some(last) = out.last_mut() {
      let want = width.saturating_sub(1).max(1);
      let tail_start = chars.len().saturating_sub(want);
      let tail: String = chars[tail_start..].iter().collect();
      *last = format!("…{tail}");
    }
  }
  out
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
        // `ID:L9` prefix surfaces the launch id alongside the port so
        // it's visible without diving into the Settings tab. The
        // label tone matches `RAM` / `CPU` so the trio reads as one
        // styled-label cadence.
        Span::styled("ID:", label_style),
        Span::styled(m.launch_id.clone(), value_style),
        Span::styled("  ", Style::default()),
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

/// Title-text view of [`block_title_with_rects`] for tests that just
/// want to grep the flattened text.
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
      device: None,
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

  /// Thin adapter so the in-module tests can keep asserting against
  /// `Vec<String>` literals after the function moved to
  /// `Vec<RankedChip>`. Returns the chip texts in source order.
  fn chip_texts(app: &App) -> Vec<String> {
    bottom_hint_chips(app).into_iter().map(|c| c.text).collect()
  }

  #[test]
  fn bottom_hint_chips_match_each_focus_tab_combo() {
    use crate::tui::keybindings::{Focus, ENTER_LABEL};
    // Navigation focuses surface the entry-point keystroke per tab.
    assert_eq!(
      chip_texts(&app_with_focus(Focus::RightPane, RightTab::Logs)),
      vec!["s:auto-scroll".to_string(), "c:copy".to_string()]
    );
    // Chat tab adds `r:think` alongside `e:edit` so the toggle is
    // discoverable from the browsing focus.
    assert_eq!(
      chip_texts(&app_with_focus(Focus::RightPane, RightTab::Chat)),
      vec!["e:edit".to_string(), "r:think".to_string()]
    );
    assert_eq!(
      chip_texts(&app_with_focus(Focus::RightPane, RightTab::Embed)),
      vec!["e:edit".to_string()]
    );
    assert_eq!(
      chip_texts(&app_with_focus(Focus::RightPane, RightTab::Rerank)),
      vec!["e:edit".to_string()]
    );
    // Settings on an unfocused selection has no model to act on,
    // so no chips fire — the bottom border stays bare instead of
    // teaching keys that no-op.
    assert!(chip_texts(&app_with_focus(Focus::RightPane, RightTab::Settings)).is_empty());
    // Edit-mode focuses surface InputField-aware modal chips. In a
    // fresh app the field is *not* editing yet, so the chip strip
    // shows `e:edit` plus the resting-state hotkeys (`Enter:send`,
    // `r:think`). `r:think` survives because the resting field
    // passes the char through to the action layer.
    let enter_send = format!("{ENTER_LABEL}:send");
    assert_eq!(
      chip_texts(&app_with_focus(Focus::ChatInput, RightTab::Chat)),
      vec![
        "e:edit".to_string(),
        enter_send.clone(),
        "r:think".to_string(),
      ]
    );
    // Same field after the user enters edit mode — chip switches to
    // `Esc:stop edit` and `r:think` drops out because the editing
    // field captures `r` as a typed character before the action
    // layer ever runs.
    let mut chat_app = app_with_focus(Focus::ChatInput, RightTab::Chat);
    chat_app.chat.prompt.enter_edit();
    assert_eq!(
      chip_texts(&chat_app),
      vec!["Esc:stop edit".to_string(), enter_send.clone()]
    );
    // After exiting edit but with a non-empty buffer (a `Esc` press
    // landed the field in its resting + non-empty state), the chip
    // strip surfaces both `e:edit` and `Esc:clear` so the user sees
    // the next step of the walk-back. `r:think` returns because
    // resting mode passes the char through.
    chat_app.chat.prompt.set_text("hi");
    chat_app.chat.prompt.exit_edit();
    assert_eq!(
      chip_texts(&chat_app),
      vec![
        "e:edit".to_string(),
        "Esc:clear".to_string(),
        enter_send,
        "r:think".to_string(),
      ]
    );
    // Embed mirrors the same shape (one fewer trailing chip).
    assert_eq!(
      chip_texts(&app_with_focus(Focus::EmbedInput, RightTab::Embed)),
      vec!["e:edit".to_string(), format!("{ENTER_LABEL}:embed")]
    );
    // Rerank input: chip strip reflects the active sub-field's
    // editing state. Default field is Query (not editing, empty
    // buffer) — `e:edit · ⏎:rerank`.
    let mut rerank_app = app_with_focus(Focus::RerankInput, RightTab::Rerank);
    assert_eq!(
      chip_texts(&rerank_app),
      vec!["e:edit".to_string(), format!("{ENTER_LABEL}:rerank")]
    );
    // Cycling to the candidate field swaps the Enter description
    // to `add candidate`.
    rerank_app.rerank.cycle_field();
    assert_eq!(
      chip_texts(&rerank_app),
      vec!["e:edit".to_string(), format!("{ENTER_LABEL}:add candidate"),]
    );
    // Entering edit on the candidate field flips the modal chip.
    rerank_app.rerank.candidate_buffer.enter_edit();
    assert_eq!(
      chip_texts(&rerank_app),
      vec![
        "Esc:stop edit".to_string(),
        format!("{ENTER_LABEL}:add candidate"),
      ]
    );
  }

  #[test]
  fn scroll_chip_appears_when_chat_input_has_response() {
    // Issue #31: ↑↓:scroll chip must appear in the ChatInput strip
    // when there is a response to navigate (resting mode only — the
    // chip hides during active editing to avoid clutter).
    use crate::tui::keybindings::Focus;
    let mut app = app_with_focus(Focus::ChatInput, RightTab::Chat);
    // No response yet — scroll chip absent.
    assert!(
      !chip_texts(&app).iter().any(|c| c.contains("scroll")),
      "scroll chip must not appear before a response exists"
    );
    // Populate a response.
    app.chat.response = "Hello from the model".into();
    let chips = chip_texts(&app);
    assert!(
      chips.iter().any(|c| c.contains("scroll")),
      "scroll chip must appear once response is non-empty; chips={chips:?}"
    );
    // During active edit the chip hides so the bar stays uncluttered.
    app.chat.prompt.enter_edit();
    let chips_editing = chip_texts(&app);
    assert!(
      !chips_editing.iter().any(|c| c.contains("scroll")),
      "scroll chip must not appear during active editing; chips={chips_editing:?}"
    );
  }

  #[test]
  fn scroll_chip_appears_when_right_pane_chat_has_response() {
    // The ↑↓:scroll chip also surfaces on the RightPane+Chat strip
    // (browsing focus) once output is available, so users who never
    // enter the ChatInput focus still discover the scroll affordance.
    use crate::tui::keybindings::Focus;
    let mut app = app_with_focus(Focus::RightPane, RightTab::Chat);
    assert!(
      !chip_texts(&app).iter().any(|c| c.contains("scroll")),
      "scroll chip absent when response is empty"
    );
    app.chat.response = "Some response".into();
    let chips = chip_texts(&app);
    assert!(
      chips.iter().any(|c| c.contains("scroll")),
      "scroll chip must appear on RightPane+Chat once response is non-empty; chips={chips:?}"
    );
  }

  #[test]
  fn scroll_chip_appears_when_embed_input_has_output() {
    // EmbedInput: scroll chip appears after a successful embed call.
    use crate::tui::keybindings::Focus;
    let mut app = app_with_focus(Focus::EmbedInput, RightTab::Embed);
    assert!(
      !chip_texts(&app).iter().any(|c| c.contains("scroll")),
      "scroll chip absent before embed result"
    );
    app.embed.dim = Some(1024);
    let chips = chip_texts(&app);
    assert!(
      chips.iter().any(|c| c.contains("scroll")),
      "scroll chip must appear once embed output is present; chips={chips:?}"
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
      display_label: None,
      multimodal: None,
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
    // `e:edit for launch` leads — it's the new explicit gate that
    // replaces the old auto-stage-on-arrow behaviour.
    assert_eq!(
      chip_texts(&app),
      vec![
        "e:edit for launch".to_string(),
        "s:stop".to_string(),
        "↑↓:scroll".to_string(),
        "p:path".to_string(),
        "u:url".to_string(),
        "c:curl".to_string(),
      ]
    );
    // Open the picker — the user is now editing a staged launch.
    // Chips switch to launch+cycle. u/c are intentionally omitted
    // on the editable form.
    use crate::tui::keybindings::ENTER_LABEL;
    app.open_launch_picker();
    let chips = chip_texts(&app);
    assert!(chips.contains(&format!("{ENTER_LABEL}:launch")));
    assert!(chips.contains(&"↑↓:cycle fields".to_string()));
    assert!(chips.contains(&"←→:cycle value".to_string()));
    assert!(chips.contains(&"p:path".to_string()));
    assert!(!chips.iter().any(|c| c.contains("u:url")));
    assert!(!chips.iter().any(|c| c.contains("c:curl")));
  }

  #[test]
  fn settings_bottom_chips_hide_e_edit_when_focused_row_is_a_boolean() {
    // `e:edit` opens an inline buffer on numeric / enum / extras rows
    // but is a no-op on booleans (which are cycled with ←/→). The
    // chip must hide on boolean rows so the affordance doesn't lie —
    // `PickerField::is_editable` is the shared rule.
    use crate::tui::keybindings::Focus;
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    app.open_launch_picker();
    // Default focus lands on Ctx (editable) — baseline chip visible.
    let baseline = chip_texts(&app);
    assert!(baseline.contains(&"e:edit".to_string()));
    // Move focus onto a boolean knob — chip disappears.
    {
      let picker = app.launch_picker.as_mut().unwrap();
      picker.field = PickerField::Knob(crate::launch::flag_aliases::KnobField::FlashAttn);
    }
    let on_bool = chip_texts(&app);
    assert!(
      !on_bool.contains(&"e:edit".to_string()),
      "e:edit must hide on boolean row: {on_bool:?}"
    );
    // Move to the extras row (editable) — chip is back.
    {
      let picker = app.launch_picker.as_mut().unwrap();
      picker.field = PickerField::Extras;
    }
    let on_extras = chip_texts(&app);
    assert!(
      on_extras.contains(&"e:edit".to_string()),
      "e:edit must reappear on the editable Extras row: {on_extras:?}"
    );
  }

  #[test]
  fn settings_bottom_chips_flip_e_edit_to_esc_clear_while_editing() {
    // When a knob row's inline edit (or the extras buffer) is open,
    // global keys are captured by the editor — surface `Esc:clear`
    // instead of `e:edit` so the escape hatch is visible.
    use crate::tui::keybindings::Focus;
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    app.open_launch_picker();
    // Baseline: picker open, no inline edit → e:edit visible.
    let baseline = chip_texts(&app);
    assert!(baseline.contains(&"e:edit".to_string()));
    assert!(!baseline.contains(&"Esc:clear".to_string()));
    // Open inline edit on a numeric field — chip flips.
    {
      let picker = app.launch_picker.as_mut().unwrap();
      picker.inline_edit.open(
        PickerField::Knob(crate::launch::flag_aliases::KnobField::Ctx),
        String::new(),
      );
    }
    let inline = chip_texts(&app);
    assert!(inline.contains(&"Esc:clear".to_string()));
    assert!(!inline.contains(&"e:edit".to_string()));
    // Close inline edit, switch to extras editing — chip still flips.
    {
      let picker = app.launch_picker.as_mut().unwrap();
      picker.inline_edit.close();
      picker.extras_input.enter_edit();
    }
    let extras = chip_texts(&app);
    assert!(extras.contains(&"Esc:clear".to_string()));
    assert!(!extras.contains(&"e:edit".to_string()));
  }

  #[test]
  fn settings_inline_edit_clear_chip_follows_exit_edit_override() {
    // Rebinding `chat_input.exit_edit` (the canonical home for the
    // `ExitEdit` action) must flow through to the inline-edit chip
    // strip — same lookup, same focus, so the chip stays honest.
    use crate::tui::keybindings::{Focus, KeyMap};
    let ctrl_x_label = crate::ctrl_label!("x");
    use crate::tui::launch_picker::PickerField;
    use std::collections::BTreeMap;
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("exit_edit"), String::from("ctrl+x"))]
      .into_iter()
      .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    app.models = vec![fake_model()];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    app.open_launch_picker();
    let picker = app.launch_picker.as_mut().unwrap();
    picker.inline_edit.open(
      PickerField::Knob(crate::launch::flag_aliases::KnobField::Ctx),
      String::new(),
    );
    let chips = chip_texts(&app);
    let expected = format!("{ctrl_x_label}:clear");
    assert!(
      chips.iter().any(|c| c == &expected),
      "expected rebound chip, got {chips:?}"
    );
  }

  #[test]
  fn settings_bottom_chips_for_unlaunched_focus_show_launch_form() {
    // Unlaunched selection: the form is the only context, so the
    // chips read launch + cycle + path.
    use crate::tui::keybindings::{Focus, ENTER_LABEL};
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    let chips = chip_texts(&app);
    assert!(chips.contains(&format!("{ENTER_LABEL}:launch")));
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
      chip_texts(&app),
      vec!["F4:edit".to_string(), "r:think".to_string()],
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
    let (line, _) = block_title_with_rects(&app, &tabs, palette, Rect::new(0, 0, 60, 10));
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
      display_label: None,
      multimodal: None,
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
      display_label: None,
      multimodal: None,
    }];
    // Row 0 is the table header, row 1 is the directory group
    // header, row 2 is the model.
    app.list_cursor = 2;
    let title = right_pane_title(&app);
    assert!(title.contains("not launched"));
  }

  #[test]
  fn render_shows_muted_path_row_under_model_name() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    let mut app = App::new(AppOptions::default());
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/models/custom/Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf"),
      parent: PathBuf::from("/models/custom"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
    }];
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 80, 14), &app, palette, false))
      .unwrap();
    let buf = term.backend().buffer().clone();

    let path = "/models/custom/Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf";
    let row = 3;
    for (idx, ch) in path.chars().enumerate() {
      let cell = buf.cell((2 + idx as u16, row)).unwrap();
      assert_eq!(cell.symbol(), ch.to_string());
      assert_eq!(cell.fg, palette.muted);
    }
  }

  #[test]
  fn header_renders_modality_glyph_after_title() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    let mut app = App::new(AppOptions::default());
    app.models = vec![crate::discovery::DiscoveredModel {
      path: PathBuf::from("/m/gemma-3-4b-it.gguf"),
      parent: PathBuf::from("/m"),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: Some(crate::discovery::Multimodal {
        vision: true,
        audio: false,
      }),
    }];
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 80, 14), &app, palette, false))
      .unwrap();
    let buf = term.backend().buffer().clone();

    // Name row is y=2 (border, blank pad, name). The vision glyph must
    // appear on it, in the accent tone.
    let name_row: String = (0..80)
      .map(|x| buf.cell((x, 2)).unwrap().symbol().to_string())
      .collect();
    assert!(
      name_row.contains('◉'),
      "vision glyph must follow the title: {name_row:?}"
    );
    let glyph_x = (0..80)
      .find(|&x| buf.cell((x, 2)).unwrap().symbol() == "◉")
      .unwrap();
    assert_eq!(buf.cell((glyph_x, 2)).unwrap().fg, palette.accent);
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
      display_label: None,
      multimodal: None,
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
    // The header now spans name → muted path → blank gap → stats.
    assert_eq!(
      stats_row,
      name_row + 3,
      "stats row should sit below the name + path rows with one blank gap"
    );
    let path_row = name_row + 1;
    assert!(
      rows[path_row].contains("/m/qwen.gguf"),
      "expected a full path row directly under the model name, got: {:?}",
      rows[path_row]
    );
    let gap_row = name_row + 2;
    let gap_inner = rows[gap_row].trim_matches(|c| c == '│' || c == ' ');
    assert!(
      gap_inner.is_empty(),
      "expected blank gap row between path and stats, got: {:?}",
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
      display_label: None,
      multimodal: None,
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
