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

use crate::discovery::split_gguf::parse_shard_name;
use crate::init::download::RepoSpec;
use crate::init::fetch::FetchError;
use crate::init::hf_api::{
  HfRepoFile, HfSearchPage, HfSearchResult, HfSortKey, ListRepoFilesError,
};
use crate::init::recommender::FileFit;
use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::input_field::{InputField, InputOutcome};
use crate::tui::keybindings::{Action as KeyAction, Focus};

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

/// One row in the File picker. Either a standalone GGUF file or a
/// collapsed split-shard set (R112). Splits surface their sum of
/// sizes and the launch filename (shard 1) for the eventual pull
/// dispatch (Unit 6 walks `shard_filenames` to enqueue every
/// sibling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerRow {
  Single {
    filename: String,
    size_bytes: Option<u64>,
  },
  Split {
    /// Display label — the shard base with `.gguf` appended.
    label: String,
    total: u32,
    /// `true` when every shard from 1..=total is present in the
    /// repo listing. Incomplete sets render greyed-out and refuse
    /// selection so the user can't pull a half-set.
    complete: bool,
    /// Sum of `size_bytes` across the shards. `None` when any
    /// sibling is missing its size (a HEAD probe at Confirm time
    /// fills the gap).
    total_size_bytes: Option<u64>,
    /// File to launch via `download_repo` — shard 1 when present,
    /// otherwise the lowest-index shard seen. `select_files` in
    /// `init::download` then expands the shard set so every
    /// sibling lands.
    launch_filename: String,
    /// All shard filenames in the set, sorted by index. Carried
    /// here so a future Unit 6 progress shim can show
    /// `<shard>-NNNNN-of-MMMMM` granularity if needed.
    shard_filenames: Vec<String>,
  },
}

impl PickerRow {
  /// Display filename in the picker row.
  pub fn label(&self) -> &str {
    match self {
      PickerRow::Single { filename, .. } => filename,
      PickerRow::Split { label, .. } => label,
    }
  }

  /// Total bytes for the row (sum across shards when applicable).
  pub fn size_bytes(&self) -> Option<u64> {
    match self {
      PickerRow::Single { size_bytes, .. } => *size_bytes,
      PickerRow::Split {
        total_size_bytes, ..
      } => *total_size_bytes,
    }
  }

  /// Filename to download. Shard 1 for collapsed split sets so the
  /// existing `download_repo` shard-expansion path takes over.
  pub fn download_filename(&self) -> &str {
    match self {
      PickerRow::Single { filename, .. } => filename,
      PickerRow::Split {
        launch_filename, ..
      } => launch_filename,
    }
  }

  /// Every filename the pull will produce on disk. Used by the
  /// pre-download cache probe so a split-shard pull only short-
  /// circuits to `AlreadyCached` when every shard is present.
  pub fn all_filenames(&self) -> Vec<String> {
    match self {
      PickerRow::Single { filename, .. } => vec![filename.clone()],
      PickerRow::Split {
        shard_filenames, ..
      } => shard_filenames.clone(),
    }
  }

  /// `true` when the row can be selected. Incomplete shard sets
  /// return `false` so the picker can grey them out.
  pub fn selectable(&self) -> bool {
    match self {
      PickerRow::Single { .. } => true,
      PickerRow::Split { complete, .. } => *complete,
    }
  }
}

/// Collapse a flat sibling list into picker rows. Shards sharing a
/// `(base, total)` tuple group into one [`PickerRow::Split`]; everything
/// else flows through as [`PickerRow::Single`]. Mirrors
/// `discovery::split_gguf::group` but operates on filenames (the HF
/// listing doesn't carry a parent path) and an `Option<u64>` size
/// instead of a filesystem stat.
pub fn collapse_picker_rows(files: Vec<HfRepoFile>) -> Vec<PickerRow> {
  use std::collections::BTreeMap;
  // Bucket by (base, total) preserving the first-seen insertion order
  // so the picker reads predictably.
  type BucketKey = (String, u32);
  type BucketShard = (u32, HfRepoFile);
  type Bucket = (usize, Vec<BucketShard>);
  let mut singles: Vec<(usize, HfRepoFile)> = Vec::new();
  let mut buckets: BTreeMap<BucketKey, Bucket> = BTreeMap::new();
  for (idx, f) in files.into_iter().enumerate() {
    match parse_shard_name(&f.filename) {
      Some(info) => {
        let entry = buckets
          .entry((info.base.clone(), info.total))
          .or_insert_with(|| (idx, Vec::new()));
        entry.0 = entry.0.min(idx);
        entry.1.push((info.index, f));
      }
      None => singles.push((idx, f)),
    }
  }
  let mut entries: Vec<(usize, PickerRow)> = singles
    .into_iter()
    .map(|(o, f)| {
      (
        o,
        PickerRow::Single {
          filename: f.filename,
          size_bytes: f.size_bytes,
        },
      )
    })
    .collect();
  for ((base, total), (order, mut shards)) in buckets {
    shards.sort_by_key(|(idx, _)| *idx);
    let complete = shards
      .iter()
      .enumerate()
      .all(|(i, (idx, _))| *idx as usize == i + 1 && shards.len() as u32 == total);
    let total_size_bytes = if shards.iter().all(|(_, f)| f.size_bytes.is_some()) {
      Some(shards.iter().map(|(_, f)| f.size_bytes.unwrap_or(0)).sum())
    } else {
      None
    };
    let launch_filename = shards
      .iter()
      .find(|(idx, _)| *idx == 1)
      .map(|(_, f)| f.filename.clone())
      .unwrap_or_else(|| shards[0].1.filename.clone());
    let shard_filenames: Vec<String> = shards.iter().map(|(_, f)| f.filename.clone()).collect();
    let label = format!("{base}.gguf");
    entries.push((
      order,
      PickerRow::Split {
        label,
        total,
        complete,
        total_size_bytes,
        launch_filename,
        shard_filenames,
      },
    ));
  }
  entries.sort_by_key(|(o, _)| *o);
  entries.into_iter().map(|(_, row)| row).collect()
}

/// Owned by `App` as `Option<HfDialogState>` — `None` when the dialog
/// is closed, `Some` when open. Not `Clone`; the modal exists at most
/// once.
#[derive(Debug)]
pub struct HfDialogState {
  pub stage: HfStage,
  /// Modal search-input field. Auto-edits on dialog open so the user
  /// can type without first pressing `e`. While editing, Esc exits
  /// edit; with a non-empty buffer a second Esc clears; with an
  /// empty buffer the third Esc reaches the dialog's keymap and
  /// closes the modal. Sort/page chords (`o`, `n`, `p`) only fire
  /// while the field is resting.
  pub input: InputField,
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
  /// Collapsed picker rows — singles plus split-shard groups (R112).
  pub picker_rows: Vec<PickerRow>,
  pub picker_idx: usize,
  /// Row selected for Confirm. Carries the download filename + sum
  /// of sizes; the caller hands this to `download_repo` (Unit 6).
  pub confirm_row: Option<PickerRow>,
  /// `true` when the FetchClient is offline so the search bar can
  /// render an "offline — paste a repo ID …" hint immediately.
  pub offline: bool,
  /// Snapshot of the inputs `vram_fit_for_file` needs to compute the
  /// File picker's per-row fit indicator. Refreshed at dialog-open
  /// time from `App::host_metrics` + the bundled benchmark snapshot.
  pub hardware_fit_ctx: HardwareFitContext,
  /// Drain endpoint for background tasks. Created on `open`.
  pub event_rx: mpsc::UnboundedReceiver<HfDialogEvent>,
  /// Clonable sender background tasks use to ship results back.
  pub event_tx: mpsc::UnboundedSender<HfDialogEvent>,
}

/// Debounce window — once this elapses after the last keystroke, the
/// dialog dispatches the buffered query as a search (R107 / live
/// search). Matches the HuggingFace web UI cadence.
pub const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// Inputs to [`crate::init::recommender::vram_fit_for_file`] the file
/// picker carries across render frames. Lifted into its own struct so
/// the dialog can be constructed in tests without faking a full
/// `HostMetricsSnapshot` + `BenchmarkSnapshot`.
#[derive(Debug, Clone, Default)]
pub struct HardwareFitContext {
  pub backend: String,
  pub vram_bytes: Option<u64>,
  pub ram_total_bytes: u64,
  pub overhead_band_bytes: Option<u64>,
  pub ctx_tokens: u32,
}

impl HardwareFitContext {
  /// Compute the [`FileFit`] for a candidate row size.
  pub fn fit_for(&self, size_bytes: Option<u64>) -> FileFit {
    let Some(size) = size_bytes else {
      return FileFit::Unknown;
    };
    if self.backend.is_empty() {
      return FileFit::Unknown;
    }
    crate::init::recommender::vram_fit_for_file(
      size,
      self.ctx_tokens,
      &self.backend,
      self.vram_bytes,
      self.ram_total_bytes,
      self.overhead_band_bytes,
    )
  }
}

impl HfDialogState {
  /// Construct a fresh dialog in the Search stage.
  pub fn open(offline: bool, hardware_fit_ctx: HardwareFitContext) -> Self {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut input = InputField::new();
    input.enter_edit();
    Self {
      stage: HfStage::Search,
      input,
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
      picker_rows: Vec::new(),
      picker_idx: 0,
      confirm_row: None,
      offline,
      hardware_fit_ctx,
      event_rx: rx,
      event_tx: tx,
    }
  }

  // ----- Search state transitions -----

  /// Route a key event through the search input. Bumps the
  /// dispatch seq (and resets the debounce timer) when the key
  /// actually mutated the buffer so a stale response can be
  /// discarded.
  pub fn handle_search_key(&mut self, key: crossterm::event::KeyEvent) -> InputOutcome {
    let before = self.input.buffer().len();
    let outcome = self.input.handle_key(key);
    if self.input.buffer().len() != before {
      self.query_seq = self.query_seq.saturating_add(1);
      self.last_keystroke_at = Instant::now();
      self.error = None;
    }
    outcome
  }

  /// Read-only view onto the current query buffer. Trimmed downstream
  /// when used as a search argument.
  pub fn query(&self) -> &str {
    self.input.buffer()
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

  /// `true` when a debounced dispatch should fire. The dialog only
  /// dispatches once `DEBOUNCE` has elapsed since the last keystroke
  /// and the current seq hasn't already been dispatched. For
  /// `Trending` sort the dispatch fires regardless of buffer content
  /// (the HF endpoint ignores `search` for trending — see
  /// [`init::hf_api::search`]); every other sort still needs a
  /// non-empty query to avoid hammering the API with empty searches.
  pub fn search_due(&self, now: Instant) -> bool {
    let seq_advanced = self.query_seq > self.last_dispatched_seq;
    let debounce_elapsed = now.duration_since(self.last_keystroke_at) >= DEBOUNCE;
    let has_query = !self.input.buffer().trim().is_empty();
    seq_advanced && debounce_elapsed && (has_query || self.sort == HfSortKey::Trending)
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
  /// results). In the FilePicker stage, skip past non-selectable
  /// rows (incomplete shard sets) so the cursor never parks where
  /// Enter would refuse — feels less rough than hitting Enter and
  /// being told "no file selected".
  pub fn move_up(&mut self) {
    match self.stage {
      HfStage::Search => {
        if self.selected_idx > 0 {
          self.selected_idx -= 1;
        }
      }
      HfStage::FilePicker => {
        let mut i = self.picker_idx;
        while i > 0 {
          i -= 1;
          if self.picker_rows[i].selectable() {
            self.picker_idx = i;
            return;
          }
        }
      }
      HfStage::Confirm => {}
    }
  }

  /// Move the cursor down by one, clamping at the row count. Same
  /// skip-non-selectable rule as [`Self::move_up`].
  pub fn move_down(&mut self) {
    match self.stage {
      HfStage::Search => {
        if !self.results.is_empty() && self.selected_idx + 1 < self.results.len() {
          self.selected_idx += 1;
        }
      }
      HfStage::FilePicker => {
        let len = self.picker_rows.len();
        let mut i = self.picker_idx + 1;
        while i < len {
          if self.picker_rows[i].selectable() {
            self.picker_idx = i;
            return;
          }
          i += 1;
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
    let slug = RepoSpec::parse(self.input.buffer().trim()).ok();
    let repo_id = if let Some(spec) = slug {
      spec.repo_id
    } else {
      self.results.get(self.selected_idx)?.repo_id.clone()
    };
    self.stage = HfStage::FilePicker;
    self.picker_repo_id = Some(repo_id.clone());
    self.picker_rows.clear();
    self.picker_idx = 0;
    self.picker_load = PickerLoad::Loading;
    Some(repo_id)
  }

  /// Apply a successful `list_repo_files` response. Filters to
  /// `.gguf` files and collapses split-shard sets (R112) into one
  /// logical row per group.
  pub fn apply_repo_files(&mut self, repo_id: &str, mut files: Vec<HfRepoFile>) {
    // Drop if the dialog moved on to a different repo.
    if self.picker_repo_id.as_deref() != Some(repo_id) {
      return;
    }
    files.retain(|f| f.filename.to_ascii_lowercase().ends_with(".gguf"));
    let rows = collapse_picker_rows(files);
    // Pre-select the first selectable row so an incomplete shard
    // set doesn't trap the cursor on a non-selectable row.
    let picker_idx = rows.iter().position(PickerRow::selectable).unwrap_or(0);
    self.picker_load = PickerLoad::Ready;
    self.picker_rows = rows;
    self.picker_idx = picker_idx;
  }

  /// Apply a `list_repo_files` failure.
  pub fn apply_repo_files_failed(&mut self, repo_id: &str, err: &ListRepoFilesError) {
    if self.picker_repo_id.as_deref() != Some(repo_id) {
      return;
    }
    self.picker_load = PickerLoad::Failed(err.to_string());
  }

  /// Move from FilePicker → Confirm. Returns `true` when a row is
  /// selectable (incomplete shard sets refuse selection per R112).
  pub fn submit_picker(&mut self) -> bool {
    let Some(row) = self.picker_rows.get(self.picker_idx).cloned() else {
      return false;
    };
    if !row.selectable() {
      return false;
    }
    self.confirm_row = Some(row);
    self.stage = HfStage::Confirm;
    true
  }

  /// Step from FilePicker back to Search (preserves the query
  /// buffer and the result page).
  pub fn back_to_search(&mut self) {
    self.stage = HfStage::Search;
    self.picker_repo_id = None;
    self.picker_rows.clear();
    self.picker_load = PickerLoad::Idle;
    self.picker_idx = 0;
    self.confirm_row = None;
  }

  /// Step from Confirm back to FilePicker.
  pub fn back_to_picker(&mut self) {
    self.stage = HfStage::FilePicker;
    self.confirm_row = None;
  }

  /// Consume the dialog's pending confirm selection (repo + row).
  /// Caller forwards this to the download orchestrator (Unit 6);
  /// closing the dialog is the caller's job.
  pub fn take_confirm_target(&self) -> Option<(String, PickerRow)> {
    let repo = self.picker_repo_id.clone()?;
    let row = self.confirm_row.clone()?;
    Some((repo, row))
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
/// `advanced_panel::render` overlay pattern). Takes `app` so the
/// footer can resolve the live label for every bound key
/// (`Submit`, `Cancel`, `MoveUp`, `MoveDown`) — a config-side
/// `keybindings:` rebind flows through to the chip strip.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let Some(state) = app.hf_dialog.as_ref() else {
    return;
  };
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

  render_header(frame, layout[0], app, state, palette);
  match state.stage {
    HfStage::Search => render_search_body(frame, layout[1], state, palette),
    HfStage::FilePicker => render_picker_body(frame, layout[1], app, state, palette),
    HfStage::Confirm => render_confirm_body(frame, layout[1], app, state, palette),
  }
  render_footer(frame, layout[2], app, state, palette);
}

/// Thin shim over [`App::resolve_label`] specialised to
/// [`Focus::HfDialog`] so the call sites below stay compact. The
/// generic helper lives on `App` so other dialogs that need the
/// same label-resolution shape don't re-invent it.
fn dialog_label(app: &App, action: KeyAction, fallback: &str) -> String {
  app.resolve_label(Focus::HfDialog, action, fallback)
}

fn render_header(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  state: &HfDialogState,
  palette: &Palette,
) {
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
  let query = state.input.buffer();
  if query.is_empty() {
    let placeholder = if state.input.is_editing() {
      "(type a query or paste owner/repo)"
    } else {
      "(press e to edit)"
    };
    spans.push(Span::styled(placeholder, muted));
  } else {
    spans.push(Span::styled(query.to_string(), value_style));
    if state.input.is_editing() {
      spans.push(crate::tui::fmt::caret(palette));
    }
  }
  let mut second: Vec<Span<'static>> = Vec::new();
  second.push(Span::styled("sort: ", label_style));
  second.push(Span::styled(sort_label.to_string(), value_style));
  second.push(Span::styled("  ·  ", muted));
  // Page indicator with prev/next arrows so the user knows whether
  // `p` / `n` will do anything before pressing them.
  let prev_mark = if state.can_prev_page() { "‹ " } else { "" };
  let next_mark = if state.can_next_page() { " ›" } else { "" };
  second.push(Span::styled(
    format!("{prev_mark}page {}{next_mark}", state.page),
    label_style,
  ));
  if state.search_in_flight {
    second.push(Span::styled("  loading…".to_string(), muted));
  }
  if state.offline && state.stage == HfStage::Search {
    second.push(Span::styled(
      "  · offline — search disabled".to_string(),
      muted,
    ));
  }
  let submit = dialog_label(app, KeyAction::Submit, "Enter");
  let cancel = dialog_label(app, KeyAction::Cancel, "Esc");
  let lines = vec![
    Line::from(spans),
    Line::from(second),
    Line::from(Span::styled(
      format!("{submit} on a row drills into files. {cancel} walks back."),
      muted,
    )),
  ];
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_search_body(frame: &mut Frame<'_>, area: Rect, state: &HfDialogState, palette: &Palette) {
  if let Some(err) = &state.error {
    let err_line = Paragraph::new(Line::from(Span::styled(err.clone(), palette.error_style())))
      .wrap(Wrap { trim: true });
    frame.render_widget(err_line, area);
    return;
  }
  if state.results.is_empty() {
    let message = if state.input.is_empty() {
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
  // Scroll the visible window so the selected row stays in view —
  // mirrors the list-pane convention. Without this the Paragraph
  // would always render rows from index 0 and arrow-down past the
  // visible bottom would silently park the cursor off-screen.
  let visible = area.height.max(1) as usize;
  let scroll_offset = scroll_offset_for(state.selected_idx, state.results.len(), visible);
  let lines: Vec<Line<'static>> = state
    .results
    .iter()
    .enumerate()
    .skip(scroll_offset)
    .take(visible)
    .map(|(idx, r)| render_search_row(idx, idx == state.selected_idx, state.sort, r, palette))
    .collect();
  frame.render_widget(Paragraph::new(lines), area);
}

/// Pure scroll-window math: given a selected row index, a total row
/// count, and the number of visible rows, return how many leading
/// rows to skip so the selection sits inside the viewport. Returns
/// `0` when everything already fits. Lifted out of the renderer so
/// the windowing rule is unit-testable.
pub(crate) fn scroll_offset_for(selected: usize, total: usize, visible: usize) -> usize {
  if visible == 0 || total <= visible {
    return 0;
  }
  if selected < visible {
    return 0;
  }
  // Park the selection at the last visible row.
  (selected + 1).saturating_sub(visible).min(total - visible)
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
  let tag = r.pipeline_tag.clone().unwrap_or_else(|| "—".to_string());
  Line::from(vec![
    Span::styled(prefix.to_string(), style),
    Span::styled(format!("{:<48}  ", truncate(&r.repo_id, 48)), style),
    Span::styled(
      format!("{:<22}  ", truncate(&tag, 22)),
      palette.muted_style(),
    ),
    Span::styled(metric, palette.label_style()),
  ])
}

fn render_picker_body(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
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
  let cancel = dialog_label(app, KeyAction::Cancel, "Esc");
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
        format!("{cancel} returns to Search."),
        palette.muted_style(),
      )));
    }
    PickerLoad::Ready if state.picker_rows.is_empty() => {
      lines.push(Line::from(Span::styled(
        "no `.gguf` files in this repo.",
        palette.muted_style(),
      )));
      lines.push(Line::from(Span::styled(
        format!("{cancel} returns to Search."),
        palette.muted_style(),
      )));
    }
    PickerLoad::Ready => {
      // Small one-line legend so the fit glyph column is
      // self-describing (R113). Coloured spans mirror the per-row
      // styles below so the legend doubles as a visual key. Stays
      // muted-tinted around the glyphs so it doesn't compete with
      // the row data.
      lines.push(Line::from(vec![
        Span::styled("fit:  ", palette.label_style()),
        Span::styled(FileFit::Fit.glyph(), palette.success_style()),
        Span::styled(" fits  ", palette.muted_style()),
        Span::styled(FileFit::Tight.glyph(), palette.warning_style()),
        Span::styled(" tight  ", palette.muted_style()),
        Span::styled(FileFit::Over.glyph(), palette.error_style()),
        Span::styled(" oversize  ", palette.muted_style()),
        Span::styled(FileFit::Unknown.glyph(), palette.muted_style()),
        Span::styled(" unknown", palette.muted_style()),
      ]));
      // Check if every row is unselectable (e.g. only incomplete
      // shard sets) so the user understands why Enter would refuse
      // and Esc walks back is the only path forward.
      if !state.picker_rows.iter().any(PickerRow::selectable) {
        lines.push(Line::from(Span::styled(
          format!(
            "no selectable files (every shard set is incomplete). {cancel} returns to Search."
          ),
          palette.warning_style(),
        )));
      }
      for (idx, row) in state.picker_rows.iter().enumerate() {
        let selected = idx == state.picker_idx;
        let prefix = if selected { "▌ " } else { "  " };
        let mut style = palette.text_style();
        if selected {
          style = style.add_modifier(Modifier::REVERSED);
        }
        let size = row
          .size_bytes()
          .map(crate::tui::fmt::format_bytes)
          .unwrap_or_else(|| "?".into());
        let label = match row {
          PickerRow::Single { filename, .. } => filename.clone(),
          PickerRow::Split {
            label,
            total,
            complete,
            ..
          } => {
            if *complete {
              format!("{label}  ({total} shards)")
            } else {
              format!("{label}  ({total} shards — incomplete)")
            }
          }
        };
        let fit = state.hardware_fit_ctx.fit_for(row.size_bytes());
        let fit_glyph = fit.glyph();
        let fit_style = match fit {
          FileFit::Fit => palette.success_style(),
          FileFit::Tight => palette.warning_style(),
          FileFit::Over => palette.error_style(),
          FileFit::Unknown => palette.muted_style(),
        };
        let row_style = if matches!(
          row,
          PickerRow::Split {
            complete: false,
            ..
          }
        ) {
          // Greyed-out incomplete shard set — refused on submit.
          palette.muted_style()
        } else {
          style
        };
        lines.push(Line::from(vec![
          Span::styled(prefix.to_string(), row_style),
          Span::styled(format!("{:<58}  ", truncate(&label, 58)), row_style),
          Span::styled(format!("{size:>9}  "), palette.label_style()),
          Span::styled(fit_glyph.to_string(), fit_style),
        ]));
      }
    }
  }
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_confirm_body(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  state: &HfDialogState,
  palette: &Palette,
) {
  let repo = state.picker_repo_id.as_deref().unwrap_or("(no repo)");
  let file = state
    .confirm_row
    .as_ref()
    .map(|r| r.label().to_string())
    .unwrap_or_else(|| "(no file)".into());
  let size = state
    .confirm_row
    .as_ref()
    .and_then(|r| r.size_bytes())
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
      format!(
        "Press {} to confirm — the download enqueues in the status strip.",
        dialog_label(app, KeyAction::Submit, "Enter")
      ),
      palette.muted_style(),
    )),
    Line::from(Span::styled(
      format!(
        "{} returns to the file picker.",
        dialog_label(app, KeyAction::Cancel, "Esc")
      ),
      palette.muted_style(),
    )),
  ];
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_footer(
  frame: &mut Frame<'_>,
  area: Rect,
  app: &App,
  state: &HfDialogState,
  palette: &Palette,
) {
  // Bound chords come from the keymap so a config-side
  // `keybindings:` rebind flows through to the chip strip. The
  // dialog-internal chords (`o`, `n`, `p`, `e`) aren't actions —
  // they're component-internal or stage-specific routing — so they
  // stay as literal labels.
  let submit = dialog_label(app, KeyAction::Submit, "Enter");
  let cancel = dialog_label(app, KeyAction::Cancel, "Esc");
  let up = dialog_label(app, KeyAction::MoveUp, "↑");
  let down = dialog_label(app, KeyAction::MoveDown, "↓");
  let arrows = format!("{up}/{down}");
  let hints = match state.stage {
    HfStage::Search if state.input.is_editing() => {
      format!("type to search · {arrows}:row · {submit}:open · {cancel}:stop edit")
    }
    HfStage::Search => {
      format!("e:edit · {arrows}:row · {submit}:open · o:sort · n/p:page · {cancel}:close")
    }
    HfStage::FilePicker => format!("{arrows}:file · {submit}:select · {cancel}:back"),
    HfStage::Confirm => format!("{submit}:pull · {cancel}:back"),
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

  fn type_query(state: &mut HfDialogState, text: &str) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for ch in text.chars() {
      state.handle_search_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
  }

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
  fn scroll_offset_keeps_selection_in_window() {
    // Everything fits → no scrolling.
    assert_eq!(scroll_offset_for(0, 5, 10), 0);
    assert_eq!(scroll_offset_for(4, 5, 10), 0);
    // Selection still within the first window.
    assert_eq!(scroll_offset_for(3, 20, 5), 0);
    assert_eq!(scroll_offset_for(4, 20, 5), 0);
    // Crossing the bottom edge of the window — scroll one row.
    assert_eq!(scroll_offset_for(5, 20, 5), 1);
    assert_eq!(scroll_offset_for(6, 20, 5), 2);
    // Pegged at the end.
    assert_eq!(scroll_offset_for(19, 20, 5), 15);
    // Edge: 0 visible rows.
    assert_eq!(scroll_offset_for(3, 20, 0), 0);
  }

  #[test]
  fn open_starts_in_search_stage() {
    let s = HfDialogState::open(false, HardwareFitContext::default());
    assert_eq!(s.stage, HfStage::Search);
    assert!(s.input.is_empty());
    assert!(s.input.is_editing(), "dialog opens in edit mode");
    assert_eq!(s.sort, HfSortKey::Downloads);
    assert_eq!(s.page, 1);
    assert!(!s.offline);
  }

  #[test]
  fn typing_bumps_seq_so_late_responses_are_dropped() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    type_query(&mut s, "q");
    let seq_after_first = s.query_seq;
    type_query(&mut s, "w");
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
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    type_query(&mut s, "q");
    let now = s.last_keystroke_at;
    assert!(
      !s.search_due(now),
      "immediate dispatch would defeat the debounce"
    );
    assert!(s.search_due(now + DEBOUNCE));
  }

  #[test]
  fn empty_query_never_dispatches_for_non_trending_sorts() {
    let s = HfDialogState::open(false, HardwareFitContext::default());
    assert!(!s.search_due(Instant::now() + DEBOUNCE + Duration::from_secs(5)));
  }

  #[test]
  fn trending_sort_dispatches_with_empty_query() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    // cycle through downloads → likes → updated → trending
    for _ in 0..3 {
      s.cycle_sort();
    }
    assert_eq!(s.sort, HfSortKey::Trending);
    let due_at = s.last_keystroke_at + DEBOUNCE + Duration::from_millis(1);
    assert!(
      s.search_due(due_at),
      "trending dispatch must fire even with an empty query"
    );
  }

  #[test]
  fn cycle_sort_walks_all_four_back_to_downloads() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
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
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.results = vec![fake_result("from-list/repo")];
    s.input.set_text("owner/typed-repo");
    let target = s.submit_search();
    assert_eq!(target.as_deref(), Some("owner/typed-repo"));
    assert_eq!(s.stage, HfStage::FilePicker);
    assert_eq!(s.picker_repo_id.as_deref(), Some("owner/typed-repo"));
  }

  #[test]
  fn submit_search_uses_selected_result_when_query_is_not_a_slug() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.results = vec![fake_result("alpha/repo"), fake_result("beta/repo")];
    s.selected_idx = 1;
    s.input.set_text("qwen");
    let target = s.submit_search();
    assert_eq!(target.as_deref(), Some("beta/repo"));
  }

  #[test]
  fn submit_search_returns_none_when_no_query_and_no_selection() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    assert!(s.submit_search().is_none());
    assert_eq!(s.stage, HfStage::Search);
  }

  #[test]
  fn back_to_search_clears_picker_state_but_keeps_query() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.input.set_text("qwen");
    s.results = vec![fake_result("a/b")];
    s.submit_search();
    assert_eq!(s.stage, HfStage::FilePicker);
    s.back_to_search();
    assert_eq!(s.stage, HfStage::Search);
    assert_eq!(
      s.input.buffer(),
      "qwen",
      "query buffer must survive back-step"
    );
    assert!(s.picker_repo_id.is_none());
  }

  #[test]
  fn apply_repo_files_filters_to_gguf_and_drops_stale_repo() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.picker_repo_id = Some("owner/repo".into());
    s.picker_load = PickerLoad::Loading;
    s.apply_repo_files(
      "owner/different",
      vec![HfRepoFile {
        filename: "file.gguf".into(),
        size_bytes: None,
      }],
    );
    assert!(s.picker_rows.is_empty(), "stale repo files leaked through");
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
    assert_eq!(s.picker_rows.len(), 1);
    assert_eq!(s.picker_rows[0].label(), "model.gguf");
    assert_eq!(s.picker_load, PickerLoad::Ready);
  }

  #[test]
  fn submit_picker_requires_a_selectable_file() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    assert!(!s.submit_picker());
    s.picker_rows = vec![PickerRow::Single {
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
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
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
    s.picker_rows = vec![
      PickerRow::Single {
        filename: "a.gguf".into(),
        size_bytes: None,
      },
      PickerRow::Single {
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
  fn move_down_in_picker_skips_non_selectable_rows() {
    // Cursor on a selectable row; the next row is an incomplete
    // shard set (refuses selection); the row after that is
    // selectable. `move_down` must land on the third row, not the
    // unselectable middle, so Enter never fires the "no file
    // selected" toast under normal arrow navigation.
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.stage = HfStage::FilePicker;
    s.picker_rows = vec![
      PickerRow::Single {
        filename: "a.gguf".into(),
        size_bytes: Some(1),
      },
      PickerRow::Split {
        label: "incomplete.gguf".into(),
        total: 3,
        complete: false,
        total_size_bytes: None,
        launch_filename: "incomplete-00001-of-00003.gguf".into(),
        shard_filenames: vec!["incomplete-00001-of-00003.gguf".into()],
      },
      PickerRow::Single {
        filename: "c.gguf".into(),
        size_bytes: Some(1),
      },
    ];
    s.picker_idx = 0;
    s.move_down();
    assert_eq!(s.picker_idx, 2, "arrow must skip the incomplete shard row");
    s.move_up();
    assert_eq!(s.picker_idx, 0, "arrow-up also skips back across it");
  }

  #[test]
  fn advance_and_retreat_page_track_cursor_history() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.input.set_text("qwen");
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
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    type_query(&mut s, "q");
    s.mark_dispatched();
    s.apply_search_failed(s.last_dispatched_seq, FetchError::Offline);
    assert!(!s.search_in_flight);
    let err = s.error.expect("error message must surface");
    assert!(err.contains("offline"), "got `{err}`");
  }

  fn file(name: &str, size: Option<u64>) -> HfRepoFile {
    HfRepoFile {
      filename: name.into(),
      size_bytes: size,
    }
  }

  #[test]
  fn collapse_picker_rows_groups_complete_shard_set_with_sum_size() {
    let rows = collapse_picker_rows(vec![
      file(
        "Qwen-32B-Q4_K_M-00001-of-00003.gguf",
        Some(7 * 1024 * 1024 * 1024),
      ),
      file(
        "Qwen-32B-Q4_K_M-00002-of-00003.gguf",
        Some(7 * 1024 * 1024 * 1024),
      ),
      file(
        "Qwen-32B-Q4_K_M-00003-of-00003.gguf",
        Some(7 * 1024 * 1024 * 1024),
      ),
      file("config.gguf", Some(1024)),
    ]);
    assert_eq!(rows.len(), 2, "3 shards collapse to 1 row + 1 single");
    match &rows[0] {
      PickerRow::Split {
        label,
        total,
        complete,
        total_size_bytes,
        launch_filename,
        shard_filenames,
        ..
      } => {
        assert_eq!(label, "Qwen-32B-Q4_K_M.gguf");
        assert_eq!(*total, 3);
        assert!(complete);
        assert_eq!(*total_size_bytes, Some(21 * 1024 * 1024 * 1024));
        assert_eq!(launch_filename, "Qwen-32B-Q4_K_M-00001-of-00003.gguf");
        assert_eq!(shard_filenames.len(), 3);
      }
      other => panic!("expected Split, got {other:?}"),
    }
    assert!(matches!(rows[1], PickerRow::Single { ref filename, .. } if filename == "config.gguf"));
  }

  #[test]
  fn collapse_picker_rows_marks_incomplete_shard_set() {
    // Missing shard 3 of 3 — Split entry must be marked incomplete
    // and refused on submit per R112.
    let rows = collapse_picker_rows(vec![
      file("partial-00001-of-00003.gguf", Some(1024)),
      file("partial-00002-of-00003.gguf", Some(1024)),
    ]);
    assert_eq!(rows.len(), 1);
    match &rows[0] {
      PickerRow::Split { complete, .. } => assert!(!complete),
      other => panic!("expected Split, got {other:?}"),
    }
    assert!(
      !rows[0].selectable(),
      "incomplete shard set must refuse selection"
    );
  }

  #[test]
  fn submit_picker_refuses_incomplete_shard_set() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.stage = HfStage::FilePicker;
    s.picker_rows = vec![PickerRow::Split {
      label: "partial.gguf".into(),
      total: 3,
      complete: false,
      total_size_bytes: None,
      launch_filename: "partial-00001-of-00003.gguf".into(),
      shard_filenames: vec!["partial-00001-of-00003.gguf".into()],
    }];
    assert!(
      !s.submit_picker(),
      "incomplete shard set must refuse on submit"
    );
    assert_eq!(s.stage, HfStage::FilePicker, "stage must not advance");
  }

  #[test]
  fn apply_repo_files_collapses_shards_and_preselects_first_selectable() {
    let mut s = HfDialogState::open(false, HardwareFitContext::default());
    s.picker_repo_id = Some("owner/repo".into());
    s.apply_repo_files(
      "owner/repo",
      vec![
        // Two-shard set that's missing shard 2 — incomplete.
        file("a-00001-of-00002.gguf", Some(1024)),
        file("b.gguf", Some(2048)),
      ],
    );
    // The first row is the incomplete Split; cursor must land on
    // the next selectable row.
    assert_eq!(s.picker_rows.len(), 2);
    assert!(matches!(
      s.picker_rows[0],
      PickerRow::Split {
        complete: false,
        ..
      }
    ));
    assert!(matches!(s.picker_rows[1], PickerRow::Single { .. }));
    assert_eq!(s.picker_idx, 1, "cursor pre-selected the selectable row");
  }

  #[test]
  fn hardware_fit_context_routes_through_recommender_helper() {
    let ctx = HardwareFitContext {
      backend: "cuda".into(),
      vram_bytes: Some(24 * 1024 * 1024 * 1024),
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      overhead_band_bytes: Some(512 * 1024 * 1024),
      ctx_tokens: crate::init::recommender::DEFAULT_CTX,
    };
    // Small file → Fit.
    assert_eq!(ctx.fit_for(Some(4 * 1024 * 1024 * 1024)), FileFit::Fit);
    // Oversized → Over.
    assert_eq!(ctx.fit_for(Some(30 * 1024 * 1024 * 1024)), FileFit::Over);
    // None size → Unknown.
    assert_eq!(ctx.fit_for(None), FileFit::Unknown);
  }

  #[test]
  fn hardware_fit_context_empty_backend_yields_unknown() {
    // Default context (backend = ""): every fit verdict must be
    // Unknown so the dialog can render without faking a verdict
    // when the daemon's host-metrics sampler hasn't reported yet.
    let ctx = HardwareFitContext::default();
    assert_eq!(ctx.fit_for(Some(4 * 1024 * 1024 * 1024)), FileFit::Unknown);
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
