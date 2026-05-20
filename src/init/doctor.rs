//! `llamastash doctor` read-only diagnostic (R74 / R75).
//!
//! Re-runs hardware + binary detection, loads `_init_snapshot.json`,
//! compares the two, emits 0-N findings. Every finding carries a
//! stable `id` agent consumers can branch on plus a
//! `fix_hint = "llamastash init --only X"` that maps to the wizard
//! step that resolves it.
//!
//! Output is always safe to paste into a public issue — see the
//! Security Contract addendum's redaction rule in the v2 plan.
//! `safe_to_log` is unconditionally `true` for v2 findings; a future
//! finding that legitimately needs differentiated redaction lands
//! the per-finding flag *then*, not preemptively.

use std::path::Path;

use serde::Serialize;

use crate::cli::cli_args::{Cli, DoctorArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;
use crate::init::detection::{detect_hardware, HardwareSnapshot};
use crate::init::snapshot::{self, InitSnapshot, InstallMethod};
use crate::util::datetime::{current_yyyymmdd, days_between, parse_yyyymmdd};

/// Schema version for `doctor --json`. Bumped on breaking shape
/// changes; current readers refuse a snapshot whose `schema_version`
/// exceeds their max.
pub const DOCTOR_JSON_SCHEMA_VERSION: u32 = 1;

/// `SnapshotStale` finding fires when the bundled snapshot is older
/// than this many days vs today.
pub const STALE_SNAPSHOT_THRESHOLD_DAYS: u64 = 14;

/// `RemoteSnapshotUnreachable` finding fires after this many
/// consecutive remote-fetch failures.
pub const REMOTE_UNREACHABLE_THRESHOLD: u32 = 3;

/// Stable finding ids. Agent consumers branch on these — never change
/// a string here without bumping `DOCTOR_JSON_SCHEMA_VERSION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingId {
  BinaryMissing,
  BinaryDigestDrift,
  HardwareDrift,
  SnapshotStale,
  ConfigModeDrift,
  RemoteSnapshotUnreachable,
}

impl FindingId {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::BinaryMissing => "binary_missing",
      Self::BinaryDigestDrift => "binary_digest_drift",
      Self::HardwareDrift => "hardware_drift",
      Self::SnapshotStale => "snapshot_stale",
      Self::ConfigModeDrift => "config_mode_drift",
      Self::RemoteSnapshotUnreachable => "remote_snapshot_unreachable",
    }
  }

  pub fn fix_hint(self) -> &'static str {
    match self {
      Self::BinaryMissing | Self::BinaryDigestDrift | Self::HardwareDrift => {
        "llamastash init --only server"
      }
      Self::SnapshotStale | Self::RemoteSnapshotUnreachable => {
        "(no action — daily CI refresh will heal automatically; re-run `llamastash doctor` later)"
      }
      Self::ConfigModeDrift => "llamastash init --only config",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
  Info,
  Warning,
  Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
  pub id: &'static str,
  pub severity: Severity,
  pub message: String,
  pub fix_hint: &'static str,
  pub safe_to_log: bool,
}

impl Finding {
  fn new(id: FindingId, severity: Severity, message: impl Into<String>) -> Self {
    Self {
      id: id.as_str(),
      severity,
      message: message.into(),
      fix_hint: id.fix_hint(),
      safe_to_log: true,
    }
  }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Baseline {
  pub snapshot_bundle_date: Option<String>,
  pub init_date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
  pub schema_version: u32,
  pub findings: Vec<Finding>,
  pub baseline: Baseline,
}

/// Build the report. Pure-ish: reads the on-disk snapshot + re-detects
/// hardware/binary but never mutates anything.
pub fn build_report(snapshot: Option<&InitSnapshot>, hardware: &HardwareSnapshot) -> DoctorReport {
  let mut findings: Vec<Finding> = Vec::new();
  let baseline = Baseline {
    snapshot_bundle_date: snapshot.and_then(|s| s.snapshot_bundle_date.clone()),
    init_date: snapshot.and_then(|s| s.init_date.clone()),
  };
  let Some(snapshot) = snapshot else {
    return DoctorReport {
      schema_version: DOCTOR_JSON_SCHEMA_VERSION,
      findings,
      baseline,
    };
  };

  if let Some(finding) = check_binary_missing(snapshot) {
    findings.push(finding);
  }
  if let Some(finding) = check_binary_digest_drift(snapshot) {
    findings.push(finding);
  }
  if let Some(finding) = check_hardware_drift(snapshot, hardware) {
    findings.push(finding);
  }
  if let Some(finding) = check_snapshot_stale(snapshot) {
    findings.push(finding);
  }
  if let Some(finding) = check_config_mode_drift() {
    findings.push(finding);
  }
  if let Some(finding) = check_remote_snapshot_unreachable(snapshot) {
    findings.push(finding);
  }
  DoctorReport {
    schema_version: DOCTOR_JSON_SCHEMA_VERSION,
    findings,
    baseline,
  }
}

fn check_binary_missing(snapshot: &InitSnapshot) -> Option<Finding> {
  let path = snapshot.llama_server_path.as_ref()?;
  if path.is_file() && is_readable(path) {
    return None;
  }
  Some(Finding::new(
    FindingId::BinaryMissing,
    Severity::Error,
    format!(
      "`{}` is missing or unreadable — reinstall `llama-server`",
      path.display()
    ),
  ))
}

fn check_binary_digest_drift(snapshot: &InitSnapshot) -> Option<Finding> {
  // Brew carve-out: digest drift after `brew upgrade` is normal; we
  // don't surface it.
  let install_method = snapshot.install_method?;
  if install_method != InstallMethod::GhReleases {
    return None;
  }
  let path = snapshot.llama_server_path.as_ref()?;
  let expected = snapshot.llama_server_digest.as_ref()?;
  let actual = match crate::init::install::sha256_file(path) {
    Ok(d) => d,
    Err(_) => return None, // BinaryMissing already covers this path
  };
  if &actual == expected {
    return None;
  }
  Some(Finding::new(
    FindingId::BinaryDigestDrift,
    Severity::Warning,
    format!(
      "SHA-256 of `{}` ({}) differs from the recorded digest ({}); \
       binary may have been replaced or corrupted",
      path.display(),
      short_hex(&actual),
      short_hex(expected),
    ),
  ))
}

fn check_hardware_drift(snapshot: &InitSnapshot, hardware: &HardwareSnapshot) -> Option<Finding> {
  let prior_vendor = snapshot.gpu_vendor.as_deref()?;
  if prior_vendor == hardware.gpu.label() {
    return None;
  }
  Some(Finding::new(
    FindingId::HardwareDrift,
    Severity::Warning,
    format!(
      "GPU vendor changed from `{prior_vendor}` to `{}` since init — \
       reinstall to pick the right `llama-server` variant",
      hardware.gpu.label()
    ),
  ))
}

fn check_snapshot_stale(snapshot: &InitSnapshot) -> Option<Finding> {
  let bundle_date = snapshot.snapshot_bundle_date.as_deref()?;
  let now = current_yyyymmdd()?;
  let bundled = parse_yyyymmdd(bundle_date)?;
  let then = parse_yyyymmdd(&now)?;
  let delta_days = days_between(bundled, then)?;
  if delta_days <= STALE_SNAPSHOT_THRESHOLD_DAYS {
    return None;
  }
  Some(Finding::new(
    FindingId::SnapshotStale,
    Severity::Info,
    format!(
      "benchmark snapshot was bundled {delta_days} days ago — \
       the daily CI refresh has not landed; recommender picks may be stale"
    ),
  ))
}

fn check_config_mode_drift() -> Option<Finding> {
  let path = crate::util::paths::user_config_file()?;
  if !path.exists() {
    return None;
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let file_meta = std::fs::metadata(&path).ok()?;
    let file_mode = file_meta.permissions().mode() & 0o777;
    if file_mode != 0o600 {
      return Some(Finding::new(
        FindingId::ConfigModeDrift,
        Severity::Warning,
        format!(
          "`{}` is mode {file_mode:#o} (expected 0600) — \
           re-run init or `chmod 600` to restore the hardening",
          path.display()
        ),
      ));
    }
    if let Some(parent) = path.parent() {
      if let Ok(pmeta) = std::fs::metadata(parent) {
        let pmode = pmeta.permissions().mode() & 0o777;
        if pmode & 0o022 != 0 {
          return Some(Finding::new(
            FindingId::ConfigModeDrift,
            Severity::Warning,
            format!(
              "parent dir `{}` is group/world-writable (mode {pmode:#o}) — \
               `chmod 700` recommended",
              parent.display()
            ),
          ));
        }
      }
    }
  }
  let _ = path;
  None
}

fn check_remote_snapshot_unreachable(snapshot: &InitSnapshot) -> Option<Finding> {
  if snapshot.remote_fetch_failures < REMOTE_UNREACHABLE_THRESHOLD {
    return None;
  }
  Some(Finding::new(
    FindingId::RemoteSnapshotUnreachable,
    Severity::Info,
    format!(
      "remote benchmark snapshot has been unreachable for \
       {} consecutive verified-fetch attempts; bundled fallback in use",
      snapshot.remote_fetch_failures
    ),
  ))
}

fn is_readable(path: &Path) -> bool {
  std::fs::File::open(path).is_ok()
}

fn short_hex(digest: &str) -> String {
  if digest.len() <= 12 {
    digest.to_string()
  } else {
    format!("{}…", &digest[..12])
  }
}

/// CLI handler entry-point. Always exits 0 — findings are informative,
/// not a failure signal. (Agents can branch on a non-empty `findings`
/// array to escalate.)
pub async fn run(args: DoctorArgs, _cli: &Cli, _config: &Config) -> CliResult {
  let hardware = detect_hardware();
  // Distinguish three snapshot states:
  //   * `Some(snap)` — read cleanly; full diff against baseline.
  //   * `None` after a parse-fail Err — file existed but was corrupt
  //     or unreadable. `snapshot::load` already quarantined it to
  //     `.broken-<ts>`; we proceed without a baseline but log so the
  //     user sees what happened.
  //   * `None` after Ok(None) — first run, no snapshot yet. Silent.
  let snapshot = match crate::util::paths::state_dir() {
    Some(dir) => match snapshot::load(&dir) {
      Ok(snap) => snap,
      Err(e) => {
        log::warn!("doctor: failed to read init_snapshot.json (quarantined to .broken-<ts>): {e}");
        None
      }
    },
    None => None,
  };
  let report = build_report(snapshot.as_ref(), &hardware);
  if args.json {
    println!(
      "{}",
      serde_json::to_string_pretty(&report).unwrap_or_default()
    );
  } else {
    render_human(&report);
  }
  Ok(())
}

fn render_human(report: &DoctorReport) {
  use crate::cli::colors;
  if report.findings.is_empty() {
    println!(
      "{}",
      colors::success("llamastash doctor: 0 findings — everything looks healthy.")
    );
    if let Some(date) = &report.baseline.init_date {
      println!("  {}", colors::dim(&format!("last init: {date}")));
    }
    return;
  }
  println!(
    "{}",
    colors::header(&format!(
      "llamastash doctor: {} finding(s).",
      report.findings.len()
    ))
  );
  for f in &report.findings {
    let line = format!("[{}] {}", f.id, f.message);
    let coloured = match f.severity {
      Severity::Error => colors::error(&line),
      Severity::Warning => colors::warning(&line),
      // Info findings need a leading glyph too so agents that classify
      // severity by the first character of each line don't lose them.
      Severity::Info => format!("{} {}", colors::dim("•"), colors::dim(&line)),
    };
    println!("\n  {coloured}");
    println!(
      "    {}",
      colors::dim(&format!("→ fix with: {}", f.fix_hint))
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::GpuInfo;
  use crate::init::detection::{CpuArch, OsFamily};

  fn cpu_hw() -> HardwareSnapshot {
    HardwareSnapshot {
      gpu: GpuInfo::CpuOnly,
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: 16 * 1024 * 1024 * 1024,
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  #[test]
  fn report_with_no_snapshot_emits_no_findings() {
    let report = build_report(None, &cpu_hw());
    assert!(report.findings.is_empty());
    assert!(report.baseline.snapshot_bundle_date.is_none());
  }

  #[test]
  fn binary_missing_finding_fires_for_nonexistent_path() {
    let snap = InitSnapshot {
      llama_server_path: Some("/nonexistent/llama-server".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(report.findings.iter().any(|f| f.id == "binary_missing"));
  }

  #[test]
  fn brew_digest_drift_is_carved_out() {
    // brew-installed binary with a missing/changed digest should
    // NOT produce a binary_digest_drift finding (only a possible
    // BinaryMissing finding if the path doesn't exist).
    let snap = InitSnapshot {
      install_method: Some(InstallMethod::Brew),
      llama_server_path: Some("/nonexistent/llama-server".into()),
      llama_server_digest: Some("a".repeat(64)),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(!report
      .findings
      .iter()
      .any(|f| f.id == "binary_digest_drift"));
  }

  #[test]
  fn hardware_drift_finding_fires_when_vendor_changes() {
    let snap = InitSnapshot {
      gpu_vendor: Some("nvidia".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    let drift = report.findings.iter().find(|f| f.id == "hardware_drift");
    assert!(
      drift.is_some(),
      "hardware_drift should fire when vendor changed"
    );
    assert_eq!(drift.unwrap().fix_hint, "llamastash init --only server");
  }

  #[test]
  fn snapshot_stale_finding_fires_after_threshold_days() {
    let snap = InitSnapshot {
      snapshot_bundle_date: Some("2000-01-01".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    let stale = report.findings.iter().find(|f| f.id == "snapshot_stale");
    assert!(
      stale.is_some(),
      "stale snapshot should fire for an ancient bundle_date"
    );
  }

  #[test]
  fn snapshot_stale_does_not_fire_for_fresh_bundle() {
    let today = current_yyyymmdd().expect("clock");
    let snap = InitSnapshot {
      snapshot_bundle_date: Some(today),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(!report.findings.iter().any(|f| f.id == "snapshot_stale"));
  }

  #[test]
  fn remote_unreachable_finding_fires_after_threshold() {
    let snap = InitSnapshot {
      remote_fetch_failures: REMOTE_UNREACHABLE_THRESHOLD,
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(report
      .findings
      .iter()
      .any(|f| f.id == "remote_snapshot_unreachable"));
  }

  #[test]
  fn remote_unreachable_does_not_fire_below_threshold() {
    let snap = InitSnapshot {
      remote_fetch_failures: REMOTE_UNREACHABLE_THRESHOLD - 1,
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(!report
      .findings
      .iter()
      .any(|f| f.id == "remote_snapshot_unreachable"));
  }

  #[test]
  fn every_finding_id_has_a_fix_hint_and_safe_to_log_true() {
    let ids = [
      FindingId::BinaryMissing,
      FindingId::BinaryDigestDrift,
      FindingId::HardwareDrift,
      FindingId::SnapshotStale,
      FindingId::ConfigModeDrift,
      FindingId::RemoteSnapshotUnreachable,
    ];
    for id in ids {
      assert!(!id.fix_hint().is_empty(), "{id:?} must have a fix_hint");
      let f = Finding::new(id, Severity::Info, "test");
      assert!(f.safe_to_log, "v2 findings must all be safe_to_log");
    }
  }

  #[test]
  fn days_between_arithmetic_matches_civil_calendar() {
    let a = (2024, 1, 1);
    let b = (2024, 1, 31);
    assert_eq!(days_between(a, b), Some(30));
    let c = (2025, 1, 1);
    assert_eq!(days_between(a, c), Some(366)); // 2024 is leap
  }

  #[test]
  fn parse_yyyymmdd_rejects_bad_shapes() {
    assert!(parse_yyyymmdd("2024/01/01").is_none());
    assert!(parse_yyyymmdd("2024-13-01").is_none());
    assert!(parse_yyyymmdd("2024-01-32").is_none());
  }

  #[test]
  fn render_human_handles_empty_report() {
    // Smoke test: no panic on rendering, the function returns ().
    let report = build_report(None, &cpu_hw());
    render_human(&report);
  }
}
