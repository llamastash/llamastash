//! `Ctrl+P` "save current launch settings as a preset" dialog.
//!
//! A small two-stage modal that captures a running model's live launch
//! knobs and saves them to `config.yaml` as a named preset via
//! `presets_save`. Stage `Name` prompts for the preset name; if that name
//! already resolves for the model, stage `Confirm` asks before either
//! overwriting the model's own preset or shadowing an arch preset with a
//! new per-model override.
//!
//! Auto / inherited markers ride through untouched: the captured
//! [`crate::launch::knobs::KnobSet`] keeps each knob's `Set` / `Auto` /
//! inherited state, so a `ctx: auto` saved here round-trips as `auto`.

use std::path::PathBuf;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::input_field::InputField;
use crate::tui::keybindings::{Action as KeyAction, Focus};

/// Which step of the save flow is in view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveStage {
  /// Typing the preset name.
  Name,
  /// The typed name already resolves — confirm an overwrite or a shadow.
  Confirm,
}

/// State for the `Ctrl+P` save-preset modal.
#[derive(Debug, Clone)]
pub struct SavePresetDialog {
  /// Canonical path of the model the preset is saved for.
  pub model_path: PathBuf,
  /// Display name shown in the dialog title.
  pub model_name: String,
  /// Captured launch knobs (ctx/reasoning folded in; Auto preserved).
  pub knobs: crate::launch::knobs::KnobSet,
  /// Captured native (per-backend) knobs — the six ds4 tunables when the
  /// captured launch is ds4-backed, so the preset stores them too. Empty for
  /// The knobs the dialog will write into the preset entry.
  /// Captured extras argv tail.
  pub extras: Vec<String>,
  /// Backend this preset pins — launch *identity*, not a knob. `None` when
  /// the capture source had no resolved backend (an untagged / default launch),
  /// so the daemon re-derives it rather than storing a guess.
  pub backend: Option<String>,
  /// Server (build/binary) this preset pins — identity, like `backend`.
  /// `None` when the launch took the backend's default server.
  pub server: Option<String>,
  /// The model's own (per-model) preset names — a save under one of these
  /// is a true overwrite.
  pub existing: Vec<String>,
  /// Names defined only by an arch preset — a per-model save shadows them
  /// (creates an override) rather than overwriting; the arch entry stays.
  pub arch_shadow: Vec<String>,
  /// The name-entry field.
  pub input: InputField,
  pub stage: SaveStage,
  /// Inline validation error (empty name), rendered under the input.
  pub error: Option<String>,
}

impl SavePresetDialog {
  /// Open the dialog at the name stage with the input ready for typing.
  #[allow(clippy::too_many_arguments)] // capture surface is inherently wide
  pub fn open(
    model_path: PathBuf,
    model_name: String,
    knobs: crate::launch::knobs::KnobSet,
    extras: Vec<String>,
    backend: Option<String>,
    server: Option<String>,
    existing: Vec<String>,
    arch_shadow: Vec<String>,
  ) -> Self {
    let mut input = InputField::new();
    input.enter_edit();
    Self {
      model_path,
      model_name,
      knobs,
      extras,
      backend,
      server,
      existing,
      arch_shadow,
      input,
      stage: SaveStage::Name,
      error: None,
    }
  }

  /// The trimmed preset name as typed.
  pub fn name(&self) -> String {
    self.input.buffer().trim().to_string()
  }

  /// Whether the typed name overwrites one of the model's **own** presets.
  pub fn name_exists(&self) -> bool {
    self.existing.contains(&self.name())
  }

  /// Whether the typed name matches only an arch preset — saving shadows
  /// it with a per-model override rather than overwriting it.
  pub fn name_is_shadow(&self) -> bool {
    let name = self.name();
    !self.existing.contains(&name) && self.arch_shadow.contains(&name)
  }

  /// Whether a confirm step is needed before saving (overwrite or shadow).
  pub fn name_needs_confirm(&self) -> bool {
    self.name_exists() || self.name_is_shadow()
  }
}

/// Submit / Cancel chip labels resolved live from the keymap (so a
/// config rebind flows through), scoped like the confirm popup. `y` / `n`
/// stay hardcoded char-matches in the dispatcher, mirroring
/// [`crate::tui::confirm_overlay`].
fn keymap_label(app: &App, action: KeyAction, fallback: &str) -> String {
  app.resolve_label(Focus::ConfirmPopup, action, fallback)
}

/// Render the modal. Caller invokes this only when the dialog is open.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  dialog: &SavePresetDialog,
  palette: &Palette,
) {
  use crate::tui::keybindings::{ENTER_LABEL, ESC_LABEL};
  let submit = keymap_label(app, KeyAction::Submit, ENTER_LABEL);
  let cancel = keymap_label(app, KeyAction::Cancel, ESC_LABEL);
  let rect = crate::tui::layout::centered_abs(area, 60, 9, 4, 2);
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  let tone = palette.accent;
  let block = palette
    .panel()
    .title(Line::from(Span::styled(
      format!(" Save preset · {} ", dialog.model_name),
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
      Constraint::Length(1), // input / confirm
      Constraint::Length(1), // error / spacer
      Constraint::Min(1),    // hint
    ])
    .split(inner);

  match dialog.stage {
    SaveStage::Name => {
      let prompt = Paragraph::new(Line::from(Span::styled(
        "Name this preset:",
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
        Span::styled(" save  ·  ", palette.muted_style()),
        Span::styled(cancel, Style::default().fg(palette.warning)),
        Span::styled(" cancel", palette.muted_style()),
      ]))
      .alignment(Alignment::Center);
      frame.render_widget(hint, chunks[3]);
    }
    SaveStage::Confirm => {
      // Two flavors: overwriting the model's own preset, or shadowing an
      // arch preset with a new per-model override (the arch entry survives
      // and still applies to other models of that arch).
      let question = if dialog.name_is_shadow() {
        format!(
          "`{}` is an arch preset for {}. Saving creates a model-specific override (the arch preset is unchanged). Continue?",
          dialog.name(),
          dialog.model_name
        )
      } else {
        format!(
          "A preset named `{}` already exists for {}. Overwrite it?",
          dialog.name(),
          dialog.model_name
        )
      };
      let verb = if dialog.name_is_shadow() {
        " override  ·  "
      } else {
        " overwrite  ·  "
      };
      let body = Paragraph::new(Line::from(Span::styled(question, palette.text_style())))
        .wrap(Wrap { trim: true })
        .alignment(Alignment::Center);
      // Span the three top rows for the wrapped question.
      let body_area = Rect {
        height: chunks[0].height + chunks[1].height + chunks[2].height,
        ..chunks[0]
      };
      frame.render_widget(body, body_area);

      let hint = Paragraph::new(Line::from(vec![
        Span::styled(
          format!("{submit} / y"),
          Style::default().fg(palette.success),
        ),
        Span::styled(verb, palette.muted_style()),
        Span::styled(
          format!("{cancel} / n"),
          Style::default().fg(palette.warning),
        ),
        Span::styled(" keep", palette.muted_style()),
      ]))
      .alignment(Alignment::Center);
      frame.render_widget(hint, chunks[3]);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  fn dialog(existing: &[&str]) -> SavePresetDialog {
    dialog_with(existing, &[])
  }

  fn dialog_with(existing: &[&str], arch_shadow: &[&str]) -> SavePresetDialog {
    SavePresetDialog::open(
      PathBuf::from("/m/a.gguf"),
      "a.gguf".into(),
      crate::launch::knobs::KnobSet::new(),
      Vec::new(),
      None,
      None,
      existing.iter().map(|s| s.to_string()).collect(),
      arch_shadow.iter().map(|s| s.to_string()).collect(),
    )
  }

  #[test]
  fn opens_at_name_stage_with_editing_input() {
    let d = dialog(&[]);
    assert_eq!(d.stage, SaveStage::Name);
    assert!(d.input.is_editing());
    assert!(d.error.is_none());
  }

  #[test]
  fn name_is_trimmed_and_existence_checked() {
    let mut d = dialog(&["coding", "long-ctx"]);
    d.input.set_text("  coding  ");
    assert_eq!(d.name(), "coding");
    assert!(d.name_exists());
    assert!(d.name_needs_confirm());
    d.input.set_text("fresh");
    assert!(!d.name_exists());
    assert!(!d.name_needs_confirm());
  }

  #[test]
  fn arch_only_name_is_a_shadow_not_an_overwrite() {
    // A name that exists only as an arch preset: not a true overwrite, but
    // a save still needs a confirm (it shadows the arch entry).
    let mut d = dialog_with(&["coding"], &["balanced"]);
    d.input.set_text("balanced");
    assert!(!d.name_exists(), "not the model's own preset");
    assert!(d.name_is_shadow(), "matches an arch preset");
    assert!(d.name_needs_confirm());
    // A per-model name that also shadows an arch entry counts as overwrite.
    d.input.set_text("coding");
    assert!(d.name_exists());
    assert!(!d.name_is_shadow());
  }
}
