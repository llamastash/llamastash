//! Host-wide reserve editor; the preview uses the same policy as admission.

use ratatui::{
  layout::Rect,
  text::Line,
  widgets::{Clear, Paragraph, Wrap},
  Frame,
};

use crate::daemon::memory_policy::MemoryBudget;
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
  pub fn open(percent: u8) -> Self {
    let mut input = InputField::with_text(percent.to_string());
    input.enter_edit();
    Self {
      input,
      error: None,
      saving: false,
    }
  }

  pub fn percent(&self) -> Result<u8, &'static str> {
    self
      .input
      .buffer()
      .trim()
      .parse::<u8>()
      .ok()
      .filter(|p| *p <= 99)
      .ok_or("Enter a whole percentage from 0 to 99.")
  }
}

fn budget_lines(budget: &MemoryBudget, percent: Option<u8>) -> Vec<String> {
  let gib = |bytes: u64| format!("{:.1} GiB", bytes as f64 / 1073741824.0);
  let Some(available) = budget.available_bytes else {
    return vec!["Waiting for an Apple Silicon memory reading.".into()];
  };
  let percent = percent.unwrap_or(budget.apple_memory_reserve_percent);
  let admissible = crate::launch::headroom::admissible_bytes(
    available,
    crate::launch::headroom::PoolKind::AppleUnified,
    percent,
  );
  vec![
    format!("Available RAM: {} (total minus used)", gib(available)),
    format!(
      "Additional reserve: {} ({percent}%)",
      gib(available.saturating_sub(admissible))
    ),
    format!("Launch budget: {}", gib(admissible)),
  ]
}

pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  dialog: &MemoryDialog,
  palette: &Palette,
) {
  let rect = crate::tui::layout::centered_abs(area, 76, 16, 2, 1);
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);
  let block = palette.panel_block(" Apple Silicon memory reserve ", true);
  let inner = block.inner(rect);
  frame.render_widget(block, rect);
  let mut lines = vec![
    Line::from("Reserve a share of currently available RAM."),
    Line::from("Open apps are already counted as used."),
    Line::from(""),
    Line::from(format!("Reserve (%): {}", dialog.input.buffer())),
  ];
  if let Some(budget) = app.memory_budget.as_ref() {
    lines.extend(
      budget_lines(budget, dialog.percent().ok())
        .into_iter()
        .map(Line::from),
    );
  }
  lines.extend([
    Line::from(""),
    Line::from("Default: 25%. Lower values leave less spare RAM."),
    Line::from("Save to config.yaml for future launches."),
    Line::from("Running models and engine limits stay unchanged."),
  ]);
  if let Some(error) = dialog.error.as_ref() {
    lines.push(Line::styled(
      error.clone(),
      ratatui::style::Style::default().fg(palette.error),
    ));
  } else if dialog.saving {
    lines.push(Line::from("Saving..."));
  } else {
    lines.push(Line::from(""));
  }
  let label = |action| {
    app
      .bindings_for(Focus::ConfirmPopup)
      .iter()
      .find(|b| b.action == action)
      .map(|b| b.label.to_string())
      .unwrap_or_default()
  };
  let body = Rect {
    height: inner.height.saturating_sub(1),
    ..inner
  };
  let footer = Rect {
    y: inner.y + inner.height.saturating_sub(1),
    height: 1,
    ..inner
  };
  frame.render_widget(
    Paragraph::new(lines)
      .style(palette.text_style())
      .wrap(Wrap { trim: false }),
    body,
  );
  frame.render_widget(
    Paragraph::new(format!(
      "{} save    {} cancel",
      label(Action::Submit),
      label(Action::Cancel)
    ))
    .style(palette.muted_style()),
    footer,
  );
  if !dialog.saving {
    let cursor_x = inner
      .x
      .saturating_add(13)
      .saturating_add(u16::try_from(dialog.input.buffer().len()).unwrap_or(u16::MAX));
    frame.set_cursor_position((cursor_x.min(inner.right().saturating_sub(1)), inner.y + 3));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn editor_rejects_out_of_range_and_fractional_reserves() {
    for text in ["", "100", "-1", "10.5", "oops"] {
      let mut dialog = MemoryDialog::open(25);
      dialog.input.set_text(text);
      assert!(dialog.percent().is_err(), "{text}");
    }
    for percent in [0, 10, 25, 99] {
      assert_eq!(MemoryDialog::open(percent).percent(), Ok(percent));
    }
  }

  #[test]
  fn preview_reserves_from_available_rather_than_total_ram() {
    let budget = MemoryBudget {
      apple_memory_reserve_percent: 25,
      apple_unified: true,
      available_bytes: Some(40 * 1024 * 1024 * 1024),
      admissible_bytes: Some(30 * 1024 * 1024 * 1024),
    };
    let lines = budget_lines(&budget, Some(10));
    assert!(lines[1].contains("4.0 GiB (10%)"));
    assert!(lines[2].contains("36.0 GiB"));
  }
}
