//! `Alt+⏎` "launch as…" modal: name the focused model before launching it.
//!
//! A single-stage dialog modelled on the save-preset dialog — the `Name`
//! stage without the `Confirm` stage. `Esc` cancels, `Enter` accepts. On
//! accept the normal launch picker opens carrying the typed name, so the
//! launch is addressable as `<model-id>@<name>`. A plain `⏎` on the list
//! still launches unnamed, exactly as before.

use std::path::PathBuf;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::input_field::InputField;
use crate::tui::keybindings::{Action as KeyAction, Focus, ALT_ENTER_LABEL, ESC_LABEL};

fn keymap_label(app: &App, action: KeyAction, fallback: &str) -> String {
  app.resolve_label(Focus::ConfirmPopup, action, fallback)
}

/// State for the `Alt+⏎` launch-name modal.
#[derive(Debug, Clone)]
pub struct LaunchNameDialog {
  /// Canonical path of the model being launched.
  pub model_path: PathBuf,
  /// Display name shown in the dialog title.
  pub model_name: String,
  /// The name-entry field.
  pub input: InputField,
  /// Inline validation error (empty name), rendered under the input.
  pub error: Option<String>,
}

/// The focused model the `Alt+⏎` dialog opens with.
pub struct LaunchNameArgs {
  pub model_path: PathBuf,
  pub model_name: String,
}

impl LaunchNameDialog {
  /// Open the dialog with the input ready for typing.
  pub fn open(args: LaunchNameArgs) -> Self {
    let mut input = InputField::new();
    input.enter_edit();
    Self {
      model_path: args.model_path,
      model_name: args.model_name,
      input,
      error: None,
    }
  }

  /// The trimmed launch name as typed. Empty when nothing was typed.
  pub fn name(&self) -> String {
    self.input.buffer().trim().to_string()
  }
}

/// Render the modal centred over the TUI.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  dialog: &LaunchNameDialog,
  palette: &Palette,
) {
  let submit = keymap_label(app, KeyAction::LaunchNamed, ALT_ENTER_LABEL);
  let cancel = keymap_label(app, KeyAction::Cancel, ESC_LABEL);
  let rect = crate::tui::layout::centered_abs(area, 60, 9, 4, 2);
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  let tone = palette.accent;
  let block = palette
    .panel()
    .title(Line::from(Span::styled(
      format!(" Launch as · {} ", dialog.model_name),
      Style::default().fg(tone).add_modifier(Modifier::BOLD),
    )))
    .border(tone)
    .padding(ratatui::widgets::Padding::horizontal(1))
    .build();
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1), // prompt
      Constraint::Length(1), // input
      Constraint::Length(1), // error / spacer
      Constraint::Min(1),    // hint
    ])
    .split(inner);

  let prompt = Paragraph::new(Line::from(Span::styled(
    "Name this launch (optional):",
    palette.text_style(),
  )));
  frame.render_widget(prompt, chunks[0]);

  let input_line = Paragraph::new(Line::from(vec![
    Span::styled("› ", Style::default().fg(tone)),
    Span::styled(dialog.input.buffer(), palette.text_style()),
    Span::styled("▏", Style::default().fg(tone)),
  ]));
  frame.render_widget(input_line, chunks[1]);

  if let Some(err) = &dialog.error {
    let e = Paragraph::new(Line::from(Span::styled(
      err.clone(),
      Style::default().fg(palette.error),
    )));
    frame.render_widget(e, chunks[2]);
  }

  let hint = Paragraph::new(Line::from(vec![
    Span::styled(submit, Style::default().fg(palette.success)),
    Span::styled(" launch  ·  ", palette.muted_style()),
    Span::styled(cancel, Style::default().fg(palette.warning)),
    Span::styled(" cancel", palette.muted_style()),
  ]))
  .alignment(Alignment::Center);
  frame.render_widget(hint, chunks[3]);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn dialog() -> LaunchNameDialog {
    LaunchNameDialog::open(LaunchNameArgs {
      model_path: PathBuf::from("/m/a.gguf"),
      model_name: "a.gguf".into(),
    })
  }

  #[test]
  fn opens_with_editing_input() {
    let d = dialog();
    assert!(d.input.is_editing());
    assert!(d.error.is_none());
  }

  #[test]
  fn name_is_trimmed() {
    let mut d = dialog();
    d.input.set_text("  coder  ");
    assert_eq!(d.name(), "coder");
    d.input.set_text("   ");
    assert_eq!(d.name(), "");
  }
}
