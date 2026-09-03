//! `llamastash pull <hf-repo>` — MVP for the HF pull primitive. Thin
//! shim into `init::download::run`, which owns the multi-shard
//! download body, plus the terminal progress line.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::cli::cli_args::{Cli, PullArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;
use crate::init::download::{DownloadProgress, PullTotals};
use crate::tui::download_strip::ema_bps;
use crate::tui::fmt::{format_bytes, format_rate, percent_of, truncate_end};

pub async fn handle(args: PullArgs, cli: &Cli, config: &Config) -> CliResult {
  crate::init::download::run(args, cli, config).await
}

/// Repaint interval. Fast enough to read as live, slow enough that a
/// multi-gigabyte pull isn't spending its time writing escape codes.
const REPAINT: Duration = Duration::from_millis(100);

/// Longest filename kept on the line before the middle is elided.
const NAME_WIDTH: usize = 44;

/// Single-line `pull` progress on stderr, mirroring the TUI download
/// strip: `⬇ <file> (2/4)  42%  1.2G / 4.1G · 85M/s`. Percent, bytes
/// and rate are pull-wide totals from the same [`PullTotals`] and EMA
/// the strip uses, so the two surfaces report the same numbers.
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
  throughput_bps: f64,
  last_sample: Option<Instant>,
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

  /// Repaint unless the last one was under [`REPAINT`] ago. `force`
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
        .is_some_and(|t| now.duration_since(t) < REPAINT)
    {
      return;
    }
    s.last_paint = Some(now);
    s.painted = true;
    let counter = if s.files > 1 {
      format!(" ({}/{})", s.index + 1, s.files)
    } else {
      String::new()
    };
    let line = format!(
      "⬇ {}{}  {:>3}%  {} / {} · {}",
      truncate_end(&s.file, NAME_WIDTH),
      counter,
      percent_of(s.bytes_done, s.bytes_total),
      format_bytes(s.bytes_done),
      format_bytes(s.bytes_total),
      format_rate(s.throughput_bps),
    );
    drop(s);
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
  }

  /// Fold a new pull-wide byte count into the state, smoothing the
  /// rate over the interval since the previous sample.
  fn advance(&self, done: u64, total: u64) {
    let mut s = self.lock();
    let now = Instant::now();
    let elapsed = s
      .last_sample
      .map_or(Duration::ZERO, |t| now.duration_since(t));
    let delta = done.saturating_sub(s.bytes_done);
    if s.last_sample.is_some() {
      s.throughput_bps = ema_bps(s.throughput_bps, delta, elapsed);
    }
    s.last_sample = Some(now);
    s.bytes_done = done;
    s.bytes_total = total;
  }
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
    let (done, total) = self.lock().totals.finish_file(filename);
    self.advance(done, total);
    self.paint(true);
  }

  fn on_bytes_progress(&self, _filename: &str, bytes_in_file: u64) {
    let (done, total) = self.lock().totals.credit_bytes(bytes_in_file);
    self.advance(done, total);
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
}
