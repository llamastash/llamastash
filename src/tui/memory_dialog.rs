//! Host-wide reserve editor; the preview uses the same policy as admission.

use ratatui::{
  layout::{Constraint, Layout, Rect},
  style::Modifier,
  text::{Line, Span},
  widgets::{Clear, Padding, Paragraph, Wrap},
  Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::daemon::memory_policy::MemoryBudget;
use crate::launch::headroom::MemoryReserve;
use crate::theme::Palette;
use crate::tui::{
  app::App,
  input_field::InputField,
  keybindings::{Action, Focus},
};

#[derive(Debug, Clone)]
pub struct MemoryDialog {
  pub input: InputField,
  pub error: Option<String>,
  pub saving: bool,
}

impl MemoryDialog {
  pub fn open(reserve: MemoryReserve) -> Self {
    let mut input = InputField::with_text(reserve.to_string());
    input.enter_edit();
    Self {
      input,
      error: None,
      saving: false,
    }
  }

  pub fn reserve(&self) -> Result<MemoryReserve, &'static str> {
    self.input.buffer().trim().parse()
  }
}

fn preview_lines(
  budget: Option<&MemoryBudget>,
  reserve: Option<MemoryReserve>,
  width: u16,
  palette: &Palette,
) -> Vec<Line<'static>> {
  let gib = |bytes: u64| format!("{:.1} GiB", bytes as f64 / 1073741824.0);
  let glyphs = crate::tui::glyphs::active();
  let available = budget.and_then(|b| b.available_bytes);
  let estimate = budget.zip(reserve).and_then(|(budget, setting)| {
    budget
      .available_bytes
      .zip(budget.preview(setting))
      .map(|(free, admissible)| (free.saturating_sub(admissible), admissible))
  });
  let metric = |label: &str, value: Option<String>, style, bold| {
    let style = if value.is_some() {
      style
    } else {
      palette.muted_style()
    };
    let value = value.unwrap_or_else(|| glyphs.placeholder().into());
    let gap = usize::from(width).saturating_sub(label.width() + value.width());
    Line::from(vec![
      Span::styled(
        label.to_string(),
        if bold {
          palette.text_style().add_modifier(Modifier::BOLD)
        } else {
          palette.label_style()
        },
      ),
      Span::raw(" ".repeat(gap)),
      Span::styled(value, style),
    ])
  };
  let default_percent = budget
    .filter(|b| b.supports_editing())
    .and_then(|b| b.default_reserve_percent);
  let bar = match available.zip(reserve.zip(default_percent)) {
    Some((free, (setting, default))) if free > 0 => {
      let reserve_percent = setting.resolve(default);
      let launch_cells = (usize::from(width) * usize::from(100 - reserve_percent) + 50) / 100;
      Line::from(vec![
        Span::styled(
          glyphs.gauge_fill().to_string().repeat(launch_cells),
          palette.success_style(),
        ),
        Span::styled(
          glyphs
            .gauge_trough()
            .to_string()
            .repeat(usize::from(width) - launch_cells),
          palette.warning_style(),
        ),
      ])
    }
    _ => Line::styled(
      glyphs.gauge_trough().to_string().repeat(usize::from(width)),
      palette.muted_style(),
    ),
  };
  vec![
    Line::styled(
      "LIVE PREVIEW",
      palette.label_style().add_modifier(Modifier::BOLD),
    ),
    metric(
      "Available pool",
      available.map(gib),
      palette.text_style(),
      false,
    ),
    metric(
      "Additional reserve",
      estimate.map(|(reserved_bytes, _)| format!("- {}", gib(reserved_bytes))),
      palette.warning_style(),
      false,
    ),
    Line::styled(
      glyphs.hline().repeat(usize::from(width)),
      palette.muted_style(),
    ),
    metric(
      "Admission budget",
      estimate.map(|(_, admissible)| gib(admissible)),
      palette.success_style().add_modifier(Modifier::BOLD),
      true,
    ),
    bar,
  ]
}

// Keep the end of a long or wide input visible without letting its cursor
// escape the field into the default-value hint.
fn input_tail(input: &str) -> String {
  let marker = if input.width() > 4 {
    crate::tui::glyphs::active().ellipsis()
  } else {
    ""
  };
  let budget = 4usize.saturating_sub(marker.width());
  let mut width = 0;
  let tail: String = input
    .chars()
    .rev()
    .take_while(|c| {
      width += c.width().unwrap_or(0);
      width <= budget
    })
    .collect::<Vec<_>>()
    .into_iter()
    .rev()
    .collect();
  format!("{marker}{tail}")
}

pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  dialog: &MemoryDialog,
  palette: &Palette,
) {
  let rect = crate::tui::layout::centered_abs(area, 74, 19, 2, 1);
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);
  let compact = rect.height < 19;
  let block = palette
    .panel()
    .title(Line::styled(
      " Unified memory reserve ",
      palette.title_style(),
    ))
    .padding(Padding::new(2, 2, u16::from(!compact), u16::from(!compact)))
    .build();
  let inner = block.inner(rect);
  frame.render_widget(block, rect);
  if inner.width == 0 || inner.height == 0 {
    return;
  }
  let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
  if inner.height < 9 || inner.width < 28 {
    frame.render_widget(
      Paragraph::new("Enlarge the terminal to edit the reserve.").wrap(Wrap { trim: false }),
      inner,
    );
  } else {
    let chunks = Layout::vertical([
      Constraint::Length(if compact { 1 } else { 2 }),
      Constraint::Length(1),
      Constraint::Length(if compact { 4 } else { 6 }),
      Constraint::Length(u16::from(!compact)),
      Constraint::Length(if compact { 0 } else { 2 }),
      Constraint::Min(0),
      Constraint::Length(2),
      Constraint::Length(1),
    ])
    .split(inner);
    let tone = if dialog.error.is_some() {
      palette.error_style()
    } else {
      palette.accent_style()
    };
    frame.render_widget(
      Paragraph::new(Span::styled("Reserve", palette.label_style())),
      chunks[0],
    );
    let field = Rect::new(inner.x + 10, inner.y, 6, 1);
    let visible = input_tail(dialog.input.buffer());
    frame.render_widget(
      Paragraph::new(format!(" {visible}"))
        .style(tone.bg(palette.selection).add_modifier(Modifier::BOLD)),
      field,
    );
    if inner.width >= 32 {
      frame.render_widget(
        Paragraph::new(Span::styled(
          format!(
            "Default {}%",
            app
              .memory_budget
              .as_ref()
              .and_then(|b| b.default_reserve_percent)
              .unwrap_or(0)
          ),
          palette.muted_style(),
        )),
        Rect::new(inner.right() - 11, inner.y, 11, 1),
      );
    }
    if !compact {
      frame.render_widget(
        Paragraph::new(Span::styled(
          "of available pool  /  auto or 0-99%",
          palette.muted_style(),
        )),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
      );
    }
    let mut preview = preview_lines(
      app.memory_budget.as_ref(),
      dialog.reserve().ok(),
      inner.width,
      palette,
    );
    if compact {
      preview.remove(0);
      preview.pop();
    }
    frame.render_widget(Paragraph::new(preview), chunks[2]);
    frame.render_widget(
      Paragraph::new(vec![
        Line::from("Engine limits and launch reservations also apply."),
        Line::from("Save applies to new launches; kept for next time."),
      ])
      .style(palette.muted_style()),
      chunks[4],
    );
    let status = if let Some(error) = dialog.error.as_ref() {
      Line::styled(format!("Cannot save: {error}"), palette.error_style())
    } else if dialog.reserve().is_err() {
      Line::styled(
        "Enter auto or a percentage (0-99) to preview.",
        palette.muted_style(),
      )
    } else if app
      .memory_budget
      .as_ref()
      .and_then(|b| b.available_bytes)
      .is_none()
    {
      Line::styled(
        "Waiting for a unified-memory reading.",
        palette.muted_style(),
      )
    } else {
      Line::styled(
        "Lower reserve leaves less room for growth.",
        palette.muted_style(),
      )
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[6]);
    if !dialog.saving {
      frame.set_cursor_position((field.x + 1 + visible.width() as u16, field.y));
    }
  }
  let label = |action| {
    app
      .bindings_for(Focus::ConfirmPopup)
      .iter()
      .find(|b| b.action == action)
      .map(|b| b.label.to_string())
      .unwrap_or_default()
  };
  let actions = if dialog.saving {
    Line::styled(
      "Saving changes...",
      palette.accent_style().add_modifier(Modifier::BOLD),
    )
  } else {
    Line::from(vec![
      Span::styled(
        format!(" {} ", label(Action::Submit)),
        palette
          .success_style()
          .bg(palette.selection)
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(
        if compact {
          " Save    "
        } else {
          " Save changes    "
        },
        palette.text_style(),
      ),
      Span::styled(
        format!(" {} ", label(Action::Cancel)),
        palette.muted_style().bg(palette.selection),
      ),
      Span::styled(" Cancel", palette.muted_style()),
    ])
  };
  frame.render_widget(Paragraph::new(actions), footer);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::theme::ThemeName;
  use crate::tui::{app::AppOptions, keybindings::KeyMap};
  use ratatui::{backend::TestBackend, Terminal};

  #[test]
  fn editor_rejects_out_of_range_and_fractional_reserves() {
    for text in ["", "100", "-1", "10.5", "oops"] {
      let mut dialog = MemoryDialog::open(MemoryReserve::percent(25).unwrap());
      dialog.input.set_text(text);
      assert!(dialog.reserve().is_err(), "{text}");
    }
    assert_eq!(
      MemoryDialog::open(MemoryReserve::AUTO).reserve(),
      Ok(MemoryReserve::AUTO)
    );
    for percent in [0, 10, 25, 99] {
      assert_eq!(
        MemoryDialog::open(MemoryReserve::percent(percent).unwrap()).reserve(),
        Ok(MemoryReserve::percent(percent).unwrap())
      );
    }
  }

  #[test]
  fn preview_reserves_from_available_rather_than_total_ram() {
    let budget = MemoryBudget {
      unified_memory_reserve_percent: MemoryReserve::AUTO,
      default_reserve_percent: Some(25),
      unified: true,
      available_bytes: Some(40 * 1024 * 1024 * 1024),
      admissible_bytes: Some(30 * 1024 * 1024 * 1024),
    };
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    let lines = preview_lines(
      Some(&budget),
      Some(MemoryReserve::percent(10).unwrap()),
      52,
      palette,
    );
    assert!(lines[2].to_string().contains("4.0 GiB"));
    assert!(lines[4].to_string().contains("36.0 GiB"));
    let invalid = preview_lines(Some(&budget), None, 52, palette);
    assert!(invalid[1].to_string().contains("40.0 GiB"));
    assert!(!invalid[4].to_string().contains("30.0 GiB"));
    assert!(invalid[4]
      .to_string()
      .contains(crate::tui::glyphs::active().placeholder()));
  }

  #[test]
  fn an_incompatible_status_during_edit_blanks_the_preview_without_panicking() {
    let mut app = App::new(AppOptions::default());
    app.memory_budget = Some(MemoryBudget {
      unified_memory_reserve_percent: MemoryReserve::AUTO,
      default_reserve_percent: Some(255),
      unified: true,
      available_bytes: Some(40 * 1024 * 1024 * 1024),
      admissible_bytes: None,
    });
    let dialog = MemoryDialog::open(MemoryReserve::AUTO);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
      .draw(|frame| {
        render(
          frame,
          frame.area(),
          &app,
          &dialog,
          crate::theme::palette_for(ThemeName::Mono),
        )
      })
      .unwrap();
    assert_eq!(
      app
        .memory_budget
        .as_ref()
        .unwrap()
        .preview(MemoryReserve::AUTO),
      None
    );
  }

  #[test]
  fn narrow_modal_keeps_budget_errors_and_rebound_actions_visible() {
    for (width, height) in [(120, 32), (80, 24), (60, 20), (40, 12)] {
      let mut keymap = KeyMap::default();
      assert!(keymap
        .apply_overrides(
          &[
            ("submit".into(), "f2".into()),
            ("cancel".into(), "f3".into()),
          ]
          .into()
        )
        .is_empty());
      let mut app = App::new(AppOptions {
        keymap,
        ..Default::default()
      });
      app.memory_budget = Some(MemoryBudget {
        unified_memory_reserve_percent: MemoryReserve::AUTO,
        default_reserve_percent: Some(25),
        unified: true,
        available_bytes: Some(40 * 1024 * 1024 * 1024),
        admissible_bytes: Some(30 * 1024 * 1024 * 1024),
      });
      let mut dialog = MemoryDialog::open(MemoryReserve::percent(10).unwrap());
      dialog.error = Some("Permission denied; try again.".into());
      let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
      let drawn = terminal
        .draw(|frame| {
          render(
            frame,
            frame.area(),
            &app,
            &dialog,
            crate::theme::palette_for(ThemeName::Mono),
          )
        })
        .unwrap();
      let text = drawn
        .buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
      for required in [
        "Admission budget",
        "36.0 GiB",
        "Cannot save:",
        "F2",
        "Save",
        "F3",
        "Cancel",
      ] {
        assert!(
          text.contains(required),
          "{width}x{height}: missing {required}"
        );
      }
    }
  }

  #[test]
  fn long_wide_input_keeps_its_cursor_inside_the_field() {
    let app = App::new(AppOptions::default());
    let mut dialog = MemoryDialog::open(MemoryReserve::percent(25).unwrap());
    dialog.input.set_text("bad input 日本語999");
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal
      .draw(|frame| {
        render(
          frame,
          frame.area(),
          &app,
          &dialog,
          crate::theme::palette_for(ThemeName::Macchiato),
        )
      })
      .unwrap();
    let cursor = terminal.get_cursor_position().unwrap();
    let cell = &terminal.backend().buffer()[(cursor.x, cursor.y)];
    assert_eq!(
      cell.bg,
      crate::theme::palette_for(ThemeName::Macchiato).selection
    );
    assert!(input_tail(dialog.input.buffer()).width() <= 4);
  }
}
