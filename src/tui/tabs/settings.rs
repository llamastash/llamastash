//! Settings tab — typed-knob launch editor for the focused model.
//!
//! Renders a vertical list of rows: `ctx`, `reasoning`, every
//! `TypedKnobs` field with a per-row source label, and an `extras`
//! free-text row at the bottom. When the focused model has a
//! running launch and the picker isn't open, shows the live params
//! (read-only).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::config::{KnobValue, KnobValueOpt};
use crate::launch::flag_aliases::{knob_display_groups, KnobField};
use crate::theme::Palette;
use crate::tui::app::{App, ManagedRow};
use crate::tui::keybindings::{Action, Focus};
use crate::tui::launch_picker::{LaunchPickerState, PickerField, INHERITED_LABEL};

/// Render the Settings tab body into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  // One render path for both the read-only running view and the editable
  // launch form. They differ only in `editable` (and where each row's
  // value comes from), so sharing the loop keeps the source-chip
  // breakpoint and the `…`-truncation identical — neither view wraps or
  // jumps as values change.
  let managed = if app.launch_picker.is_none() {
    app.focused_managed()
  } else {
    None
  };
  let editable = managed.is_none();

  // The editable path resolves a picker (the live one, or a default built
  // from the focused model); the read-only path reads `managed` directly.
  let default_picker: LaunchPickerState;
  let picker_view: Option<&LaunchPickerState> = if editable {
    Some(match app.launch_picker.as_ref() {
      Some(p) => p,
      None => {
        default_picker = app.build_default_picker().unwrap_or_else(|| {
          let name = app.focused_name().unwrap_or_else(|| "(none)".into());
          LaunchPickerState::for_model(name)
        });
        &default_picker
      }
    })
  } else {
    None
  };
  let no_focus = editable && app.focused_path().is_none();

  let show_source = area.width >= SHOW_SOURCE_MIN_WIDTH;
  // Track the focused row's index so the editable view keeps it on-screen
  // with a margin; the read-only view leaves this `None` and scrolls free.
  let mut focused_line: Option<u16> = None;

  let mut lines: Vec<Line<'static>> = Vec::new();
  lines.push(heading(
    if !editable {
      "Running launch"
    } else if no_focus {
      "No model focused"
    } else {
      "Launch settings"
    },
    palette,
  ));

  if let Some(m) = managed {
    // Read-only: name the launch (port / state / rss live in the header).
    lines.push(crate::tui::fmt::kv_row(
      "launch",
      m.launch_id.clone(),
      palette,
    ));
    // Server (build/binary) the launch ran on — mirrors the editable picker's
    // server row so the operator can see which build served the model. Shown
    // only when the model has more than one compatible server (a real choice);
    // a single-server model has nothing to disambiguate.
    if let Some(label) = running_server_label(app, m) {
      lines.push(crate::tui::fmt::kv_row("server", label, palette));
    }
  } else if let Some(pv) = picker_view {
    // Editable: duplicate-launch heads-up, then the preset cycle row.
    if pv.active_instances > 0 {
      lines.push(
        Span::styled(
          format!(
            "⚠ {n} instance{plural} already running — Enter launches a new one on a fresh port",
            n = pv.active_instances,
            plural = if pv.active_instances == 1 { "" } else { "s" }
          ),
          Style::default()
            .fg(palette.warning)
            .add_modifier(Modifier::BOLD),
        )
        .into(),
      );
    }
    // Preset cycle row leads the form. No source chip: it's a selector,
    // not an inherited value. The label carries the count of named presets
    // available for this model (`preset (0)` when none).
    let focused = pv.field == PickerField::Preset;
    if focused {
      focused_line = Some(lines.len() as u16);
    }
    let preset_label = format!("preset ({})", pv.presets.len());
    lines.push(crate::tui::fmt::kv_row_focused(
      &preset_label,
      pv.preset_value_label(),
      None,
      focused,
      true,
      palette,
      show_source,
    ));
    // Server (build/binary) cycle row, just under the preset row — shown only
    // when the model has more than one compatible server (a real choice). No
    // source chip: it's a selector, not an inherited value.
    if pv.field_visible(PickerField::Server) {
      let server_focused = pv.field == PickerField::Server;
      if server_focused {
        focused_line = Some(lines.len() as u16);
      }
      lines.push(crate::tui::fmt::kv_row_focused(
        "server",
        pv.server_value_label(),
        None,
        server_focused,
        true,
        palette,
        show_source,
      ));
    }
  }

  // Every typed knob flows through the same `value (chip)` row shape in
  // both views. The read-only view shows the *dispatched* values (`auto`
  // for a fit-delegated row), with ctx overlaid by the `--fit`-resolved
  // window read from `/props`; no chip, since a live value has no
  // inheritance layer to name.
  let resolved_ctx = managed.map(|m| m.resolved_ctx.or(m.knobs.ctx.set_value().copied()));
  // A running model on a backend that has its own native knobs (not the
  // llama.cpp typed IR) renders those native rows (ctx + its tunables) from the
  // live params instead of the empty llama.cpp groups. Keyed on the launch's
  // *actual* backend (not the routing prediction), so a compatible file launched
  // with `--backend llamacpp` still shows the llama.cpp knobs it ran with.
  // Registry-driven — names no backend. Editable launches keep the picker path.
  let native_descriptors: &[crate::launch::native_knobs::NativeKnobDescriptor] = managed
    .and_then(|m| m.backend.as_deref())
    .map(crate::backend::native_knobs_for)
    .unwrap_or(&[]);
  let native_readonly = !native_descriptors.is_empty();
  if native_readonly {
    let m = managed.expect("native_readonly implies a managed row");
    push_native_readonly_rows(
      &mut lines,
      m,
      native_descriptors,
      resolved_ctx.flatten(),
      palette,
      show_source,
    );
  }
  for group in knob_display_groups() {
    // The native read-only view rendered its rows above; skip the llama.cpp groups.
    if native_readonly {
      break;
    }
    // Skip the whole group — header included — when it has no visible row
    // (the Multi-GPU placement group on single-GPU / CPU-only hosts).
    let group_visible = match picker_view {
      Some(pv) => group
        .fields
        .iter()
        .any(|f| pv.field_visible(PickerField::Knob(*f))),
      None => !group.multi_device_only || app.multi_device(),
    };
    if !group_visible {
      continue;
    }
    lines.push(group_header(group.title, palette));
    for field in group.fields {
      let field = *field;
      match picker_view {
        Some(pv) => {
          if !pv.field_visible(PickerField::Knob(field)) {
            continue;
          }
          let focused = pv.field == PickerField::Knob(field);
          if focused {
            focused_line = Some(lines.len() as u16);
          }
          if pv.inline_edit.is_open() && pv.inline_edit.field == Some(PickerField::Knob(field)) {
            lines.push(inline_edit_row(
              field.field_name(),
              pv.inline_edit.input.buffer(),
              focused,
              palette,
            ));
            if let Some(err) = &pv.inline_edit.error {
              lines.push(inline_warning_row(err, palette));
            }
          } else {
            let value = format_knob_value(pv, field);
            let source = pv.source_for(field).label();
            lines.push(crate::tui::fmt::kv_row_focused(
              field.field_name(),
              value,
              Some(source),
              focused,
              true,
              palette,
              show_source,
            ));
          }
        }
        None => {
          let m = managed.expect("read-only view implies a managed row");
          let value = match field {
            KnobField::Ctx => resolved_ctx
              .flatten()
              .map(|v| {
                // Flag a memory-driven clamp so the user knows the window
                // was squeezed to the floor, not chosen freely.
                if m.ctx_clamped {
                  format!("{v} · clamped to floor")
                } else {
                  v.to_string()
                }
              })
              .unwrap_or_else(|| format_persisted_knob_value(&m.knobs, KnobField::Ctx)),
            _ => format_persisted_knob_value(&m.knobs, field),
          };
          // Not focused, not cyclable, no source chip — renders as a plain
          // `label  value` row through the shared formatter.
          lines.push(crate::tui::fmt::kv_row_focused(
            field.field_name(),
            value,
            None,
            false,
            false,
            palette,
            show_source,
          ));
        }
      }
    }
  }

  // Per-backend native-knob rows, then the extras row — extras always last.
  match picker_view {
    Some(pv) => {
      // Native knobs render under their own group header (matching the
      // llama.cpp knob groups above), above extras. ds4 surfaces six rows
      // here; llama.cpp / Lemonade have none.
      let any_native =
        (0..pv.native_descriptors.len()).any(|i| pv.field_visible(PickerField::NativeKnob(i)));
      if any_native {
        lines.push(group_header(
          &format!("{} native", pv.active_backend_id()),
          palette,
        ));
      }
      for (i, descriptor) in pv.native_descriptors.iter().enumerate() {
        let field = PickerField::NativeKnob(i);
        if !pv.field_visible(field) {
          continue;
        }
        let focused = pv.field == field;
        if focused {
          focused_line = Some(lines.len() as u16);
        }
        if pv.inline_edit.is_open() && pv.inline_edit.field == Some(field) {
          lines.push(inline_edit_row(
            descriptor.label,
            pv.inline_edit.input.buffer(),
            focused,
            palette,
          ));
          if let Some(err) = &pv.inline_edit.error {
            lines.push(inline_warning_row(err, palette));
          }
        } else {
          lines.push(crate::tui::fmt::kv_row_focused(
            descriptor.label,
            pv.native_value_label(i),
            None,
            focused,
            // Cycle / bool rows take ←/→; free-text rows are `e`-edited.
            !descriptor.is_editable(),
            palette,
            show_source,
          ));
        }
      }

      // Extras row — always the last field.
      let extras_focused = pv.field == PickerField::Extras;
      if extras_focused {
        focused_line = Some(lines.len() as u16);
      }
      if pv.extras_input.is_editing() {
        lines.push(inline_edit_row(
          "extras",
          pv.extras_input.buffer(),
          extras_focused,
          palette,
        ));
      } else {
        let extras_text = if pv.extras.is_empty() {
          "(none)".to_string()
        } else {
          pv.extras
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
        };
        lines.push(crate::tui::fmt::kv_row_focused(
          "extras",
          extras_text,
          None,
          extras_focused,
          false,
          palette,
          show_source,
        ));
      }
      // Forbidden-flag warning under the extras row.
      if !crate::launch::params::forbidden_in_extras(&pv.extras).is_empty() {
        let redacted = crate::launch::params::redact_for_display(&pv.extras);
        lines.push(inline_warning_row(
          &format!("forbidden: {redacted}"),
          palette,
        ));
      }
    }
    None => {
      let m = managed.expect("read-only view implies a managed row");
      let extras: String = app
        .last_params
        .get(&m.path)
        .map(|p| p.extras.join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into());
      lines.push(crate::tui::fmt::kv_row_focused(
        "extras",
        extras,
        None,
        false,
        false,
        palette,
        show_source,
      ));
    }
  }

  lines.push(Line::default());
  let hint = if managed.is_some() {
    let edit_chip = app
      .hint_with(Focus::RightPane, Action::EnterEdit, "edit for launch")
      .map(|c| chip_label(&c).to_string())
      .unwrap_or_else(|| "e".to_string());
    format!("Press `{edit_chip}` to edit next-launch params, or `s` to stop and re-launch.")
  } else if no_focus {
    "Select a model in the list to configure launch settings.".to_string()
  } else {
    app
      .hint_with(Focus::RightPane, Action::Submit, "launch")
      .map(|chip| {
        format!(
          "Press {} again to launch with these settings.",
          chip_label(&chip)
        )
      })
      .unwrap_or_else(|| "Launch binding removed — set `submit` in config.".to_string())
  };
  lines.push(Span::styled(hint, palette.muted_style()).into());

  // Clip each row to the pane width with `…` and render without `Wrap`,
  // so an overlong `value  (server default)` row truncates on one line
  // instead of wrapping (which shifts the rows below it and makes preset
  // cycling / live updates jump). With nothing wrapping, the rendered row
  // count equals the logical line count, so scroll clamps stay exact.
  let max_w = area.width as usize;
  let total_rows = lines.len() as u16;
  let clipped: Vec<Line<'static>> = lines
    .into_iter()
    .map(|l| crate::tui::fmt::clip_line(l, max_w, palette))
    .collect();

  let scroll = if let Some(pv) = picker_view {
    // Editable: keep the focused row visible with ≥1 row of margin.
    let s = clamp_scroll_with_margin(
      pv.scroll_offset.get(),
      focused_line.unwrap_or(0),
      area.height,
      total_rows,
    );
    pv.scroll_offset.set(s);
    s
  } else {
    // Read-only: free scroll, clamped in-bounds.
    let s = app
      .running_view_scroll
      .get()
      .min(total_rows.saturating_sub(area.height));
    app.running_view_scroll.set(s);
    s
  };

  frame.render_widget(Paragraph::new(clipped).scroll((scroll, 0)), area);
}

/// Read-only ds4 running rows: the resolved ctx, then the six ds4 native
/// knobs read from the recorded `last_params` (value or `inherited`). ds4
/// ignores the llama.cpp typed knobs, so the running view shows these — the
/// same set the launch picker offers — instead of a wall of `inherited`.
fn push_native_readonly_rows(
  lines: &mut Vec<Line<'static>>,
  m: &ManagedRow,
  descriptors: &[crate::launch::native_knobs::NativeKnobDescriptor],
  resolved_ctx: Option<u32>,
  palette: &Palette,
  show_source: bool,
) {
  lines.push(group_header("Context", palette));
  let ctx_value = resolved_ctx
    .map(|v| {
      if m.ctx_clamped {
        format!("{v} · clamped to floor")
      } else {
        v.to_string()
      }
    })
    .unwrap_or_else(|| format_persisted_knob_value(&m.knobs, KnobField::Ctx));
  lines.push(crate::tui::fmt::kv_row_focused(
    "ctx",
    ctx_value,
    None,
    false,
    false,
    palette,
    show_source,
  ));
  let header = m
    .backend
    .as_deref()
    .map(|b| format!("{b} native"))
    .unwrap_or_else(|| "native".to_string());
  lines.push(group_header(&header, palette));
  // Read the values from the launch's *live* `backend_knobs` (the running row,
  // sourced from `status`), so the panel reflects what the server is actually
  // running with — including an auto-enabled knob — rather than the saved
  // `last_params` delta.
  for d in descriptors {
    let value = m
      .backend_knobs
      .get(d.id)
      .and_then(KnobValue::as_set)
      .cloned()
      .unwrap_or_else(|| INHERITED_LABEL.to_string());
    lines.push(crate::tui::fmt::kv_row_focused(
      d.label,
      value,
      None,
      false,
      false,
      palette,
      show_source,
    ));
  }
}

/// Minimal scroll with margin: keep the focused row visible with
/// `MARGIN` rows of context above and below where possible. Returns
/// the new scroll offset. Clamped to `[0, max_scroll]`.
fn clamp_scroll_with_margin(current: u16, focused: u16, viewport: u16, total: u16) -> u16 {
  const MARGIN: u16 = 1;
  let max_scroll = total.saturating_sub(viewport);
  if viewport == 0 {
    return 0;
  }
  // Scroll up so focused is at least MARGIN rows below the top.
  let upper_bound = focused.saturating_sub(MARGIN);
  // Scroll down so focused is at least MARGIN rows above the bottom.
  let lower_bound = focused.saturating_add(MARGIN + 1).saturating_sub(viewport);
  let mut next = current;
  if next > upper_bound {
    next = upper_bound;
  }
  if next < lower_bound {
    next = lower_bound;
  }
  next.min(max_scroll)
}

/// Read-only formatter for the running-launch view. Same vocabulary
/// as `format_knob_value` (value or `inherited` / `on` / `off`) but
/// reads straight from a persisted `TypedKnobs` instead of a picker
/// state. Untouched fields render `inherited` — the user can open the
/// editor (`e`) to see the resolved chip.
/// Render a persisted knob slot: a pinned value verbatim, the literal
/// `auto` for the Auto state, and `inherited` for an unset (Inherited)
/// slot. Empty strings (cleared `device` / `tensor_split` / `split_mode`)
/// read as `inherited` too.
fn knob_value_label<T: std::fmt::Display>(slot: &Option<KnobValue<T>>) -> String {
  match slot {
    Some(KnobValue::Set(v)) => {
      let s = v.to_string();
      if s.is_empty() {
        INHERITED_LABEL.into()
      } else {
        s
      }
    }
    Some(KnobValue::Auto) => "auto".into(),
    None => INHERITED_LABEL.into(),
  }
}

fn format_persisted_knob_value(knobs: &crate::config::TypedKnobs, field: KnobField) -> String {
  match field {
    KnobField::Ctx => knob_value_label(&knobs.ctx),
    KnobField::NGpuLayers => knob_value_label(&knobs.n_gpu_layers),
    KnobField::NCpuMoe => knob_value_label(&knobs.n_cpu_moe),
    KnobField::Threads => knob_value_label(&knobs.threads),
    KnobField::Parallel => knob_value_label(&knobs.parallel),
    KnobField::BatchSize => knob_value_label(&knobs.batch_size),
    KnobField::UbatchSize => knob_value_label(&knobs.ubatch_size),
    KnobField::Keep => knob_value_label(&knobs.keep),
    KnobField::RopeFreqScale => knob_value_label(&knobs.rope_freq_scale),
    KnobField::CacheTypeK => knob_value_label(&knobs.cache_type_k),
    KnobField::CacheTypeV => knob_value_label(&knobs.cache_type_v),
    KnobField::Reasoning => bool_label(&knobs.reasoning),
    KnobField::FlashAttn => bool_label(&knobs.flash_attn),
    KnobField::Mlock => bool_label(&knobs.mlock),
    KnobField::NoMmap => bool_label(&knobs.no_mmap),
    KnobField::Device => knob_value_label(&knobs.device),
    KnobField::MainGpu => knob_value_label(&knobs.main_gpu),
    KnobField::TensorSplit => knob_value_label(&knobs.tensor_split),
    KnobField::SplitMode => knob_value_label(&knobs.split_mode),
  }
}

/// Read-only `server` row label for a running launch, or `None` when the model
/// has one (or zero) compatible servers so there's nothing to disambiguate.
/// Mirrors the editable picker's `server_value_label`: the priority-default
/// server (catalog first) reads `<id> (default)`, an explicitly-picked
/// non-default server reads its bare id. A launch that recorded no server pick
/// took the default, so it renders the default label too.
fn running_server_label(app: &App, m: &ManagedRow) -> Option<String> {
  let servers = app.compatible_servers(&m.path);
  if servers.len() < 2 {
    return None;
  }
  let default_id = servers.first().map(|s| s.id.as_str());
  let label = match m.server.as_deref() {
    Some(id) if Some(id) != default_id => id.to_string(),
    Some(id) => format!("{id} (default)"),
    None => match default_id {
      Some(id) => format!("{id} (default)"),
      None => "default".to_string(),
    },
  };
  Some(label)
}

fn bool_label(v: &Option<KnobValue<bool>>) -> String {
  match v.set_value().copied() {
    Some(true) => "on".into(),
    Some(false) => "off".into(),
    None if v.is_auto() => "auto".into(),
    None => INHERITED_LABEL.into(),
  }
}

fn format_knob_value(state: &LaunchPickerState, field: KnobField) -> String {
  // The Auto stop renders as `auto` regardless of value kind — fit
  // governs the knob, so there is no concrete value to show.
  if state.effective_is_auto(field) {
    return "auto".into();
  }
  match field {
    KnobField::Ctx
    | KnobField::NGpuLayers
    | KnobField::NCpuMoe
    | KnobField::Threads
    | KnobField::Parallel
    | KnobField::BatchSize
    | KnobField::UbatchSize
    | KnobField::Keep
    | KnobField::MainGpu => state
      .effective_u32(field)
      .map(|v| v.to_string())
      .unwrap_or_else(|| INHERITED_LABEL.into()),
    KnobField::RopeFreqScale => state
      .effective_f32(field)
      .map(|v| format!("{v}"))
      .unwrap_or_else(|| INHERITED_LABEL.into()),
    KnobField::CacheTypeK
    | KnobField::CacheTypeV
    | KnobField::TensorSplit
    | KnobField::SplitMode => state
      .effective_str(field)
      .unwrap_or_else(|| INHERITED_LABEL.into()),
    KnobField::Device => state.device_value_display(),
    KnobField::Reasoning | KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => {
      match state.effective_bool(field) {
        Some(true) => "on".into(),
        Some(false) => "off".into(),
        None => INHERITED_LABEL.into(),
      }
    }
  }
}

fn heading<'a>(text: &'a str, palette: &Palette) -> Line<'a> {
  Line::from(Span::styled(
    text,
    Style::default()
      .fg(palette.highlight)
      .add_modifier(Modifier::BOLD),
  ))
}

/// Quiet divider above each knob cluster — `── Title`, indented to
/// align with the rows below it and painted in the muted tone so it
/// reads as a separator, not a value row.
fn group_header(title: &str, palette: &Palette) -> Line<'static> {
  Line::from(Span::styled(
    format!(
      "  {}{} {title}",
      crate::tui::glyphs::active().hline(),
      crate::tui::glyphs::active().hline()
    ),
    palette.muted_style().add_modifier(Modifier::BOLD),
  ))
}

const LABEL_W: usize = 16;

/// Pane width at/above which a knob row has room for its `(source)` chip.
/// In wide mode the right pane is only ~35% of the terminal, so the gate
/// trips well below 50 cols. Shared by both Settings views.
const SHOW_SOURCE_MIN_WIDTH: u16 = 40;

fn chip_label(chip: &str) -> &str {
  chip.split(':').next().unwrap_or(chip)
}

fn inline_edit_row(label: &str, buffer: &str, focused: bool, palette: &Palette) -> Line<'static> {
  let marker = if focused {
    crate::tui::glyphs::active().focus_marker()
  } else {
    "  "
  };
  let label_style = Style::default()
    .fg(palette.accent)
    .add_modifier(Modifier::BOLD);
  Line::from(vec![
    Span::styled(
      format!("{marker}{label:<width$}", width = LABEL_W),
      label_style,
    ),
    Span::styled("[ ".to_string(), palette.muted_style()),
    Span::styled(buffer.to_string(), palette.text_style()),
    crate::tui::fmt::caret(palette),
    Span::styled(" ]".to_string(), palette.muted_style()),
  ])
}

fn inline_warning_row(message: &str, palette: &Palette) -> Line<'static> {
  Line::from(Span::styled(
    format!("    ⚠ {message}"),
    Style::default()
      .fg(palette.warning)
      .add_modifier(Modifier::BOLD),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::{App, AppOptions};
  use std::path::PathBuf;

  #[test]
  fn clamp_scroll_keeps_focused_visible_with_margin() {
    // Focused row inside the viewport — no change.
    assert_eq!(clamp_scroll_with_margin(0, 5, 20, 30), 0);
    // Focused below the viewport bottom — scroll just enough to land
    // with one row of margin below.
    assert_eq!(clamp_scroll_with_margin(0, 19, 10, 30), 11);
    // Focused above the viewport top — scroll up to land one row
    // below the top edge.
    assert_eq!(clamp_scroll_with_margin(15, 5, 10, 30), 4);
    // Focused at index 0 with no margin available — saturate at 0.
    assert_eq!(clamp_scroll_with_margin(5, 0, 10, 30), 0);
    // Viewport bigger than content — never scroll.
    assert_eq!(clamp_scroll_with_margin(0, 5, 50, 10), 0);
    // Zero viewport returns 0 (would otherwise underflow).
    assert_eq!(clamp_scroll_with_margin(5, 5, 0, 30), 0);
  }

  fn server_with(id: &str, selector: &str) -> crate::backend::Server {
    crate::backend::Server {
      id: id.into(),
      backend_id: crate::backend::DEFAULT_BACKEND_ID.into(),
      binary: PathBuf::from(format!("/builds/{id}/llama-server")),
      name: id.into(),
      devices: vec![crate::backend::Device {
        selector: selector.into(),
        gpu_backend: "ROCm".into(),
        name: "Test GPU".into(),
        total_mib: Some(24576),
        free_mib: Some(24000),
      }],
    }
  }

  fn app_with_two_servers(path: &str) -> App {
    let mut app = App::new(AppOptions::default());
    let mut model = fake_model(path, "/m");
    model.supported_backends = vec![crate::backend::DEFAULT_BACKEND_ID.to_string()];
    app.models = vec![model];
    app.servers = vec![
      server_with("llamacpp-rocm", "ROCm0"),
      server_with("llamacpp-vulkan", "Vulkan0"),
    ];
    app
  }

  #[test]
  fn running_server_label_reports_default_and_explicit_builds() {
    use crate::tui::app::ManagedRow;
    let app = app_with_two_servers("/m/a.gguf");
    let path = PathBuf::from("/m/a.gguf");
    // No recorded pick → the launch took the priority-default (catalog first).
    let m_default = ManagedRow {
      path: path.clone(),
      server: None,
      ..Default::default()
    };
    assert_eq!(
      running_server_label(&app, &m_default),
      Some("llamacpp-rocm (default)".to_string())
    );
    // An explicit non-default build reads its bare id.
    let m_vk = ManagedRow {
      path: path.clone(),
      server: Some("llamacpp-vulkan".into()),
      ..Default::default()
    };
    assert_eq!(
      running_server_label(&app, &m_vk),
      Some("llamacpp-vulkan".to_string())
    );
    // Pinning the priority-default build reads `(default)` too.
    let m_rocm = ManagedRow {
      path,
      server: Some("llamacpp-rocm".into()),
      ..Default::default()
    };
    assert_eq!(
      running_server_label(&app, &m_rocm),
      Some("llamacpp-rocm (default)".to_string())
    );
  }

  #[test]
  fn running_server_label_hidden_with_a_single_server() {
    use crate::tui::app::ManagedRow;
    let mut app = app_with_two_servers("/m/a.gguf");
    app.servers.truncate(1);
    let m = ManagedRow {
      path: PathBuf::from("/m/a.gguf"),
      server: None,
      ..Default::default()
    };
    assert_eq!(
      running_server_label(&app, &m),
      None,
      "a single server has nothing to disambiguate"
    );
  }

  fn fake_model(path: &str, parent: &str) -> crate::discovery::DiscoveredModel {
    crate::discovery::DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
    }
  }

  #[test]
  fn settings_form_reflects_last_params_on_first_render() {
    use crate::tui::app::LastParamsRow;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    let path = PathBuf::from("/m/qwen.gguf");
    app.models = vec![fake_model("/m/qwen.gguf", "/m")];
    app.last_params.insert(
      path.clone(),
      LastParamsRow {
        ctx: Some(16384),
        reasoning: true,
        // ctx/reasoning now live inside `knobs`; the picker seeds
        // `user_knobs` straight from `knobs` so a returning user
        // sees their last-shipped values with `(user)` chips.
        knobs: crate::config::TypedKnobs {
          ctx: Some(KnobValue::Set(16384)),
          reasoning: Some(KnobValue::Set(true)),
          ..Default::default()
        },
        backend_knobs: Default::default(),
        extras: vec!["--rope-freq-base".into(), "10000".into()],
        port: Some(41100),
        server: None,
      },
    );
    app.list_cursor = 2;
    assert!(app.launch_picker.is_none());
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 32)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 32), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    assert!(joined.contains("16384"), "{joined}");
    assert!(joined.contains("on"), "{joined}");
  }

  #[test]
  fn ds4_picker_groups_native_knobs_under_header_with_extras_last() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    let mut picker = LaunchPickerState::for_model("DeepSeek-V4-Flash");
    picker.model_backend = crate::launch::params::BackendChoice::Explicit("ds4".into());
    picker.native_descriptors = crate::backend::ds4::DS4_NATIVE_KNOBS;
    app.launch_picker = Some(picker);
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 40), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let rows: Vec<String> = (0..buf.area.height)
      .map(|y| {
        (0..buf.area.width)
          .map(|x| buf.cell((x, y)).unwrap().symbol())
          .collect()
      })
      .collect();
    let row_of = |needle: &str| rows.iter().position(|r| r.contains(needle));
    // The native knobs carry their own separator group header, like the
    // llama.cpp groups, and the free-text extras row is last of all.
    let header = row_of("ds4 native").expect("ds4 native group header");
    let ssd = row_of("SSD streaming").expect("a ds4 native knob row");
    let extras = row_of("extras").expect("extras row");
    assert!(header < ssd, "group header precedes its knobs");
    assert!(ssd < extras, "extras comes after the ds4 native knobs");
  }

  #[test]
  fn ds4_running_view_shows_native_knobs_not_llamacpp() {
    use crate::tui::app::ManagedRow;
    use ratatui::text::Line;
    let app = App::new(AppOptions::default());
    let path = PathBuf::from("/m/DeepSeek-V4-Flash.gguf");
    // The running row carries the launch's live native knobs (from `status`)
    // and the resolved `ds4` backend tag.
    let mut backend_knobs = std::collections::BTreeMap::new();
    backend_knobs.insert("ssd_streaming".into(), KnobValue::Set("true".into()));
    backend_knobs.insert("power".into(), KnobValue::Set("80".into()));
    let m = ManagedRow {
      launch_id: "L1".into(),
      path,
      port: 41100,
      backend_knobs,
      backend: Some("ds4".into()),
      ..Default::default()
    };
    let palette = app.palette();
    let mut lines: Vec<Line<'static>> = Vec::new();
    push_native_readonly_rows(
      &mut lines,
      &m,
      crate::backend::ds4::DS4_NATIVE_KNOBS,
      Some(32768),
      palette,
      true,
    );
    let text: String = lines
      .iter()
      .flat_map(|l| l.spans.iter())
      .map(|s| s.content.as_ref())
      .collect::<Vec<_>>()
      .join(" ");
    // The six ds4 native knobs render with their recorded values…
    assert!(text.contains("ds4 native"), "{text}");
    assert!(
      text.contains("SSD streaming") && text.contains("true"),
      "{text}"
    );
    assert!(
      text.contains("GPU power %") && text.contains("80"),
      "{text}"
    );
    assert!(
      text.contains("CPU threads") && text.contains("inherited"),
      "{text}"
    );
    // …the resolved ctx shows, but no llama.cpp typed knob leaks into it.
    assert!(text.contains("32768"), "{text}");
    assert!(!text.contains("n_gpu_layers"), "{text}");
    assert!(!text.contains("flash_attn"), "{text}");
  }

  #[test]
  fn source_chip_shows_at_40_cols_hidden_at_39() {
    use crate::tui::app::LastParamsRow;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    // A user-set ctx earns a `(user)` source chip on its row, which is
    // the marker we assert on. The right pane in wide mode is only ~35%
    // of the terminal, so the chip gate must trip well below 50 cols.
    let render_at = |w: u16| -> String {
      let mut app = App::new(AppOptions::default());
      let path = PathBuf::from("/m/qwen.gguf");
      app.models = vec![fake_model("/m/qwen.gguf", "/m")];
      app.last_params.insert(
        path,
        LastParamsRow {
          ctx: Some(16384),
          reasoning: false,
          knobs: crate::config::TypedKnobs {
            ctx: Some(KnobValue::Set(16384)),
            ..Default::default()
          },
          backend_knobs: Default::default(),
          extras: vec![],
          port: Some(41100),
          server: None,
        },
      );
      app.list_cursor = 2;
      let palette = app.palette();
      let mut term = Terminal::new(TestBackend::new(w, 32)).unwrap();
      term
        .draw(|f| render(f, Rect::new(0, 0, w, 32), &app, palette))
        .unwrap();
      let buf = term.backend().buffer().clone();
      let mut joined = String::new();
      for y in 0..buf.area.height {
        for x in 0..buf.area.width {
          joined.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        joined.push('\n');
      }
      joined
    };

    let wide = render_at(40);
    assert!(
      wide.contains("(user)"),
      "source chip must show at 40 cols: {wide}"
    );
    let narrow = render_at(39);
    assert!(
      !narrow.contains("(user)"),
      "source chip must be hidden below 40 cols: {narrow}"
    );
  }

  #[test]
  fn running_view_shows_resolved_ctx_and_dispatched_auto_knobs() {
    use crate::config::{KnobValue, TypedKnobs};
    use crate::tui::app::ManagedRow;
    use crate::tui::status_icons::SurfaceState;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    let path = PathBuf::from("/m/gemma.gguf");
    app.models = vec![fake_model("/m/gemma.gguf", "/m")];
    // The managed row carries the *dispatched* knobs (what the server is
    // running with) — all `auto` for an all-auto launch — plus the ctx
    // `--fit` actually resolved, read from `/props`. `last_params` is
    // deliberately left empty to prove the running view reads the live
    // dispatch, not the saved delta.
    app.managed = vec![ManagedRow {
      launch_id: "L1".into(),
      path: path.clone(),
      port: 41101,
      state: SurfaceState::Ready,
      resolved_ctx: Some(262144),
      knobs: TypedKnobs {
        ctx: Some(KnobValue::Auto),
        parallel: Some(KnobValue::Auto),
        threads: Some(KnobValue::Auto),
        ..Default::default()
      },
      ..Default::default()
    }];
    // Row 0 header, row 1 `▶ Running`, row 2 the running launch.
    app.list_cursor = 2;
    assert!(app.launch_picker.is_none());
    assert!(
      app.focused_managed().is_some(),
      "cursor must land on the launch"
    );
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(70, 40)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 70, 40), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    // ctx shows the resolved number, NOT `default` and NOT `auto`.
    assert!(joined.contains("262144"), "resolved ctx missing: {joined}");
    // A fit-delegated knob reads `auto` (from the dispatched knobs),
    // never `default` — even though `last_params` is empty.
    let threads_line = joined.lines().find(|l| l.contains("threads")).unwrap_or("");
    assert!(
      threads_line.contains("auto"),
      "dispatched auto knob should read auto, not default: {threads_line:?}"
    );
    let parallel_line = joined
      .lines()
      .find(|l| l.contains("parallel"))
      .unwrap_or("");
    assert!(
      parallel_line.contains("auto"),
      "parallel is fit-delegated and not read back, so it reads auto: {parallel_line:?}"
    );
  }

  #[test]
  fn launch_hint_reads_press_enter_again_to_launch() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/qwen.gguf", "/m")];
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(70, 36)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 70, 36), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    use crate::tui::keybindings::ENTER_LABEL;
    let expected = format!("{ENTER_LABEL} again to launch with these settings.");
    assert!(joined.contains(&expected), "{joined}");
  }
}
