//! Pinned single-line download status strip (Unit 6 / R115–R117).
//!
//! Sits between the info row and the body of the dashboard. Renders
//! one active pull at a time with `<friendly name> <pct%>
//! <bytes/total> · <throughput>` — additional pulls queue FIFO and
//! promote when the active one finishes (R115). Errors render as
//! a one-line message for a few seconds then dequeue the next
//! pending pull (R117).
//!
//! State lives on `App::download_strip`, not on `HfDialogState`, so
//! the strip survives the dialog opening / closing while a pull
//! ticks underneath.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::theme::Palette;
use crate::tui::hf_dialog::PickerRow;

/// How long an `Error` message lingers on the strip before the
/// next queued pull is promoted (R117). 5 seconds matches the
/// brainstorm's "one-line error in the strip; full diagnostics
/// flow to logs" guidance.
pub const ERROR_LINGER: Duration = Duration::from_secs(5);

/// Outcome of [`DownloadStripState::cancel_active`] — fold the
/// "active vs idle" branch into a single returned value so callers
/// (event dispatch, tests) read one source of truth.
#[derive(Debug)]
pub enum CancelOutcome {
  /// No active pull; the cancel was a no-op.
  NothingActive,
  /// The active pull was aborted. `cancelled_friendly_name` is what
  /// the caller surfaces in the success toast. `next` is the queued
  /// pull (if any) the caller should now spawn.
  Cancelled {
    cancelled_repo_id: String,
    cancelled_friendly_name: String,
    next: Option<QueuedPull>,
  },
}

/// A pull queued from the dialog's Confirm action. `friendly_name`
/// is what the strip shows mid-flight (R115); `repo_id` + `row`
/// drive the actual `download_repo` call.
#[derive(Debug, Clone)]
pub struct QueuedPull {
  pub repo_id: String,
  pub row: PickerRow,
  pub friendly_name: String,
}

/// The currently-active pull. `bytes_done` and `bytes_total` drive
/// the percent + bandwidth display; `throughput_bps` is smoothed
/// across the last few progress events (EMA).
#[derive(Debug, Clone)]
pub struct ActivePull {
  pub repo_id: String,
  pub friendly_name: String,
  pub bytes_total: u64,
  pub bytes_done: u64,
  pub throughput_bps: f64,
  pub last_progress_at: Instant,
}

/// One event the download-task shim fires back over the mpsc.
/// Carries enough identity (`repo_id`) so the strip drops events
/// belonging to a pull that's already been replaced (e.g. by an
/// AlreadyCached short-circuit).
#[derive(Debug)]
pub enum DownloadEvent {
  /// Repo listing + HEAD probes resolved; the strip now knows the
  /// `bytes_total` for the active pull.
  Started { repo_id: String, bytes_total: u64 },
  /// One chunk landed.
  Progress {
    repo_id: String,
    bytes_done: u64,
    bytes_total: u64,
  },
  /// All files downloaded.
  Finished { repo_id: String },
  /// hf-hub or the disk precheck refused. `message` lands on the
  /// strip for [`ERROR_LINGER`] then the strip clears.
  Error { repo_id: String, message: String },
  /// The file is already in the HF cache. The dialog toasts +
  /// selects the matching catalog row; the strip skips the active
  /// state and drains the next queued pull immediately (R116).
  AlreadyCached {
    repo_id: String,
    cached_path: std::path::PathBuf,
  },
}

/// Pinned download strip state owned by `App`. Render-side only —
/// the actual download task spawns from `events.rs` and ships
/// progress back over `event_tx`.
#[derive(Debug)]
pub struct DownloadStripState {
  pub queue: VecDeque<QueuedPull>,
  pub active: Option<ActivePull>,
  pub last_error: Option<(String, Instant)>,
  pub event_rx: mpsc::UnboundedReceiver<DownloadEvent>,
  pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
  /// Last `AlreadyCached` event observed. The events.rs drain
  /// consumes this once to toast + select the matching list-pane
  /// row, then clears it.
  pub pending_cache_hit: Option<std::path::PathBuf>,
  /// Abort handle for the tokio task currently writing to the cache.
  /// Held next to `active` so `Ctrl+X:cancel download` can stop the
  /// in-flight pull without touching the FIFO queue. Cleared on every
  /// terminal event (Finished / Error / AlreadyCached / explicit
  /// cancel) so stale handles never accumulate.
  pub active_abort: Option<tokio::task::AbortHandle>,
}

impl Default for DownloadStripState {
  fn default() -> Self {
    let (tx, rx) = mpsc::unbounded_channel();
    Self {
      queue: VecDeque::new(),
      active: None,
      last_error: None,
      event_rx: rx,
      event_tx: tx,
      pending_cache_hit: None,
      active_abort: None,
    }
  }
}

impl DownloadStripState {
  /// `true` when the strip should be rendered — there's an active
  /// pull, a queued pull about to start, or a lingering error.
  pub fn is_active(&self) -> bool {
    self.active.is_some() || !self.queue.is_empty() || self.lingering_error().is_some()
  }

  /// `Some(msg)` while an error message is still inside the
  /// [`ERROR_LINGER`] window.
  pub fn lingering_error(&self) -> Option<&str> {
    self
      .last_error
      .as_ref()
      .filter(|(_, t)| t.elapsed() < ERROR_LINGER)
      .map(|(s, _)| s.as_str())
  }

  /// Enqueue a pull. Returns the queue position the caller can
  /// surface in a toast.
  pub fn enqueue(&mut self, pull: QueuedPull) -> usize {
    self.queue.push_back(pull);
    self.queue.len() + self.active.as_ref().map(|_| 1).unwrap_or(0)
  }

  /// Promote the next queued pull to active. Returns the promoted
  /// pull (so the caller can spawn the download task).
  pub fn promote_next(&mut self) -> Option<QueuedPull> {
    if self.active.is_some() {
      return None;
    }
    self.queue.pop_front()
  }

  /// Apply a `Started` event. Initialises the active pull with
  /// the resolved `bytes_total`.
  pub fn apply_started(&mut self, repo_id: &str, bytes_total: u64) {
    let Some(active) = self.active.as_mut() else {
      return;
    };
    if active.repo_id != repo_id {
      return;
    }
    active.bytes_total = bytes_total;
    active.bytes_done = 0;
    active.throughput_bps = 0.0;
    active.last_progress_at = Instant::now();
  }

  /// Apply a `Progress` event. Updates `bytes_done` + computes an
  /// EMA-smoothed throughput.
  pub fn apply_progress(&mut self, repo_id: &str, bytes_done: u64, bytes_total: u64) {
    let Some(active) = self.active.as_mut() else {
      return;
    };
    if active.repo_id != repo_id {
      return;
    }
    let now = Instant::now();
    let elapsed = now
      .saturating_duration_since(active.last_progress_at)
      .as_secs_f64()
      .max(1e-6);
    let delta = bytes_done.saturating_sub(active.bytes_done) as f64;
    let instant_bps = delta / elapsed;
    // EMA with α = 0.3 — quick enough to track throughput swings
    // without flicker.
    active.throughput_bps = 0.3 * instant_bps + 0.7 * active.throughput_bps;
    active.bytes_done = bytes_done;
    active.bytes_total = bytes_total.max(active.bytes_total);
    active.last_progress_at = now;
  }

  /// Apply a `Finished` event. Clears the active slot and returns
  /// the next queued pull (if any) for the caller to spawn.
  pub fn apply_finished(&mut self, repo_id: &str) -> Option<QueuedPull> {
    if self.active.as_ref().map(|a| a.repo_id.as_str()) != Some(repo_id) {
      return None;
    }
    self.active = None;
    self.active_abort = None;
    self.last_error = None;
    self.promote_next()
  }

  /// Apply an `Error` event. Clears the active slot, parks the
  /// message for [`ERROR_LINGER`], and returns the next queued pull.
  pub fn apply_error(&mut self, repo_id: &str, message: String) -> Option<QueuedPull> {
    if self.active.as_ref().map(|a| a.repo_id.as_str()) != Some(repo_id) {
      return None;
    }
    self.active = None;
    self.active_abort = None;
    self.last_error = Some((message, Instant::now()));
    self.promote_next()
  }

  /// Apply an `AlreadyCached` event. The active pull (if any matches
  /// the repo id) clears so the strip stops claiming a row; the
  /// caller will toast + select the matching catalog row. Also fires
  /// when no active pull is installed — the pre-flight cache probe
  /// in `enqueue_hf_pull` emits this event without ever promoting,
  /// so the drain still needs to surface the toast + row-snap.
  pub fn apply_already_cached(
    &mut self,
    repo_id: &str,
    cached_path: std::path::PathBuf,
  ) -> Option<QueuedPull> {
    if let Some(active) = self.active.as_ref() {
      if active.repo_id == repo_id {
        self.active = None;
        self.active_abort = None;
      }
    }
    self.pending_cache_hit = Some(cached_path);
    self.promote_next()
  }

  /// Cancel the currently-active pull. Aborts the spawned download
  /// task (so hf-hub stops writing to the cache mid-chunk), clears
  /// the active slot + its abort handle, and returns the next queued
  /// pull for the caller to spawn. Returns `None` when nothing was
  /// active — caller should toast in that case.
  ///
  /// Queue ordering: the queued tail stays intact. Pressing Ctrl+X
  /// again once the next pull promotes will cancel that one too.
  pub fn cancel_active(&mut self) -> CancelOutcome {
    let Some(active) = self.active.take() else {
      return CancelOutcome::NothingActive;
    };
    if let Some(handle) = self.active_abort.take() {
      handle.abort();
    }
    let next = self.promote_next();
    CancelOutcome::Cancelled {
      cancelled_repo_id: active.repo_id,
      cancelled_friendly_name: active.friendly_name,
      next,
    }
  }

  /// Construct the [`ActivePull`] shell that's filled in by the
  /// first `Started` / `Progress` event.
  pub fn install_active(&mut self, pull: &QueuedPull) {
    self.active = Some(ActivePull {
      repo_id: pull.repo_id.clone(),
      friendly_name: pull.friendly_name.clone(),
      bytes_total: pull.row.size_bytes().unwrap_or(0),
      bytes_done: 0,
      throughput_bps: 0.0,
      last_progress_at: Instant::now(),
    });
  }
}

/// Paint the 1-line strip into `area`. `cancel_hint` (when supplied)
/// renders to the right of the progress text so the `Ctrl+X:cancel`
/// chip is discoverable without opening the help overlay. Caller
/// resolves the live key label off the keymap so a config rebind
/// flows through.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  state: &DownloadStripState,
  cancel_hint: Option<&str>,
  palette: &Palette,
) {
  if let Some(active) = state.active.as_ref() {
    let percent = if active.bytes_total > 0 {
      (active.bytes_done as f64 / active.bytes_total as f64 * 100.0) as u32
    } else {
      0
    };
    let bytes = format!(
      "{} / {}",
      crate::tui::fmt::format_bytes(active.bytes_done),
      crate::tui::fmt::format_bytes(active.bytes_total)
    );
    let throughput = if active.throughput_bps > 0.0 {
      format!(
        "{}/s",
        crate::tui::fmt::format_bytes(active.throughput_bps as u64)
      )
    } else {
      "—".into()
    };
    let queue_tail = if state.queue.is_empty() {
      String::new()
    } else {
      format!(" · +{} queued", state.queue.len())
    };
    let mut spans = vec![
      Span::styled("⬇ ", palette.label_style()),
      Span::styled(active.friendly_name.clone(), palette.text_style()),
      Span::raw("  "),
      Span::styled(format!("{percent:>3}%"), palette.text_style()),
      Span::raw("  "),
      Span::styled(bytes, palette.muted_style()),
      Span::styled(" · ", palette.muted_style()),
      Span::styled(throughput, palette.label_style()),
      Span::styled(queue_tail, palette.muted_style()),
    ];
    if let Some(hint) = cancel_hint {
      spans.push(Span::styled("  · ", palette.muted_style()));
      spans.push(Span::styled(hint.to_string(), palette.warning_style()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    return;
  }
  if let Some(err) = state.lingering_error() {
    let line = Line::from(vec![
      Span::styled("✗ ", palette.error_style()),
      Span::styled(err.to_string(), palette.error_style()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    return;
  }
  // Queued-but-not-yet-promoted: rare interstitial; just show "queuing…"
  // to avoid a blank reserved row.
  if !state.queue.is_empty() {
    let line = Line::from(vec![Span::styled(
      format!("⬇ queuing {} pull(s)…", state.queue.len()),
      palette.muted_style(),
    )]);
    frame.render_widget(Paragraph::new(line), area);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_pull(repo: &str, filename: &str, size: Option<u64>) -> QueuedPull {
    QueuedPull {
      repo_id: repo.into(),
      friendly_name: format!("{repo} ({filename})"),
      row: PickerRow::Single {
        filename: filename.into(),
        size_bytes: size,
      },
    }
  }

  #[test]
  fn empty_strip_is_not_active() {
    let strip = DownloadStripState::default();
    assert!(!strip.is_active());
  }

  #[test]
  fn enqueue_then_promote_installs_active() {
    let mut strip = DownloadStripState::default();
    let pull = make_pull("owner/repo", "model.gguf", Some(123));
    strip.enqueue(pull);
    let promoted = strip.promote_next().expect("queue has an item");
    strip.install_active(&promoted);
    assert!(strip.active.is_some());
    assert!(strip.is_active());
  }

  #[test]
  fn fifo_order_promotes_first_pull_first() {
    let mut strip = DownloadStripState::default();
    strip.enqueue(make_pull("a/b", "a.gguf", None));
    strip.enqueue(make_pull("c/d", "c.gguf", None));
    let first = strip.promote_next().unwrap();
    assert_eq!(first.repo_id, "a/b");
    strip.install_active(&first);
    // Second one stays in queue while first is active.
    let next = strip.promote_next();
    assert!(next.is_none(), "promote refuses while active is set");
  }

  #[test]
  fn finished_clears_active_and_returns_next() {
    let mut strip = DownloadStripState::default();
    strip.enqueue(make_pull("a/b", "a.gguf", None));
    strip.enqueue(make_pull("c/d", "c.gguf", None));
    let first = strip.promote_next().unwrap();
    strip.install_active(&first);
    let next = strip.apply_finished("a/b").expect("queue still has c/d");
    assert_eq!(next.repo_id, "c/d");
  }

  #[test]
  fn error_event_clears_active_and_lingers() {
    let mut strip = DownloadStripState::default();
    let pull = make_pull("owner/repo", "model.gguf", None);
    strip.enqueue(pull.clone());
    let promoted = strip.promote_next().unwrap();
    strip.install_active(&promoted);
    strip.apply_error("owner/repo", "rate-limited".into());
    assert!(strip.active.is_none());
    assert_eq!(strip.lingering_error(), Some("rate-limited"));
  }

  #[test]
  fn progress_updates_bytes_and_throughput_within_active_pull() {
    let mut strip = DownloadStripState::default();
    let pull = make_pull("owner/repo", "model.gguf", Some(1_000_000));
    strip.enqueue(pull);
    let promoted = strip.promote_next().unwrap();
    strip.install_active(&promoted);
    strip.apply_started("owner/repo", 1_000_000);
    strip.apply_progress("owner/repo", 500_000, 1_000_000);
    let active = strip.active.as_ref().unwrap();
    assert_eq!(active.bytes_done, 500_000);
    assert_eq!(active.bytes_total, 1_000_000);
  }

  #[test]
  fn stale_progress_for_different_repo_is_dropped() {
    let mut strip = DownloadStripState::default();
    let pull = make_pull("owner/repo", "model.gguf", Some(1_000_000));
    strip.enqueue(pull);
    let promoted = strip.promote_next().unwrap();
    strip.install_active(&promoted);
    strip.apply_progress("other/repo", 999_999, 999_999);
    assert_eq!(strip.active.as_ref().unwrap().bytes_done, 0);
  }

  #[test]
  fn already_cached_clears_active_and_records_cached_path() {
    let mut strip = DownloadStripState::default();
    let pull = make_pull("owner/repo", "model.gguf", Some(1));
    strip.enqueue(pull);
    let promoted = strip.promote_next().unwrap();
    strip.install_active(&promoted);
    let path = std::path::PathBuf::from("/cache/hf/.../snapshot/file.gguf");
    strip.apply_already_cached("owner/repo", path.clone());
    assert!(strip.active.is_none());
    assert_eq!(strip.pending_cache_hit.as_ref(), Some(&path));
  }

  #[test]
  fn cancel_active_on_idle_strip_returns_nothing_active() {
    let mut strip = DownloadStripState::default();
    match strip.cancel_active() {
      CancelOutcome::NothingActive => {}
      other => panic!("expected NothingActive, got {other:?}"),
    }
  }

  #[test]
  fn cancel_active_aborts_promotes_next_and_returns_friendly_name() {
    // Two pulls queued; cancel the active one and assert the second
    // is returned for the caller to spawn. The cancelled repo id
    // matches what the popup confirmed against so toasts stay honest.
    let mut strip = DownloadStripState::default();
    strip.enqueue(make_pull("a/b", "a.gguf", Some(100)));
    strip.enqueue(make_pull("c/d", "c.gguf", Some(200)));
    let first = strip.promote_next().unwrap();
    strip.install_active(&first);
    match strip.cancel_active() {
      CancelOutcome::Cancelled {
        cancelled_repo_id,
        cancelled_friendly_name,
        next,
      } => {
        assert_eq!(cancelled_repo_id, "a/b");
        assert!(cancelled_friendly_name.contains("a.gguf"));
        let next = next.expect("c/d must be returned for the caller to spawn");
        assert_eq!(next.repo_id, "c/d");
      }
      other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(strip.active.is_none(), "active slot must be cleared");
    assert!(
      strip.active_abort.is_none(),
      "abort handle must be cleared so a later cancel doesn't abort the wrong task"
    );
  }

  #[test]
  fn cancel_active_with_empty_queue_clears_active_and_returns_no_next() {
    let mut strip = DownloadStripState::default();
    strip.enqueue(make_pull("solo/repo", "solo.gguf", Some(123)));
    let pull = strip.promote_next().unwrap();
    strip.install_active(&pull);
    match strip.cancel_active() {
      CancelOutcome::Cancelled {
        cancelled_repo_id,
        next,
        ..
      } => {
        assert_eq!(cancelled_repo_id, "solo/repo");
        assert!(next.is_none(), "empty queue → no next pull");
      }
      other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(!strip.is_active());
  }

  #[test]
  fn finished_event_clears_abort_handle_too() {
    // Regression: a Finished / Error event must drop the active
    // abort handle alongside the active slot, otherwise a later
    // Ctrl+X would try to abort a tokio task that has already
    // completed (a no-op, but indicates a state-machine bug).
    let mut strip = DownloadStripState::default();
    strip.enqueue(make_pull("a/b", "a.gguf", None));
    let p = strip.promote_next().unwrap();
    strip.install_active(&p);
    // Simulate "we just spawned a task" by parking a fake abort
    // handle. Using a freshly-aborted handle so no real task leaks.
    let placeholder = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap()
      .block_on(async {
        let h = tokio::spawn(async {});
        h.abort_handle()
      });
    strip.active_abort = Some(placeholder);
    let _ = strip.apply_finished("a/b");
    assert!(
      strip.active_abort.is_none(),
      "abort handle survived Finished"
    );
  }

  #[test]
  fn already_cached_records_path_even_without_active_pull() {
    // Regression: the pre-flight cache probe in `enqueue_hf_pull`
    // fires `AlreadyCached` without ever calling `install_active`.
    // The drain still needs to surface the toast + row-snap, so
    // `pending_cache_hit` must be populated even when no pull is
    // currently active.
    let mut strip = DownloadStripState::default();
    let path = std::path::PathBuf::from("/cache/hf/snapshot/file.gguf");
    strip.apply_already_cached("owner/repo", path.clone());
    assert!(strip.active.is_none());
    assert_eq!(strip.pending_cache_hit.as_ref(), Some(&path));
  }
}
