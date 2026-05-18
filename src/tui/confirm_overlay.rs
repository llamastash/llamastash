//! Modal "are you sure?" confirmation popup used by destructive
//! actions (stop a managed launch, kill the whole daemon).
//!
//! Enter / `y` confirms, Esc / `n` (or any other key) cancels — see
//! [`events::handle_key`] for the key-routing precedence.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::{App, ConfirmAction};
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

  let rect = centred(area, 60, 8);
  frame.render_widget(Clear, rect);

  let block = Block::default()
    .title(Line::from(Span::styled(
      format!(" {title} "),
      Style::default()
        .fg(palette.error)
        .add_modifier(Modifier::BOLD),
    )))
    .borders(Borders::ALL)
    .border_style(palette.error_style());
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Min(1), Constraint::Length(1)])
    .split(inner);

  let prompt = Paragraph::new(Line::from(Span::styled(
    body,
    palette.text_style(),
  )))
  .wrap(Wrap { trim: true })
  .alignment(Alignment::Center);
  frame.render_widget(prompt, chunks[0]);

  // Resolve the Submit/Cancel labels live from the keymap so a
  // config rebind flows through. `y` / `n` are hardcoded char-
  // matches in the dispatcher (see `events::handle_key`) and stay
  // as the universal foot-gun-resistant fallback regardless of
  // what the user maps Submit/Cancel to.
  let submit_label = keymap_label(app, KeyAction::Submit, "Enter");
  let cancel_label = keymap_label(app, KeyAction::Cancel, "Esc");
  let hint = Paragraph::new(Line::from(vec![
    Span::styled(
      format!("{submit_label} / y"),
      Style::default()
        .fg(palette.success)
        .add_modifier(Modifier::BOLD),
    ),
    Span::styled(" confirm  ·  ", palette.muted_style()),
    Span::styled(
      format!("{cancel_label} / n"),
      Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD),
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
      "Shut down the llamadash daemon? All managed launches will be stopped.".to_string(),
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
  }
}

/// Pull just the key label off the confirm popup's `(focus, action)`
/// binding, falling back to a literal default when the user has
/// unbound the action entirely.
fn keymap_label(app: &App, action: KeyAction, fallback: &str) -> String {
  app
    .bindings_for(Focus::ConfirmPopup)
    .iter()
    .find(|b| b.action == action)
    .map(|b| b.label.to_string())
    .unwrap_or_else(|| fallback.to_string())
}

fn centred(area: Rect, w: u16, h: u16) -> Rect {
  let w = w.min(area.width.saturating_sub(4));
  let h = h.min(area.height.saturating_sub(2));
  let x = area.x + (area.width.saturating_sub(w)) / 2;
  let y = area.y + (area.height.saturating_sub(h)) / 2;
  Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crate::tui::keybindings::KeyMap;
  use std::collections::BTreeMap;

  #[test]
  fn confirm_popup_labels_default_to_enter_esc() {
    let app = App::new(AppOptions::default());
    assert_eq!(keymap_label(&app, KeyAction::Submit, "fallback"), "Enter");
    assert_eq!(keymap_label(&app, KeyAction::Cancel, "fallback"), "Esc");
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
