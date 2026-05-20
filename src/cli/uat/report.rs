//! Structured UAT report (Unit 4).
//!
//! Schema is additive-only within v1; consumers ignore unknown fields.
//! Bumping `SCHEMA_VERSION` signals a breaking shape change — pair with
//! a CHANGELOG entry under the release the bump lands in.
//!
//! `backend.detected` is the actual `GpuInfo` value serialized verbatim
//! via `serde_json::to_value`. That preserves the tagged-union shape so
//! per-backend fields (NVIDIA `devices[]`, Metal `total_memory_bytes`)
//! survive without a lossy scalar projection. `backend.expected` is the
//! CLI's `--backend` value passed through as-is — Unit 3's `UatBackend`
//! value-enum already restricts the spelling set to canonical
//! discriminants, so no normalization step is needed at the report layer.

use std::fmt;
use std::time::Duration;

use serde::Serialize;

use crate::cli::cli_args::UatBackend;
use crate::gpu::GpuInfo;

/// Schema version for `llamastash uat`'s JSON output. Additive-only
/// changes within v1; renames or removals bump to v2.
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level report. Serialized to the file at `--report-out`, or to
/// stdout when `--report-out -` is used. The TTY-pretty form is built
/// by `render_tty_summary` (separate so a `--quiet` run still produces
/// a structurally identical JSON).
#[derive(Debug, Clone, Serialize)]
pub struct UatReport {
  pub schema_version: u32,
  /// RFC-3339 timestamp captured at orchestrator entry.
  pub started_at: String,
  /// Wall-clock elapsed seconds across the entire 5-step lifecycle.
  pub duration_secs: f64,
  pub host: HostBlock,
  pub backend: BackendBlock,
  pub steps: Vec<StepResult>,
  pub verdict: Verdict,
  /// Populated only when `verdict != "pass"`. `null` on success so the
  /// human-readable scan-down lands on success without a phantom error
  /// frame.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub failure_summary: Option<FailureSummary>,
}

/// Static + process-derived host context. Not GPU-specific; that
/// lives under `BackendBlock`.
#[derive(Debug, Clone, Serialize)]
pub struct HostBlock {
  /// Lowercase `target_os` (linux / macos / windows).
  pub os: String,
  /// `target_arch` (x86_64 / aarch64 / …).
  pub arch: String,
  /// `uname -r` style kernel string. Best-effort; `""` if unavailable.
  pub kernel: String,
  /// `env!("CARGO_PKG_VERSION")` of the binary running the UAT.
  pub llamastash_version: String,
  /// `llama-server --version` output trimmed to the first line.
  /// `None` when the binary isn't on PATH at start time.
  pub llama_server_version: Option<String>,
  /// Repo of the model actually downloaded — `PRIMARY.repo` on the
  /// happy path, `FALLBACK.repo` if the primary fetch failed and the
  /// fallback succeeded. Empty when the init step itself failed before
  /// any model download attempt.
  pub model_used: String,
  /// Non-fatal anomalies surfaced by the orchestrator. Notable
  /// inhabitants:
  ///
  /// * "preserved tempdir at <path>" — emitted by the TempdirGuard on
  ///   any non-success path so the maintainer can find the diagnostic
  ///   tree.
  /// * "primary model fetch failed: <reason>; used fallback" — emitted
  ///   when the primary HF resolve errored and the fallback ran.
  /// * "reference model SHA unlocked (placeholder)" — emitted when
  ///   `model::is_unlocked` reports the placeholder SHA was used.
  pub warnings: Vec<String>,
}

/// Expected vs. detected backend pair. The orchestrator's pre-flight
/// step writes both; subsequent steps only flow forward when they
/// match.
#[derive(Debug, Clone, Serialize)]
pub struct BackendBlock {
  pub expected: ExpectedBackend,
  /// Serialized `GpuInfo` (tagged union, `tag = "backend"`). `Value`
  /// rather than the typed enum so the report module doesn't pull a
  /// dependency on the gpu module's shape into every downstream
  /// consumer. Filled by `set_detected`.
  pub detected: serde_json::Value,
}

/// Pass-through string for the CLI's `--backend` value. Restricted by
/// `UatBackend` so the report's expected vs detected comparison is a
/// direct string compare against the `GpuInfo` discriminant.
pub type ExpectedBackend = &'static str;

impl BackendBlock {
  /// Build an empty block from a `UatBackend`. The detected slot is
  /// initialized to `Value::Null` so the pre-flight step can overwrite
  /// it without re-allocating the parent struct.
  pub fn new(expected: UatBackend) -> Self {
    Self {
      expected: backend_label(expected),
      detected: serde_json::Value::Null,
    }
  }

  /// Replace `detected` with the verbatim serde projection of an
  /// actual `GpuInfo`. Stays tagged-union so per-backend fields
  /// survive without a lossy scalar projection.
  pub fn set_detected(&mut self, gpu: &GpuInfo) {
    self.detected = serde_json::to_value(gpu)
      .expect("GpuInfo serialization is infallible — only String / numeric / Option fields");
  }
}

/// Map the CLI value-enum to the static string the report carries.
/// Stays in sync with `GpuInfo::label()` for the four backends the
/// UAT exercises; `cpu_only` is intentionally absent because the
/// CLI refuses it (no useful UAT signal on a CPU-only host).
pub const fn backend_label(b: UatBackend) -> ExpectedBackend {
  match b {
    UatBackend::Nvidia => "nvidia",
    UatBackend::Amd => "amd",
    UatBackend::AppleMetal => "apple_metal",
    UatBackend::Vulkan => "unknown", // GpuInfo::Unknown is the Vulkan-only fallback discriminant
  }
}

/// One row of the 5-step lifecycle. Steps that the orchestrator
/// short-circuited carry `Verdict::Skipped` and `duration_ms = 0`.
#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
  pub name: StepName,
  pub verdict: StepVerdict,
  pub duration_ms: u64,
  /// Step-specific observations. JSON-shaped so different steps can
  /// carry different sub-shapes without changing the top-level
  /// envelope. `null` when the step has nothing useful to say.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub observed: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepName {
  DoctorPreflight,
  Init,
  SmokeChat,
  Stop,
  DoctorPostrun,
}

impl StepName {
  pub const fn ordered() -> &'static [StepName] {
    &[
      Self::DoctorPreflight,
      Self::Init,
      Self::SmokeChat,
      Self::Stop,
      Self::DoctorPostrun,
    ]
  }

  /// snake_case wire spelling — matches the serde `rename_all` shape
  /// used in JSON, so the TTY summary and the JSON report agree.
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::DoctorPreflight => "doctor_preflight",
      Self::Init => "init",
      Self::SmokeChat => "smoke_chat",
      Self::Stop => "stop",
      Self::DoctorPostrun => "doctor_postrun",
    }
  }
}

impl fmt::Display for StepName {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepVerdict {
  Pass,
  Fail,
  Skipped,
  /// Set by the SIGINT handler on the in-flight step before the
  /// orchestrator unwinds.
  Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
  Pass,
  Fail,
  Interrupted,
}

/// Populated when `verdict` is `fail` or `interrupted`. Carries the
/// failing child wrapper's exit code verbatim (e.g., `73` for an
/// init-download failure), not a remapped UAT code.
#[derive(Debug, Clone, Serialize)]
pub struct FailureSummary {
  pub step: StepName,
  /// Stable snake_case classification consumed by the nightly
  /// workflow's rolling-issue comment and any agent triaging the
  /// report. Decoupled from `message` so prose tweaks don't break
  /// downstream pattern matching.
  pub classification: FailureClass,
  /// Exit code of the *failing child wrapper*. For the smoke-chat step
  /// this is the orchestrator's own synthetic code (1 for HTTP failure,
  /// 2 for timeout) since smoke chat doesn't spawn a separate process.
  pub exit_code: i32,
  pub message: String,
}

/// Stable, machine-readable failure-class enum. Snake-case spellings
/// are the contract agents pattern-match on; renames are a breaking
/// schema change (bump `SCHEMA_VERSION`). New variants append at the
/// end and are otherwise additive within v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
  BackendMismatch,
  InitInstall,
  InitDownload,
  InitOther,
  SmokeHttp,
  SmokeParse,
  SmokeStatus,
  StopFailed,
  DoctorPostrunFailed,
  Timeout,
  Interrupted,
  Other,
}

impl UatReport {
  /// Build a fresh report scaffold with all five steps preallocated as
  /// `Skipped` + `duration_ms = 0`. Each step's verdict is overwritten
  /// as the lifecycle progresses; a `verdict: fail` short-circuit
  /// leaves the remaining rows in their initial `Skipped` state.
  pub fn skeleton(expected: UatBackend, started_at: String, llamastash_version: String) -> Self {
    let steps = StepName::ordered()
      .iter()
      .map(|name| StepResult {
        name: *name,
        verdict: StepVerdict::Skipped,
        duration_ms: 0,
        observed: None,
      })
      .collect();
    Self {
      schema_version: SCHEMA_VERSION,
      started_at,
      duration_secs: 0.0,
      host: HostBlock {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        kernel: read_kernel().unwrap_or_default(),
        llamastash_version,
        llama_server_version: None,
        model_used: String::new(),
        warnings: Vec::new(),
      },
      backend: BackendBlock::new(expected),
      steps,
      verdict: Verdict::Fail,
      failure_summary: None,
    }
  }

  /// Locate a step by name. The lifecycle uses this to flip a step's
  /// verdict in-place as it advances.
  pub fn step_mut(&mut self, name: StepName) -> &mut StepResult {
    self
      .steps
      .iter_mut()
      .find(|s| s.name == name)
      .expect("skeleton preallocates every step name in order; lookup is total")
  }

  /// Re-assert that the subject-pipeline steps after `failed_step`
  /// stay `Skipped`. Only `Init` and `SmokeChat` are touched —
  /// `Stop` and `DoctorPostrun` are cleanup steps that own their own
  /// verdicts (Pass / Fail / Skipped depending on whether the
  /// lifecycle scheduled them this run). Resetting them here would
  /// misrepresent a real cleanup outcome as Skipped, so they stay
  /// untouched.
  pub fn skip_after(&mut self, failed_step: StepName) {
    let mut past = false;
    for s in &mut self.steps {
      if past && matches!(s.name, StepName::Init | StepName::SmokeChat) {
        s.verdict = StepVerdict::Skipped;
        s.duration_ms = 0;
      }
      if s.name == failed_step {
        past = true;
      }
    }
  }

  /// Apply the total `duration_secs` from a `Duration`.
  pub fn set_duration(&mut self, d: Duration) {
    // Sub-millisecond precision is irrelevant for a wall-clock-minute
    // budget but we keep three decimal places so the JSON looks like
    // the brainstorm illustration.
    self.duration_secs = (d.as_secs_f64() * 1000.0).round() / 1000.0;
  }
}

impl StepResult {
  pub fn pass(&mut self, duration: Duration, observed: Option<serde_json::Value>) {
    self.verdict = StepVerdict::Pass;
    self.duration_ms = duration.as_millis() as u64;
    self.observed = observed;
  }

  pub fn fail(&mut self, duration: Duration, observed: Option<serde_json::Value>) {
    self.verdict = StepVerdict::Fail;
    self.duration_ms = duration.as_millis() as u64;
    self.observed = observed;
  }

  pub fn interrupt(&mut self, duration: Duration) {
    self.verdict = StepVerdict::Interrupted;
    self.duration_ms = duration.as_millis() as u64;
  }
}

/// Snake_case spelling for a `FailureClass`, used by TTY rendering and
/// any debug formatter that wants the same wire shape as the JSON.
pub const fn classification_label(c: FailureClass) -> &'static str {
  match c {
    FailureClass::BackendMismatch => "backend_mismatch",
    FailureClass::InitInstall => "init_install",
    FailureClass::InitDownload => "init_download",
    FailureClass::InitOther => "init_other",
    FailureClass::SmokeHttp => "smoke_http",
    FailureClass::SmokeParse => "smoke_parse",
    FailureClass::SmokeStatus => "smoke_status",
    FailureClass::StopFailed => "stop_failed",
    FailureClass::DoctorPostrunFailed => "doctor_postrun_failed",
    FailureClass::Timeout => "timeout",
    FailureClass::Interrupted => "interrupted",
    FailureClass::Other => "other",
  }
}

/// Best-effort kernel-version readout (Unix `uname -r`). Reports a
/// String instead of bubbling I/O errors because a missing kernel
/// string is metadata noise, not a UAT failure.
fn read_kernel() -> Option<String> {
  #[cfg(unix)]
  {
    let out = std::process::Command::new("uname")
      .arg("-r")
      .output()
      .ok()?;
    if !out.status.success() {
      return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
  }
  #[cfg(not(unix))]
  {
    None
  }
}

/// Render a human-readable summary of `report` for stdout. The
/// orchestrator emits both the JSON (to `--report-out`) and this TTY
/// form unless `--quiet` is set. Single render site so the TTY form
/// and the JSON can't drift.
pub fn render_tty_summary(report: &UatReport) -> String {
  use std::fmt::Write;
  let mut out = String::with_capacity(512);
  let _ = writeln!(
    out,
    "UAT verdict: {} ({} backend, {:.1}s)",
    match report.verdict {
      Verdict::Pass => "pass",
      Verdict::Fail => "fail",
      Verdict::Interrupted => "interrupted",
    },
    report.backend.expected,
    report.duration_secs
  );
  for step in &report.steps {
    let verdict = match step.verdict {
      StepVerdict::Pass => "pass",
      StepVerdict::Fail => "fail",
      StepVerdict::Skipped => "skip",
      StepVerdict::Interrupted => "intr",
    };
    let _ = writeln!(
      out,
      "  {:<18}  {:<4}  {} ms",
      step.name, verdict, step.duration_ms
    );
  }
  if let Some(fs) = &report.failure_summary {
    let _ = writeln!(
      out,
      "failure: step={} classification={} exit_code={} message={}",
      fs.step,
      classification_label(fs.classification),
      fs.exit_code,
      fs.message
    );
  }
  if !report.host.warnings.is_empty() {
    let _ = writeln!(out, "warnings:");
    for w in &report.host.warnings {
      let _ = writeln!(out, "  - {w}");
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample_skeleton() -> UatReport {
    UatReport::skeleton(
      UatBackend::Nvidia,
      "2026-05-20T09:00:00Z".to_string(),
      "0.2.0-test".to_string(),
    )
  }

  #[test]
  fn skeleton_preallocates_five_steps_in_order() {
    let r = sample_skeleton();
    assert_eq!(r.steps.len(), 5);
    assert_eq!(r.steps[0].name, StepName::DoctorPreflight);
    assert_eq!(r.steps[1].name, StepName::Init);
    assert_eq!(r.steps[2].name, StepName::SmokeChat);
    assert_eq!(r.steps[3].name, StepName::Stop);
    assert_eq!(r.steps[4].name, StepName::DoctorPostrun);
    for s in &r.steps {
      assert_eq!(s.verdict, StepVerdict::Skipped);
      assert_eq!(s.duration_ms, 0);
    }
  }

  #[test]
  fn step_mut_returns_a_writable_handle() {
    let mut r = sample_skeleton();
    r.step_mut(StepName::Init).pass(
      Duration::from_millis(135_120),
      Some(serde_json::json!({"note": "init ok"})),
    );
    assert_eq!(r.step_mut(StepName::Init).verdict, StepVerdict::Pass);
    assert_eq!(r.step_mut(StepName::Init).duration_ms, 135_120);
  }

  #[test]
  fn skip_after_clears_downstream_subject_durations() {
    let mut r = sample_skeleton();
    r.step_mut(StepName::DoctorPreflight)
      .pass(Duration::from_millis(312), None);
    r.step_mut(StepName::Init).fail(
      Duration::from_millis(800),
      Some(serde_json::json!({"phase": "download"})),
    );
    r.step_mut(StepName::SmokeChat)
      .pass(Duration::from_millis(999), None);
    r.skip_after(StepName::Init);
    assert_eq!(
      r.step_mut(StepName::SmokeChat).verdict,
      StepVerdict::Skipped
    );
    assert_eq!(r.step_mut(StepName::SmokeChat).duration_ms, 0);
    // Preflight (before the failed step) is left alone.
    assert_eq!(
      r.step_mut(StepName::DoctorPreflight).verdict,
      StepVerdict::Pass
    );
    assert_eq!(r.step_mut(StepName::DoctorPreflight).duration_ms, 312);
  }

  #[test]
  fn skip_after_leaves_cleanup_step_verdicts_intact() {
    // Stop and DoctorPostrun own their own verdicts — skip_after must
    // not overwrite Pass/Fail set by the cleanup step bodies even when
    // an earlier subject step failed.
    let mut r = sample_skeleton();
    r.step_mut(StepName::Init).fail(
      Duration::from_millis(800),
      Some(serde_json::json!({"phase": "download"})),
    );
    r.step_mut(StepName::Stop)
      .pass(Duration::from_millis(120), None);
    r.step_mut(StepName::DoctorPostrun)
      .fail(Duration::from_millis(250), None);
    r.skip_after(StepName::Init);
    assert_eq!(r.step_mut(StepName::Stop).verdict, StepVerdict::Pass);
    assert_eq!(r.step_mut(StepName::Stop).duration_ms, 120);
    assert_eq!(
      r.step_mut(StepName::DoctorPostrun).verdict,
      StepVerdict::Fail
    );
    assert_eq!(r.step_mut(StepName::DoctorPostrun).duration_ms, 250);
  }

  #[test]
  fn backend_block_set_detected_preserves_tagged_union() {
    let mut bb = BackendBlock::new(UatBackend::AppleMetal);
    bb.set_detected(&GpuInfo::AppleMetal {
      total_memory_bytes: 38_654_705_664,
    });
    // The serde shape carries both the `backend` discriminant and the
    // backend-specific payload — verifying the projection here keeps
    // a future GpuInfo rename from silently dropping fields.
    let detected = bb.detected.as_object().unwrap();
    assert_eq!(detected["backend"], "apple_metal");
    assert_eq!(detected["total_memory_bytes"], 38_654_705_664u64);
  }

  #[test]
  fn backend_label_round_trip_matches_gpuinfo_label() {
    // Vulkan maps to `unknown` because `GpuInfo::Unknown` is the
    // Vulkan-only fallback discriminant — the lifecycle's pre-flight
    // assertion compares the report's expected against the detected
    // `backend` field, so the spelling MUST agree on every value.
    assert_eq!(backend_label(UatBackend::Nvidia), "nvidia");
    assert_eq!(backend_label(UatBackend::Amd), "amd");
    assert_eq!(backend_label(UatBackend::AppleMetal), "apple_metal");
    assert_eq!(backend_label(UatBackend::Vulkan), "unknown");
  }

  #[test]
  fn backend_label_vulkan_tracks_gpuinfo_unknown_label() {
    // The Vulkan→"unknown" mapping is structurally coupled to
    // GpuInfo::Unknown::label(). If a future refactor renames the
    // discriminant to "vulkan", this assertion fails loud — keeping
    // backend_label's hand-rolled mapping from silently drifting.
    let unknown_label = GpuInfo::Unknown {
      devices: Vec::new(),
    }
    .label();
    assert_eq!(
      backend_label(UatBackend::Vulkan),
      unknown_label,
      "backend_label(Vulkan) must agree with GpuInfo::Unknown::label(); refactor surfaced silently"
    );
  }

  #[test]
  fn schema_version_is_one_until_breaking_change_lands() {
    assert_eq!(SCHEMA_VERSION, 1);
  }

  #[test]
  fn report_renders_all_five_step_rows_for_tty_summary() {
    let mut r = sample_skeleton();
    r.set_duration(Duration::from_secs_f64(142.3));
    r.verdict = Verdict::Pass;
    for step in StepName::ordered() {
      r.step_mut(*step).pass(Duration::from_millis(123), None);
    }
    let summary = render_tty_summary(&r);
    assert!(summary.contains("UAT verdict: pass"));
    // TTY uses snake_case spellings — same as the JSON wire shape, so
    // agents reading both don't have to maintain a case-translation
    // table.
    for label in [
      "doctor_preflight",
      "init",
      "smoke_chat",
      "stop",
      "doctor_postrun",
    ] {
      assert!(
        summary.contains(label),
        "TTY summary should list step `{label}`: {summary}"
      );
    }
  }

  #[test]
  fn failure_summary_renders_into_tty_output() {
    let mut r = sample_skeleton();
    r.verdict = Verdict::Fail;
    r.failure_summary = Some(FailureSummary {
      step: StepName::Init,
      classification: FailureClass::InitDownload,
      exit_code: 73,
      message: "init download failed: hf-hub: 404 Not Found".to_string(),
    });
    let summary = render_tty_summary(&r);
    assert!(summary.contains("failure"));
    assert!(summary.contains("classification=init_download"));
    assert!(summary.contains("exit_code=73"));
    assert!(summary.contains("hf-hub"));
  }

  #[test]
  fn failure_summary_serializes_classification_as_snake_case() {
    let mut r = sample_skeleton();
    r.verdict = Verdict::Fail;
    r.failure_summary = Some(FailureSummary {
      step: StepName::DoctorPreflight,
      classification: FailureClass::BackendMismatch,
      exit_code: 10,
      message: "pre-flight mismatch".to_string(),
    });
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(
      json["failure_summary"]["classification"],
      "backend_mismatch"
    );
  }

  #[test]
  fn skeleton_failure_summary_serializes_as_null_when_pass() {
    let mut r = sample_skeleton();
    r.verdict = Verdict::Pass;
    let json = serde_json::to_value(&r).unwrap();
    // `skip_serializing_if = "Option::is_none"` is what keeps the
    // success scan-down free of a phantom `failure_summary: null`.
    assert!(json.get("failure_summary").is_none());
  }
}
