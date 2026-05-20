//! HuggingFace pull dialog (Unit 4 / R104–R109).
//!
//! Three-state modal overlay: Search → File picker → Confirm. State
//! transitions are pure functions so the unit tests can exercise them
//! without a tokio runtime; the async dispatch shim that fires
//! `init::hf_api::search` / `list_repo_files` lives in `events.rs` and
//! ships results back via the `mpsc::UnboundedSender<HfDialogEvent>`
//! the dialog state owns.
//!
//! Search is debounced (300 ms after the last keystroke). Each
//! dispatch is tagged with the dialog's monotonic `query_seq`; the
//! response handler drops results whose stamp is older than the
//! current seq so a late reply from a stale query doesn't flicker the
//! results pane.

use std::time::Instant;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::init::download::RepoSpec;
use crate::init::fetch::FetchError;
use crate::init::hf_api::{HfRepoFile, HfSearchPage, HfSearchResult, HfSortKey, ListRepoFilesError};
use crate::theme::Palette;

/// Three-state modal contract (R105).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfStage {
  Search,
  FilePicker,
  Confirm,
}

/// Lookup state for the File picker — the dialog asks the network
/// task to fetch the sibling list for a chosen `repo_id` and re-renders
/// once results arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerLoad {
  /// Picker hasn't requested files yet (initial state).
  Idle,
  /// A `list_repo_files` task is in flight.
  Loading,
  /// Files arrived; the picker iterates over `files` (after the
  /// shard-collapse pass from Unit 5).
  Ready,
  /// Listing failed; the user can back up to Search to retry.
  Failed(String),
}

/// Events the dialog drains from background tasks (search, repo
/// listing). Each one carries the `seq` it was tagged with at
/// dispatch time so stale responses can be dropped.
#[derive(Debug)]
pub enum HfDialogEvent {
  SearchResults {
    seq: u64,
    page: HfSearchPage,
  },
  SearchFailed {
    seq: u64,
    error: FetchError,
  },
  RepoFiles {
    repo_id: String,
    files: Vec<HfRepoFile>,
  },
  RepoFilesFailed {
    repo_id: String,
    error: ListRepoFilesError,
  },
}

/// Owned by `App` as `Option<HfDialogState>` — `None` when the dialog
/// is closed, `Some` when open. Not `Clone`; the modal exists at most
/// once.
#[derive(Debug)]
pub struct HfDialogState {
  pub stage: HfStage,
  /// User-typed query buffer. Debounced; an in-flight task may still
  /// be running against an older snapshot of this buffer.
  pub query: String,
  /// Monotonic dispatch counter. Bumped on every keystroke so a
  /// background search response that arrives after a newer keystroke
  /// can be discarded.
  pub query_seq: u64,
  /// Last `query_seq` value a network task was actually dispatched
  /// for. The drain compares against the response's seq to decide
  /// whether to apply or drop.
  pub last_dispatched_seq: u64,
  /// Last time the user touched the query buffer. Drives the
  /// debounce — a dispatch fires once 300 ms has elapsed without a
  /// new keystroke.
  pub last_keystroke_at: Instant,
  pub sort: HfSortKey,
  /// Opaque cursor that was used to fetch the currently-displayed
  /// page. `None` for page 1; mutated on every advance / retreat.
  pub current_cursor: Option<String>,
  /// Cursor parsed from the current response's `Link: rel="next"`
  /// header. Drives the next-page affordance; absent when the prior
  /// fetch under-filled.
  pub next_cursor: Option<String>,
  /// Historical `current_cursor` values, one per previous page. The
  /// retreat-page action pops one off so backward navigation re-fires
  /// the request that produced the prior page.
  pub prev_cursors: Vec<Option<String>>,
  /// 1-indexed page number, surfaced in the page indicator.
  pub page: u32,
  pub results: Vec<HfSearchResult>,
  pub selected_idx: usize,
  /// `true` when a search task is in flight; the search bar renders
  /// a `loading…` hint and arrow keys keep working over the stale
  /// results list.
  pub search_in_flight: bool,
  /// Most recent search error to surface inline (rate-limit, offline,
  /// transport). Cleared on next successful search.
  pub error: Option<String>,
  /// Repo selected for the File picker (either from search results or
  /// a pasted `owner/repo` slug).
  pub picker_repo_id: Option<String>,
  pub picker_load: PickerLoad,
  pub picker_files: Vec<HfRepoFile>,
  pub picker_idx: usize,
  /// File selected from the picker, surfaced on Confirm.
  pub confirm_file: Option<HfRepoFile>,
  /// `true` when the FetchClient is offline so the search bar can
  /// render an "offline — paste a repo ID …" hint immediately.
  pub offline: bool,
  /// Drain endpoint for background tasks. Created on `open`.
  pub event_rx: mpsc::UnboundedReceiver<HfDialogEvent>,
  /// Clonable sender background tasks use to ship results back.
  pub event_tx: mpsc::UnboundedSender<HfDialogEvent>,
}

/// Debounce window — once this elapses after the last keystroke, the
/// dialog dispatches the buffered query as a search (R107 / live
/// search). Matches the HuggingFace web UI cadence.
pub const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

impl HfDialogState {
  /// Construct a fresh dialog in the Search stage.
  pub fn open(offline: bool) -> Self {
    let (tx, rx) = mpsc::unbounded_channel();
    Self {
      stage: HfStage::Search,
      query: String::new(),
      query_seq: 0,
      last_dispatched_seq: 0,
      last_keystroke_at: Instant::now(),
      sort: HfSortKey::Downloads,
      current_cursor: None,
      next_cursor: None,
      prev_cursors: Vec::new(),
      page: 1,
      results: Vec::new(),
      selected_idx: 0,
      search_in_flight: false,
      error: None,
      picker_repo_id: None,
      picker_load: PickerLoad::Idle,
      picker_files: Vec::new(),
      picker_idx: 0,
      confirm_file: None,
      offline,
      event_rx: rx,
      event_tx: tx,
    }
  }

  // ----- Search state transitions -----

  /// Append a character to the query and bump the seq so any
  /// in-flight task's result is treated as stale.
  pub fn insert(&mut self, ch: char) {
    self.query.push(ch);
    self.query_seq = self.query_seq.saturating_add(1);
    self.last_keystroke_at = Instant::now();
    self.error = None;
  }

  /// Delete the trailing character. Same seq-bump semantics as
  /// [`Self::insert`] so the search bar's `loading…` indicator
  /// reflects the new query.
  pub fn backspace_query(&mut self) {
    if self.query.pop().is_some() {
      self.query_seq = self.query_seq.saturating_add(1);
      self.last_keystroke_at = Instant::now();
      self.error = None;
    }
  }

  /// Cycle to the next sort key (R107). Resets pagination to page 1
  /// and bumps the seq so a stale search-by-old-sort response can't
  /// land.
  pub fn cycle_sort(&mut self) {
    self.sort = self.sort.cycle_next();
    self.current_cursor = None;
    self.next_cursor = None;
    self.prev_cursors.clear();
    self.page = 1;
    self.query_seq = self.query_seq.saturating_add(1);
    self.last_keystroke_at = Instant::now();
  }

  /// `true` when a debounced dispatch should fire — the buffer has a
  /// non-empty query, `DEBOUNCE` has elapsed since the last keystroke,
  /// and the current seq hasn't already been dispatched. Caller
  /// records the seq via [`Self::mark_dispatched`] when it spawns the
  /// task.
  pub fn search_due(&self, now: Instant) -> bool {
    !self.query.trim().is_empty()
      && now.duration_since(self.last_keystroke_at) >= DEBOUNCE
      && self.query_seq > self.last_dispatched_seq
  }

  /// Record that a search task has been spawned for the current seq.
  pub fn mark_dispatched(&mut self) {
    self.last_dispatched_seq = self.query_seq;
    self.search_in_flight = true;
  }

  /// Apply a SearchResults event. Drops stale responses (seq below
  /// the dialog's current `last_dispatched_seq`) so the user's most
  /// recent dispatch always wins.
  pub fn apply_search_results(&mut self, seq: u64, page: HfSearchPage) {
    if seq < self.last_dispatched_seq {
      return;
    }
    self.search_in_flight = false;
    self.error = None;
    self.results = page.results;
    self.next_cursor = page.next_cursor;
    self.selected_idx = 0;
  }

  /// Apply a SearchFailed event. Same stale-drop rule as
  /// [`Self::apply_search_results`].
  pub fn apply_search_failed(&mut self, seq: u64, error: FetchError) {
    if seq < self.last_dispatched_seq {
      return;
    }
    self.search_in_flight = false;
    self.error = Some(format_fetch_error(&error));
  }

  /// Move the search-result cursor up by one (no-op when no
  /// results).
  pub fn move_up(&mut self) {
    match self.stage {
      HfStage::Search => {
        if self.selected_idx > 0 {
          self.selected_idx -= 1;
        }
      }
      HfStage::FilePicker => {
        if self.picker_idx > 0 {
          self.picker_idx -= 1;
        }
      }
      HfStage::Confirm => {}
    }
  }

  /// Move the cursor down by one, clamping at the row count.
  pub fn move_down(&mut self) {
    match self.stage {
      HfStage::Search => {
        if !self.results.is_empty() && self.selected_idx + 1 < self.results.len() {
          self.selected_idx += 1;
        }
      }
      HfStage::FilePicker => {
        if !self.picker_files.is_empty() && self.picker_idx + 1 < self.picker_files.len() {
          self.picker_idx += 1;
        }
      }
      HfStage::Confirm => {}
    }
  }

  /// Whether a next-page action is sensible (the current response
  /// carried a `Link: rel="next"` cursor).
  pub fn can_next_page(&self) -> bool {
    self.stage == HfStage::Search && self.next_cursor.is_some()
  }

  /// Whether the prev-page action is sensible (we have stored
  /// history of cursors used by previous pages).
  pub fn can_prev_page(&self) -> bool {
    self.stage == HfStage::Search && !self.prev_cursors.is_empty()
  }

  /// Stage the next-page request. The caller spawns the task with
  /// the returned cursor value; the run-loop drain applies the
  /// arriving `HfDialogEvent::SearchResults`. Pushes the cursor that
  /// fetched the current page onto the history stack so a later
  /// retreat can return to it.
  pub fn advance_page(&mut self) -> Option<Option<String>> {
    if !self.can_next_page() {
      return None;
    }
    self.prev_cursors.push(self.current_cursor.clone());
    let to_send = self.next_cursor.take();
    self.current_cursor = to_send.clone();
    self.page = self.page.saturating_add(1);
    self.query_seq = self.query_seq.saturating_add(1);
    self.mark_dispatched();
    Some(to_send)
  }

  /// Step back one page. Pops the cursor that fetched the previous
  /// page off the stack and re-issues with it.
  pub fn retreat_page(&mut self) -> Option<Option<String>> {
    if !self.can_prev_page() {
      return None;
    }
    let prev = self.prev_cursors.pop()?;
    let to_send = prev.clone();
    self.current_cursor = prev;
    self.next_cursor = None;
    self.page = self.page.saturating_sub(1).max(1);
    self.query_seq = self.query_seq.saturating_add(1);
    self.mark_dispatched();
    Some(to_send)
  }

  // ----- Stage transitions -----

  /// Move from Search → FilePicker. Returns the repo id the caller
  /// should spawn `list_repo_files` against. Honours the
  /// slug-shortcut (R106): if the query buffer parses as an
  /// `owner/repo[:filename]` RepoSpec, that wins over the selected
  /// search-result row.
  pub fn submit_search(&mut self) -> Option<String> {
    let slug = RepoSpec::parse(self.query.trim()).ok();
    let repo_id = if let Some(spec) = slug {
      spec.repo_id
    } else {
      self.results.get(self.selected_idx)?.repo_id.clone()
    };
    self.stage = HfStage::FilePicker;
    self.picker_repo_id = Some(repo_id.clone());
    self.picker_files.clear();
    self.picker_idx = 0;
    self.picker_load = PickerLoad::Loading;
    Some(repo_id)
  }

  /// Apply a successful `list_repo_files` response.
  pub fn apply_repo_files(&mut self, repo_id: &str, mut files: Vec<HfRepoFile>) {
    // Drop if the dialog moved on to a different repo.
    if self.picker_repo_id.as_deref() != Some(repo_id) {
      return;
    }
    // GGUF filter — Unit 5 will overlay shard collapse on top.
    files.retain(|f| f.filename.to_ascii_lowercase().ends_with(".gguf"));
    self.picker_load = PickerLoad::Ready;
    self.picker_files = files;
    self.picker_idx = 0;
  }

  /// Apply a `list_repo_files` failure.
  pub fn apply_repo_files_failed(&mut self, repo_id: &str, err: &ListRepoFilesError) {
    if self.picker_repo_id.as_deref() != Some(repo_id) {
      return;
    }
    self.picker_load = PickerLoad::Failed(err.to_string());
  }

  /// Move from FilePicker → Confirm. Returns `true` when a file is
  /// selectable and the transition happened.
  pub fn submit_picker(&mut self) -> bool {
    let Some(file) = self.picker_files.get(self.picker_idx).cloned() else {
      return false;
    };
    self.confirm_file = Some(file);
    self.stage = HfStage::Confirm;
    true
  }

  /// Step from FilePicker back to Search (preserves the query
  /// buffer and the result page).
  pub fn back_to_search(&mut self) {
    self.stage = HfStage::Search;
    self.picker_repo_id = None;
    self.picker_files.clear();
    self.picker_load = PickerLoad::Idle;
    self.picker_idx = 0;
    self.confirm_file = None;
  }

  /// Step from Confirm back to FilePicker.
  pub fn back_to_picker(&mut self) {
    self.stage = HfStage::FilePicker;
    self.confirm_file = None;
  }

  /// Consume the dialog's pending confirm selection (repo +
  /// filename). Caller forwards this to the download orchestrator;
  /// closing the dialog is the caller's job too.
  pub fn take_confirm_target(&self) -> Option<(String, HfRepoFile)> {
    let repo = self.picker_repo_id.clone()?;
    let file = self.confirm_file.clone()?;
    Some((repo, file))
  }
}

fn format_fetch_error(error: &FetchError) -> String {
  match error {
    FetchError::Offline => "offline — search disabled. paste a repo id and press Enter.".into(),
    FetchError::RateLimited { status } => format!("rate-limited by huggingface.co (HTTP {status})"),
    FetchError::HostNotAllowed { host } => format!("host `{host}` not on allowlist"),
    other => format!("search failed: {other}"),
  }
}

// ============================================================
// Render
// ============================================================

/// Paint the dialog centred over `area` (matches the
/// `advanced_panel::render` overlay pattern).
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &HfDialogState, palette: &Palette) {
  let modal = centered_rect(86, 70, area);
  frame.render_widget(Clear, modal);
  crate::tui::render::paint_theme_bg(frame, modal, palette);
  let title = match state.stage {
    HfStage::Search => " Pull from HuggingFace — Search ",
    HfStage::FilePicker => " Pull from HuggingFace — Files ",
    HfStage::Confirm => " Pull from HuggingFace — Confirm ",
  };
  let block = palette.panel_block(title, true);
  frame.render_widget(block.clone(), modal);
  let inner = block.inner(modal);

  let layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(3),
      Constraint::Min(0),
      Constraint::Length(1),
    ])
    .split(inner);

  render_header(frame, layout[0], state, palette);
  match state.stage {
    HfStage::Search => render_search_body(frame, layout[1], state, palette),
    HfStage::FilePicker => render_picker_body(frame, layout[1], state, palette),
    HfStage::Confirm => render_confirm_body(frame, layout[1], state, palette),
  }
  render_footer(frame, layout[2], state, palette);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &HfDialogState, palette: &Palette) {
  let sort_label = match state.sort {
    HfSortKey::Downloads => "↓ downloads",
    HfSortKey::Likes => "♡ likes",
    HfSortKey::RecentlyUpdated => "⏱ recently updated",
    HfSortKey::Trending => "★ trending",
  };
  let label_style = palette.label_style();
  let value_style = palette.text_style();
  let muted = palette.muted_style();
  let mut spans: Vec<Span<'static>> = Vec::new();
  spans.push(Span::styled("search: ", label_style));
  if state.query.is_empty() {
    spans.push(Span::styled("(type a query or paste owner/repo)", muted));
  } else {
    spans.push(Span::styled(state.query.clone(), value_style));
    spans.push(crate::tui::fmt::caret(palette));
  }
  let mut second: Vec<Span<'static>> = Vec::new();
  second.push(Span::styled("sort: ", label_style));
  second.push(Span::styled(sort_label.to_string(), value_style));
  second.push(Span::styled("  ·  ", muted));
  second.push(Span::styled(format!("page {}", state.page), label_style));
  if state.search_in_flight {
    second.push(Span::styled("  loading…".to_string(), muted));
  }
  if state.offline && state.stage == HfStage::Search {
    second.push(Span::styled(
      "  · offline — search disabled".to_string(),
      muted,
    ));
  }
  let lines = vec![
    Line::from(spans),
    Line::from(second),
    Line::from(Span::styled(
      "Enter on a row drills into files. Backspace steps back. Esc closes.",
      muted,
    )),
  ];
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_search_body(
  frame: &mut Frame<'_>,
  area: Rect,
  state: &HfDialogState,
  palette: &Palette,
) {
  if let Some(err) = &state.error {
    let err_line = Paragraph::new(Line::from(Span::styled(
      err.clone(),
      palette.error_style(),
    )))
    .wrap(Wrap { trim: true });
    frame.render_widget(err_line, area);
    return;
  }
  if state.results.is_empty() {
    let message = if state.query.is_empty() {
      "Start typing to search HuggingFace, or paste an owner/repo slug."
    } else if state.search_in_flight {
      "loading…"
    } else {
      "no matches"
    };
    frame.render_widget(
      Paragraph::new(Line::from(Span::styled(message, palette.muted_style())))
        .wrap(Wrap { trim: true }),
      area,
    );
    return;
  }
  let lines: Vec<Line<'static>> = state
    .results
    .iter()
    .enumerate()
    .map(|(idx, r)| render_search_row(idx, idx == state.selected_idx, state.sort, r, palette))
    .collect();
  frame.render_widget(Paragraph::new(lines), area);
}

fn render_search_row(
  _idx: usize,
  selected: bool,
  sort: HfSortKey,
  r: &HfSearchResult,
  palette: &Palette,
) -> Line<'static> {
  let prefix = if selected { "▌ " } else { "  " };
  let mut style = palette.text_style();
  if selected {
    style = style.add_modifier(Modifier::REVERSED);
  }
  let metric = match sort {
    HfSortKey::Downloads => match r.downloads {
      Some(n) => format!("↓ {}", short_count(n)),
      None => "↓ —".into(),
    },
    HfSortKey::Likes => match r.likes {
      Some(n) => format!("♡ {n}"),
      None => "♡ —".into(),
    },
    HfSortKey::RecentlyUpdated => r
      .last_modified
      .as_deref()
      .map(|s| format!("⏱ {}", s.chars().take(10).collect::<String>()))
      .unwrap_or_else(|| "⏱ —".into()),
    HfSortKey::Trending => "★ trending".into(),
  };
  let tag = r
    .pipeline_tag
    .clone()
    .unwrap_or_else(|| "—".to_string());
  Line::from(vec![
    Span::styled(prefix.to_string(), style),
    Span::styled(format!("{:<48}  ", truncate(&r.repo_id, 48)), style),
    Span::styled(format!("{:<22}  ", truncate(&tag, 22)), palette.muted_style()),
    Span::styled(metric, palette.label_style()),
  ])
}

fn render_picker_body(
  frame: &mut Frame<'_>,
  area: Rect,
  state: &HfDialogState,
  palette: &Palette,
) {
  let repo = state
    .picker_repo_id
    .as_deref()
    .unwrap_or("(no repo selected)");
  let mut lines = vec![Line::from(vec![
    Span::styled("repo: ", palette.label_style()),
    Span::styled(repo.to_string(), palette.text_style()),
  ])];
  match &state.picker_load {
    PickerLoad::Idle | PickerLoad::Loading => {
      lines.push(Line::from(Span::styled(
        "loading file list…",
        palette.muted_style(),
      )));
    }
    PickerLoad::Failed(msg) => {
      lines.push(Line::from(Span::styled(
        format!("repo listing failed: {msg}"),
        palette.error_style(),
      )));
      lines.push(Line::from(Span::styled(
        "Backspace returns to Search.",
        palette.muted_style(),
      )));
    }
    PickerLoad::Ready if state.picker_files.is_empty() => {
      lines.push(Line::from(Span::styled(
        "no `.gguf` files in this repo.",
        palette.muted_style(),
      )));
      lines.push(Line::from(Span::styled(
        "Backspace returns to Search.",
        palette.muted_style(),
      )));
    }
    PickerLoad::Ready => {
      for (idx, f) in state.picker_files.iter().enumerate() {
        let selected = idx == state.picker_idx;
        let prefix = if selected { "▌ " } else { "  " };
        let mut style = palette.text_style();
        if selected {
          style = style.add_modifier(Modifier::REVERSED);
        }
        let size = f
          .size_bytes
          .map(crate::tui::fmt::format_bytes)
          .unwrap_or_else(|| "?".into());
        lines.push(Line::from(vec![
          Span::styled(prefix.to_string(), style),
          Span::styled(format!("{:<58}  ", truncate(&f.filename, 58)), style),
          Span::styled(size, palette.label_style()),
        ]));
      }
    }
  }
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_confirm_body(
  frame: &mut Frame<'_>,
  area: Rect,
  state: &HfDialogState,
  palette: &Palette,
) {
  let repo = state
    .picker_repo_id
    .as_deref()
    .unwrap_or("(no repo)");
  let file = state
    .confirm_file
    .as_ref()
    .map(|f| f.filename.clone())
    .unwrap_or_else(|| "(no file)".into());
  let size = state
    .confirm_file
    .as_ref()
    .and_then(|f| f.size_bytes)
    .map(crate::tui::fmt::format_bytes)
    .unwrap_or_else(|| "size unknown until probe".into());
  let lines = vec![
    Line::from(vec![
      Span::styled("repo:  ", palette.label_style()),
      Span::styled(repo.to_string(), palette.text_style()),
    ]),
    Line::from(vec![
      Span::styled("file:  ", palette.label_style()),
      Span::styled(file, palette.text_style()),
    ]),
    Line::from(vec![
      Span::styled("size:  ", palette.label_style()),
      Span::styled(size, palette.text_style()),
    ]),
    Line::from(Span::raw("")),
    Line::from(Span::styled(
      "Press Enter to confirm — the download enqueues in the status strip.",
      palette.muted_style(),
    )),
    Line::from(Span::styled(
      "Backspace returns to the file picker.",
      palette.muted_style(),
    )),
  ];
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &HfDialogState, palette: &Palette) {
  let hints = match state.stage {
    HfStage::Search => "↑/↓: row · Enter: open · o: sort · n/p: page · Esc: close",
    HfStage::FilePicker => "↑/↓: file · Enter: select · Backspace: search · Esc: close",
    HfStage::Confirm => "Enter: pull · Backspace: files · Esc: close",
  };
  let line = Line::from(Span::styled(hints, palette.muted_style()));
  frame.render_widget(Paragraph::new(line).alignment(Alignment::Right), area);
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
  let v = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Percentage((100 - pct_y) / 2),
      Constraint::Percentage(pct_y),
      Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
  Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
      Constraint::Percentage((100 - pct_x) / 2),
      Constraint::Percentage(pct_x),
      Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

/// Truncate with a single-char ellipsis when necessary.
fn truncate(s: &str, max: usize) -> String {
  if s.chars().count() <= max {
    return s.to_string();
  }
  let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
  out.push('…');
  out
}

/// Short K/M/B counter for download / like totals so the row stays
/// scannable without expanding the column.
fn short_count(n: u64) -> String {
  match n {
    0..=999 => n.to_string(),
    1_000..=999_999 => format!("{:.1}K", n as f64 / 1000.0),
    1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
    _ => format!("{:.1}B", n as f64 / 1_000_000_000.0),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  fn fake_result(id: &str) -> HfSearchResult {
    HfSearchResult {
      repo_id: id.into(),
      downloads: Some(1_234_567),
      likes: Some(42),
      last_modified: Some("2026-04-18T12:00:00Z".into()),
      pipeline_tag: Some("text-generation".into()),
      tags: vec!["gguf".into()],
    }
  }

  #[test]
  fn open_starts_in_search_stage() {
    let s = HfDialogState::open(false);
    assert_eq!(s.stage, HfStage::Search);
    assert!(s.query.is_empty());
    assert_eq!(s.sort, HfSortKey::Downloads);
    assert_eq!(s.page, 1);
    assert!(!s.offline);
  }

  #[test]
  fn typing_bumps_seq_so_late_responses_are_dropped() {
    let mut s = HfDialogState::open(false);
    s.insert('q');
    let seq_after_first = s.query_seq;
    s.insert('w');
    assert!(s.query_seq > seq_after_first);
    // Mark the most recent typed seq dispatched.
    s.mark_dispatched();
    // A stale response (seq from before the second keystroke)
    // must be ignored.
    s.apply_search_results(
      seq_after_first,
      HfSearchPage {
        results: vec![fake_result("stale/repo")],
        next_cursor: None,
      },
    );
    assert!(s.results.is_empty(), "stale response leaked into state");
    // A fresh response wins.
    s.apply_search_results(
      s.last_dispatched_seq,
      HfSearchPage {
        results: vec![fake_result("fresh/repo")],
        next_cursor: None,
      },
    );
    assert_eq!(s.results.len(), 1);
    assert_eq!(s.results[0].repo_id, "fresh/repo");
  }

  #[test]
  fn search_due_requires_debounce_window_to_elapse() {
    let mut s = HfDialogState::open(false);
    s.insert('q');
    let now = s.last_keystroke_at;
    assert!(!s.search_due(now), "immediate dispatch would defeat the debounce");
    assert!(s.search_due(now + DEBOUNCE));
  }

  #[test]
  fn empty_query_never_dispatches() {
    let s = HfDialogState::open(false);
    assert!(!s.search_due(Instant::now() + DEBOUNCE + Duration::from_secs(5)));
  }

  #[test]
  fn cycle_sort_walks_all_four_back_to_downloads() {
    let mut s = HfDialogState::open(false);
    let start = s.sort;
    for _ in 0..4 {
      s.cycle_sort();
    }
    assert_eq!(s.sort, start);
    // Cycling resets pagination.
    s.page = 5;
    s.cycle_sort();
    assert_eq!(s.page, 1);
  }

  #[test]
  fn submit_search_prefers_pasted_slug_over_selected_row() {
    let mut s = HfDialogState::open(false);
    s.results = vec![fake_result("from-list/repo")];
    s.query = "owner/typed-repo".into();
    let target = s.submit_search();
    assert_eq!(target.as_deref(), Some("owner/typed-repo"));
    assert_eq!(s.stage, HfStage::FilePicker);
    assert_eq!(s.picker_repo_id.as_deref(), Some("owner/typed-repo"));
  }

  #[test]
  fn submit_search_uses_selected_result_when_query_is_not_a_slug() {
    let mut s = HfDialogState::open(false);
    s.results = vec![
      fake_result("alpha/repo"),
      fake_result("beta/repo"),
    ];
    s.selected_idx = 1;
    s.query = "qwen".into();
    let target = s.submit_search();
    assert_eq!(target.as_deref(), Some("beta/repo"));
  }

  #[test]
  fn submit_search_returns_none_when_no_query_and_no_selection() {
    let mut s = HfDialogState::open(false);
    assert!(s.submit_search().is_none());
    assert_eq!(s.stage, HfStage::Search);
  }

  #[test]
  fn back_to_search_clears_picker_state_but_keeps_query() {
    let mut s = HfDialogState::open(false);
    s.query = "qwen".into();
    s.results = vec![fake_result("a/b")];
    s.submit_search();
    assert_eq!(s.stage, HfStage::FilePicker);
    s.back_to_search();
    assert_eq!(s.stage, HfStage::Search);
    assert_eq!(s.query, "qwen", "query buffer must survive back-step");
    assert!(s.picker_repo_id.is_none());
  }

  #[test]
  fn apply_repo_files_filters_to_gguf_and_drops_stale_repo() {
    let mut s = HfDialogState::open(false);
    s.picker_repo_id = Some("owner/repo".into());
    s.picker_load = PickerLoad::Loading;
    s.apply_repo_files(
      "owner/different",
      vec![HfRepoFile {
        filename: "file.gguf".into(),
        size_bytes: None,
      }],
    );
    assert!(s.picker_files.is_empty(), "stale repo files leaked through");
    s.apply_repo_files(
      "owner/repo",
      vec![
        HfRepoFile {
          filename: "README.md".into(),
          size_bytes: None,
        },
        HfRepoFile {
          filename: "model.gguf".into(),
          size_bytes: Some(123),
        },
      ],
    );
    assert_eq!(s.picker_files.len(), 1);
    assert_eq!(s.picker_files[0].filename, "model.gguf");
    assert_eq!(s.picker_load, PickerLoad::Ready);
  }

  #[test]
  fn submit_picker_requires_a_selectable_file() {
    let mut s = HfDialogState::open(false);
    assert!(!s.submit_picker());
    s.picker_files = vec![HfRepoFile {
      filename: "x.gguf".into(),
      size_bytes: Some(4096),
    }];
    assert!(s.submit_picker());
    assert_eq!(s.stage, HfStage::Confirm);
    let target = s.take_confirm_target();
    assert!(target.is_none(), "picker_repo_id must be set first");
  }

  #[test]
  fn move_up_and_down_respect_stage() {
    let mut s = HfDialogState::open(false);
    s.results = vec![fake_result("a/b"), fake_result("c/d"), fake_result("e/f")];
    s.move_down();
    assert_eq!(s.selected_idx, 1);
    s.move_down();
    s.move_down();
    assert_eq!(s.selected_idx, 2, "must clamp at last row");
    s.move_up();
    assert_eq!(s.selected_idx, 1);
    // Switch stages; picker has separate cursor.
    s.stage = HfStage::FilePicker;
    s.picker_files = vec![
      HfRepoFile {
        filename: "a.gguf".into(),
        size_bytes: None,
      },
      HfRepoFile {
        filename: "b.gguf".into(),
        size_bytes: None,
      },
    ];
    s.move_down();
    assert_eq!(s.picker_idx, 1);
    // Search cursor untouched.
    assert_eq!(s.selected_idx, 1);
  }

  #[test]
  fn advance_and_retreat_page_track_cursor_history() {
    let mut s = HfDialogState::open(false);
    s.query = "qwen".into();
    // After page 1's response: current_cursor=None, next_cursor=cursor-1.
    s.next_cursor = Some("cursor-1".into());
    let first = s.advance_page();
    assert_eq!(first, Some(Some("cursor-1".into())));
    assert_eq!(s.page, 2);
    assert_eq!(s.current_cursor.as_deref(), Some("cursor-1"));
    assert!(s.can_prev_page());
    // Simulate page 2's response: next_cursor=cursor-2.
    s.next_cursor = Some("cursor-2".into());
    let second = s.advance_page();
    assert_eq!(second, Some(Some("cursor-2".into())));
    assert_eq!(s.page, 3);
    assert_eq!(s.current_cursor.as_deref(), Some("cursor-2"));
    // Retreat: should re-issue using cursor-1 (the cursor that
    // produced page 2). prev_cursors had pushed (None, cursor-1)
    // along the way; pop returns cursor-1.
    let back = s.retreat_page();
    assert_eq!(back, Some(Some("cursor-1".into())));
    assert_eq!(s.page, 2);
    assert_eq!(s.current_cursor.as_deref(), Some("cursor-1"));
    // One more retreat reaches page 1 (no cursor).
    let back_to_one = s.retreat_page();
    assert_eq!(back_to_one, Some(None));
    assert_eq!(s.page, 1);
    assert!(s.current_cursor.is_none());
    assert!(!s.can_prev_page(), "history is exhausted at page 1");
  }

  #[test]
  fn search_failed_with_offline_clears_in_flight_and_renders_hint() {
    let mut s = HfDialogState::open(false);
    s.insert('q');
    s.mark_dispatched();
    s.apply_search_failed(s.last_dispatched_seq, FetchError::Offline);
    assert!(!s.search_in_flight);
    let err = s.error.expect("error message must surface");
    assert!(err.contains("offline"), "got `{err}`");
  }

  #[test]
  fn short_count_formats_at_each_magnitude_band() {
    assert_eq!(short_count(7), "7");
    assert_eq!(short_count(1500), "1.5K");
    assert_eq!(short_count(2_500_000), "2.5M");
    assert_eq!(short_count(3_500_000_000), "3.5B");
  }

  #[test]
  fn truncate_inserts_ellipsis_for_long_strings() {
    let out = truncate("supercalifragilisticexpialidocious", 10);
    assert_eq!(out.chars().count(), 10);
    assert!(out.ends_with('…'));
  }
}
