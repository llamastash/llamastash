//! Read-only smoke for `llamastash doctor` (Unit 3 stub).
//! Run binary as a subprocess and assert clap accepts the surface,
//! plus the stub emits a parseable JSON envelope under `--json`.

use std::process::Command;

fn bin() -> Command {
  Command::new(env!("CARGO_BIN_EXE_llamastash"))
}

#[test]
fn doctor_help_is_accepted() {
  let out = bin().args(["doctor", "--help"]).output().expect("run");
  assert!(out.status.success(), "doctor --help should exit 0");
  let stdout = String::from_utf8_lossy(&out.stdout);
  assert!(stdout.contains("--json"), "--help must mention --json");
}

#[test]
fn doctor_json_emits_findings_envelope() {
  let out = bin().args(["doctor", "--json"]).output().expect("run");
  assert!(out.status.success(), "stub must exit 0");
  let stdout = String::from_utf8_lossy(&out.stdout);
  let parsed: serde_json::Value =
    serde_json::from_str(&stdout).expect("--json output must parse as JSON");
  assert!(
    parsed.get("findings").is_some(),
    "envelope must carry findings"
  );
  assert!(parsed["findings"].is_array(), "findings must be an array");
}

#[test]
fn doctor_plain_run_succeeds_zero_findings_today() {
  let out = bin().arg("doctor").output().expect("run");
  assert!(
    out.status.success(),
    "doctor must exit 0 when no findings are present"
  );
}
