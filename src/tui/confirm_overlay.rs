//! Modal "are you sure?" confirmation popup used by destructive
//! actions (stop a managed launch, kill the whole daemon).
//!
//! Enter / `y` confirms, Esc / `n` (or any other key) cancels — see
//! `events::handle_key` for the key-routing precedence.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::{App, ConfirmAction, ConfirmSeverity};
use crate::tui::keybindings::{Action, Focus};
use crate::tui::popup::{self, Button, Size, Spec};

/// Render the confirmation popup. Caller should only invoke this
/// when `app.confirm_dialog` is `Some(...)`.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  action: &ConfirmAction,
  palette: &Palette,
) {
  let (title, body) = describe(app, action);

  // Tone the border/title off the action's severity: red for prompts
  // that lose work (stop/kill/delete/cancel), the warning hue for
  // neutral/additive prompts so red stays meaningful.
  let (tone, primary) = match action.severity() {
    ConfirmSeverity::Destructive => (palette.error, Button::destructive("confirm")),
    ConfirmSeverity::Neutral => (palette.warning, Button::submit("confirm")),
  };
  let rect = popup::area(
    area,
    Size::Fixed {
      w: 60,
      h: 8,
      x_margin: 4,
      y_margin: 2,
    },
  );
  let inner = popup::frame(
    frame,
    rect,
    Spec {
      tone: Some(tone),
      ..Spec::new(title)
    },
    palette,
  );

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Min(1), Constraint::Length(1)])
    .split(inner);

  let prompt = Paragraph::new(Line::from(Span::styled(body, palette.text_style())))
    .wrap(Wrap { trim: true })
    .alignment(Alignment::Center);
  frame.render_widget(prompt, chunks[0]);

  // Any key other than Submit / `y` cancels (see `events::handle_key`), so a
  // stray keypress can't drop a running model or kill the daemon.
  popup::render_buttons(
    frame,
    chunks[1],
    app,
    &[primary.yes_no(), Button::cancel("cancel").yes_no()],
    palette,
  );
}

/// Title + body text for a confirm action. Kept here so the action
/// definition stays a pure data carrier — the renderer owns its
/// copy of the human strings.
fn describe(app: &App, action: &ConfirmAction) -> (&'static str, String) {
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
    ConfirmAction::DaemonBuildMismatch {
      daemon_build,
      client_build,
    } => {
      let restart = app
        .bindings_for(Focus::List)
        .iter()
        .find(|b| b.action == Action::RestartDaemon)
        .map(|b| format!("Press {} to restart it", b.label))
        .unwrap_or_else(|| "Run `llamastash daemon restart`".to_string());
      (
        "Daemon from another build",
        format!(
          "The daemon runs build {daemon_build}, this TUI is {client_build}. \
           {restart}; running models stop."
        ),
      )
    }
    ConfirmAction::DeleteModel { plan, display_name } => {
      ("Delete model", plan.describe(display_name))
    }
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
      popup::key_label(&app, crate::tui::keybindings::Action::Submit),
      ENTER_LABEL
    );
    assert_eq!(
      popup::key_label(&app, crate::tui::keybindings::Action::Cancel),
      ESC_LABEL
    );
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
        plan: Box::new(crate::discovery::delete::DeletePlan::single("/m/x.gguf")),
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
        server: None,
        name: None,
        preset: None,
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
    assert_eq!(
      popup::key_label(&app, crate::tui::keybindings::Action::Submit),
      "F12"
    );
  }

  #[test]
  fn build_notice_names_the_restart_key_from_the_keymap() {
    let notice = ConfirmAction::DaemonBuildMismatch {
      daemon_build: "0.0.1".into(),
      client_build: "0.0.2".into(),
    };
    let (_, body) = describe(&App::new(AppOptions::default()), &notice);
    assert!(body.contains("0.0.1") && body.contains("0.0.2"), "{body}");
    assert!(body.contains(crate::ctrl_label!("r")), "{body}");

    let mut keymap = KeyMap::default();
    let overrides: BTreeMap<String, String> =
      [(String::from("restart_daemon"), String::from("f9"))]
        .into_iter()
        .collect();
    assert!(keymap.apply_overrides(&overrides).is_empty());
    let app = App::new(AppOptions {
      keymap,
      ..AppOptions::default()
    });
    let (_, remapped) = describe(&app, &notice);
    assert!(remapped.contains("F9"), "{remapped}");
  }
}
