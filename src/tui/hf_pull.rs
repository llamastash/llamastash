//! HuggingFace-pull / download behavior for the TUI.
//!
//! The state half of this subsystem lives in [`crate::tui::hf_dialog`]
//! (the modal browse/search/file-pick state machine) and
//! [`crate::tui::download_strip`] (the queue + active-pull progress
//! strip). This module is their behavior half: it routes dialog
//! keystrokes, spawns the background HF search / repo-listing /
//! download tasks, bridges hf-hub progress callbacks back onto the
//! unified TUI event channel, and applies the resulting
//! `HfDialogEvent` / `DownloadEvent` updates to `App`.
//!
//! The input/event-loop call sites that dispatch into these handlers
//! stay in [`crate::tui::events`]; this module owns the handlers
//! themselves.

use tokio::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent};

use crate::tui::app::{App, ConfirmAction};
use crate::tui::events::{Event, WriterCmd};

/// Open the HuggingFace pull dialog (`d` in `Focus::List`).
/// Offline state is resolved inside `App::open_hf_dialog` from
/// `app.options.offline` ∨ `LLAMASTASH_OFFLINE`, so the call site
/// stays a single line and the runtime offline value travels through
/// `AppOptions`.
pub(crate) fn apply_open_hf_dialog(app: &mut App) {
  app.open_hf_dialog();
}

/// Stage a cancel-download confirmation. Refuses (with a toast) when
/// no pull is currently active — pressing Ctrl+X on an empty strip
/// shouldn't bring up a popup with nothing to confirm. The popup
/// payload mirrors what the strip is showing so the user reads the
/// same identifier they pressed Ctrl+X over.
pub(crate) fn apply_cancel_download(app: &mut App) {
  let Some(active) = app.download_strip.active.as_ref() else {
    app.show_toast("no active download to cancel");
    return;
  };
  app.confirm_dialog = Some(ConfirmAction::CancelDownload {
    repo_id: active.repo_id.clone(),
    friendly_name: active.friendly_name.clone(),
  });
}

/// Outcome of dispatching a key into the HF dialog. The router
/// resolves the state-mutating action under a single `&mut
/// state` borrow, then surfaces any side effect (toast, close) for
/// the caller to apply against `&mut App` once the borrow ends.
enum HfDialogOutcome {
  None,
  Toast(&'static str),
  EnqueuePull {
    repo: String,
    row: crate::tui::hf_dialog::PickerRow,
  },
  Close,
}

/// Per-stage key router for `Focus::HfDialog`.
///
/// Search stage routes keys through the modal `InputField` first:
/// while editing, printable chars / Backspace mutate the query and
/// Esc steps the field out of edit; while resting `e` re-enters
/// edit, Esc clears a non-empty buffer (or closes the dialog when
/// already empty), and the dialog's own keymap (`o`, `n`, `p`, …)
/// fires through. Enter always means "submit the current query /
/// row" regardless of edit mode. FilePicker and Confirm stages use
/// Esc to walk back one stage; arrow keys move the cursor.
pub(crate) fn handle_hf_dialog_input(
  app: &mut App,
  key: KeyEvent,
  _writer: Option<&mpsc::Sender<WriterCmd>>,
) {
  use crate::tui::hf_dialog::HfStage;
  use crate::tui::input_field::InputOutcome;
  // Cloned out before the `&mut app.hf_dialog` borrow lands so the spawn
  // helpers can keep their `Option<Sender>` signature without forcing
  // every caller to thread the tx explicitly.
  let events_tx = app.events_tx.clone();
  let outcome = {
    let Some(state) = app.hf_dialog.as_mut() else {
      return;
    };
    match state.stage {
      HfStage::Search => {
        match state.handle_search_key(key) {
          InputOutcome::Handled => HfDialogOutcome::None,
          InputOutcome::Submit => match state.submit_search() {
            Some(repo_id) => {
              spawn_hf_list_repo_files(state, repo_id, events_tx.clone());
              HfDialogOutcome::None
            }
            None => HfDialogOutcome::Toast("type a query, paste a slug, or pick a row"),
          },
          InputOutcome::PassThrough => match key.code {
            // The input only PassThroughs Esc when the buffer is
            // empty and the field is resting, so this arm always
            // means "close the dialog."
            KeyCode::Esc => HfDialogOutcome::Close,
            KeyCode::Up => {
              state.move_up();
              HfDialogOutcome::None
            }
            KeyCode::Down => {
              state.move_down();
              HfDialogOutcome::None
            }
            KeyCode::Enter => match state.submit_search() {
              Some(repo_id) => {
                spawn_hf_list_repo_files(state, repo_id, events_tx.clone());
                HfDialogOutcome::None
              }
              None => HfDialogOutcome::Toast("type a query, paste a slug, or pick a row"),
            },
            KeyCode::Char('o') => {
              state.cycle_sort();
              HfDialogOutcome::None
            }
            KeyCode::Char('n') => {
              if let Some(cursor) = state.advance_page() {
                spawn_hf_search(state, cursor, events_tx.clone());
              }
              HfDialogOutcome::None
            }
            KeyCode::Char('p') => {
              if let Some(cursor) = state.retreat_page() {
                spawn_hf_search(state, cursor, events_tx.clone());
              }
              HfDialogOutcome::None
            }
            _ => HfDialogOutcome::None,
          },
        }
      }
      HfStage::FilePicker => match key.code {
        // Esc walks back to Search; a further Esc on Search closes
        // the dialog.
        KeyCode::Esc => {
          state.back_to_search();
          HfDialogOutcome::None
        }
        KeyCode::Up => {
          state.move_up();
          HfDialogOutcome::None
        }
        KeyCode::Down => {
          state.move_down();
          HfDialogOutcome::None
        }
        KeyCode::Enter => {
          if state.submit_picker() {
            HfDialogOutcome::None
          } else {
            HfDialogOutcome::Toast("no file selected")
          }
        }
        _ => HfDialogOutcome::None,
      },
      HfStage::Confirm => match key.code {
        KeyCode::Esc => {
          state.back_to_picker();
          HfDialogOutcome::None
        }
        KeyCode::Enter => {
          if let Some((repo, row)) = state.take_confirm_target() {
            HfDialogOutcome::EnqueuePull { repo, row }
          } else {
            HfDialogOutcome::Close
          }
        }
        _ => HfDialogOutcome::None,
      },
    }
  };
  match outcome {
    HfDialogOutcome::None => {}
    HfDialogOutcome::Toast(msg) => app.show_toast(msg),
    HfDialogOutcome::EnqueuePull { repo, row } => {
      enqueue_hf_pull(app, repo, row);
      app.close_hf_dialog();
    }
    HfDialogOutcome::Close => app.close_hf_dialog(),
  }
}

/// Push a pull onto the download-strip queue and — when no pull is
/// currently active — promote it and spawn the background download
/// task that ships progress back over the strip's mpsc.
pub(crate) fn enqueue_hf_pull(app: &mut App, repo: String, row: crate::tui::hf_dialog::PickerRow) {
  use crate::tui::download_strip::{DownloadEvent, QueuedPull};
  let filename = row.download_filename().to_string();
  // Cache-hit short-circuit: probe the HF cache before queuing
  // anything. When every requested file already lives under a single
  // snapshot dir, emit AlreadyCached directly via the strip's mpsc
  // and skip both the queue and the download spawn. Deterministic —
  // a partial cache still falls through to a real download.
  if let Some(cached_path) = probe_cached_pull(&repo, &row.all_filenames()) {
    if let Some(tx) = &app.events_tx {
      let _ = tx.try_send(Event::Download(DownloadEvent::AlreadyCached {
        repo_id: repo.clone(),
        cached_path,
      }));
    }
    return;
  }
  let pull = QueuedPull {
    repo_id: repo.clone(),
    friendly_name: format!("{repo} :{filename}"),
    row,
  };
  let queue_pos = app.download_strip.enqueue(pull);
  app.show_toast(format!("pull queued: {repo} :{filename} (#{queue_pos})"));
  // Promote immediately when nothing's active so the user sees the
  // strip light up on the next render.
  if app.download_strip.active.is_none() {
    if let Some(promoted) = app.download_strip.promote_next() {
      app.download_strip.install_active(&promoted);
      if let Some(tx) = app.events_tx.clone() {
        let abort = spawn_download_task(promoted, app.options.offline, tx);
        app.download_strip.active_abort = Some(abort);
      }
    }
  }
}

/// Probe the HF cache for every filename the pull would produce on
/// disk and, when all are present under the same snapshot directory,
/// return the path to the user-facing first file (the row's
/// `download_filename`). Used by `enqueue_hf_pull` to short-circuit a
/// redundant pull deterministically. Returns `None` when the
/// repo isn't cached, any shard is missing, or the cache root can't
/// be resolved on this platform.
pub(crate) fn probe_cached_pull(repo_id: &str, filenames: &[String]) -> Option<std::path::PathBuf> {
  if filenames.is_empty() {
    return None;
  }
  let cache_root = crate::init::download::hf_cache_dir().ok()?;
  let repo_dir = cache_root.join(crate::util::model_caches::repo_folder_name(repo_id));
  let snapshots = repo_dir.join("snapshots");
  let entries = std::fs::read_dir(&snapshots).ok()?;
  for entry in entries.filter_map(|e| e.ok()) {
    let snapshot = entry.path();
    if !snapshot.is_dir() {
      continue;
    }
    // The HF cache exposes files as symlinks under `snapshots/<rev>/`.
    // A snapshot only counts as a hit when every requested filename
    // resolves there — partial caches (e.g. only shard 1) must fall
    // through to the real download path.
    if filenames.iter().all(|f| snapshot.join(f).exists()) {
      return Some(snapshot.join(&filenames[0]));
    }
  }
  None
}

/// Spawn a tokio task that calls `init::download::download_repo`
/// with a `DownloadProgress` shim relaying every callback to the
/// strip's mpsc. Caller has already run `probe_cached_pull` against
/// the requested files, so this path only fires for real downloads —
/// the cache-hit short-circuit lives next to the queue
/// enqueue, not inside the spawn.
///
/// Returns the spawned task's [`tokio::task::AbortHandle`] so the
/// `Ctrl+X:cancel download` flow can interrupt an in-flight pull
/// mid-chunk. Aborting drops hf-hub's stream future, leaves any
/// partial blob in the cache, and (because the task never sends
/// `Finished` / `Error` after the abort point) lets the strip's
/// own state transition drive the next promotion.
///
/// `offline` is the runtime-resolved offline flag (CLI `--offline` ∨
/// `LLAMASTASH_OFFLINE`). Passing `true` ensures the spawned task's
/// FetchClient short-circuits before it issues any HF traffic — the
/// pull surfaces as a clean offline error in the strip rather than
/// silently bypassing the user's chosen network policy.
pub(crate) fn spawn_download_task(
  pull: crate::tui::download_strip::QueuedPull,
  offline: bool,
  tx: mpsc::Sender<Event>,
) -> tokio::task::AbortHandle {
  use crate::init::download::{DownloadOptions, RepoSpec};
  use crate::init::fetch;
  let handle = tokio::spawn(async move {
    let push_dl = |evt: crate::tui::download_strip::DownloadEvent| {
      let tx = tx.clone();
      async move {
        let _ = tx.send(Event::Download(evt)).await;
      }
    };
    let spec = match RepoSpec::parse(&format!(
      "{}:{}",
      pull.repo_id,
      pull.row.download_filename()
    )) {
      Ok(s) => s,
      Err(e) => {
        push_dl(crate::tui::download_strip::DownloadEvent::Error {
          repo_id: pull.repo_id.clone(),
          message: e.to_string(),
        })
        .await;
        return;
      }
    };
    let fetch_client =
      fetch::build_with_offline_check(offline, fetch::FetchClientConfig::default())
        .unwrap_or_else(|_| fetch::FetchClient::offline());
    let progress = std::sync::Arc::new(StripProgress {
      tx: tx.clone(),
      repo_id: pull.repo_id.clone(),
      inner: std::sync::Mutex::new(StripProgressInner::default()),
    });
    let options = DownloadOptions {
      extension_filter: Some(".gguf".into()),
      estimated_bytes: pull.row.size_bytes(),
      progress: Some(
        progress.clone() as std::sync::Arc<dyn crate::init::download::DownloadProgress>
      ),
      revision: None,
      fallback_repos: Vec::new(),
      quant_hint: None,
    };
    match crate::init::download::download_repo(&spec, &fetch_client, &options).await {
      Ok(_) => {
        push_dl(crate::tui::download_strip::DownloadEvent::Finished {
          repo_id: pull.repo_id.clone(),
        })
        .await;
      }
      Err(e) => {
        push_dl(crate::tui::download_strip::DownloadEvent::Error {
          repo_id: pull.repo_id.clone(),
          message: e.to_string(),
        })
        .await;
      }
    }
  });
  handle.abort_handle()
}

/// DownloadProgress shim that forwards hf-hub callbacks into the
/// download strip's mpsc. Tracks per-file sizes resolved at the
/// listing pass so per-file finish callbacks aggregate cleanly
/// across multi-shard pulls. Byte-level progress flows via
/// `on_bytes_progress` — driven by `HfHubProgressAdapter` bridging
/// hf-hub's `Progress::update(size)` chunk callbacks into our
/// cumulative `(filename, bytes_in_file)` shape. The `bytes_total`
/// clamp inside [`StripProgressInner`] protects against the
/// (theoretical) race where a late `update` chunk lands after the
/// per-file `Finished` callback — `bytes_done.saturating_add` is
/// clamped to `bytes_total` so the strip can't overshoot 100%.
struct StripProgress {
  tx: mpsc::Sender<Event>,
  repo_id: String,
  inner: std::sync::Mutex<StripProgressInner>,
}

impl StripProgress {
  /// Push a `DownloadEvent` onto the unified TUI channel from a sync
  /// trait-method context. Bounded `try_send` so a wedged main loop
  /// drops progress frames rather than blocking the download path —
  /// progress is firehose, the next chunk reflects the same state.
  ///
  /// Dropping a `Started` here is recoverable: every subsequent
  /// `Progress` carries `bytes_total`, and
  /// [`crate::tui::download_strip::DownloadStripState::apply_progress`]
  /// lifts the strip's `bytes_total` via `.max()` so the first Progress
  /// to land after a dropped Started repairs the state machine. See the
  /// `progress_without_started_repairs_state` test in `download_strip.rs`.
  /// `Finished` + `Error` from this trait impl are not emitted (only the
  /// outer `spawn_download_task` posts those, via the awaiting
  /// `push_dl` closure that survives backpressure).
  fn push(&self, evt: crate::tui::download_strip::DownloadEvent) {
    let _ = self.tx.try_send(Event::Download(evt));
  }
}

#[derive(Default)]
struct StripProgressInner {
  /// `filename → size_bytes` snapshot captured at
  /// `on_files_resolved` time.
  file_sizes: std::collections::HashMap<String, u64>,
  bytes_total: u64,
  bytes_done: u64,
  /// Bytes credited so far for the file currently downloading.
  /// Replaces the running per-file counter every `on_bytes_progress`;
  /// reset to zero at `on_file_finished` so the next file starts
  /// fresh.
  bytes_in_current_file: u64,
}

impl crate::init::download::DownloadProgress for StripProgress {
  fn on_files_resolved(&self, files: &[(String, u64)]) {
    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.file_sizes = files.iter().cloned().collect();
    inner.bytes_total = files.iter().map(|(_, n)| *n).sum();
    inner.bytes_done = 0;
    let bytes_total = inner.bytes_total;
    drop(inner);
    self.push(crate::tui::download_strip::DownloadEvent::Started {
      repo_id: self.repo_id.clone(),
      bytes_total,
    });
  }

  fn on_file_started(&self, _filename: &str, _size: u64, _index: usize, _total: usize) {
    // Per-file byte counter resets on every file boundary; the
    // hf-hub adapter then drives `on_bytes_progress` as chunks land.
    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.bytes_in_current_file = 0;
  }

  fn on_file_finished(&self, filename: &str, _index: usize, _total: usize) {
    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    let size = inner.file_sizes.get(filename).copied().unwrap_or(0);
    let prior_in_file = inner.bytes_in_current_file;
    // Aggregate the file's full size into the pull total (subtract any
    // partial credit `on_bytes_progress` already attributed so we don't
    // double-count).
    let credit = size.saturating_sub(prior_in_file);
    inner.bytes_done = inner
      .bytes_total
      .min(inner.bytes_done.saturating_add(credit));
    inner.bytes_in_current_file = 0;
    let bytes_done = inner.bytes_done;
    let bytes_total = inner.bytes_total;
    drop(inner);
    self.push(crate::tui::download_strip::DownloadEvent::Progress {
      repo_id: self.repo_id.clone(),
      bytes_done,
      bytes_total,
    });
  }

  fn on_bytes_progress(&self, _filename: &str, bytes_in_file: u64) {
    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    // Replace the running per-file count with the new cumulative
    // value. Subtract the previous in-file credit so the pull's
    // aggregate `bytes_done` only ever grows monotonically.
    let prior = inner.bytes_in_current_file;
    let delta = bytes_in_file.saturating_sub(prior);
    inner.bytes_in_current_file = bytes_in_file;
    inner.bytes_done = inner
      .bytes_total
      .min(inner.bytes_done.saturating_add(delta));
    let bytes_done = inner.bytes_done;
    let bytes_total = inner.bytes_total;
    drop(inner);
    self.push(crate::tui::download_strip::DownloadEvent::Progress {
      repo_id: self.repo_id.clone(),
      bytes_done,
      bytes_total,
    });
  }
}

/// Spawn a background `init::hf_api::search` task whose result lands
/// on the unified TUI event channel as
/// `Event::HfDialog(HfDialogEvent::Search*)`. Tagged with the dialog's
/// current `query_seq` so the apply step can discard stale responses.
/// `events_tx` is `None` only in tests that drive the dialog without
/// a tokio runtime — the dispatch is then a no-op.
fn spawn_hf_search(
  state: &mut crate::tui::hf_dialog::HfDialogState,
  cursor: Option<String>,
  events_tx: Option<mpsc::Sender<Event>>,
) {
  use crate::init::hf_api;
  let query = state.input.buffer().to_string();
  let sort = state.sort;
  let seq = state.query_seq;
  let offline = state.offline;
  state.mark_dispatched();
  let Some(tx) = events_tx else {
    return;
  };
  tokio::spawn(async move {
    let fetch_client = build_tui_fetch_client(offline);
    let evt = match hf_api::search(&fetch_client, &query, sort, cursor.as_deref()).await {
      Ok(page) => crate::tui::hf_dialog::HfDialogEvent::SearchResults { seq, page },
      Err(e) => crate::tui::hf_dialog::HfDialogEvent::SearchFailed { seq, error: e },
    };
    let _ = tx.send(Event::HfDialog(evt)).await;
  });
}

/// Spawn a background `list_repo_files` task whose result lands on
/// the unified TUI event channel as
/// `Event::HfDialog(HfDialogEvent::RepoFiles*)`.
fn spawn_hf_list_repo_files(
  state: &mut crate::tui::hf_dialog::HfDialogState,
  repo_id: String,
  events_tx: Option<mpsc::Sender<Event>>,
) {
  use crate::init::hf_api;
  let offline = state.offline;
  let Some(tx) = events_tx else {
    return;
  };
  tokio::spawn(async move {
    let fetch_client = build_tui_fetch_client(offline);
    let evt = match hf_api::list_repo_files(&fetch_client, &repo_id).await {
      Ok(files) => crate::tui::hf_dialog::HfDialogEvent::RepoFiles {
        repo_id: repo_id.clone(),
        files,
      },
      Err(error) => crate::tui::hf_dialog::HfDialogEvent::RepoFilesFailed {
        repo_id: repo_id.clone(),
        error,
      },
    };
    let _ = tx.send(Event::HfDialog(evt)).await;
  });
}

/// Build the dialog's FetchClient. Mirrors the wizard's resolution:
/// honour `LLAMASTASH_OFFLINE`, fall back to a fresh client with the
/// default config (host allowlist + redirect cap + body cap). The
/// `offline` arg threads the runtime's resolved offline state
/// (CLI `--offline` ∨ `LLAMASTASH_OFFLINE`) so the dialog can't make
/// network calls behind the user's back. On builder error we hand
/// back an offline stub so the dialog's network calls fail with a
/// clean typed error instead of panicking.
fn build_tui_fetch_client(offline: bool) -> crate::init::fetch::FetchClient {
  use crate::init::fetch::{build_with_offline_check, FetchClient, FetchClientConfig};
  build_with_offline_check(offline, FetchClientConfig::default())
    .unwrap_or_else(|_| FetchClient::offline())
}

/// Apply one [`crate::tui::download_strip::DownloadEvent`] to the strip. Promotes the next
/// queued pull when the active one finishes / errors / hits the
/// cache; surfaces a toast when a cache-hit short-circuit lands.
pub fn apply_download_event(app: &mut App, evt: crate::tui::download_strip::DownloadEvent) {
  use crate::tui::download_strip::DownloadEvent;
  let next_pull = match evt {
    DownloadEvent::Started {
      repo_id,
      bytes_total,
    } => {
      app.download_strip.apply_started(&repo_id, bytes_total);
      None
    }
    DownloadEvent::Progress {
      repo_id,
      bytes_done,
      bytes_total,
    } => {
      app
        .download_strip
        .apply_progress(&repo_id, bytes_done, bytes_total);
      None
    }
    DownloadEvent::Finished { repo_id } => {
      let label = app
        .download_strip
        .active
        .as_ref()
        .map(|a| a.friendly_name.clone());
      let next = app.download_strip.apply_finished(&repo_id);
      if let Some(name) = label {
        app.show_toast(format!("downloaded {name}"));
      }
      next
    }
    DownloadEvent::Error { repo_id, message } => app.download_strip.apply_error(&repo_id, message),
    DownloadEvent::AlreadyCached {
      repo_id,
      cached_path,
    } => {
      let next = app
        .download_strip
        .apply_already_cached(&repo_id, cached_path);
      if let Some(path) = app.download_strip.pending_cache_hit.take() {
        app.show_toast(format!(
          "already downloaded — {}",
          path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
        ));
        // Select the matching catalog row (path equality) so the
        // user lands on it. Best-effort — `models` may not yet
        // reflect the just-cached file until the next refresh.
        if let Some(idx) = app.models.iter().position(|m| m.path == path) {
          // Find the row index in rendered_rows that matches this
          // model path so the cursor visibly snaps.
          let target = app.models[idx].path.clone();
          let rows = app.rendered_rows();
          if let Some(row_idx) = rows
            .iter()
            .position(|r| r.path().map(|p| p == target).unwrap_or(false))
          {
            app.list_cursor = row_idx;
          }
        }
      }
      next
    }
  };
  if let Some(pull) = next_pull {
    app.download_strip.install_active(&pull);
    if let Some(tx) = app.events_tx.clone() {
      let abort = spawn_download_task(pull, app.options.offline, tx);
      app.download_strip.active_abort = Some(abort);
    }
  }
}

/// Apply one [`crate::tui::hf_dialog::HfDialogEvent`] to the dialog state. Replaces the
/// prior `drain_hf_dialog` polling helper — the unified loop hands
/// one event in at a time.
pub fn apply_hf_dialog_event(app: &mut App, evt: crate::tui::hf_dialog::HfDialogEvent) {
  use crate::tui::hf_dialog::HfDialogEvent;
  let Some(state) = app.hf_dialog.as_mut() else {
    return;
  };
  match evt {
    HfDialogEvent::SearchResults { seq, page } => state.apply_search_results(seq, page),
    HfDialogEvent::SearchFailed { seq, error } => state.apply_search_failed(seq, error),
    HfDialogEvent::RepoFiles { repo_id, files } => state.apply_repo_files(&repo_id, files),
    HfDialogEvent::RepoFilesFailed { repo_id, error } => {
      state.apply_repo_files_failed(&repo_id, &error)
    }
  }
}

/// Service the HF dialog's debounced live-search dispatch. The
/// unified loop calls this on every tick — once the debounce window
/// elapses since the last keystroke, it fires a fresh search. The
/// `query_seq` monotonicity inside the dialog state still drops
/// stale responses if the user keeps typing.
///
/// Returns `true` iff a new search was dispatched this tick — the
/// caller uses that to decide whether the tick warrants a redraw.
pub fn service_hf_dialog_debounce(app: &mut App) -> bool {
  let events_tx = app.events_tx.clone();
  let Some(state) = app.hf_dialog.as_mut() else {
    return false;
  };
  if state.search_due(std::time::Instant::now()) {
    let cursor = state.current_cursor.clone();
    spawn_hf_search(state, cursor, events_tx);
    return true;
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::App;
  use crate::tui::events::pump_input;
  use crate::tui::keybindings::Focus;
  use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};

  fn key(code: KeyCode, mods: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent::new(code, mods))
  }

  #[test]
  fn shift_p_opens_hf_dialog_and_esc_closes_it() {
    use crate::tui::hf_dialog::HfStage;
    let mut app = App::new(Default::default());
    assert!(app.hf_dialog.is_none());
    pump_input(&mut app, key(KeyCode::Char('P'), KeyModifiers::SHIFT));
    let dialog = app
      .hf_dialog
      .as_ref()
      .expect("Shift+P must open the HF dialog");
    assert_eq!(dialog.stage, HfStage::Search);
    assert!(
      dialog.input.is_editing(),
      "search field must auto-enter edit mode so the user can type immediately"
    );
    assert_eq!(app.focus, Focus::HfDialog);
    // Type into the search buffer.
    pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('w'), KeyModifiers::NONE));
    assert_eq!(app.hf_dialog.as_ref().map(|d| d.input.buffer()), Some("qw"));
    // First Esc: exit edit (buffer kept, dialog still open).
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    let after_first_esc = app.hf_dialog.as_ref().expect("first Esc must keep dialog");
    assert!(!after_first_esc.input.is_editing());
    assert_eq!(after_first_esc.input.buffer(), "qw");
    // Second Esc: clear buffer (still open, still resting).
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    let after_second_esc = app.hf_dialog.as_ref().expect("second Esc must keep dialog");
    assert!(after_second_esc.input.is_empty());
    // Third Esc: closes the dialog and returns focus.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.hf_dialog.is_none());
    assert_eq!(app.focus, Focus::List);
  }

  #[test]
  fn shift_p_opens_hf_dialog_from_right_pane() {
    // Shift+P scope was widened from LIST to NAV so the chord fires
    // from the right pane too — not just the models list.
    let mut app = App::new(Default::default());
    app.focus = Focus::RightPane;
    assert!(app.hf_dialog.is_none());
    pump_input(&mut app, key(KeyCode::Char('P'), KeyModifiers::SHIFT));
    assert!(
      app.hf_dialog.is_some(),
      "Shift+P must open the HF dialog from the right pane"
    );
    assert_eq!(app.focus, Focus::HfDialog);
  }

  #[test]
  fn hf_dialog_o_in_resting_mode_cycles_sort_key() {
    use crate::init::hf_api::HfSortKey;
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('P'), KeyModifiers::SHIFT));
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.sort),
      Some(HfSortKey::Downloads)
    );
    // First Esc exits edit so the dialog's keymap (o / n / p) fires.
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    pump_input(&mut app, key(KeyCode::Char('o'), KeyModifiers::NONE));
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.sort),
      Some(HfSortKey::Likes)
    );
  }

  #[test]
  fn opening_hf_dialog_inherits_offline_flag_from_app_options() {
    // Regression: app.options.offline must propagate into the dialog
    // state at open time so the dialog renders "search disabled" and
    // its spawned fetch tasks short-circuit before HF traffic. A
    // false `app.options.offline` plus `LLAMASTASH_OFFLINE` unset
    // means the dialog stays online; a true value forces offline.
    let mut online = App::new(crate::tui::app::AppOptions {
      offline: false,
      ..Default::default()
    });
    online.open_hf_dialog();
    assert_eq!(
      online.hf_dialog.as_ref().map(|d| d.offline),
      Some(false),
      "online AppOptions must not flip the dialog into offline mode"
    );

    let mut offline = App::new(crate::tui::app::AppOptions {
      offline: true,
      ..Default::default()
    });
    offline.open_hf_dialog();
    assert_eq!(
      offline.hf_dialog.as_ref().map(|d| d.offline),
      Some(true),
      "offline AppOptions must flip the dialog into offline mode"
    );
  }

  #[test]
  fn ctrl_x_with_no_active_download_toasts_refusal() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(
      app.confirm_dialog.is_none(),
      "idle strip must not stage cancel confirm"
    );
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("no active download"),
      "expected refusal toast, got `{toast}`"
    );
  }

  #[test]
  fn ctrl_x_with_active_download_stages_cancel_confirm() {
    use crate::tui::app::ConfirmAction;
    use crate::tui::download_strip::QueuedPull;
    use crate::tui::hf_dialog::PickerRow;
    let mut app = App::new(Default::default());
    let pull = QueuedPull {
      repo_id: "owner/repo".into(),
      friendly_name: "owner/repo :model.gguf".into(),
      row: PickerRow::Single {
        filename: "model.gguf".into(),
        size_bytes: Some(123),
      },
    };
    app.download_strip.enqueue(pull);
    let promoted = app.download_strip.promote_next().unwrap();
    app.download_strip.install_active(&promoted);
    pump_input(&mut app, key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    match app
      .confirm_dialog
      .as_ref()
      .expect("cancel popup must stage")
    {
      ConfirmAction::CancelDownload {
        repo_id,
        friendly_name,
      } => {
        assert_eq!(repo_id, "owner/repo");
        assert!(friendly_name.contains("model.gguf"));
      }
      other => panic!("expected CancelDownload, got {other:?}"),
    }
  }

  #[test]
  fn confirmed_cancel_download_clears_active_and_keeps_queue() {
    // Confirm flow: stage the cancel popup, press Enter, then assert
    // the active slot is empty + the queued pull stayed in line.
    // (The queued pull is auto-promoted by apply_confirmed; with no
    // tokio runtime here we just verify the strip state.)
    use crate::tui::download_strip::QueuedPull;
    use crate::tui::hf_dialog::PickerRow;
    let mut app = App::new(Default::default());
    for (repo, file) in [("a/active", "active.gguf"), ("b/queued", "queued.gguf")] {
      app.download_strip.enqueue(QueuedPull {
        repo_id: repo.into(),
        friendly_name: format!("{repo} :{file}"),
        row: PickerRow::Single {
          filename: file.into(),
          size_bytes: Some(1),
        },
      });
    }
    let promoted = app.download_strip.promote_next().unwrap();
    app.download_strip.install_active(&promoted);
    // Stage the popup, then confirm with `y` (named cancel keys + y
    // are the confirmation chord per `handle_key`).
    pump_input(&mut app, key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(app.confirm_dialog.is_some());
    // The confirm dispatch spawns the next pull through tokio. We
    // can't run tokio here, so use a current-thread runtime to drive
    // the dispatch synchronously.
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap();
    rt.block_on(async {
      pump_input(&mut app, key(KeyCode::Char('y'), KeyModifiers::NONE));
    });
    assert!(app.confirm_dialog.is_none(), "popup must close on confirm");
    let toast = app.toast_message().unwrap_or("");
    assert!(
      toast.contains("cancelled"),
      "expected cancelled toast, got `{toast}`"
    );
    // The next pull was promoted, so active is now `b/queued`.
    let active = app
      .download_strip
      .active
      .as_ref()
      .expect("queued pull must have been promoted");
    assert_eq!(active.repo_id, "b/queued");
  }

  #[test]
  fn hf_dialog_o_while_editing_is_typed_not_cycled() {
    use crate::init::hf_api::HfSortKey;
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('P'), KeyModifiers::SHIFT));
    // Field is auto-edit on open, so `o` is typed.
    pump_input(&mut app, key(KeyCode::Char('o'), KeyModifiers::NONE));
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.input.buffer()),
      Some("o"),
      "`o` while editing must go into the buffer, not cycle sort"
    );
    assert_eq!(
      app.hf_dialog.as_ref().map(|d| d.sort),
      Some(HfSortKey::Downloads),
      "sort must not have cycled while editing"
    );
  }

  #[test]
  // Poisoning a Mutex requires unwinding through the guard's Drop. Under
  // `panic=abort` (the nightly coverage lane) the worker panic aborts the
  // process instead, and the poison-recovery path this guards is itself
  // unreachable. Skip rather than abort the run.
  #[cfg_attr(panic = "abort", ignore = "mutex poisoning needs unwinding")]
  fn poisoned_strip_progress_lock_degrades_instead_of_panicking() {
    use crate::init::download::DownloadProgress;
    use crate::tui::events::Event;
    use std::sync::Arc;
    let (tx, _rx) = mpsc::channel::<Event>(1);
    let strip = Arc::new(StripProgress {
      tx,
      repo_id: "owner/repo".into(),
      inner: std::sync::Mutex::new(StripProgressInner::default()),
    });
    // Poison the cell: a worker thread panics mid-update.
    let poisoner = Arc::clone(&strip);
    let _ = std::thread::spawn(move || {
      let _guard = poisoner.inner.lock().expect("first lock is clean");
      panic!("simulated download-thread panic");
    })
    .join();
    assert!(strip.inner.is_poisoned(), "lock should be poisoned now");
    // Every trait method must recover the inner value, not propagate the panic.
    strip.on_files_resolved(&[("f.gguf".to_string(), 10)]);
    strip.on_file_started("f.gguf", 10, 0, 1);
    strip.on_bytes_progress("f.gguf", 5);
    strip.on_file_finished("f.gguf", 0, 1);
  }
}
