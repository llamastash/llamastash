//! Settings tab — typed-knob launch editor for the focused model.
//!
//! Renders a vertical list of rows: `ctx`, `reasoning`, every
//! `crate::launch::knobs::KnobSet` field with a per-row source label, and an `extras`
//! free-text row at the bottom. When the focused model has a
//! running launch and the picker isn't open, shows the live params
//! (read-only).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

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

  // Every knob flows through the same `value (chip)` row shape in both views.
  // The read-only view shows the *dispatched* values (`auto` for a fit-delegated
  // row), with ctx overlaid by the `--fit`-resolved window read from `/props`;
  // no chip, since a live value has no inheritance layer to name.
  let resolved_ctx = managed.map(|m| {
    m.resolved_ctx.or_else(|| {
      let id = m
        .backend
        .as_deref()
        .unwrap_or(crate::backend::DEFAULT_BACKEND_ID);
      crate::launch::knobs::def_for_backend_concept(
        id,
        crate::launch::knobs::Concept::ContextLength,
      )
      .and_then(|d| m.knobs.u32(d.knob_id()))
    })
  });

  // Rows are generated from whichever backend is in scope: the picker's active
  // one while editing, the launch's *actual* backend when read-only (not the
  // routing prediction, so a compatible file launched with `--backend llamacpp`
  // shows the llama.cpp knobs it ran with). Names no backend.
  let groups: Vec<(
    crate::launch::knobs::Group,
    Vec<&'static crate::launch::knobs::KnobDef>,
  )> = match picker_view {
    Some(pv) => pv.visible_groups(),
    None => {
      let m = managed.expect("read-only view implies a managed row");
      let id = m
        .backend
        .as_deref()
        .unwrap_or(crate::backend::DEFAULT_BACKEND_ID);
      // The read-only view answers the same group gates the editor does, from
      // the running launch rather than the form: a placement group is noise on
      // a one-GPU host either way, and so is a speculation group on a model
      // that cannot speculate.
      let multi = app.multi_device();
      let speculates = app.mtp_capable_for(&m.path);
      crate::launch::knobs::registry::grouped_for_backend(id)
        .into_iter()
        .filter(|(g, _)| match g.gate() {
          None => true,
          Some(crate::launch::knobs::GroupGate::MultiDevice) => multi,
          Some(crate::launch::knobs::GroupGate::SpeculationCapable) => speculates,
        })
        .collect()
    }
  };

  for (group, defs) in groups {
    lines.push(group_header(group.title(), palette));
    for def in defs {
      let id = def.knob_id();
      match picker_view {
        Some(pv) => {
          let field = PickerField::Knob(id);
          let focused = pv.field == field;
          if focused {
            focused_line = Some(lines.len() as u16);
          }
          if pv.inline_edit.is_open() && pv.inline_edit.field == Some(field) {
            lines.push(inline_edit_row(
              def.id,
              pv.inline_edit.input.buffer(),
              focused,
              palette,
            ));
            if let Some(err) = &pv.inline_edit.error {
              lines.push(inline_warning_row(err, palette));
            }
          } else {
            lines.push(crate::tui::fmt::kv_row_focused(
              def.id,
              pv.value_label(id),
              Some(pv.source_for(id).label()),
              focused,
              // A bool has no ring to declare but always toggles.
              def.kind == crate::launch::knobs::KnobKind::Bool
                || def.ring() != crate::launch::knobs::Ring::None,
              palette,
              show_source,
            ));
          }
        }
        None => {
          let m = managed.expect("read-only view implies a managed row");
          let is_ctx = def.concept == Some(crate::launch::knobs::Concept::ContextLength);
          let value = match (is_ctx, resolved_ctx.flatten()) {
            // Flag a memory-driven clamp so the user knows the window was
            // squeezed to the floor, not chosen freely.
            (true, Some(v)) if m.ctx_clamped => format!("{v} · clamped to floor"),
            (true, Some(v)) => v.to_string(),
            _ => format_persisted_knob_value(&m.knobs, id),
          };
          // Not focused, not cyclable, no source chip — renders as a plain
          // `label  value` row through the shared formatter.
          lines.push(crate::tui::fmt::kv_row_focused(
            def.id,
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

  // The extras row — always last.
  match picker_view {
    Some(pv) => {
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

/// A persisted knob's value as the read-only running view renders it.
///
/// Same spellings the editor uses, so one knob does not read `on` in the form
/// and `true` two lines away in the running view.
fn format_persisted_knob_value(
  knobs: &crate::launch::knobs::KnobSet,
  id: crate::launch::knobs::KnobId,
) -> String {
  match knobs.get(id) {
    Some(crate::launch::knobs::KnobValue::Auto) => crate::launch::knobs::AUTO_TOKEN.to_string(),
    Some(crate::launch::knobs::KnobValue::Set(crate::launch::knobs::Scalar::Bool(b))) => {
      if *b { "on" } else { "off" }.to_string()
    }
    Some(crate::launch::knobs::KnobValue::Set(v)) => v.to_arg(),
    None => INHERITED_LABEL.to_string(),
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
      format!(
        "{marker}{label:<width$}",
        width = crate::tui::fmt::kv_label_width()
      ),
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
      mtp_head: None,
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
        knobs: crate::knobset! {
          ctx: 16384,
          reasoning: true
        },
        extras: vec!["--rope-freq-base".into(), "10000".into()],
        port: Some(41100),
        mtp: Default::default(),
        ..Default::default()
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
  fn a_non_default_backends_rows_render_under_the_shared_group_headers() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    let mut picker = LaunchPickerState::for_model("DeepSeek-V4-Flash");
    picker.model_backend = crate::launch::params::BackendChoice::Explicit("ds4".into());
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
    // ds4's own tunables are ordinary rows now: they sit under the same group
    // headers llama.cpp's do, keyed by the flag ds4 itself takes, with the
    // free-text extras row last of all.
    let header = row_of(crate::launch::knobs::Group::Memory.title())
      .expect("the shared group header ds4's memory knobs declare");
    let ssd = row_of("ssd-streaming").expect("a ds4 knob row");
    let extras = row_of("extras").expect("extras row");
    assert!(header < ssd, "group header precedes its knobs");
    assert!(ssd < extras, "extras comes last");
    // A llama.cpp-only knob has no row here — the editor renders what *this*
    // backend declared, nothing more.
    assert!(row_of("n-gpu-layers").is_none());
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
          knobs: crate::knobset! {
            ctx: 16384
          },
          extras: vec![],
          port: Some(41100),
          mtp: Default::default(),
          ..Default::default()
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
