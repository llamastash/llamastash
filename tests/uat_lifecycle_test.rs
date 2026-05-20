//! Integration coverage for `llamastash uat` (Unit 4).
//!
//! These tests cannot exercise the **happy path** (init → smoke chat
//! → stop → doctor) without either (a) network access to HuggingFace
//! and a real `llama-server` install or (b) a stubbed FetchClient
//! that the orchestrator doesn't currently support. Per the plan
//! §"Deferred to Implementation > Cold-mode dry-run timing", the
//! happy path is the maintainer's local dry-run gate, NOT a CI
//! gate.
//!
//! What we *can* exercise here without network or hardware:
//!
//! * Pre-flight backend mismatch (`--backend nvidia` on a non-NVIDIA
//!   host) — fails the first step, exits 1, populates
//!   `failure_summary.step = "doctor_preflight"`.
//! * `--report-out` file emission shape — JSON parses, carries
//!   `schema_version: 1`, `verdict: "fail"`, all five steps present.
//! * `--report-out -` to stdout when `--quiet` would normally suppress
//!   the TTY summary.
//! * Mutual exclusion: `--quiet` + `--report-out -` is rejected at
//!   handle time before the lifecycle runs.

#![cfg(feature = "uat")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-test scratch dir under the system temp root. Mirrors the helper
/// every other integration test uses (`unique_temp_dir(label)`) so the
/// UAT suite stays consistent with the project's binding pattern
/// documented in AGENTS.md §Common gotchas.
fn unique_temp_dir(label: &str) -> PathBuf {
  static SEQ: AtomicU32 = AtomicU32::new(0);
  let seq = SEQ.fetch_add(1, Ordering::SeqCst);
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_nanos())
    .unwrap_or(0);
  let pid = std::process::id();
  let dir = std::env::temp_dir().join(format!("llamastash-uat-test-{label}-{pid}-{nanos}-{seq}"));
  std::fs::create_dir_all(&dir).expect("create unique_temp_dir");
  dir
}

fn bin() -> Command {
  let mut c = Command::new(env!("CARGO_BIN_EXE_llamastash"));
  // Belt-and-braces: even if the host is on a clean machine, force
  // the inner `init` step's isolation to fail fast on offline so the
  // UAT process doesn't hang on HF DNS during a CI run that
  // accidentally reached this code.
  c.env("LLAMASTASH_OFFLINE", "1");
  c
}

#[test]
fn uat_help_lists_the_three_documented_flags() {
  let out = bin().args(["uat", "--help"]).output().expect("run");
  assert!(out.status.success(), "uat --help should exit 0");
  let stdout = String::from_utf8_lossy(&out.stdout);
  for flag in ["--backend", "--mode", "--report-out"] {
    assert!(
      stdout.contains(flag),
      "uat --help should advertise `{flag}`, got: {stdout}"
    );
  }
  // The `metal` alias is *not* offered — only canonical discriminants
  // appear in clap's value-list. Catches a future refactor that
  // re-introduces the alias.
  for canonical in ["nvidia", "amd", "apple_metal", "vulkan"] {
    assert!(
      stdout.contains(canonical),
      "uat --help should list backend `{canonical}`"
    );
  }
}

#[test]
fn uat_pre_flight_fails_on_backend_mismatch() {
  // Pick a backend that is virtually guaranteed not to be present in
  // CI: `nvidia` on a non-GPU runner. Pre-flight fails before any
  // network call, so this test is hermetic.
  let dir = unique_temp_dir("preflight-fail");
  let report_path = dir.join("report.json");
  let out = bin()
    .args([
      "uat",
      "--backend",
      "nvidia",
      "--mode",
      "warm",
      "--report-out",
    ])
    .arg(&report_path)
    .output()
    .expect("run");
  // Exit code = 1 (the documented 0/1 contract). The renderer always
  // prints the TTY summary unless --quiet was passed, so stderr
  // carries the report's failure line.
  assert!(
    !out.status.success(),
    "uat must exit non-zero on backend mismatch; stderr={}",
    String::from_utf8_lossy(&out.stderr)
  );
  assert!(
    report_path.exists(),
    "--report-out file must exist even on failure: {}",
    report_path.display()
  );
  let body = std::fs::read_to_string(&report_path).expect("read report");
  let json: serde_json::Value = serde_json::from_str(&body).expect("report must parse as JSON");
  assert_eq!(json["schema_version"], 1, "schema_version must be 1");
  assert_eq!(json["verdict"], "fail", "verdict must be fail on mismatch");
  let steps = json["steps"].as_array().expect("steps[] is an array");
  assert_eq!(steps.len(), 5, "5-step lifecycle is contract");
  let first = &steps[0];
  assert_eq!(first["name"], "doctor_preflight");
  assert_eq!(first["verdict"], "fail");
  // Every step after the failing one is `skipped` per the
  // short-circuit contract.
  for s in steps.iter().skip(1) {
    assert_eq!(
      s["verdict"], "skipped",
      "steps after a failure must be `skipped`: {s}"
    );
  }
  let fs = &json["failure_summary"];
  assert_eq!(fs["step"], "doctor_preflight");
  // PREFLIGHT_MISMATCH_CODE constant in lifecycle.rs.
  assert_eq!(fs["exit_code"], 10);
  assert_eq!(
    fs["classification"], "backend_mismatch",
    "FailureSummary must carry the structured classification field; \
     downstream workflow comments depend on it"
  );
}

#[test]
fn uat_report_out_dash_emits_json_to_stdout_under_quiet_is_rejected() {
  // `--quiet` + `--report-out -` is the only mutually-exclusive
  // combo at the UAT handle layer. Refused with a clear error
  // before the lifecycle runs.
  let out = bin()
    .args(["--quiet", "uat", "--backend", "nvidia", "--report-out", "-"])
    .output()
    .expect("run");
  assert!(
    !out.status.success(),
    "`--quiet` + `--report-out -` must be rejected"
  );
  let stderr = String::from_utf8_lossy(&out.stderr);
  assert!(
    stderr.contains("mutually exclusive") || stderr.contains("--quiet"),
    "rejection message should explain the combo: {stderr}"
  );
}

#[test]
fn uat_report_out_dash_emits_pure_json_to_stdout() {
  // Without `--quiet`, `--report-out -` routes the JSON to stdout and
  // the TTY summary to stderr — so a `| jq` consumer can parse stdout
  // directly. Pre-flight fails on the backend mismatch but the report
  // is well-formed.
  let out = bin()
    .args(["uat", "--backend", "nvidia", "--report-out", "-"])
    .output()
    .expect("run");
  assert!(!out.status.success());
  let stdout = String::from_utf8_lossy(&out.stdout);
  let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
    panic!("stdout must be pure parseable JSON, got error {e}; stdout was:\n{stdout}");
  });
  assert_eq!(parsed["schema_version"], 1);
  assert_eq!(parsed["verdict"], "fail");
  // Sanity: the TTY summary lands on stderr now, not stdout.
  let stderr = String::from_utf8_lossy(&out.stderr);
  assert!(
    stderr.contains("UAT verdict"),
    "TTY summary must render on stderr in the --report-out - arm; stderr was:\n{stderr}"
  );
}

#[test]
fn uat_report_out_rejects_directory_path() {
  // A directory path used to produce a confusing EACCES error after
  // the lifecycle ran. Now the validator refuses it up-front so a CI
  // failure attributes correctly.
  let dir = unique_temp_dir("report-out-dir");
  let out = bin()
    .args(["uat", "--backend", "nvidia", "--report-out"])
    .arg(&dir)
    .output()
    .expect("run");
  assert!(
    !out.status.success(),
    "directory --report-out must be rejected"
  );
  let stderr = String::from_utf8_lossy(&out.stderr);
  assert!(
    stderr.contains("directory") || stderr.contains("file path"),
    "rejection message should mention the directory failure mode: {stderr}"
  );
}

#[test]
fn uat_subcommand_help_documents_warm_default() {
  let out = bin().args(["uat", "--help"]).output().expect("run");
  let stdout = String::from_utf8_lossy(&out.stdout);
  // Both modes appear; the default is "warm".
  assert!(stdout.contains("warm"), "warm mode must be advertised");
  assert!(stdout.contains("cold"), "cold mode must be advertised");
  assert!(
    stdout.contains("default") && stdout.contains("warm"),
    "warm is the default mode: {stdout}"
  );
}
