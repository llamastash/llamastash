//! Requests tab — the proxy requests of the focused model.
//!
//! A summary strip on top (totals and averages the daemon computed) and
//! below it a table of requests, newest first. Each column has a width and a
//! rank; as the pane narrows the higher-rank ones drop out.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::proxy::request_log::{Aggregates, RequestCells, RequestRow, RequestSummary};
use crate::theme::Palette;
use crate::tui::columns::{self, cell, cell_words, Column};

/// More than any pane shows at once, so a scroll has something to move through.
pub const POLL_ROWS: usize = 200;

/// Cell text for a value the log does not have.
const NONE: &str = "—";
const COL_SEP_W: usize = 1;
/// The least `Note` is worth showing at.
const MIN_NOTE_W: usize = 10;
/// `Route`'s own width. It is the first column dropped.
const ROUTE_W: usize = 16;
/// Gap between two figures on the summary strip.
const SUMMARY_GAP: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnId {
  Time,
  Status,
  Speed,
  Prefill,
  Total,
  Ttfb,
  In,
  Cache,
  Out,
  Note,
  RequestFor,
  Client,
  Route,
}

/// Table columns in display order. Keeping them is by `rank`, not by
/// position (see [`columns::fit`]): the figures that answer "what did this
/// request cost" stay longest, and what is left drops from the right —
/// `Route` first because the tab already implies one engine's routes, then
/// loopback-only `Client`, then `Request for`.
const COLUMNS: &[Column<ColumnId>] = &[
  Column {
    id: ColumnId::Time,
    label: "Time",
    width: 8,
    rank: 10,
  },
  Column {
    id: ColumnId::Status,
    label: "Code",
    width: 4,
    rank: 20,
  },
  Column {
    id: ColumnId::Speed,
    label: "Tok/s",
    width: 6,
    rank: 30,
  },
  Column {
    id: ColumnId::Prefill,
    label: "PP/s",
    // Prompt tokens per second, beside the generation speed it follows. Same
    // width, and it drops after `Note`.
    width: 6,
    rank: 70,
  },
  Column {
    id: ColumnId::Total,
    label: "Total",
    width: 7,
    rank: 40,
  },
  Column {
    id: ColumnId::Ttfb,
    label: "TTFB",
    width: 7,
    rank: 45,
  },
  Column {
    id: ColumnId::In,
    label: "In",
    width: 6,
    rank: 50,
  },
  Column {
    id: ColumnId::Cache,
    label: "Cache",
    // The cached share of `In`, beside it. Fits `100.0%`, and rank 65 keeps it
    // until `PP/s` and `Note` have gone, and gives way before `In` and `Out`.
    width: 6,
    rank: 65,
  },
  Column {
    id: ColumnId::Out,
    label: "Out",
    width: 6,
    rank: 60,
  },
  Column {
    id: ColumnId::RequestFor,
    label: "Request for",
    // The same 24 cells the CLI table gives a name, enough for a repo id
    // with its quant tag; a longer one cuts with an ellipsis. It outranks
    // `Client` and `Route` because it is the reason a row reads apart from
    // its neighbours: the model an alias or a fallback answered for.
    width: 24,
    rank: 80,
  },
  Column {
    id: ColumnId::Client,
    label: "Client",
    width: 21,
    rank: 90,
  },
  Column {
    id: ColumnId::Route,
    label: "Route",
    // Fits `chat/completions`, the longest of the common routes. The tab
    // already implies one engine's routes, so it goes first.
    width: ROUTE_W,
    rank: 100,
  },
  Column {
    id: ColumnId::Note,
    // Last, so the free text grows to the right of the fixed figures and
    // nothing shifts under it. Rank 75 keeps it until `Request for`,
    // `Client` and `Route` have gone, and drops it before `PP/s`.
    label: "Note",
    width: MIN_NOTE_W,
    rank: 75,
  },
];

/// What the tab shows: the last poll's summary and rows for one scope.
#[derive(Debug, Clone, Default)]
pub struct RequestsTabState {
  /// Scope the data belongs to. `None` until the first poll lands.
  pub scope: Option<RequestsScope>,
  pub summary: RequestSummary,
  /// Newest first.
  pub rows: Vec<RequestRow>,
  /// Rows scrolled down from the newest. `0` keeps the newest on top.
  pub scroll_offset: usize,
  /// Rows the last render fit on the page, so a scroll never parks where
  /// the render would clamp it back and swallow the keypress.
  page_rows: std::cell::Cell<usize>,
}

/// What the tab asks the daemon for: one launch, because a running model row
/// is one launch and its numbers must not come from another one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestsScope(String);

impl RequestsScope {
  pub fn new(launch_id: impl Into<String>) -> Self {
    Self(launch_id.into())
  }

  /// The `requests_tail` params that ask for this scope.
  pub fn params(&self, limit: usize) -> serde_json::Value {
    serde_json::json!({ "launch_id": &self.0, "limit": limit })
  }
}

impl RequestsTabState {
  pub fn set(&mut self, scope: RequestsScope, summary: RequestSummary, rows: Vec<RequestRow>) {
    self.scope = Some(scope);
    self.summary = summary;
    // A poll that brings newer requests must not slide the table out from
    // under a scroll, or a keypress reads as doing nothing: hold the row on
    // top, and follow the newest only while nothing is scrolled past.
    let anchor = match self.scroll_offset {
      0 => None,
      _ => self.rows.get(self.scroll_offset).map(|row| row.seq),
    };
    self.rows = rows;
    self.scroll_offset = match anchor {
      Some(seq) => self
        .rows
        .iter()
        .position(|row| row.seq == seq)
        .unwrap_or_else(|| self.last_page_start()),
      None => 0,
    };
  }

  /// The largest `scroll_offset` the render honours: past it the page
  /// starts here instead, so a scroll parked there reads as a dead key.
  fn last_page_start(&self) -> usize {
    self.rows.len().saturating_sub(self.page_rows.get().max(1))
  }

  /// Drop what is shown, scroll included, when focus moves to a row the
  /// data is not for.
  pub fn clear(&mut self) {
    *self = Self::default();
  }

  /// Toward the newest request.
  pub fn scroll_up(&mut self) {
    // A page that grew since — a taller pane, a summary strip that wrapped into
    // fewer lines — can leave the offset past the last page the render
    // honours, where a press would be clamped back and read as a dead key.
    let parked = self.scroll_offset.min(self.last_page_start());
    self.scroll_offset = parked.saturating_sub(1);
  }

  /// Toward older requests.
  pub fn scroll_down(&mut self) {
    self.scroll_offset = self
      .scroll_offset
      .saturating_add(1)
      .min(self.last_page_start());
  }
}

/// The right pane owns the block.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &RequestsTabState, palette: &Palette) {
  // No poll has landed for this scope yet. Zeros and "no requests" would
  // read as an answer.
  if state.scope.is_none() {
    let waiting = Line::from(Span::styled("loading", palette.muted_style()));
    frame.render_widget(Paragraph::new(waiting), area);
    return;
  }
  let width = area.width as usize;
  let mut lines = summary_lines(&state.summary, width, palette);
  lines.push(Line::default());
  if state.rows.is_empty() {
    lines.push(Line::from(Span::styled(
      // The Chat / Embed / Rerank tabs talk to the launch's own port, so
      // what they send does not show here.
      "no proxy requests for this model yet",
      palette.muted_style(),
    )));
  } else {
    let layout = layout_columns(width);
    lines.push(header_line(&layout, palette));
    let room = (area.height as usize).saturating_sub(lines.len());
    state.page_rows.set(room.max(1));
    // Keep a full page on screen when the offset runs past the end.
    let offset = state
      .scroll_offset
      .min(state.rows.len().saturating_sub(room));
    lines.extend(
      state
        .rows
        .iter()
        .skip(offset)
        .map(|row| row_line(row, &layout, palette))
        .take(room),
    );
  }
  frame.render_widget(Paragraph::new(lines), area);
}

/// The summary figures as `Label value` cells, in the two rows the layout
/// asks for, each wrapped to `width` on its own. A figure is never split
/// across lines. An aggregate rides inside the average it belongs to as
/// `Avg tok/s 48.4 (Agg 56.1 over 12.0s)`, with the wall it averages over kept
/// unbold so it reads like a label.
fn summary_lines(summary: &RequestSummary, width: usize, palette: &Palette) -> Vec<Line<'static>> {
  let mut lines: Vec<Line<'static>> = Vec::new();
  for row in summary.cell_rows(NONE, Aggregates::WhenTheyDiffer) {
    let mut cells: Vec<(&'static str, Vec<Span<'static>>)> = Vec::new();
    for (label, value) in row {
      let value_style = if label == "Errors" && summary.errors > 0 {
        palette.error_style()
      } else {
        palette.text_style().add_modifier(Modifier::BOLD)
      };
      // Its average is always earlier in the row, but not always the cell
      // before it: the session figure can sit between the two.
      let parent = label
        .strip_prefix("Agg ")
        .and_then(|rest| cells.iter().position(|(l, _)| *l == format!("Avg {rest}")));
      if let Some(i) = parent {
        let (speed, wall) = value.split_once(' ').unwrap_or((value.as_str(), ""));
        let cell = &mut cells[i].1;
        cell.push(Span::styled(" (Agg ", palette.muted_style()));
        cell.push(Span::styled(speed.to_string(), value_style));
        cell.push(Span::styled(
          if wall.is_empty() {
            ")".to_string()
          } else {
            format!(" {wall})")
          },
          palette.muted_style(),
        ));
      } else {
        cells.push((label, vec![Span::styled(value, value_style)]));
      }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (label, value) in cells {
      let value_w: usize = value.iter().map(|s| s.content.chars().count()).sum();
      let label_w = label.chars().count() + 1;
      let cell_w = label_w + value_w;
      if !spans.is_empty() && used + SUMMARY_GAP + cell_w > width {
        lines.push(Line::from(std::mem::take(&mut spans)));
        used = 0;
      }
      if !spans.is_empty() {
        spans.push(Span::raw(" ".repeat(SUMMARY_GAP)));
        used += SUMMARY_GAP;
      }
      spans.push(Span::styled(format!("{label} "), palette.muted_style()));
      spans.extend(value);
      used += cell_w;
    }
    if !spans.is_empty() {
      lines.push(Line::from(spans));
    }
  }
  lines
}

/// The columns one render pass shows, plus the width the flexible `Note`
/// column got (0 when it does not show). `Route` keeps its own width.
struct ColumnLayout {
  visible: Vec<&'static Column<ColumnId>>,
  note_w: usize,
}

impl ColumnLayout {
  fn width_of(&self, column: &Column<ColumnId>) -> usize {
    if column.id == ColumnId::Note {
      self.note_w
    } else {
      column.width
    }
  }
}

fn layout_columns(content_w: usize) -> ColumnLayout {
  let (visible, spent) = columns::fit(COLUMNS, content_w, COL_SEP_W);
  // The separator `fit` counted after the last column is the absorber's to use too.
  let spare = content_w.saturating_sub(spent) + COL_SEP_W;
  let note_w = if visible.iter().any(|c| c.id == ColumnId::Note) {
    MIN_NOTE_W + spare
  } else {
    0
  };
  ColumnLayout { visible, note_w }
}

fn header_line(layout: &ColumnLayout, palette: &Palette) -> Line<'static> {
  let labels: Vec<String> = layout
    .visible
    .iter()
    .map(|c| cell(c.label, layout.width_of(c)))
    .collect();
  Line::from(Span::styled(
    labels.join(" "),
    palette.muted_style().add_modifier(Modifier::BOLD),
  ))
}

/// One row on one line. A `Note` longer than its column keeps its first
/// words and ends in an ellipsis, so a long engine cause never costs the
/// rows below it a line.
fn row_line(row: &RequestRow, layout: &ColumnLayout, palette: &Palette) -> Line<'static> {
  let cells = row.cells(NONE);
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(layout.visible.len());
  for c in &layout.visible {
    let text = format!(
      "{} ",
      if c.id == ColumnId::Note {
        cell_words(&cells.note, layout.note_w)
      } else {
        cell(column_value(c.id, &cells), layout.width_of(c))
      }
    );
    let style = match c.id {
      ColumnId::Status if row.is_error() => palette.error_style(),
      ColumnId::Status if row.status.is_some() => palette.success_style(),
      ColumnId::Note => palette.muted_style(),
      _ => palette.text_style(),
    };
    spans.push(Span::styled(text, style));
  }
  Line::from(spans)
}

fn column_value(id: ColumnId, cells: &RequestCells) -> &str {
  match id {
    ColumnId::Time => &cells.time,
    ColumnId::Status => &cells.status,
    ColumnId::Speed => &cells.speed,
    ColumnId::Prefill => &cells.prefill,
    ColumnId::Total => &cells.total,
    ColumnId::In => &cells.tokens_in,
    ColumnId::Cache => &cells.cache_hit,
    ColumnId::Out => &cells.tokens_out,
    ColumnId::Ttfb => &cells.ttfb,
    ColumnId::Client => &cells.client,
    ColumnId::RequestFor => &cells.request_for,
    ColumnId::Note => &cells.note,
    ColumnId::Route => &cells.route,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::proxy::request_log::RequestState;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  #[test]
  fn the_poll_params_name_exactly_one_launch() {
    assert_eq!(
      RequestsScope::new("L7").params(50),
      serde_json::json!({ "launch_id": "L7", "limit": 50 })
    );
  }

  fn row(seq: u64) -> RequestRow {
    RequestRow {
      seq,
      started_at_ms: 1_700_000_000_000 + seq * 1000,
      client: Some("127.0.0.1:5000".to_string()),
      route: "/v1/chat/completions".to_string(),
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(180),
      duration_ms: Some(1_240),
      prompt_tokens: Some(41),
      completion_tokens: Some(7),
      tokens_per_second: Some(39.64),
      ..RequestRow::default()
    }
  }

  fn palette() -> &'static Palette {
    crate::theme::palette_for(crate::theme::ThemeName::Macchiato)
  }

  fn labels(width: usize) -> Vec<&'static str> {
    layout_columns(width)
      .visible
      .iter()
      .map(|c| c.label)
      .collect()
  }

  fn rendered(state: &RequestsTabState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let palette = palette();
    terminal
      .draw(|f| render(f, f.area(), state, palette))
      .unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..height)
      .map(|y| {
        (0..width)
          .map(|x| buf[(x, y)].symbol().to_string())
          .collect::<String>()
          .trim_end()
          .to_string()
      })
      .collect()
  }

  #[test]
  fn columns_drop_by_rank_as_the_pane_narrows() {
    // Each column costs its width plus one separator. Running totals in
    // rank order: Time 9, Code 14, Tok/s 21, Total 29, TTFB 37, In 44,
    // Out 51, Cache 58, PP/s 65, Note 76, Request for 101, Client 123, Route 140.
    // `Note` shows last but outranks the four columns before it, so it stays
    // when they have gone.
    let full = [
      "Time",
      "Code",
      "Tok/s",
      "PP/s",
      "Total",
      "TTFB",
      "In",
      "Cache",
      "Out",
      "Request for",
      "Client",
      "Route",
      "Note",
    ];
    assert_eq!(labels(140), full);
    // Route ranks highest of all, so it is the first to go.
    let no_route = [
      "Time",
      "Code",
      "Tok/s",
      "PP/s",
      "Total",
      "TTFB",
      "In",
      "Cache",
      "Out",
      "Request for",
      "Client",
      "Note",
    ];
    assert_eq!(labels(139), no_route);
    assert_eq!(labels(123), no_route);
    let no_client = [
      "Time",
      "Code",
      "Tok/s",
      "PP/s",
      "Total",
      "TTFB",
      "In",
      "Cache",
      "Out",
      "Request for",
      "Note",
    ];
    assert_eq!(labels(122), no_client);
    assert_eq!(labels(101), no_client);
    // `Note` is the next to go, and `PP/s` outlasts it.
    let no_request_for = [
      "Time", "Code", "Tok/s", "PP/s", "Total", "TTFB", "In", "Cache", "Out", "Note",
    ];
    assert_eq!(labels(100), no_request_for);
    assert_eq!(labels(76), no_request_for);
    let no_note = [
      "Time", "Code", "Tok/s", "PP/s", "Total", "TTFB", "In", "Cache", "Out",
    ];
    assert_eq!(labels(75), no_note);
    assert_eq!(labels(65), no_note);
    // `Cache` outlasts `PP/s` and `Note`, and gives way before `Out`.
    assert_eq!(
      labels(64),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Cache", "Out"]
    );
    assert_eq!(
      labels(58),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Cache", "Out"]
    );
    assert_eq!(
      labels(57),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out"]
    );
    assert_eq!(
      labels(51),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out"]
    );
    assert_eq!(
      labels(50),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In"]
    );
    assert_eq!(labels(43), vec!["Time", "Code", "Tok/s", "Total", "TTFB"]);
    assert_eq!(labels(36), vec!["Time", "Code", "Tok/s", "Total"]);
    assert_eq!(labels(28), vec!["Time", "Code", "Tok/s"]);
    assert_eq!(labels(21), vec!["Time", "Code", "Tok/s"]);
    assert_eq!(labels(20), vec!["Time", "Code"]);
    assert_eq!(labels(9), vec!["Time"]);
    assert_eq!(labels(8), Vec::<&str>::new());
  }

  #[test]
  fn note_absorbs_the_spare_width() {
    // The visible columns plus their separators fill the pane, with one
    // cell of slack for the separator after the last column.
    for width in [95usize, 100, 110, 120, 160, 200] {
      let layout = layout_columns(width);
      let spent: usize = layout
        .visible
        .iter()
        .map(|c| layout.width_of(c) + COL_SEP_W)
        .sum();
      assert_eq!(spent, width + 1, "width {width}");
      assert!(layout.note_w >= MIN_NOTE_W, "width {width}");
    }
    // A wide pane gives `Note` everything past the fixed columns, so a long
    // engine cause shows more; `Route` keeps its own width.
    let wide = layout_columns(200);
    assert!(wide.note_w > 60, "{}", wide.note_w);
    let route = wide
      .visible
      .iter()
      .find(|c| c.id == ColumnId::Route)
      .expect("route column");
    assert_eq!(wide.width_of(route), ROUTE_W);
    // At the width `Request for` just fits, the only spare cell is the
    // separator after the last column, and `Note` grows first.
    assert_eq!(layout_columns(101).note_w, MIN_NOTE_W + 1);
    // Too narrow for `Note`: it gets nothing and the row ends earlier.
    assert_eq!(layout_columns(61).note_w, 0);
  }

  #[test]
  fn note_shows_after_route_and_takes_the_spare_width() {
    let wide = layout_columns(160);
    let labels: Vec<&str> = wide.visible.iter().map(|c| c.label).collect();
    assert_eq!(
      labels,
      vec![
        "Time",
        "Code",
        "Tok/s",
        "PP/s",
        "Total",
        "TTFB",
        "In",
        "Cache",
        "Out",
        "Request for",
        "Client",
        "Route",
        "Note"
      ]
    );

    // The header and a row line up, and a long note runs to the right edge
    // instead of pushing the fixed columns.
    let mut state = RequestsTabState::default();
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      vec![RequestRow {
        requested_model: Some("asked-7b".to_string()),
        error: Some("launch_failed".to_string()),
        cause: Some("lower --ctx-size".to_string()),
        status: Some(502),
        ..row(1)
      }],
    );
    let lines = rendered(&state, 160, 6);
    let header = lines.iter().find(|l| l.contains("Route")).expect("header");
    let body = lines
      .iter()
      .find(|l| l.contains("chat/completions"))
      .expect("row");
    let route_at = header.find("Route").unwrap();
    let note_at = header.find("Note").unwrap();
    assert!(route_at < note_at, "{header}");
    // Measured in cells, because the row's placeholder is not one byte wide.
    assert_eq!(
      console::measure_text_width(&body[..body.find("chat/completions").unwrap()]),
      route_at,
      "header: {header}\nrow: {body}"
    );
    let note = "launch_failed: lower --ctx-size";
    assert!(body.contains(note), "{body}");
    // `Note` gets the whole width past `Route`, not just its own 10 cells,
    // so the free text reads without an ellipsis here.
    assert!(body.trim_end().ends_with(note), "{body}");
    assert!(
      body.find(note).unwrap() + note.len() > route_at + ROUTE_W,
      "note_w is wider than Route: {body}"
    );
    assert!(
      console::measure_text_width(body) <= 160,
      "{} cells",
      console::measure_text_width(body)
    );
  }

  #[test]
  fn request_for_gets_the_width_the_cli_gives_a_name() {
    let wide = layout_columns(200);
    let asked = wide
      .visible
      .iter()
      .find(|c| c.id == ColumnId::RequestFor)
      .expect("request for column");
    assert_eq!(wide.width_of(asked), 24);

    // A name that fits the column reaches the screen whole; a longer one
    // cuts, and the row still renders.
    let whole = "n".repeat(24);
    let mut state = RequestsTabState::default();
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      vec![
        RequestRow {
          requested_model: Some(whole.clone()),
          ..row(2)
        },
        RequestRow {
          requested_model: Some("x".repeat(60)),
          ..row(1)
        },
      ],
    );
    let lines = rendered(&state, 150, 8);
    assert!(lines.iter().any(|l| l.contains(&whole)), "{lines:?}");
    assert!(
      !lines.iter().any(|l| l.contains(&"x".repeat(30))),
      "{lines:?}"
    );
  }

  #[test]
  fn summary_wraps_without_splitting_a_figure() {
    let palette = palette();
    let summary = RequestSummary {
      requests: 12,
      errors: 1,
      avg_duration_ms: Some(1_240),
      ..RequestSummary::default()
    };
    let wide = summary_lines(&summary, 200, palette);
    // Two rows: the counts, then the speeds and token totals.
    assert_eq!(wide.len(), 2, "{wide:?}");
    assert!(wide[0].to_string().starts_with("Hosts "), "{wide:?}");
    assert!(wide[1].to_string().starts_with("Avg tok/s "), "{wide:?}");
    let narrow = summary_lines(&summary, 40, palette);
    assert!(narrow.len() > 2, "{narrow:?}");
    for line in &narrow {
      assert!(line.width() <= 40, "{line:?}");
    }
    let text: String = narrow
      .iter()
      .map(|l| l.to_string())
      .collect::<Vec<_>>()
      .join("\n");
    assert!(text.contains("Requests 12"), "{text}");
    assert!(text.contains("Avg total 1.2s"), "{text}");
    assert!(text.contains("Avg tok/s —"), "{text}");
  }

  #[test]
  fn summary_folds_an_aggregate_into_its_own_average() {
    let palette = palette();
    let summary = RequestSummary {
      tokens_per_second_avg: Some(48.4),
      tokens_per_second_aggregate: Some(56.1),
      busy_ms: Some(12_000),
      prefill_tokens_per_second: Some(1_379.0),
      prefill_tokens_per_second_aggregate: Some(1_200.0),
      prefill_busy_ms: Some(12_000),
      ..RequestSummary::default()
    };
    let lines = summary_lines(&summary, 200, palette);
    let text = lines[1].to_string();
    assert!(
      text.contains("Avg tok/s 48.4 (Agg 56.1 over 12.0s)"),
      "{text}"
    );
    assert!(
      text.contains("Avg PP tok/s 1379 (Agg 1200 over 12.0s)"),
      "{text}"
    );
    // The wall reads like a label: only the speed carries the value style.
    let bold: Vec<String> = lines[1]
      .spans
      .iter()
      .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
      .map(|s| s.content.to_string())
      .collect();
    assert!(bold.contains(&"56.1".to_string()), "{bold:?}");
    assert!(!bold.iter().any(|s| s.contains("over")), "{bold:?}");

    // An aggregate that reads as its own average still drops out of the strip.
    let quiet = RequestSummary {
      tokens_per_second_avg: Some(48.4),
      tokens_per_second_aggregate: Some(48.9),
      busy_ms: Some(12_000),
      ..RequestSummary::default()
    };
    let quiet_text = summary_lines(&quiet, 200, palette)[1].to_string();
    assert!(quiet_text.starts_with("Avg tok/s 48.4 "), "{quiet_text}");
    assert!(!quiet_text.contains("Agg"), "{quiet_text}");
  }

  #[test]
  fn renders_the_summary_then_the_table_newest_first() {
    let mut state = RequestsTabState::default();
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary {
        requests: 2,
        ..RequestSummary::default()
      },
      vec![
        RequestRow {
          requested_model: Some("asked-7b".to_string()),
          ..row(2)
        },
        row(1),
      ],
    );
    let lines = rendered(&state, 150, 8);
    assert!(lines[0].contains("Requests 2"), "{lines:?}");
    assert!(lines[1].starts_with("Avg tok/s "), "{lines:?}");
    // The summary wraps to as many lines as it needs; the table starts
    // one blank line under it.
    let header = lines
      .iter()
      .position(|l| l.starts_with("Time     Code Tok/s  PP/s"))
      .expect("header row");
    assert_eq!(lines[header - 1], "");
    assert!(lines[header].ends_with("Note"), "{lines:?}");
    assert!(lines[header].contains("Request for"), "{lines:?}");
    // The row has no prefill figure, so its cell is the placeholder.
    assert!(
      lines[header + 1].contains("200  39.6   —      1.2s"),
      "{lines:?}"
    );
    assert!(lines[header + 1].contains("asked-7b"), "{lines:?}");
    assert!(lines[header + 1].contains("chat/completions"), "{lines:?}");
    // Two rows, newest (`seq` 2, one second later) first.
    assert_eq!(&lines[header + 1][..8], row(2).cells(NONE).time);
    assert_eq!(&lines[header + 2][..8], row(1).cells(NONE).time);
    assert_eq!(lines[header + 3], "");
  }

  #[test]
  fn a_long_note_keeps_its_first_words() {
    let mut state = RequestsTabState::default();
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      vec![RequestRow {
        seq: 1,
        started_at_ms: 1_700_000_000_000,
        route: "/v1/chat/completions".to_string(),
        state: RequestState::Done,
        status: Some(503),
        error: Some("launch_failed".to_string()),
        cause: Some(
          "auto-start of Qwen3.8-27B failed: could not allocate the KV cache".to_string(),
        ),
        ..RequestRow::default()
      }],
    );
    let lines = rendered(&state, 120, 10);
    let header = lines
      .iter()
      .position(|l| l.starts_with("Time     Code Tok/s  PP/s"))
      .expect("header row");
    let ellipsis = crate::tui::glyphs::active().ellipsis();
    let row_line = lines[header + 1].trim_end();
    assert!(row_line.contains("launch_failed:"), "{row_line:?}");
    assert!(row_line.contains(ellipsis), "{row_line:?}");
    assert!(!row_line.contains("KV cache"), "{row_line:?}");
    // One row is one line: nothing follows it but the empty rows below.
    assert_eq!(lines[header + 2], "");
  }

  #[test]
  fn an_empty_log_says_so_under_the_summary() {
    let mut state = RequestsTabState::default();
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      Vec::new(),
    );
    let lines = rendered(&state, 80, 6);
    assert!(lines[0].contains("Requests 0"), "{lines:?}");
    assert!(
      lines
        .iter()
        .any(|l| l == "no proxy requests for this model yet"),
      "{lines:?}"
    );
  }

  #[test]
  fn before_the_first_poll_lands_the_tab_says_loading() {
    let lines = rendered(&RequestsTabState::default(), 80, 6);
    assert_eq!(lines[0], "loading");
    assert!(lines[1..].iter().all(String::is_empty), "{lines:?}");
  }

  #[test]
  fn scroll_moves_toward_older_rows_and_stops_at_the_last_page() {
    let mut state = RequestsTabState::default();
    let rows: Vec<RequestRow> = (1..=10).rev().map(row).collect();
    state.set(RequestsScope::new("L-1"), RequestSummary::default(), rows);
    state.scroll_up();
    assert_eq!(state.scroll_offset, 0);
    for _ in 0..50 {
      state.scroll_down();
    }
    assert_eq!(state.scroll_offset, 9);
    // 2 lines of summary, blank and header leave 2 for rows: the last
    // page is the 2 oldest, not a single row with blank space under it.
    let lines = rendered(&state, 200, 6);
    assert_eq!(&lines[4][..8], row(2).cells(NONE).time);
    assert_eq!(&lines[5][..8], row(1).cells(NONE).time);
  }

  #[test]
  fn a_long_note_costs_no_extra_line() {
    let rows: Vec<RequestRow> = (1..=6)
      .rev()
      .map(|seq| RequestRow {
        status: Some(503),
        error: Some("launch_failed".to_string()),
        cause: Some(
          "auto-start of Qwen3.8-27B failed: could not allocate the KV cache".to_string(),
        ),
        ..row(seq)
      })
      .collect();
    let mut state = RequestsTabState::default();
    state.set(RequestsScope::new("L-1"), RequestSummary::default(), rows);
    for _ in 0..50 {
      state.scroll_down();
    }
    let ellipsis = crate::tui::glyphs::active().ellipsis();
    // The page is counted in rows, so the last page ends on the oldest
    // request and every line under the header is a row of its own.
    let lines = rendered(&state, 120, 7);
    let header = lines
      .iter()
      .position(|l| l.starts_with("Time     Code Tok/s  PP/s"))
      .expect("header row");
    let rows_shown = &lines[header + 1..];
    assert!(rows_shown.len() >= 2, "{lines:?}");
    assert!(rows_shown[0].contains(ellipsis), "{lines:?}");
    assert_eq!(
      &rows_shown[rows_shown.len() - 1][..8],
      row(1).cells(NONE).time
    );
    for line in rows_shown {
      assert!(line.starts_with(|c: char| c.is_ascii_digit()), "{line:?}");
    }
  }

  #[test]
  fn set_clamps_the_scroll_and_clear_resets_it() {
    let mut state = RequestsTabState::default();
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      (1..=10).rev().map(row).collect(),
    );
    state.scroll_offset = 8;
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      (1..=4).rev().map(row).collect(),
    );
    // The row on top (seq 2) is still there, so it stays on top.
    assert_eq!(state.scroll_offset, 2);
    state.scroll_offset = 3;
    state.set(
      RequestsScope::new("L-1"),
      RequestSummary::default(),
      vec![row(9)],
    );
    assert_eq!(state.scroll_offset, 0);
    state.clear();
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.scope, None);
  }

  #[test]
  fn an_evicted_anchor_parks_on_the_last_page_not_past_it() {
    let scope = || RequestsScope::new("L-1");
    let mut state = RequestsTabState::default();
    state.set(
      scope(),
      RequestSummary::default(),
      (1..=200).rev().map(row).collect(),
    );
    // The render fits two rows here, and that is what a scroll stops at.
    rendered(&state, 200, 6);
    for _ in 0..500 {
      state.scroll_down();
    }
    assert_eq!(state.scroll_offset, 198);
    // A window that no longer holds the row on top parks on the last page,
    // and the next Up moves it.
    state.set(
      scope(),
      RequestSummary::default(),
      (1..=200).rev().map(|seq| row(seq + 1000)).collect(),
    );
    assert_eq!(state.scroll_offset, 198);
    let before = rendered(&state, 200, 6)[4].clone();
    state.scroll_up();
    let after = rendered(&state, 200, 6);
    assert_ne!(before, after[4], "{after:?}");
  }

  #[test]
  fn a_page_that_grew_does_not_park_the_scroll_past_itself() {
    let scope = || RequestsScope::new("L-1");
    let mut state = RequestsTabState::default();
    state.set(
      scope(),
      RequestSummary::default(),
      (1..=200).rev().map(row).collect(),
    );
    // A short pane fits two rows, and scrolling stops on the last of them.
    rendered(&state, 200, 6);
    for _ in 0..50 {
      state.scroll_down();
    }
    // The pane grows, so the page the render honours starts higher up than the
    // parked offset. The next Up has to move the table, not be clamped back.
    let before = rendered(&state, 200, 26);
    state.scroll_up();
    let after = rendered(&state, 200, 26);
    assert_ne!(before[4], after[4], "{after:?}");
  }

  #[test]
  fn a_poll_holds_the_top_row_and_follows_the_newest_only_at_the_top() {
    let scope = || RequestsScope::new("L-1");
    let mut state = RequestsTabState::default();
    state.set(
      scope(),
      RequestSummary::default(),
      (1..=10).rev().map(row).collect(),
    );
    state.scroll_down();
    let top = state.rows[state.scroll_offset].seq;
    state.set(
      scope(),
      RequestSummary::default(),
      (1..=11).rev().map(row).collect(),
    );
    assert_eq!(state.rows[state.scroll_offset].seq, top);
    state.scroll_offset = 0;
    state.set(
      scope(),
      RequestSummary::default(),
      (1..=12).rev().map(row).collect(),
    );
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.rows[0].seq, 12, "at the top the newest leads");
  }
}
