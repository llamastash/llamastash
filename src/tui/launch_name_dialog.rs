//! `Alt+⏎` "launch as…" modal: name a launch before the picker dispatches it.
//!
//! A single-stage dialog modelled on the save-preset dialog — the `Name`
//! stage without the `Confirm` stage. `Esc` cancels, `Enter` accepts. On
//! accept the picker that is already open launches carrying the typed name,
//! so it is addressable as `<model-id>@<name>`. A plain `⏎` on the picker
//! still launches unnamed, exactly as before.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::input_field::InputField;
use crate::tui::keybindings::{Action as KeyAction, Focus, ENTER_LABEL, ESC_LABEL};

/// State for the `Alt+⏎` launch-name modal.
#[derive(Debug, Clone)]
pub struct LaunchNameDialog {
  /// Display name shown in the dialog title.
  pub model_name: String,
  /// The name-entry field.
  pub input: InputField,
  /// Inline validation error (a name the `--name` flag would refuse too),
  /// rendered under the input. Nothing typed at all is not an error — it
  /// launches unnamed.
  pub error: Option<String>,
}

impl LaunchNameDialog {
  /// Open the dialog with the input ready for typing.
  pub fn open(model_name: String) -> Self {
    let mut input = InputField::new();
    input.enter_edit();
    Self {
      model_name,
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
  // Each label resolves under the focus its action is actually scoped to, so a
  // user's `keybindings:` override shows up here. The field submits on a bare
  // `⏎` — `Alt+⏎` is what *opened* this dialog and cannot accept it — so the
  // hint carries the picker's own launch key.
  let submit = app.resolve_label(Focus::RightPane, KeyAction::Submit, ENTER_LABEL);
  let cancel = app.resolve_label(Focus::ConfirmPopup, KeyAction::Cancel, ESC_LABEL);
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
    LaunchNameDialog::open("a.gguf".into())
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
