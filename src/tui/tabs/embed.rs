//! Embed tab — call `/v1/embeddings` on the focused model and
//! show the result's dimensionality + first eight values + L2 norm.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus};
use crate::tui::tabs::input_pane::{self, InputPaneOpts, PromptField};

#[derive(Debug, Default)]
pub struct EmbedTabState {
  pub input: String,
  pub dim: Option<usize>,
  pub preview: Vec<f64>,
  pub norm: Option<f64>,
  pub last_error: Option<String>,
  pub busy: bool,
  /// Receiver fed by the background `oai_client::embed` task. The
  /// render loop drains it via `try_recv` so a slow `/v1/embeddings`
  /// call never blocks input.
  pub pending: Option<tokio::sync::mpsc::UnboundedReceiver<crate::tui::tabs::TabEvent>>,
}

impl EmbedTabState {
  pub fn record(&mut self, result: crate::tui::oai_client::EmbedResult) {
    self.dim = Some(result.dim);
    self.preview = result.preview;
    self.norm = Some(result.norm);
    self.last_error = None;
    self.busy = false;
  }

  pub fn record_error(&mut self, msg: String) {
    self.last_error = Some(msg);
    self.busy = false;
  }
}

/// Render the Embed tab body into `area`. Block borders are owned
/// by the right pane caller.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let state = &app.embed;
  let active = app.focus == Focus::EmbedInput;

  let mut body: Vec<Line<'_>> = Vec::new();
  if let Some(dim) = state.dim {
    body.push(Line::from(Span::styled(
      format!("dim = {dim}"),
      Style::default().fg(palette.fg),
    )));
    if !state.preview.is_empty() {
      let preview = state
        .preview
        .iter()
        .map(|v| format!("{v:+.4}"))
        .collect::<Vec<_>>()
        .join(", ");
      body.push(Line::from(Span::styled(
        format!("first8 = [{preview}]"),
        Style::default().fg(palette.muted),
      )));
    }
    if let Some(n) = state.norm {
      body.push(Line::from(Span::styled(
        format!("L2 norm = {n:.4}"),
        Style::default().fg(palette.muted),
      )));
    }
  } else {
    body.push(Line::from(Span::styled(
      "Embed the input with Enter.",
      Style::default().fg(palette.muted),
    )));
  }

  let status = match (state.busy, &state.last_error) {
    (true, _) => Line::from(Span::styled(
      "calling /v1/embeddings…",
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
    )),
    (_, Some(err)) => Line::from(Span::styled(
      format!("error: {err}"),
      Style::default().fg(palette.error),
    )),
    _ => input_pane::idle_status_line(&idle_status_chips(app, active), palette),
  };

  let prompt = PromptField {
    title: "Input",
    text: &state.input,
    active,
  };
  input_pane::render(
    frame,
    area,
    InputPaneOpts {
      prompts: &[prompt],
      body,
      status,
      bold_body: false,
    },
    palette,
  );
}

/// Chip strip for the idle status line. Mirrors Chat's chip
/// strategy: `Shift+Enter:newline` is always available; the
/// trailing chip is `Esc:clear` when the input is focused and
/// `e:edit` when navigation focus has the right pane.
pub(crate) fn idle_status_chips(app: &App, input_active: bool) -> Vec<String> {
  let mut chips: Vec<String> = Vec::with_capacity(2);
  if let Some(c) = app.hint(Focus::EmbedInput, Action::InsertNewline) {
    chips.push(c);
  }
  let trailing = if input_active {
    app.hint_with(Focus::EmbedInput, Action::ExitEdit, "clear")
  } else {
    app.hint(Focus::RightPane, Action::EnterEdit)
  };
  if let Some(c) = trailing {
    chips.push(c);
  }
  chips
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crate::tui::keybindings::KeyMap;
  use crate::tui::oai_client::EmbedResult;
  use std::collections::BTreeMap;

  #[test]
  fn record_overrides_previous_error() {
    let mut s = EmbedTabState {
      last_error: Some("stale".into()),
      ..Default::default()
    };
    s.record(EmbedResult {
      dim: 1024,
      preview: vec![0.0; 8],
      norm: 1.0,
    });
    assert_eq!(s.dim, Some(1024));
    assert!(s.last_error.is_none());
  }

  #[test]
  fn idle_chips_when_input_active_use_keymap_labels() {
    let app = App::new(AppOptions::default());
    let chips = idle_status_chips(&app, true);
    assert_eq!(
      chips,
      vec!["Shift+Enter:newline".to_string(), "Esc:clear".to_string(),]
    );
  }

  #[test]
  fn idle_chips_when_input_inactive_swap_clear_for_edit() {
    let app = App::new(AppOptions::default());
    let chips = idle_status_chips(&app, false);
    assert_eq!(chips.last().map(String::as_str), Some("e:edit"));
  }

  #[test]
  fn idle_chips_pick_up_config_keybinding_overrides() {
    // Rebind insert_newline to alt+enter — Embed chip must follow.
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> =
      [(String::from("insert_newline"), String::from("alt+enter"))]
        .into_iter()
        .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let chips = idle_status_chips(&app, true);
    assert!(
      chips.iter().any(|c| c == "Alt+Enter:newline"),
      "Alt+Enter binding missing: {chips:?}"
    );
  }
}
