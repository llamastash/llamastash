//! Maintainer-only `llamastash uat` subcommand (Units 3 & 4).
//!
//! Only compiled when the `uat` Cargo feature is enabled; the release
//! binary never carries this entry point. Unit 3 lands the scaffold;
//! Unit 4 hangs the 5-step lifecycle + isolation guard + structured
//! JSON report off `handle()`.

pub mod isolation;
pub mod lifecycle;
pub mod model;
pub mod report;

use std::io::Write as _;
use std::time::Duration;

use crate::cli::cli_args::{Cli, UatArgs};
use crate::cli::exit_codes::{CliExit, CliResult, UNKNOWN};
use crate::config::Config;

use isolation::TempdirGuard;
use lifecycle::{LifecyclePlan, SIGINT_CODE};
use report::{
  backend_label, render_tty_summary, FailureClass, FailureSummary, StepName, StepVerdict,
  UatReport, Verdict,
};

/// Entry point invoked by `cli::dispatch`. Drives the lifecycle,
/// renders the JSON + TTY report, and chooses an exit code per the
/// 0/1 contract.
pub async fn handle(args: UatArgs, cli: &Cli, _config: &Config) -> CliResult {
  // Mutual exclusion: `--report-out -` writes JSON to stdout, so
  // global `--quiet` would suppress the only output the run produces.
  if args.report_out.as_deref() == Some(std::path::Path::new("-")) && cli.quiet {
    return Err(CliExit::new(
      UNKNOWN,
      "uat: `--report-out -` (stdout JSON) is mutually exclusive with `--quiet`".to_string(),
    ));
  }
  validate_report_out_path(args.report_out.as_deref())?;

  let backend = args.backend;
  let mode = args.mode;
  let label = format!(
    "{}-{}",
    backend_label(backend),
    match mode {
      crate::cli::cli_args::UatMode::Warm => "warm",
      crate::cli::cli_args::UatMode::Cold => "cold",
    }
  );

  let guard = TempdirGuard::new(&label)
    .map_err(|e| CliExit::new(UNKNOWN, format!("uat: tempdir setup failed: {e}")))?;
  let plan = LifecyclePlan::from_args(backend, mode)
    .map_err(|e| CliExit::new(UNKNOWN, format!("uat: plan setup failed: {e}")))?;

  let started_at = rfc3339_now();
  let llamastash_version = env!("CARGO_PKG_VERSION").to_string();
  let mut report = UatReport::skeleton(backend, started_at, llamastash_version);

  // Race the lifecycle against SIGINT so a Ctrl-C produces a partial
  // report instead of silent termination. The lifecycle future is
  // wrapped in `catch_unwind` (via futures::FutureExt) so an
  // unexpected panic returns a structured `verdict=fail` instead of
  // bypassing Drop and producing no report at all. `AssertUnwindSafe`
  // is required because `&mut UatReport` is not `UnwindSafe` —
  // safe here because, on panic, the caller treats the report as a
  // best-effort snapshot of state up to the panic point, never
  // re-uses it as input to another lifecycle.
  use futures::FutureExt;
  use std::panic::AssertUnwindSafe;
  let lifecycle_outcome = tokio::select! {
    result = AssertUnwindSafe(lifecycle::run(&plan, &guard, &mut report)).catch_unwind() => match result {
      Ok(()) => Outcome::Completed,
      Err(panic) => Outcome::Panicked(panic_message(&panic)),
    },
    _ = tokio::signal::ctrl_c() => Outcome::Interrupted,
  };

  match &lifecycle_outcome {
    Outcome::Completed => {}
    Outcome::Panicked(message) => {
      report.verdict = Verdict::Fail;
      mark_first_skipped_as_interrupted(&mut report);
      if report.failure_summary.is_none() {
        report.failure_summary = Some(FailureSummary {
          step: in_flight_step(&report).unwrap_or(StepName::DoctorPreflight),
          classification: FailureClass::Other,
          exit_code: 1,
          message: format!("orchestrator panicked: {message}"),
        });
      }
    }
    Outcome::Interrupted => {
      report.verdict = Verdict::Interrupted;
      mark_first_skipped_as_interrupted(&mut report);
      if report.failure_summary.is_none() {
        report.failure_summary = Some(FailureSummary {
          step: in_flight_step(&report).unwrap_or(StepName::DoctorPreflight),
          classification: FailureClass::Interrupted,
          exit_code: SIGINT_CODE,
          message: "interrupted by SIGINT".to_string(),
        });
      }
    }
  }

  let pass = matches!(report.verdict, Verdict::Pass);
  if pass {
    guard.release_on_success();
  } else {
    // Surface the preserved tempdir path as a warning so it lands in
    // both the JSON report and the TTY scan-down.
    report
      .host
      .warnings
      .push(format!("preserved tempdir at {}", guard.root().display()));
  }

  emit_report(&report, &args, cli.quiet)?;

  if pass {
    Ok(())
  } else {
    Err(CliExit::new(
      UNKNOWN,
      format!(
        "uat: {} (see failure_summary in report)",
        report
          .failure_summary
          .as_ref()
          .map(|f| f.message.as_str())
          .unwrap_or("failed without a populated failure_summary")
      ),
    ))
  }
}

enum Outcome {
  Completed,
  Interrupted,
  Panicked(String),
}

/// First step still marked Skipped at the time SIGINT or a panic was
/// observed — the most useful single-step attribution for the
/// failure summary when the lifecycle never recorded its own failing
/// step.
fn in_flight_step(report: &UatReport) -> Option<StepName> {
  report
    .steps
    .iter()
    .find(|s| s.verdict == StepVerdict::Skipped)
    .map(|s| s.name)
}

/// Mark the first step still `Skipped` as `Interrupted` so the TTY
/// summary visually attributes "where things stopped" to a real row.
fn mark_first_skipped_as_interrupted(report: &mut UatReport) {
  if let Some(s) = report
    .steps
    .iter_mut()
    .find(|s| s.verdict == StepVerdict::Skipped)
  {
    s.interrupt(Duration::ZERO);
  }
}

/// Best-effort extract of a panic payload's `&str` / `String` for the
/// failure-summary message. `Box<dyn Any>` is the shape catch_unwind
/// returns; the two common payload types cover `panic!("…")` and
/// `assert!(…)`.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
  if let Some(s) = payload.downcast_ref::<&'static str>() {
    return (*s).to_string();
  }
  if let Some(s) = payload.downcast_ref::<String>() {
    return s.clone();
  }
  "<non-string panic payload>".to_string()
}

/// Refuse `--report-out` values that would silently produce confusing
/// errors: an empty string (writes `.json.tmp` in CWD), a directory
/// path (rename fails EACCES later), or whitespace-only.
fn validate_report_out_path(path: Option<&std::path::Path>) -> CliResult {
  let Some(p) = path else {
    return Ok(());
  };
  if p == std::path::Path::new("-") {
    return Ok(());
  }
  let raw = p.as_os_str();
  if raw.is_empty() {
    return Err(CliExit::new(
      UNKNOWN,
      "uat: `--report-out` requires a non-empty path".to_string(),
    ));
  }
  if let Some(s) = raw.to_str() {
    if s.trim().is_empty() {
      return Err(CliExit::new(
        UNKNOWN,
        "uat: `--report-out` must not be whitespace-only".to_string(),
      ));
    }
  }
  if p.is_dir() {
    return Err(CliExit::new(
      UNKNOWN,
      format!(
        "uat: `--report-out {}` is a directory — pass a file path",
        p.display()
      ),
    ));
  }
  Ok(())
}

fn emit_report(report: &UatReport, args: &UatArgs, quiet: bool) -> CliResult {
  // JSON destination: file path, stdout via "-", or nowhere when
  // omitted. The TTY summary lands on stdout when the JSON is in a
  // file (or omitted) so humans see the scan-down. On the `-` arm
  // the JSON owns stdout — the TTY summary moves to stderr so a
  // `| jq` consumer gets pure JSON.
  match args.report_out.as_deref() {
    Some(p) if p == std::path::Path::new("-") => {
      let json = serde_json::to_string_pretty(report)
        .map_err(|e| CliExit::new(UNKNOWN, format!("uat: report serialization failed: {e}")))?;
      println!("{json}");
      if !quiet {
        let tty = render_tty_summary(report);
        let _ = std::io::stderr().write_all(tty.as_bytes());
      }
    }
    Some(p) => {
      let json = serde_json::to_vec_pretty(report)
        .map_err(|e| CliExit::new(UNKNOWN, format!("uat: report serialization failed: {e}")))?;
      // Atomic-ish write: write to a sibling tmp file under the
      // parent dir, then rename onto the final path. Constructing
      // the tmp name with `with_extension` blows up on directory
      // paths (`/tmp/` → `/tmp.json.tmp`); we precomputed
      // `validate_report_out_path` rejected those, but the tmp
      // suffix is still a stable pid-discriminated sibling so two
      // concurrent UAT runs targeting the same final path don't
      // clobber each other's tmp file.
      let tmp = atomic_tmp_path_for(p);
      std::fs::write(&tmp, &json)
        .map_err(|e| CliExit::new(UNKNOWN, format!("uat: write {}: {e}", tmp.display())))?;
      std::fs::rename(&tmp, p)
        .map_err(|e| CliExit::new(UNKNOWN, format!("uat: rename {}: {e}", p.display())))?;
      if !quiet {
        let tty = render_tty_summary(report);
        let _ = std::io::stdout().write_all(tty.as_bytes());
      }
    }
    None => {
      if !quiet {
        let tty = render_tty_summary(report);
        let _ = std::io::stdout().write_all(tty.as_bytes());
      }
    }
  }
  Ok(())
}

/// Build a pid-stable sibling tmp path for atomic file writes. The
/// pid suffix gives concurrent UAT runs writing to the same final
/// path a unique scratch file each (the rename is atomic per the
/// POSIX contract, so only the last writer's final report survives —
/// but neither sees the other's truncated state mid-write).
fn atomic_tmp_path_for(final_path: &std::path::Path) -> std::path::PathBuf {
  let parent = final_path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .map(std::path::Path::to_path_buf)
    .unwrap_or_else(|| std::path::PathBuf::from("."));
  let stem = final_path
    .file_name()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| "report".to_string());
  parent.join(format!(".{}.tmp.{}", stem, std::process::id()))
}

/// Produce an RFC-3339 / ISO-8601 UTC timestamp without pulling in a
/// chrono / time dependency. Format: `2026-05-20T09:00:00Z`.
fn rfc3339_now() -> String {
  use std::time::{SystemTime, UNIX_EPOCH};
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs();
  format_unix_seconds_as_rfc3339(now)
}

fn format_unix_seconds_as_rfc3339(secs: u64) -> String {
  // Date arithmetic without a crate. Civil-from-days algorithm
  // adapted from Howard Hinnant's "date algorithms" public-domain
  // reference (`https://howardhinnant.github.io/date_algorithms.html`,
  // §"days_from_civil"). The UAT report's only timestamp consumer is
  // an offline JSON parser, not a wall-clock-precise scheduler, so
  // dragging in `chrono` / `time` to format one string per run is
  // not worth the dep weight. Inlining this routine keeps the
  // release binary lean and lets the report layer stay free of
  // crate-level time machinery.
  const SECS_PER_DAY: u64 = 86_400;
  let days = secs / SECS_PER_DAY;
  let secs_of_day = secs % SECS_PER_DAY;
  let hour = secs_of_day / 3_600;
  let minute = (secs_of_day % 3_600) / 60;
  let second = secs_of_day % 60;

  let days_since_epoch: i64 = days as i64;
  let z = days_since_epoch + 719_468;
  let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
  let doe = (z - era * 146_097) as u64;
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
  let y = yoe as i64 + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
  let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
  let year = y + if m <= 2 { 1 } else { 0 };
  format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rfc3339_format_handles_unix_epoch() {
    assert_eq!(format_unix_seconds_as_rfc3339(0), "1970-01-01T00:00:00Z");
  }

  #[test]
  fn rfc3339_format_handles_y2k() {
    // 2000-01-01 00:00:00 UTC = 946_684_800
    assert_eq!(
      format_unix_seconds_as_rfc3339(946_684_800),
      "2000-01-01T00:00:00Z"
    );
  }

  #[test]
  fn rfc3339_format_handles_a_recent_timestamp() {
    // 2026-05-20 00:00:00 UTC.
    // 2024-01-01 = 1_704_067_200; +(366+365) days = 2026-01-01 = 1_767_225_600;
    // +31+28+31+30+19 days = 2026-05-20 = 1_779_235_200.
    assert_eq!(
      format_unix_seconds_as_rfc3339(1_779_235_200),
      "2026-05-20T00:00:00Z"
    );
  }

  #[test]
  fn rfc3339_now_produces_a_shape_we_can_parse() {
    // Smoke check the live function — content varies, shape is fixed.
    let s = rfc3339_now();
    // YYYY-MM-DDTHH:MM:SSZ is exactly 20 chars.
    assert_eq!(s.len(), 20, "{s}");
    assert!(s.ends_with('Z'));
    assert_eq!(s.chars().nth(10), Some('T'));
  }
}
