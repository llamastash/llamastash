//! Modal "are you sure?" confirmation popup used by destructive
//! actions (stop a managed launch, kill the whole daemon).
//!
//! Enter / `y` confirms, Esc / `n` (or any other key) cancels — see
//! `events::handle_key` for the key-routing precedence.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::{App, ConfirmAction, ConfirmSeverity};
use crate::tui::keybindings::{Action as KeyAction, Focus};

/// Render the confirmation popup. Caller should only invoke this
/// when `app.confirm_dialog` is `Some(...)`.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  action: &ConfirmAction,
  palette: &Palette,
) {
  let (title, body) = describe(action);

  // Tone the border/title off the action's severity: red for prompts
  // that lose work (stop/kill/delete/cancel), the warning hue for
  // neutral/additive prompts so red stays meaningful.
  let tone = match action.severity() {
    ConfirmSeverity::Destructive => palette.error,
    ConfirmSeverity::Neutral => palette.warning,
  };

  let rect = crate::tui::layout::centered_abs(area, 60, 8, 4, 2);
  frame.render_widget(Clear, rect);
  // Restore the theme surface tone after `Clear` so the popup
  // body reads on `palette.bg` instead of the terminal default.
  // Mono opts out (`palette.bg == Color::Reset`).
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  let block = palette
    .panel()
    .title(Line::from(Span::styled(
      format!(" {title} "),
      Style::default().fg(tone).add_modifier(Modifier::BOLD),
    )))
    .border(tone)
    .build();
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Min(1), Constraint::Length(1)])
    .split(inner);

  let prompt = Paragraph::new(Line::from(Span::styled(body, palette.text_style())))
    .wrap(Wrap { trim: true })
    .alignment(Alignment::Center);
  frame.render_widget(prompt, chunks[0]);

  // Resolve the Submit/Cancel labels live from the keymap so a
  // config rebind flows through. `y` / `n` are hardcoded char-
  // matches in the dispatcher (see `events::handle_key`) and stay
  // as the universal foot-gun-resistant fallback regardless of
  // what the user maps Submit/Cancel to.
  let submit_label = keymap_label(app, KeyAction::Submit, crate::tui::keybindings::ENTER_LABEL);
  let cancel_label = keymap_label(app, KeyAction::Cancel, crate::tui::keybindings::ESC_LABEL);
  let hint = Paragraph::new(Line::from(vec![
    Span::styled(
      format!("{submit_label} / y"),
      Style::default().fg(palette.success),
    ),
    Span::styled(" confirm  ·  ", palette.muted_style()),
    Span::styled(
      format!("{cancel_label} / n"),
      Style::default().fg(palette.warning),
    ),
    Span::styled(" cancel", palette.muted_style()),
  ]))
  .alignment(Alignment::Center);
  frame.render_widget(hint, chunks[1]);
}

/// Title + body text for a confirm action. Kept here so the action
/// definition stays a pure data carrier — the renderer owns its
/// copy of the human strings.
fn describe(action: &ConfirmAction) -> (&'static str, String) {
  match action {
    ConfirmAction::StopModel { name, .. } => (
      "Stop model",
      format!("Stop the running launch of `{name}`?"),
    ),
    ConfirmAction::KillDaemon => (
      "Kill daemon",
      "Shut down the llamastash daemon? All managed launches will be stopped.".to_string(),
    ),
    ConfirmAction::RestartDaemon => (
      "Restart daemon",
      "Restart the llamastash daemon? Managed launches will be stopped and the daemon will re-spawn."
        .to_string(),
    ),
    ConfirmAction::LaunchDuplicate {
      name,
      active_instances,
      ..
    } => (
      "Launch again",
      format!(
        "`{name}` already has {active_instances} active instance{plural}. \
         Launch another on a fresh port?",
        plural = if *active_instances == 1 { "" } else { "s" }
      ),
    ),
    ConfirmAction::DeleteModel { display_name, .. } => (
      "Delete model",
      format!(
        "Delete `{display_name}` from disk?\n\n\
         If the file lives in the HuggingFace cache (`~/.cache/huggingface/hub/\
         models--<owner>--<repo>/snapshots/<rev>/<file>`), the entire repo \
         directory — every revision, every shard, every blob — is removed to \
         reclaim cache space. Otherwise just the single GGUF file is unlinked."
      ),
    ),
    ConfirmAction::CancelDownload { friendly_name, .. } => (
      "Cancel download",
      format!(
        "Cancel the active pull `{friendly_name}`? Any partial file in the HF \
         cache stays where it is. Queued pulls behind this one keep their \
         place; press {} again on the next promoted pull to cancel it too.",
        crate::ctrl_label!("x")
      ),
    ),
  }
}

/// Thin shim over [`App::resolve_label`] specialised to
/// [`Focus::ConfirmPopup`] so the per-line key chips below stay
/// compact. Falls back to a literal default when the user has
/// unbound the action entirely so the popup still reads sensibly.
fn keymap_label(app: &App, action: KeyAction, fallback: &str) -> String {
  app.resolve_label(Focus::ConfirmPopup, action, fallback)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crate::tui::keybindings::KeyMap;
  use std::collections::BTreeMap;

  #[test]
  fn confirm_popup_labels_default_to_enter_esc() {
    use crate::tui::keybindings::{ENTER_LABEL, ESC_LABEL};
    let app = App::new(AppOptions::default());
    assert_eq!(
      keymap_label(&app, KeyAction::Submit, "fallback"),
      ENTER_LABEL
    );
    assert_eq!(keymap_label(&app, KeyAction::Cancel, "fallback"), ESC_LABEL);
  }

  #[test]
  fn destructive_actions_are_red_neutral_actions_are_not() {
    use crate::tui::app::{ConfirmSeverity, StartModelArgs};
    // Work-losing prompts: red.
    for action in [
      ConfirmAction::StopModel {
        launch_id: "L1".into(),
        name: "qwen".into(),
      },
      ConfirmAction::KillDaemon,
      ConfirmAction::RestartDaemon,
      ConfirmAction::DeleteModel {
        path: "/m/x.gguf".into(),
        display_name: "x".into(),
      },
      ConfirmAction::CancelDownload {
        repo_id: "owner/repo".into(),
        friendly_name: "repo".into(),
      },
    ] {
      assert_eq!(
        action.severity(),
        ConfirmSeverity::Destructive,
        "{action:?} should read destructive"
      );
    }
    // Additive duplicate launch: neutral, so red stays meaningful.
    let dup = ConfirmAction::LaunchDuplicate {
      name: "qwen".into(),
      active_instances: 1,
      args: Box::new(StartModelArgs {
        model_path: "/m/x.gguf".into(),
        ctx: None,
        reasoning: None,
        knobs: Default::default(),
        extras: Vec::new(),
        mode: None,
        prefer_port: None,
        backend: Default::default(),
        selection: "explicit",
        backend_knobs: Default::default(),
        server: None,
        mtp: Default::default(),
      }),
    };
    assert_eq!(dup.severity(), ConfirmSeverity::Neutral);
  }

  #[test]
  fn confirm_popup_labels_follow_keymap_overrides() {
    // Rebind submit to F12 — the confirm popup's confirm chip must
    // read `F12`, not `Enter`.
    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> = [(String::from("submit"), String::from("f12"))]
      .into_iter()
      .collect();
    let warnings = keymap.apply_overrides(&overrides);
    assert!(warnings.is_empty(), "{warnings:?}");
    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    assert_eq!(keymap_label(&app, KeyAction::Submit, "Enter"), "F12");
  }
}
