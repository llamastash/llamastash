//! Shared modal frame, button row and Submit/Cancel routing. Each popup owns
//! only its content, so every frame and button looks and behaves the same.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
  layout::{Alignment, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Clear, Padding, Paragraph},
  Frame,
};

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus, ENTER_LABEL, ESC_LABEL};

/// Popup size against the full screen.
pub enum Size {
  Fixed {
    w: u16,
    h: u16,
    x_margin: u16,
    y_margin: u16,
  },
  Percent {
    x: u16,
    y: u16,
  },
}

/// Where a popup of `size` sits on `screen`.
pub fn area(screen: Rect, size: Size) -> Rect {
  match size {
    Size::Fixed {
      w,
      h,
      x_margin,
      y_margin,
    } => crate::tui::layout::centered_abs(screen, w, h, x_margin, y_margin),
    Size::Percent { x, y } => crate::tui::layout::centered_rect(x, y, screen),
  }
}

pub struct Spec<'a> {
  pub title: &'a str,
  /// Muted `· hint` strips after the title, for keys that are not buttons.
  pub hints: &'a [String],
  /// Border and title colour; `None` is the accent.
  pub tone: Option<Color>,
  pub pad_y: u16,
}

impl<'a> Spec<'a> {
  pub fn new(title: &'a str) -> Self {
    Self {
      title,
      hints: &[],
      tone: None,
      pad_y: 0,
    }
  }
}

/// Clear `rect`, paint the theme surface and draw the frame. Returns the
/// content area inside the border and padding.
pub fn frame(frame: &mut Frame<'_>, rect: Rect, spec: Spec<'_>, palette: &Palette) -> Rect {
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);
  let tone = spec.tone.unwrap_or(palette.accent);
  let mut title = vec![Span::styled(
    format!(" {} ", spec.title),
    Style::default().fg(tone).add_modifier(Modifier::BOLD),
  )];
  title.extend(
    spec
      .hints
      .iter()
      .filter(|hint| !hint.is_empty())
      .map(|hint| Span::styled(format!("· {hint} "), palette.muted_style())),
  );
  let block = palette
    .panel()
    .title(Line::from(title))
    .border(tone)
    .padding(Padding::new(1, 1, spec.pad_y, spec.pad_y))
    .build();
  let inner = block.inner(rect);
  frame.render_widget(block, rect);
  inner
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
  Primary,
  Destructive,
  Secondary,
}

#[derive(Clone, Copy, Debug)]
pub struct Button<'a> {
  pub action: Action,
  pub label: &'a str,
  pub role: Role,
  /// Also accept `y` (Submit) / `n` (Cancel); shown in the key chip.
  pub yes_no: bool,
}

impl<'a> Button<'a> {
  pub fn submit(label: &'a str) -> Self {
    Self {
      action: Action::Submit,
      label,
      role: Role::Primary,
      yes_no: false,
    }
  }

  pub fn destructive(label: &'a str) -> Self {
    Self {
      role: Role::Destructive,
      ..Self::submit(label)
    }
  }

  pub fn cancel(label: &'a str) -> Self {
    Self {
      action: Action::Cancel,
      label,
      role: Role::Secondary,
      yes_no: false,
    }
  }

  pub fn yes_no(self) -> Self {
    Self {
      yes_no: true,
      ..self
    }
  }
}

/// Key text for a popup action, from the live keymap.
pub fn key_label(app: &App, action: Action) -> String {
  let fallback = if action == Action::Cancel {
    ESC_LABEL
  } else {
    ENTER_LABEL
  };
  app.resolve_label(Focus::ConfirmPopup, action, fallback)
}

/// One button row: a key chip on the selection tone, then its label.
pub fn buttons(app: &App, buttons: &[Button<'_>], palette: &Palette) -> Line<'static> {
  let mut spans = Vec::new();
  for (i, button) in buttons.iter().enumerate() {
    if i > 0 {
      spans.push(Span::raw("    "));
    }
    let mut key = key_label(app, button.action);
    if button.yes_no {
      key.push_str(if button.action == Action::Cancel {
        " / n"
      } else {
        " / y"
      });
    }
    let (chip, label) = match button.role {
      Role::Primary => (
        palette.success_style().add_modifier(Modifier::BOLD),
        palette.text_style(),
      ),
      Role::Destructive => (
        palette.error_style().add_modifier(Modifier::BOLD),
        palette.text_style(),
      ),
      Role::Secondary => (palette.muted_style(), palette.muted_style()),
    };
    spans.push(Span::styled(format!(" {key} "), chip.bg(palette.selection)));
    spans.push(Span::styled(format!(" {}", button.label), label));
  }
  Line::from(spans)
}

pub fn render_buttons(
  frame: &mut Frame<'_>,
  rect: Rect,
  app: &App,
  row: &[Button<'_>],
  palette: &Palette,
) {
  frame.render_widget(
    Paragraph::new(buttons(app, row, palette)).alignment(Alignment::Center),
    rect,
  );
}

/// Replaces the button row while a popup waits on the daemon.
pub fn render_busy(frame: &mut Frame<'_>, rect: Rect, text: &str, palette: &Palette) {
  frame.render_widget(
    Paragraph::new(Line::styled(
      format!("{text}{}", crate::tui::glyphs::active().ellipsis()),
      palette.accent_style().add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Center),
    rect,
  );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupKey {
  Submit,
  Cancel,
  Other,
}

/// Resolve a key against the popup's Submit / Cancel bindings. `yes_no`
/// popups also take `y` / `n`; text-entry popups must not, or typing breaks.
pub fn key(app: &App, key: KeyEvent, yes_no: bool) -> PopupKey {
  match app.action_for(Focus::ConfirmPopup, key.code, key.modifiers) {
    Some(Action::Submit) => PopupKey::Submit,
    Some(Action::Cancel) => PopupKey::Cancel,
    _ if yes_no => match key.code {
      KeyCode::Char('y' | 'Y') => PopupKey::Submit,
      KeyCode::Char('n' | 'N') => PopupKey::Cancel,
      _ => PopupKey::Other,
    },
    _ => PopupKey::Other,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use crossterm::event::KeyModifiers;

  fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
  }

  #[test]
  fn keys_follow_the_keymap_and_yes_no_only_where_asked() {
    let app = App::new(AppOptions::default());
    assert_eq!(key(&app, press(KeyCode::Enter), false), PopupKey::Submit);
    assert_eq!(key(&app, press(KeyCode::Esc), false), PopupKey::Cancel);
    assert_eq!(key(&app, press(KeyCode::Char('y')), false), PopupKey::Other);
    assert_eq!(key(&app, press(KeyCode::Char('y')), true), PopupKey::Submit);
    assert_eq!(key(&app, press(KeyCode::Char('N')), true), PopupKey::Cancel);
    assert_eq!(key(&app, press(KeyCode::Char('x')), true), PopupKey::Other);
  }

  #[test]
  fn button_row_shows_live_keys_alternates_and_labels() {
    let app = App::new(AppOptions::default());
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    let row = buttons(
      &app,
      &[
        Button::destructive("confirm").yes_no(),
        Button::cancel("cancel").yes_no(),
      ],
      palette,
    );
    let text = row.to_string();
    assert!(text.contains(&format!(
      " {} / y  confirm",
      key_label(&app, Action::Submit)
    )));
    assert!(text.contains(&format!(" {} / n  cancel", key_label(&app, Action::Cancel))));
    assert!(row
      .spans
      .iter()
      .filter(|s| s.content.contains(" / "))
      .all(|s| s.style.bg == Some(palette.selection)));
  }
}
