//! Settings tab — launch-parameter form for the focused model.
//!
//! Renders the editable form fields when no managed launch exists
//! for the focused model, or the live params when a launch is
//! running (read-only). Field-editing state lives in
//! `LaunchPickerState` — round-6 dropped the centred picker overlay
//! and these helpers paint the same state inline in the right pane.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus};
use crate::tui::launch_picker::{LaunchPickerState, PickerField};

/// Render the Settings tab body into `area`. The caller (right
/// pane) owns the surrounding block.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let mut lines: Vec<Line<'_>> = Vec::new();

  // If the user explicitly opened the picker (via Enter on a row or
  // the launch chip), show the editable form even when a managed
  // launch already exists for the focused path — they're staging a
  // *new* launch (v1 supports duplicate instances on fresh ports).
  // The duplicate-launch heads-up rendered below keys off
  // `picker_view.active_instances`.
  //
  // If a managed launch is running and the picker isn't open, show
  // what params were used.
  if app.launch_picker.is_none() {
    if let Some(m) = app.focused_managed() {
      lines.push(heading("Running launch", palette));
      lines.push(kv("launch", m.launch_id.clone(), palette));
      lines.push(kv("port", format!(":{}", m.port), palette));
      lines.push(kv(
        "state",
        crate::tui::status_icons::label_for(m.state).to_string(),
        palette,
      ));
      if let Some(rss) = m.rss_bytes {
        lines.push(kv("rss", crate::tui::fmt::format_bytes(rss), palette));
      }
      if let Some(cpu) = m.cpu_pct {
        lines.push(kv("cpu", format!("{cpu:.0}%"), palette));
      }
      // Surface the last-known launch parameters (ctx, reasoning,
      // advanced argv) when the daemon's last_params_list snapshot
      // covers this model. Falls back to "—" rows so the user still
      // sees the field labels and knows the slot exists.
      let last = app.last_params.get(&m.path);
      lines.push(Line::default());
      lines.push(heading("Launch params", palette));
      let ctx_value = last
        .and_then(|p| p.ctx)
        .map(|c| c.to_string())
        .unwrap_or_else(|| "native".into());
      lines.push(kv("ctx", ctx_value, palette));
      let reasoning_value = last.map(|p| p.reasoning).unwrap_or(false);
      lines.push(kv(
        "reasoning",
        if reasoning_value { "on" } else { "off" }.into(),
        palette,
      ));
      let advanced: String = last
        .map(|p| p.advanced.join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into());
      lines.push(kv("advanced", advanced, palette));
      lines.push(Line::default());
      lines.push(
        Span::styled(
          "Stop this launch with `s` to re-launch with new settings.",
          palette.muted_style(),
        )
        .into(),
      );
      frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
      return;
    }
  }

  // Otherwise show the editable form. When the user hasn't opened
  // the picker yet, derive the would-be-default contents from
  // `App::build_default_picker` so the form already reflects
  // persisted `last_params` (ctx / reasoning / preset) on first
  // landing — without this, ctx and reasoning silently read as
  // `native` / `model default` until the user taps an arrow or
  // presses Enter, which surprised users who expected the last-
  // used values to show up immediately on selection.
  // Audit §F4.1 #1: borrow the live picker when one exists so the
  // Settings tab doesn't pay a `LaunchPickerState::clone()` per
  // frame; only materialise the default when the picker is absent.
  let default_picker: LaunchPickerState;
  let picker_view: &LaunchPickerState = match app.launch_picker.as_ref() {
    Some(p) => p,
    None => {
      default_picker = app.build_default_picker().unwrap_or_else(|| {
        let name = app.focused_name().unwrap_or_else(|| "(none)".into());
        LaunchPickerState::for_model(name)
      });
      &default_picker
    }
  };
  let no_focus = app.focused_path().is_none();

  lines.push(heading(
    if no_focus {
      "No model focused"
    } else {
      "Launch settings"
    },
    palette,
  ));
  lines.push(kv("model", picker_view.model_name.clone(), palette));
  let ctx_value = match picker_view.ctx {
    Some(n) => format!("{n}"),
    None => "native (GGUF default)".to_string(),
  };
  lines.push(kv_focused(
    "ctx",
    ctx_value,
    picker_view.field == PickerField::Ctx,
    true,
    palette,
  ));
  lines.push(kv_focused(
    "reasoning",
    picker_view.reasoning.label().to_string(),
    picker_view.field == PickerField::Reasoning,
    true,
    palette,
  ));
  let advanced_hint = app
    .hint(Focus::RightPane, Action::OpenAdvancedPanel)
    .map(|chip| format!("(open with `{}`)", chip_label(&chip)))
    .unwrap_or_else(|| "(advanced binding removed)".to_string());
  lines.push(kv_focused(
    "advanced",
    advanced_hint,
    picker_view.field == PickerField::Advanced,
    // Advanced opens a separate editor; Up/Down don't cycle a
    // value here, so the `◀ … ▶` glyphs would be misleading.
    false,
    palette,
  ));
  lines.push(Line::default());
  // Heads-up when a launch already exists for this model. The
  // picker still happily spawns another instance on a fresh port —
  // duplicate launches are a v1 feature — but the user shouldn't
  // be surprised by it.
  if picker_view.active_instances > 0 {
    lines.push(
      Span::styled(
        format!(
          "⚠ {n} instance{plural} already running — Enter launches a new one on a fresh port",
          n = picker_view.active_instances,
          plural = if picker_view.active_instances == 1 {
            ""
          } else {
            "s"
          }
        ),
        Style::default()
          .fg(palette.warning)
          .add_modifier(Modifier::BOLD),
      )
      .into(),
    );
    lines.push(Line::default());
  }
  let launch_chip = app
    .hint_with(Focus::RightPane, Action::Submit, "launch")
    .map(|chip| {
      format!(
        "Press {} again to launch with these settings.",
        chip_label(&chip)
      )
    })
    .unwrap_or_else(|| "Launch binding removed — set `submit` in config.".to_string());
  lines.push(
    Span::styled(
      if no_focus {
        "Select a model in the list to configure launch settings.".to_string()
      } else {
        launch_chip
      },
      palette.muted_style(),
    )
    .into(),
  );

  // Round-9: the in-body chip strip moved to the right pane's
  // bottom border. Settings here owns its prose and form rows; the
  // bottom border owns the contextual key hints. Saves a row of
  // vertical space and stops duplicate-chip noise.

  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn heading<'a>(text: &'a str, palette: &Palette) -> Line<'a> {
  Line::from(Span::styled(
    text,
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD),
  ))
}

/// Width of the label column in `kv` / `kv_focused` rows. Wide
/// enough to fit the longest label (`reasoning`, `advanced`, `ctx`,
/// `model`) plus one trailing space gap so values never kiss the
/// label.
const LABEL_W: usize = 12;

fn kv(label: &str, value: String, palette: &Palette) -> Line<'static> {
  Line::from(vec![
    Span::styled(
      format!("  {label:<width$}", width = LABEL_W),
      palette.muted_style(),
    ),
    Span::styled(value, palette.text_style()),
  ])
}

/// Strip the `:description` suffix off a chip string, leaving just
/// the key label (e.g. `"a:advanced"` → `"a"`). Used by inline
/// hints that want a bare keycap, not a full `key:label` chip.
fn chip_label(chip: &str) -> &str {
  chip.split(':').next().unwrap_or(chip)
}

/// Render an editable form row. When focused **and** the field is
/// cyclable (`cyclable = true`), the value is wrapped in `◀ … ▶`
/// so the user sees that Up/Down change it. Non-cyclable focused
/// rows (Advanced) just get the `→` cursor without arrow glyphs.
fn kv_focused(
  label: &str,
  value: String,
  focused: bool,
  cyclable: bool,
  palette: &Palette,
) -> Line<'static> {
  let marker = if focused { "→ " } else { "  " };
  let label_style = if focused {
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD)
  } else {
    palette.muted_style()
  };
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
  spans.push(Span::styled(
    format!("{marker}{label:<width$}", width = LABEL_W),
    label_style,
  ));
  if focused && cyclable {
    spans.push(Span::styled("◀ ".to_string(), palette.accent_style()));
    spans.push(Span::styled(value, palette.text_style()));
    spans.push(Span::styled(" ▶".to_string(), palette.accent_style()));
  } else {
    spans.push(Span::styled(value, palette.text_style()));
  }
  Line::from(spans)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::{App, AppOptions};
  use std::path::PathBuf;

  fn fake_model(path: &str, parent: &str) -> crate::discovery::DiscoveredModel {
    crate::discovery::DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }
  }

  #[test]
  fn settings_form_reflects_last_params_on_first_render() {
    // User-reported bug: ctx / reasoning fields rendered as the
    // hard defaults (`native`, `model default`) until the user
    // pressed Enter or an arrow to materialise the picker. The
    // Settings tab now derives its default-render contents from
    // `App::build_default_picker`, which seeds from persisted
    // `last_params` — so the form shows the user's previous choices
    // the moment they highlight the model, without interaction.
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
        advanced: vec!["--flash-attn".into()],
        port: Some(41100),
      },
    );
    app.list_cursor = 2;
    // No picker staged; no arrow key pressed.
    assert!(app.launch_picker.is_none());
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 24), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    assert!(
      joined.contains("16384"),
      "ctx must reflect persisted last_params on first render: {joined}"
    );
    assert!(
      joined.contains("on"),
      "reasoning must reflect persisted last_params on first render: {joined}"
    );
  }

  #[test]
  fn launch_hint_reads_press_enter_again_to_launch() {
    // The Settings tab confirms the two-step launch flow ("Enter
    // first stages the picker, Enter again dispatches") via the
    // muted hint line under the form. Tab → Enter must read like
    // a *re-press*, not the first press.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/qwen.gguf", "/m")];
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 20), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    assert!(
      joined.contains("Enter again to launch with these settings."),
      "launch hint must read 'Enter again to launch': {joined}"
    );
  }
}
