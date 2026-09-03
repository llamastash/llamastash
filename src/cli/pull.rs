//! `llamastash pull <hf-repo>` — MVP for the HF pull primitive. Thin
//! shim into `init::download::run`, which owns the multi-shard
//! download body, plus the terminal progress line.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use crate::cli::cli_args::{Cli, PullArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;
use crate::init::download::{DownloadProgress, PullProgress, PullTotals};
use crate::tui::download_strip::{RateMeter, PROGRESS_REPAINT};
use crate::tui::fmt::{format_bytes, format_rate, percent_of, take_head_by_width, truncate_middle};

pub async fn handle(args: PullArgs, cli: &Cli, config: &Config) -> CliResult {
  crate::init::download::run(args, cli, config).await
}

/// Longest filename kept on the line before the middle is elided.
/// A narrow terminal shrinks it further; see [`render_line`].
const NAME_WIDTH: usize = 44;

/// Assumed terminal width when the size query fails (no controlling
/// tty, an ioctl the platform refuses).
const FALLBACK_COLS: usize = 80;

/// Leading marker, matching the TUI download strip.
const HEAD: &str = "⬇ ";

/// Budgeted width of [`HEAD`]. Two cells for the arrow, not the one
/// `unicode-width` reports: U+2B07 carries `Emoji_Presentation`, so a
/// terminal that follows the emoji spec paints it double-width. The
/// line has to fit under either reading.
const HEAD_WIDTH: usize = 3;

/// Single-line `pull` progress on stderr, mirroring the TUI download
/// strip: `⬇ <file> (2/4)  42%  1.2G / 4.1G · 85M/s`. Percent, bytes
/// and rate are pull-wide totals from the same [`PullTotals`] and
/// rate meter the strip uses, so the two surfaces report the same
/// numbers.
pub(crate) struct ProgressLine {
  /// Whether stderr is a terminal. False leaves the reporter inert:
  /// a redirected stream (or a captured test log) would collect one
  /// `\r`-overwritten line per chunk.
  tty: bool,
  state: Mutex<LineState>,
}

#[derive(Default)]
struct LineState {
  totals: PullTotals,
  file: String,
  index: usize,
  files: usize,
  bytes_done: u64,
  bytes_total: u64,
  rate: RateMeter,
  last_paint: Option<Instant>,
  /// Set once anything has been written, so `finish` only clears a
  /// line that actually exists.
  painted: bool,
}

impl ProgressLine {
  pub(crate) fn for_stderr() -> std::sync::Arc<Self> {
    std::sync::Arc::new(Self {
      tty: std::io::stderr().is_terminal(),
      state: Mutex::new(LineState::default()),
    })
  }

  /// Erase the in-place line so the caller's summary starts clean.
  pub(crate) fn finish(&self) {
    let mut s = self.lock();
    if !self.tty || !s.painted {
      return;
    }
    s.painted = false;
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
  }

  fn lock(&self) -> std::sync::MutexGuard<'_, LineState> {
    self.state.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// Repaint unless the last one was under [`PROGRESS_REPAINT`] ago. `force`
  /// skips the throttle for file boundaries, which are rare and worth
  /// showing immediately.
  fn paint(&self, force: bool) {
    if !self.tty {
      return;
    }
    let mut s = self.lock();
    let now = Instant::now();
    if !force
      && s
        .last_paint
        .is_some_and(|t| now.duration_since(t) < PROGRESS_REPAINT)
    {
      return;
    }
    s.last_paint = Some(now);
    s.painted = true;
    // Age the meter even when no chunk arrived, so a stalled pull
    // reads as slowing down rather than holding its last figure.
    s.rate.record(0, now);
    let line = render_line(&s, terminal_cols());
    drop(s);
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
  }

  /// Fold one aggregation step into the state. Only `transferred`
  /// reaches the rate meter — `bytes_done` also jumps when a cached
  /// file is credited its size, which is not throughput.
  fn advance(&self, p: PullProgress) {
    let mut s = self.lock();
    let now = Instant::now();
    s.rate.record(p.transferred, now);
    s.bytes_done = p.bytes_done;
    s.bytes_total = p.bytes_total;
  }
}

/// Terminal columns for the progress line. `crossterm` reads
/// `/dev/tty`, so a redirected stdout doesn't hide the real width.
fn terminal_cols() -> usize {
  crossterm::terminal::size()
    .map(|(cols, _)| cols as usize)
    .unwrap_or(FALLBACK_COLS)
}

/// Render the line for `cols` columns. Pure so the width arithmetic
/// is testable: the escape sequence around it clears one physical
/// row, so a line wider than the terminal wraps and leaves the
/// overflowed row on screen — once per repaint, ten times a second.
fn render_line(s: &LineState, cols: usize) -> String {
  let counter = if s.files > 1 {
    format!(" ({}/{})", s.index + 1, s.files)
  } else {
    String::new()
  };
  let tail = format!(
    "{}  {:>3}%  {} / {} · {}",
    counter,
    percent_of(s.bytes_done, s.bytes_total),
    format_bytes(s.bytes_done),
    format_bytes(s.bytes_total),
    format_rate(s.rate.bps()),
  );
  // Leave the last column free: writing into it parks the cursor on
  // the wrap boundary, which some terminals resolve by scrolling.
  let budget = cols.saturating_sub(1);
  if budget <= HEAD_WIDTH {
    return String::new();
  }
  let inner = budget - HEAD_WIDTH;
  let name_budget = inner.saturating_sub(tail.width()).min(NAME_WIDTH);
  let body = format!("{}{tail}", truncate_middle(&s.file, name_budget));
  // Backstop for a terminal too narrow even for the fixed part.
  if body.width() > inner {
    return format!("{HEAD}{}", take_head_by_width(&body, inner));
  }
  format!("{HEAD}{body}")
}

impl DownloadProgress for ProgressLine {
  fn on_files_resolved(&self, files: &[(String, u64)]) {
    let mut s = self.lock();
    s.bytes_total = s.totals.resolve_files(files);
    s.bytes_done = 0;
    s.files = files.len();
  }

  fn on_file_started(&self, filename: &str, _size: u64, index: usize, total: usize) {
    let mut s = self.lock();
    s.totals.start_file();
    s.file = filename.to_string();
    s.index = index;
    s.files = total;
    drop(s);
    self.paint(true);
  }

  fn on_file_finished(&self, filename: &str, _index: usize, _total: usize) {
    let step = self.lock().totals.finish_file(filename);
    self.advance(step);
    self.paint(true);
  }

  fn on_bytes_progress(&self, _filename: &str, bytes_in_file: u64) {
    let step = self.lock().totals.credit_bytes(bytes_in_file);
    self.advance(step);
    self.paint(false);
  }

  fn on_retry(&self, filename: &str, attempt: u32) {
    self.finish();
    if self.tty {
      eprintln!("retrying `{filename}` (attempt {attempt})");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn line() -> ProgressLine {
    ProgressLine {
      tty: false,
      state: Mutex::new(LineState::default()),
    }
  }

  #[test]
  fn callbacks_aggregate_across_files() {
    let p = line();
    p.on_files_resolved(&[("a.gguf".into(), 100), ("b.gguf".into(), 300)]);
    p.on_file_started("a.gguf", 100, 0, 2);
    p.on_bytes_progress("a.gguf", 40);
    assert_eq!(p.lock().bytes_done, 40);
    p.on_file_finished("a.gguf", 0, 2);
    assert_eq!(
      p.lock().bytes_done,
      100,
      "finish credits the file's full size"
    );
    p.on_file_started("b.gguf", 300, 1, 2);
    p.on_bytes_progress("b.gguf", 150);
    let s = p.lock();
    assert_eq!(s.bytes_done, 250);
    assert_eq!(s.bytes_total, 400);
  }

  #[test]
  fn a_retry_replaying_the_current_file_does_not_inflate_the_total() {
    // hf-hub re-`init`s the adapter on a retry and jumps it to the
    // committed offset, so the second pass reports byte counts the
    // first attempt already reported. Only the bytes that survived
    // may be counted.
    let p = line();
    p.on_files_resolved(&[("a.gguf".into(), 100), ("b.gguf".into(), 100)]);
    p.on_file_started("a.gguf", 100, 0, 2);
    p.on_bytes_progress("a.gguf", 60);
    p.on_bytes_progress("a.gguf", 40); // retry resumed at 40; 20 never landed
    assert_eq!(p.lock().bytes_done, 40);
    p.on_file_finished("a.gguf", 0, 2);
    p.on_file_started("b.gguf", 100, 1, 2);
    p.on_bytes_progress("b.gguf", 50);
    assert_eq!(
      p.lock().bytes_done,
      150,
      "no carry-over from the lost bytes"
    );
  }

  #[test]
  fn progress_is_clamped_to_the_resolved_total() {
    let p = line();
    p.on_files_resolved(&[("a.gguf".into(), 100)]);
    p.on_file_started("a.gguf", 100, 0, 1);
    p.on_bytes_progress("a.gguf", 500);
    assert_eq!(p.lock().bytes_done, 100);
  }

  #[test]
  fn crediting_a_finished_file_is_not_billed_as_throughput() {
    // A cached file is credited its whole size on completion with no
    // byte off the wire. Billing that jump to the meter had a re-pull
    // of an already-complete repo reporting tens of GB/s.
    let p = line();
    p.on_files_resolved(&[("a.gguf".into(), 1 << 30), ("b.gguf".into(), 1 << 30)]);
    for (idx, name) in ["a.gguf", "b.gguf"].iter().enumerate() {
      p.on_file_started(name, 1 << 30, idx, 2);
      p.on_file_finished(name, idx, 2);
    }
    let s = p.lock();
    assert_eq!(s.bytes_done, 2 << 30, "percent still reaches 100%");
    assert_eq!(s.rate.bps(), 0.0, "but no traffic was reported");
  }

  /// `LineState` mid-pull, for the width cases.
  fn state(file: &str, index: usize, files: usize) -> LineState {
    LineState {
      file: file.to_string(),
      index,
      files,
      bytes_done: 42_000_000_000,
      bytes_total: 103_000_000_000,
      ..LineState::default()
    }
  }

  #[test]
  fn the_line_never_outgrows_the_terminal() {
    // `\r\x1b[2K` clears one physical row, so a wrapped line leaves
    // its overflow on screen once per repaint.
    let long = "tinyllamas/split/stories15M-q8_0-00001-of-00003.gguf";
    for cols in [0usize, 1, 4, 5, 20, 40, 60, 80, 100, 200] {
      let line = render_line(&state(long, 9, 12), cols);
      // Worst case: a terminal that honours U+2B07's emoji
      // presentation and paints the arrow two cells wide.
      let rendered = if line.is_empty() {
        0
      } else {
        line.width() + (HEAD_WIDTH - HEAD.width())
      };
      assert!(
        rendered <= cols.saturating_sub(1),
        "{cols} cols: {rendered} wide — {line:?}"
      );
    }
  }

  #[test]
  fn a_shard_name_keeps_the_end_that_identifies_it() {
    let long = "tinyllamas/split/stories15M-q8_0-00002-of-00003.gguf";
    let line = render_line(&state(long, 1, 3), 100);
    assert!(
      line.contains("-00002-of-00003.gguf"),
      "the shard suffix is what tells two files apart: {line:?}"
    );
    assert!(line.contains("tinyllamas/"), "and the repo path: {line:?}");
  }

  #[test]
  fn a_single_file_pull_drops_the_counter() {
    let line = render_line(&state("model.gguf", 0, 1), 100);
    assert!(!line.contains('('), "{line:?}");
  }
}
