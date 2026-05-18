//! Settings tab — launch-parameter form for the focused model.
//!
//! MVP rendering: shows the editable form fields when no managed
//! launch exists for the focused model, or the live params when a
//! launch is running (read-only). The form's actual field-editing
//! plumbing still flows through `LaunchPickerState` for now; this
//! renderer just paints its current contents inline in the right
//! pane instead of as a centred overlay.

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

  // If a managed launch is running, show what params were used.
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
        "Stop this launch with `s` from the model list to re-launch with new settings.",
        Style::default().fg(palette.muted),
      )
      .into(),
    );
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    return;
  }

  // Otherwise show the editable form. When the user hasn't opened
  // the picker yet, render its would-be-default contents so they
  // can see what pressing Enter will dispatch.
  let picker_view = app.launch_picker.clone().unwrap_or_else(|| {
    let name = app.focused_name().unwrap_or_else(|| "(none)".into());
    LaunchPickerState::for_model(name)
  });
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
    palette,
  ));
  lines.push(kv_focused(
    "reasoning",
    if picker_view.reasoning { "on" } else { "off" }.into(),
    picker_view.field == PickerField::Reasoning,
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
    palette,
  ));
  lines.push(Line::default());
  let launch_chip = app
    .hint_with(Focus::RightPane, Action::Submit, "launch")
    .map(|chip| format!("Press {} to launch with these settings.", chip_label(&chip)))
    .unwrap_or_else(|| "Launch binding removed — set `submit` in config.".to_string());
  lines.push(
    Span::styled(
      if no_focus {
        "Select a model in the list to configure launch settings.".to_string()
      } else {
        launch_chip
      },
      Style::default().fg(palette.muted),
    )
    .into(),
  );

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

fn kv(label: &str, value: String, palette: &Palette) -> Line<'static> {
  Line::from(vec![
    Span::styled(format!("  {label:<10}"), Style::default().fg(palette.muted)),
    Span::styled(value, Style::default().fg(palette.fg)),
  ])
}

/// Strip the `:description` suffix off a chip string, leaving just
/// the key label (e.g. `"a:advanced"` → `"a"`). Used by inline
/// hints that want a bare keycap, not a full `key:label` chip.
fn chip_label(chip: &str) -> &str {
  chip.split(':').next().unwrap_or(chip)
}

fn kv_focused(label: &str, value: String, focused: bool, palette: &Palette) -> Line<'static> {
  let marker = if focused { "→ " } else { "  " };
  let style = if focused {
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD)
  } else {
    Style::default().fg(palette.muted)
  };
  Line::from(vec![
    Span::styled(format!("{marker}{label:<8}"), style),
    Span::styled(value, Style::default().fg(palette.fg)),
  ])
}
