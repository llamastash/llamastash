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
use ratatui::widgets::{Padding, Paragraph};
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
  let (title_line, tab_rects) = block_title_with_rects(app, &tabs, palette, area, focused);
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
    crate::tui::glyphs::active().middot_sep(),
  );
  let border_color = palette.focus_border(focused);

  // All right-pane key hints live on the bottom border now —
  // contextual to the active tab and the current focus. Keeps the
  // top reserved for the tab strip alone (cleaner mnemonic
  // underlines) and gives the user one stable place to scan for
  // active keys.
  let footer = (!bottom_chips.is_empty()).then(|| bottom_hint_line(&bottom_chips, palette));
  let outer = palette
    .panel()
    .title(title_line)
    .footer(footer)
    .border(border_color)
    .padding(Padding::horizontal(1))
    .build();
  let inner = outer.inner(area);
  frame.render_widget(outer, area);

  // Inner stack: blank pad, name (bold), path (muted, wraps to as
  // many lines as needed up to 3), ds4 badge (only for ds4 rows, else
  // zero-height), a blank gap, stats (`:port  state  RAM  CPU`),
  // separator, tab content. Wrapping the path means narrow panes still
  // surface the full filesystem location instead of a left-truncated
  // stub. Capped at 3 lines so a pathological path can't push the tab
  // body off-screen. The gap is its own always-present row so there is
  // exactly one blank line before the stats whether or not the badge shows.
  let path_lines = focused_path_line_count(app, inner.width);
  let badge = focused_backend_badge(app);
  let badge_lines = if badge.is_some() { 1 } else { 0 };
  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(path_lines),
      Constraint::Length(badge_lines),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Length(1),
      Constraint::Min(1),
    ])
    .split(inner);

  render_header_name(frame, layout[1], app, palette);
  render_header_path(frame, layout[2], app, palette);
  if let Some(badge) = &badge {
    render_header_badge(frame, layout[3], badge, palette);
  }
  // layout[4] is the always-blank gap row.
  render_header_stats(frame, layout[5], app, palette);
  render_separator(frame, layout[6], palette);
  let body_area = layout[7];

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
  let line: String = crate::tui::glyphs::active()
    .hline()
    .repeat(area.width as usize);
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
  // The always-on top-bar `↑↓:scroll` chip (`help_bar`) covers the
  // scroll affordance, so the per-pane bottom strips no longer repeat
  // it — they carry only the pane-specific verbs.
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
      // `r:think` only fires when the field is resting (editing mode
      // captures the char). Hiding the chip during edit keeps the
      // hint truthful instead of teaching a key that won't fire.
      if !editing {
        push(
          &mut chips,
          40,
          app.hint_with(Focus::ChatInput, Action::ToggleThinkCollapse, "think"),
        );
      }
    }
    (Focus::EmbedInput, _) => {
      push_input_field_chips(
        &mut chips,
        app.embed.input.is_editing(),
        app.embed.input.is_empty(),
      );
      push(&mut chips, 30, app.hint(Focus::EmbedInput, Action::Submit));
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
        // Save the live knobs as a preset — ranked above the yank trio so
        // it survives width pressure longer (the user asked for `Ctrl+P` to
        // outrank `↑↓` / `p`).
        push(
          &mut chips,
          35,
          app.hint_with(Focus::RightPane, Action::SavePreset, "save preset"),
        );
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
        // `focused_is_editable` (not `field.is_editable`) so a backend's
        // free-text native-knob row also advertises the `e:edit` chip.
        let focused_editable = picker_ref.map(|p| p.focused_is_editable()).unwrap_or(false);
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
        // On the multi-GPU Device row, `Space` toggles the cursor GPU in/out of
        // the selection — the primary action there, and not obvious from the
        // ←/→ cycle affordance. Surface `space:choose` with a high priority
        // (rank 12, just under `launch`) so it survives width pressure while the
        // row is active.
        let device_row_focused = picker_ref
          .map(|p| {
            matches!(p.field, crate::tui::launch_picker::PickerField::Knob(id)
            if p.def(id).is_some_and(|d| {
              d.concept == Some(crate::launch::knobs::Concept::Device)
            }))
          })
          .unwrap_or(false);
        if device_row_focused {
          push(
            &mut chips,
            12,
            app.hint_with(Focus::RightPane, Action::ToggleDevice, "choose"),
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
        // Save the form as a preset — displayed after the cycle chips, but
        // ranked above `↑↓:cycle fields` (40) and `p:path` (60) so it
        // survives width pressure longer.
        push(
          &mut chips,
          35,
          app.hint_with(Focus::RightPane, Action::SavePreset, "save preset"),
        );
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
      spans.push(Span::styled(
        crate::tui::glyphs::active().middot_sep(),
        palette.muted_style(),
      ));
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
  focused: bool,
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
    // When the whole right pane is unfocused, every tab drops to the
    // muted + first-letter-underlined treatment (matching the Models
    // pane title) so the heading recedes and the active list/other
    // pane reads as live. The mnemonic underline is applied by
    // [`mnemonic_spans`].
    let active = *tab == app.right_tab && focused;
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
  let name_style = palette.title_style();
  // Resolve the focused path (running launch first, then the list
  // selection) so the title and its modality glyphs describe one model.
  // The name itself comes from the shared `App::model_label` resolver
  // (catalog `display_label`, else scheme-aware path fallback) so the header
  // matches the list/info surfaces in every state.
  let focused = app
    .right_pane_focus()
    .map(|m| m.path.clone())
    .or_else(|| app.focused_path());
  let name_line = match focused {
    Some(path) => {
      let mut spans = vec![Span::styled(app.model_label(&path), name_style)];
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
      // `↯` MTP after any modality glyphs when discovery found the model
      // MTP-capable (embedded nextn head or a separate drafter sibling) — see
      // `discovery::MTP_LEGEND` (also in the help overlay).
      if app.mtp_capable_for(&path) {
        spans.push(Span::styled(
          format!("  {}", crate::discovery::MTP_LEGEND.0),
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
  // Lemonade registry rows reserve zero height for the path (see
  // `focused_path_line_count`); nothing to paint.
  if focused_is_lemonade_registry(app) {
    return;
  }
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

/// Render a resolved [`BackendBadge`] in the header row that sits directly
/// under the path (the former blank gap slot): an accent-filled chip naming the
/// backend the focused model runs on. Backend-agnostic — see
/// [`focused_backend_badge`].
fn render_header_badge(frame: &mut Frame<'_>, area: Rect, badge: &BackendBadge, palette: &Palette) {
  let chip_style = Style::default()
    .fg(palette.on_accent)
    .bg(palette.accent)
    .add_modifier(Modifier::BOLD);
  // Each backend id is its own accent-filled chip, separated by a plain
  // (no-background) space so two backends read as two distinct badges
  // (` ds4 ` ` llamacpp `) rather than one merged strip.
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(badge.ids.len() * 2);
  for id in &badge.ids {
    if !spans.is_empty() {
      spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(format!(" {id} "), chip_style));
  }
  frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Resolve the path the right pane is currently focused on — running
/// launch first, falling back to the list-pane selection.
fn focused_path(app: &App) -> Option<std::path::PathBuf> {
  app
    .right_pane_focus()
    .map(|m| m.path.clone())
    .or_else(|| app.focused_path())
}

/// Whether the focused row is a Lemonade registry model (synthetic
/// `lemonade://<id>` path). Such rows have no real file on disk — the path is
/// dead weight (it repeats the name + the ` lemonade ` badge), so the header
/// drops the path row for them.
fn focused_is_lemonade_registry(app: &App) -> bool {
  match focused_path(app) {
    Some(p) => crate::backend::lemonade::registry_name_from_path(&p).is_some(),
    None => false,
  }
}

/// A backend identity chip for the header row. Any focused model that resolves
/// at least one backend returns one via [`focused_backend_badge`].
/// Backend-agnostic so a future backend adds a chip without touching the layout.
struct BackendBadge {
  /// One or more backend ids, priority-ordered. Each renders as its own
  /// accent-filled chip (` ds4 `, ` llamacpp `) with a plain gap between, so a
  /// multi-backend model reads as separate badges. Owned so it names any
  /// backend generically.
  ids: Vec<String>,
}

/// Resolve the header badge for the focused model, or `None` when no backend
/// resolves. Drives both the badge render and the header layout (the badge row
/// collapses to zero height when this is `None`, so its slot doesn't double the
/// header gap).
///
/// A **running** row keys on the launch's real backend (a single id — honest
/// even for a compatible file force-run on the default). A **selected** row
/// shows every backend that can serve the model — its `supported_backends`
/// (priority order, so llama.cpp is **not hidden** when a second engine also
/// serves the model, e.g. a deepseek4's ` ds4  llamacpp `), falling back to the
/// `list_models` routing prediction, then the discovery-source backend.
///
/// Every model with a resolved backend carries a chip, including plain
/// llama.cpp-only rows. Plain id chips throughout — no per-backend
/// special-casing, so a new backend surfaces without touching this.
fn focused_backend_badge(app: &App) -> Option<BackendBadge> {
  let path = focused_path(app)?;
  let running = app.right_pane_focus();
  let ids: Vec<String> = match running {
    Some(m) => vec![m.backend.clone()?],
    None => {
      let supported = app
        .models
        .iter()
        .find(|m| m.path == path)
        .map(|m| m.supported_backends.clone())
        .unwrap_or_default();
      if !supported.is_empty() {
        supported
      } else {
        app
          .predicted_backend(&path)
          .map(str::to_string)
          .or_else(|| {
            app
              .models
              .iter()
              .find(|m| m.path == path)
              .map(|m| m.source.backend_id().to_string())
          })
          .into_iter()
          .collect()
      }
    }
  };
  if ids.is_empty() {
    return None;
  }
  Some(BackendBadge { ids })
}

/// Number of vertical rows the focused path needs at `inner_width`,
/// clamped to `[1, 3]`. Used by the right-pane layout to reserve a
/// variable-height slot for the path row so the path wraps cleanly
/// without pushing tab content off-screen on pathological paths.
fn focused_path_line_count(app: &App, inner_width: u16) -> u16 {
  // Lemonade registry models have no real file path — drop the row entirely.
  if focused_is_lemonade_registry(app) {
    return 0;
  }
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
  // chunk with an ellipsis-prefixed slice that keeps the path tail
  // visible instead of cleaving off the filename.
  if i < chars.len() {
    if let Some(last) = out.last_mut() {
      let ellipsis = crate::tui::glyphs::active().ellipsis();
      // Reserve the ellipsis cells (1 for Unicode `…`, 3 for ASCII `...`).
      let want = width.saturating_sub(ellipsis.chars().count()).max(1);
      let tail_start = chars.len().saturating_sub(want);
      let tail: String = chars[tail_start..].iter().collect();
      *last = format!("{ellipsis}{tail}");
    }
  }
  out
}

/// Suffix marking a value shared across all Lemonade models by the one umbrella
/// process — its port, RAM, and CPU are the umbrella's, not per-model. Same `*`
/// the Host pane uses for shared/aggregate pools (`MEM*` / `GPU*`); explained in
/// the help overlay's Legend.
pub(crate) const SHARED_UMBRELLA_MARKER: &str = "*";

/// Render line 2 of the header: `:port  state  RAM  CPU` for a
/// running model, `not launched` when the focused model has no
/// supervisor row, blank when nothing is focused.
fn render_header_stats(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let stats_line = match app.right_pane_focus() {
    Some(m) => {
      let (rss, cpu) = stats_pair(m);
      let label_style = palette.label_style();
      let value_style = palette.text_style();
      let middot = || {
        Span::styled(
          crate::tui::glyphs::active().middot_sep(),
          palette.muted_style(),
        )
      };
      // Managed-multiplexer delegated rows share the umbrella's port/RAM/CPU —
      // mark them. Keyed on the backend's lifecycle shape, not a specific id.
      let mark = if m
        .backend
        .as_deref()
        .is_some_and(crate::backend::is_managed_multiplexer)
      {
        SHARED_UMBRELLA_MARKER
      } else {
        ""
      };
      let mut spans = vec![
        // `ID:L9` prefix surfaces the launch id alongside the port so
        // it's visible without diving into the Settings tab. The
        // label tone matches `RAM` / `CPU` so the trio reads as one
        // styled-label cadence.
        Span::styled("ID:", label_style),
        Span::styled(m.launch_id.clone(), value_style),
        Span::styled("  ", Style::default()),
        Span::styled(format!(":{}{}  ", m.port, mark), palette.muted_style()),
        Span::styled(
          format!("{} ", glyph_for(m.state)),
          Style::default().fg(crate::tui::status_icons::colour_for(m.state, palette)),
        ),
        Span::styled(
          label_for(m.state).to_ascii_lowercase(),
          palette.text_style(),
        ),
      ];
      // ctx — the resolved `--fit` window (llama.cpp) or the pinned value; omit
      // when neither is known (ds4/lemonade launched without an explicit ctx).
      let known_ctx = m.resolved_ctx.or_else(|| {
        m.knobs
          .u32(crate::launch::knobs::resolve_id("ctx-size").expect("ctx knob"))
      });
      if let Some(ctx) = known_ctx {
        spans.push(middot());
        spans.push(Span::styled(
          crate::tui::fmt::format_tokens(ctx as u64),
          value_style,
        ));
        spans.push(Span::styled(" ctx", label_style));
      }
      // Split stats into label/value spans so `RAM` and `CPU` read as blue
      // labels matching the in-pane convention (Host / Daemon panes) instead of
      // disappearing into the same muted tone as the value digits.
      spans.push(middot());
      spans.push(Span::styled(format!("{rss}{mark}"), value_style));
      spans.push(Span::styled(" RAM", label_style));
      spans.push(middot());
      spans.push(Span::styled(format!("{cpu}{mark}"), value_style));
      spans.push(Span::styled(" CPU", label_style));
      Line::from(spans)
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
      ..Default::default()
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
  fn ds4_badge_renders_in_header_gap_for_ds4_model() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    // The badge sits in the header row under the path, resolved via
    // `focused_backend_badge` and painted by `render_header_badge`. A running
    // row keys on the launch's actual backend (a single id). The model name
    // itself never contains "ds4".
    let render_badge = |app: &App| -> String {
      let palette = app.palette();
      let Some(badge) = focused_backend_badge(app) else {
        return String::new();
      };
      let mut term = Terminal::new(TestBackend::new(60, 1)).unwrap();
      term
        .draw(|f| render_header_badge(f, Rect::new(0, 0, 60, 1), &badge, palette))
        .unwrap();
      let buf = term.backend().buffer().clone();
      let mut joined = String::new();
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, 0)).unwrap().symbol());
      }
      joined
    };
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    // A *running* focused row keys on the launch's actual backend, not the
    // routing prediction: tag it ds4 → the chip renders.
    app.managed[0].backend = Some("ds4".into());
    let row = render_badge(&app);
    assert!(row.contains("ds4"), "ds4 chip missing");
    // Chip only — no "serves as" alias disclosure (ds4-server echoes the request
    // model, not a fixed alias).
    assert!(
      !row.contains("serves as"),
      "ds4 row must not disclose an alias"
    );
    // The prediction alone must NOT badge a running row launched on llama.cpp.
    app.managed[0].backend = Some("llamacpp".into());
    app
      .backend_by_path
      .insert(PathBuf::from("/m/qwen.gguf"), "ds4".into());
    let llama_row = render_badge(&app);
    assert!(
      !llama_row.contains("ds4"),
      "running llama.cpp row must not badge ds4 from the prediction"
    );
  }

  #[test]
  fn lemonade_badge_renders_chip() {
    // Running row keys on the launch's real backend (mirrors the ds4 setup).
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("npu-model", None, None)];
    app.list_cursor = 2;
    app.managed[0].backend = Some("lemonade".into());
    let badge = focused_backend_badge(&app).expect("running lemonade → badge");
    assert_eq!(badge.ids, vec!["lemonade".to_string()]);

    // A selected (not-running) Lemonade-registry model badges from its source.
    let mut sel_app = App::new(AppOptions::default());
    let mut m = fake_model();
    m.source = crate::discovery::ModelSource::Lemonade;
    let model_path = m.path.clone();
    sel_app.models = vec![m];
    // Land the cursor on the model row (past any section headers).
    sel_app.list_cursor = sel_app
      .rendered_rows()
      .iter()
      .position(|r| r.path() == Some(model_path.as_path()))
      .expect("model row present");
    let sel_badge = focused_backend_badge(&sel_app).expect("lemonade source → badge");
    assert_eq!(sel_badge.ids, vec!["lemonade".to_string()]);
  }

  #[test]
  fn selected_row_badges_all_supported_backends_including_llamacpp() {
    // A selected (not-running) deepseek4 that both ds4 and llama.cpp can serve
    // shows both, in priority order — and llama.cpp is no longer suppressed.
    let mut app = App::new(AppOptions::default());
    let mut m = fake_model();
    m.supported_backends = vec!["ds4".into(), "llamacpp".into()];
    app.models = vec![m];
    app.list_cursor = app
      .rendered_rows()
      .iter()
      .position(|r| r.path() == Some(std::path::Path::new("/m/qwen.gguf")))
      .expect("model row present");
    let badge = focused_backend_badge(&app).expect("selected multi-backend → badge");
    // llama.cpp is not hidden when a second engine (ds4) also serves the model —
    // and each id is a separate chip (` ds4 ` ` llamacpp `), not one merged strip.
    assert_eq!(badge.ids, vec!["ds4".to_string(), "llamacpp".to_string()]);

    // A llama.cpp-only model now carries its own chip too — the badge is no
    // longer suppressed for the default backend.
    app.models[0].supported_backends = vec!["llamacpp".into()];
    let llama_only = focused_backend_badge(&app).expect("llamacpp-only → badge");
    assert_eq!(llama_only.ids, vec!["llamacpp".to_string()]);
  }

  #[test]
  fn multi_backend_badge_paints_separate_chips_with_a_plain_gap() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    // Two backends must read as two distinct badges: each accent-filled, with a
    // plain (non-accent) space between them — not one merged accent strip.
    let mut app = App::new(AppOptions::default());
    let mut m = fake_model();
    m.supported_backends = vec!["ds4".into(), "llamacpp".into()];
    app.models = vec![m];
    app.list_cursor = app
      .rendered_rows()
      .iter()
      .position(|r| r.path() == Some(std::path::Path::new("/m/qwen.gguf")))
      .expect("model row present");
    let badge = focused_backend_badge(&app).expect("multi-backend badge");
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 1)).unwrap();
    term
      .draw(|f| render_header_badge(f, Rect::new(0, 0, 60, 1), &badge, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let accent = palette.accent;
    let is_accent = |x: u16| buf.cell((x, 0)).unwrap().bg == accent;
    // A plain space cell flanked on both sides by accent cells is the inter-chip
    // gap; the trailing blank area (default bg, no accent neighbour) can't
    // satisfy this, so it uniquely proves the two chips are separated.
    let gap = (1..buf.area.width - 1).any(|x| {
      let c = buf.cell((x, 0)).unwrap();
      c.symbol() == " " && c.bg != accent && is_accent(x - 1) && is_accent(x + 1)
    });
    assert!(
      gap,
      "a plain gap cell must sit between the two accent-filled backend chips"
    );
  }

  fn render_stats_text(app: &App) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(100, 1)).unwrap();
    term
      .draw(|f| render_header_stats(f, Rect::new(0, 0, 100, 1), app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    (0..buf.area.width)
      .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
      .collect()
  }

  #[test]
  fn header_stats_shows_ctx_and_marks_lemonade_shared() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", Some(4_000_000), Some(1.0))];
    app.list_cursor = 2;
    app.managed[0].resolved_ctx = Some(32768);
    app.managed[0].backend = Some("llamacpp".into());
    let s = render_stats_text(&app);
    assert!(s.contains("32k") && s.contains("ctx"), "ctx missing: {s:?}");
    assert!(
      !s.contains('*'),
      "llama.cpp must not carry the shared marker: {s:?}"
    );

    // Lemonade → port/RAM/CPU carry the shared-umbrella marker.
    app.managed[0].backend = Some("lemonade".into());
    let lemon = render_stats_text(&app);
    assert!(
      lemon.contains('*'),
      "lemonade row must carry the shared marker: {lemon:?}"
    );
  }

  #[test]
  fn lemonade_registry_drops_the_path_row() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("Llama-3.1-8B", None, None)];
    app.managed[0].path = PathBuf::from("lemonade://Llama-3.1-8B");
    app.managed[0].backend = Some("lemonade".into());
    app.list_cursor = 2;
    assert!(focused_is_lemonade_registry(&app));
    assert_eq!(
      focused_path_line_count(&app, 50),
      0,
      "lemonade path row must reserve zero height"
    );
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
    // The `↑↓:scroll` affordance lives in the always-on top bar now,
    // so the bottom strips carry only pane-specific verbs.
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
    use crate::tui::keybindings::{Focus, CTRL_PREFIX};
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    // Read-only view: no picker open, managed launch present.
    // `e:edit for launch` leads — it's the new explicit gate that
    // replaces the old auto-stage-on-arrow behaviour. `Ctrl+P:save preset`
    // sits above the yank trio (ranked to survive width pressure longer).
    assert_eq!(
      chip_texts(&app),
      vec![
        "e:edit for launch".to_string(),
        "s:stop".to_string(),
        format!("{CTRL_PREFIX}p:save preset"),
        "p:path".to_string(),
        "u:url".to_string(),
        "c:curl".to_string(),
      ]
    );
    // Open the picker — the user is now editing a staged launch.
    // Chips switch to launch+cycle. u/c are intentionally omitted
    // on the editable form. Save-preset stays available.
    use crate::tui::keybindings::ENTER_LABEL;
    app.open_launch_picker();
    let chips = chip_texts(&app);
    assert!(chips.contains(&format!("{ENTER_LABEL}:launch")));
    assert!(chips.contains(&format!("{CTRL_PREFIX}p:save preset")));
    assert!(chips.contains(&"↑↓:cycle fields".to_string()));
    assert!(chips.contains(&"←→:cycle value".to_string()));
    assert!(chips.contains(&"p:path".to_string()));
    assert!(!chips.iter().any(|c| c.contains("u:url")));
    assert!(!chips.iter().any(|c| c.contains("c:curl")));
  }

  #[test]
  fn settings_save_preset_chip_outranks_cycle_and_path() {
    // The `Ctrl+P:save preset` chip must survive width pressure longer
    // than `↑↓:cycle fields` and `p:path` — i.e. a strictly lower rank.
    use crate::tui::keybindings::Focus;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.list_cursor = 2;
    app.focus = Focus::RightPane;
    app.right_tab = RightTab::Settings;
    app.open_launch_picker();
    let chips = bottom_hint_chips(&app);
    let rank_of = |needle: &str| {
      chips
        .iter()
        .find(|c| c.text.contains(needle))
        .unwrap_or_else(|| panic!("{needle} chip present: {chips:?}"))
        .rank
    };
    let save = rank_of("save preset");
    assert!(
      save < rank_of("cycle fields"),
      "save preset must outrank ↑↓:cycle fields"
    );
    assert!(save < rank_of("path"), "save preset must outrank p:path");
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
    // Focus the Ctx row (editable) — the cursor opens on the Preset row.
    app.launch_picker.as_mut().unwrap().field =
      PickerField::Knob(crate::launch::knobs::kid("ctx-size"));
    let baseline = chip_texts(&app);
    assert!(baseline.contains(&"e:edit".to_string()));
    // Move focus onto a boolean knob — chip disappears.
    {
      let picker = app.launch_picker.as_mut().unwrap();
      picker.field = PickerField::Knob(crate::launch::knobs::kid("flash-attn"));
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
    // Focus an editable knob (the cursor opens on the Preset row).
    app.launch_picker.as_mut().unwrap().field =
      PickerField::Knob(crate::launch::knobs::kid("ctx-size"));
    // Baseline: picker open, no inline edit → e:edit visible.
    let baseline = chip_texts(&app);
    assert!(baseline.contains(&"e:edit".to_string()));
    assert!(!baseline.contains(&"Esc:clear".to_string()));
    // Open inline edit on a numeric field — chip flips.
    {
      let picker = app.launch_picker.as_mut().unwrap();
      picker.inline_edit.open(
        PickerField::Knob(crate::launch::knobs::kid("ctx-size")),
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
      PickerField::Knob(crate::launch::knobs::kid("ctx-size")),
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
    let (line, _) = block_title_with_rects(&app, &tabs, palette, Rect::new(0, 0, 60, 10), true);
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
    // With the right pane FOCUSED: the first letter of each *inactive*
    // tab label carries the UNDERLINED modifier so it reads as a
    // press-this-letter shortcut hint. The active tab drops the
    // underline so it doesn't double up with the bold focus styling.
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
      .draw(|f| render(f, Rect::new(0, 0, 80, 18), &app, palette, true))
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
  fn unfocused_right_pane_dims_active_tab_to_muted_underlined_non_bold() {
    // When the right pane is UNFOCUSED, the active tab loses its bold
    // panel_title styling and reads like an inactive tab: muted fg,
    // first letter underlined, non-bold — so the heading recedes and
    // the focused (list) pane reads as live. Mirrors the Models pane
    // title's unfocused treatment.
    use crate::tui::keybindings::Focus;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model()];
    app.managed = vec![ready_managed("qwen", None, None)];
    app.list_cursor = 2;
    app.right_tab = RightTab::Settings; // active tab
    app.focus = Focus::List; // but the LIST owns focus
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(80, 18)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 80, 18), &app, palette, false))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut settings_x: Option<u16> = None;
    for x in 0..buf.area.width {
      if buf.cell((x, 0)).unwrap().symbol() == "S" {
        settings_x = Some(x);
        break;
      }
    }
    let s_cell = buf
      .cell((settings_x.expect("S of Settings on top row"), 0))
      .unwrap();
    assert!(
      s_cell.modifier.contains(Modifier::UNDERLINED),
      "unfocused pane: active tab's first letter must be underlined"
    );
    assert!(
      !s_cell.modifier.contains(Modifier::BOLD),
      "unfocused pane: active tab must not be bold"
    );
    assert_eq!(
      s_cell.fg, palette.muted,
      "unfocused pane: active tab must be painted muted"
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
      supported_backends: Vec::new(),
      mtp_head: None,
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
