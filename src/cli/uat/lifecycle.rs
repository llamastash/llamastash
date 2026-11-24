//! 6-step UAT orchestrator: pre-flight, init, start, chat probe, stop,
//! doctor. The first `Fail` short-circuits the rest, and every step's
//! verdict lands in the `UatReport`.
//!
//! Child processes inherit the `TempdirGuard`'s isolation env vars,
//! so they never write to the maintainer's real state / cache / HF
//! cache paths.

use std::{
  path::PathBuf,
  process::Stdio,
  sync::atomic::Ordering,
  time::{Duration, Instant},
};

use crate::cli::cli_args::{UatBackend, UatMode};
use crate::gpu::GpuInfo;

use super::{
  isolation::TempdirGuard,
  model::{self, ReferenceModel},
  report::{FailureClass, FailureSummary, StepName, UatReport, Verdict},
};

/// Inputs to the orchestrator. Built by `handle()` from `UatArgs` and
/// the resolved `TempdirGuard`; passed by reference so each step can
/// read the immutable plan without owning it.
pub struct LifecyclePlan {
  pub host_backend: UatBackend,
  pub runtime_backend: Option<UatBackend>,
  pub mode: UatMode,
  /// Absolute path to `llamastash` (this binary) — captured at the
  /// orchestrator entry so each step's `Command::new` is byte-stable.
  pub llamastash_path: std::path::PathBuf,
  /// Per-step subprocess timeout. 5 minutes covers the warm budget.
  /// Cold mode is given 15 minutes.
  pub per_step_timeout: Duration,
  /// When set, skip the HF download and use this GGUF directly.
  pub local_gguf: Option<PathBuf>,
}

impl LifecyclePlan {
  pub fn from_args(
    host_backend: UatBackend,
    runtime_backend: Option<UatBackend>,
    mode: UatMode,
    local_gguf: Option<PathBuf>,
  ) -> std::io::Result<Self> {
    let llamastash_path = std::env::current_exe()?;
    let per_step_timeout = match mode {
      UatMode::Warm => Duration::from_secs(5 * 60),
      UatMode::Cold => Duration::from_secs(15 * 60),
    };
    Ok(Self {
      host_backend,
      runtime_backend,
      mode,
      llamastash_path,
      per_step_timeout,
      local_gguf,
    })
  }
}

/// Top-level entry point invoked by `cli::uat::handle`. Drives the
/// 6-step lifecycle and returns the populated report. Never panics on
/// step failures — every failure mode flows through the report's
/// `verdict`/`failure_summary` fields.
pub async fn run(plan: &LifecyclePlan, guard: &TempdirGuard, report: &mut UatReport) {
  let started = Instant::now();
  let mut failed_step: Option<StepName> = None;

  match step_doctor_preflight(plan, report).await {
    Ok(()) => {}
    Err(e) => {
      record_failure(report, StepName::DoctorPreflight, &e);
      failed_step = Some(StepName::DoctorPreflight);
    }
  }

  let mut gguf_path: Option<PathBuf> = None;
  if failed_step.is_none() {
    match step_init(plan, guard, report).await {
      Ok(path) => gguf_path = Some(path),
      Err(e) => {
        record_failure(report, StepName::Init, &e);
        failed_step = Some(StepName::Init);
      }
    }
  }

  if failed_step.is_none() {
    if let Some(path) = gguf_path.as_ref() {
      match step_start_model(plan, guard, report, path).await {
        Ok(()) => {}
        Err(e) => {
          record_failure(report, StepName::StartModel, &e);
          failed_step = Some(StepName::StartModel);
        }
      }
    }
  }

  if failed_step.is_none() {
    match step_smoke_chat(plan, guard, report).await {
      Ok(()) => {}
      Err(e) => {
        record_failure(report, StepName::SmokeChat, &e);
        failed_step = Some(StepName::SmokeChat);
      }
    }
  }

  // `step_stop` + `step_doctor_postrun` are cleanup. They only make
  // sense when a daemon / engine child were actually brought up, so
  // cleanup runs when no failure happened, OR when start_model /
  // smoke_chat failed (the daemon is up and a child may be loaded). A
  // preflight or init failure leaves nothing to stop, and cleanup would
  // waste 30s+ producing noisy verdicts.
  let cleanup_needed = matches!(
    failed_step,
    None | Some(StepName::StartModel) | Some(StepName::SmokeChat)
  );
  if cleanup_needed {
    let _ = step_stop(plan, guard, report).await;
    let _ = step_doctor_postrun(plan, guard, report).await;
  }

  if let Some(failed) = failed_step {
    report.skip_after(failed);
  }
  report.set_duration(started.elapsed());
  report.verdict = if failed_step.is_some() {
    Verdict::Fail
  } else {
    Verdict::Pass
  };
}

/// Step 1. Read-only — calls `gpu::probe()` directly (library code, no
/// subprocess needed). Pre-flight is the place a runner-image
/// regression must fail loudly: a Metal-less macOS-14 image still
/// passes init (no GPU = CPU fallback) and a fake "pass" verdict would
/// look like green CI on a fundamentally broken state.
///
/// `gpu::probe()` is synchronous and may busy-wait up to ~20s across
/// the four vendor sub-probes (each 5s, hand-rolled timeout). Running
/// it on the tokio runtime thread would block the `select!` arm
/// listening for ctrl_c — `spawn_blocking` keeps SIGINT responsive
/// during preflight.
async fn step_doctor_preflight(
  plan: &LifecyclePlan,
  report: &mut UatReport,
) -> Result<(), StepError> {
  let started = Instant::now();
  let detected = tokio::task::spawn_blocking(crate::gpu::probe)
    .await
    .unwrap_or(GpuInfo::CpuOnly);
  report.backend.set_detected(&detected);
  let detected_label = detected.label();
  let expected_label = report.backend.expected;
  if detected_label != expected_label {
    return Err(StepError {
      message: format!(
        "pre-flight: expected backend `{expected_label}`, detected `{detected_label}`"
      ),
      exit_code: PREFLIGHT_MISMATCH_CODE,
      classification: FailureClass::BackendMismatch,
      duration: started.elapsed(),
    });
  }
  // Populate host.llama_server_version best-effort. A missing binary
  // is fine in pre-flight — the install step will surface it.
  report.host.llama_server_version = read_llama_server_version();
  report.step_mut(StepName::DoctorPreflight).pass(
    started.elapsed(),
    Some(serde_json::json!({
      "expected_backend": expected_label,
      "detected_backend": detected_label,
      "runtime_backend": plan.runtime_backend.map(crate::cli::uat::report::runtime_backend_label),
      "gpu_device_count": gpu_device_count(&detected),
    })),
  );
  Ok(())
}

fn gpu_device_count(gpu: &GpuInfo) -> usize {
  match gpu {
    GpuInfo::CpuOnly => 0,
    GpuInfo::AppleMetal { .. } => 1,
    GpuInfo::Nvidia { devices }
    | GpuInfo::Amd { devices }
    | GpuInfo::Unknown { devices }
    | GpuInfo::Multi { devices } => devices.len(),
  }
}

fn read_llama_server_version() -> Option<String> {
  let path = which::which("llama-server").ok()?;
  let out = std::process::Command::new(&path)
    .arg("--version")
    .output()
    .ok()?;
  if !out.status.success() {
    return None;
  }
  String::from_utf8_lossy(&out.stdout)
    .lines()
    .next()
    .map(|s| s.trim().to_string())
}

/// Step 2. Spawns `llamastash init` with the primary reference model;
/// on any failure during the model-pull phase, retries with the
/// fallback. Records the substitution in `host.warnings` so the
/// report flags silent fallback before the maintainer scans down.
///
/// Returns the absolute path of the downloaded GGUF (from
/// `model.files[0]` in init's `--json` stdout) so the next step
/// (`start_model`) can register it with the daemon.
async fn step_init(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  report: &mut UatReport,
) -> Result<PathBuf, StepError> {
  let started = Instant::now();
  if let Some(local) = &plan.local_gguf {
    if !local.exists() {
      return Err(StepError {
        message: format!("--local-gguf path does not exist: {}", local.display()),
        exit_code: 1,
        classification: FailureClass::InitOther,
        duration: started.elapsed(),
      });
    }
    report.host.model_used = local.display().to_string();
    report
      .host
      .warnings
      .push("--local-gguf: skipped HF download".to_string());
    report.step_mut(StepName::Init).pass(
      started.elapsed(),
      Some(serde_json::json!({
        "model_repo": "local",
        "mode": match plan.mode { UatMode::Warm => "warm", UatMode::Cold => "cold" },
        "gguf_path": local,
      })),
    );
    return Ok(local.clone());
  }
  let primary_outcome = run_init_for(plan, guard, &model::PRIMARY).await;
  let (used, init_stdout) = match primary_outcome {
    Ok(stdout) => (&model::PRIMARY, stdout),
    Err(primary_err) => {
      report.host.warnings.push(format!(
        "primary model fetch failed: {}; trying fallback",
        primary_err.message
      ));
      let fallback_outcome = run_init_for(plan, guard, &model::FALLBACK).await;
      match fallback_outcome {
        Ok(stdout) => (&model::FALLBACK, stdout),
        Err(fallback_err) => {
          return Err(StepError {
            message: format!(
              "init: primary failed ({}); fallback failed ({})",
              primary_err.message, fallback_err.message
            ),
            exit_code: fallback_err.exit_code,
            classification: classify_init_exit(fallback_err.exit_code),
            duration: started.elapsed(),
          });
        }
      }
    }
  };
  report.host.model_used = used.repo.to_string();
  if model::is_unlocked(used) {
    report
      .host
      .warnings
      .push("reference model SHA unlocked (placeholder)".to_string());
  }
  // Compare actual download size to the reference model's expected
  // envelope. A >±10% drift surfaces a silent file substitution
  // upstream (HF re-uploaded the GGUF, the recommender's pick changed)
  // without failing the run.
  if let Some(actual_bytes) = parse_init_total_bytes(&init_stdout) {
    if let Some(deviation_warning) = check_size_deviation(used.expected_size_bytes, actual_bytes) {
      report.host.warnings.push(deviation_warning);
    }
  }
  let gguf_path = parse_init_gguf_path(&init_stdout).ok_or_else(|| StepError {
    message:
      "init: `model.files[0]` missing or unparseable in init --json stdout (cannot start model)"
        .to_string(),
    exit_code: 1,
    classification: FailureClass::InitOther,
    duration: started.elapsed(),
  })?;
  report.step_mut(StepName::Init).pass(
    started.elapsed(),
    Some(serde_json::json!({
      "model_repo": used.repo,
      "mode": match plan.mode { UatMode::Warm => "warm", UatMode::Cold => "cold" },
      "gguf_path": gguf_path,
    })),
  );
  Ok(gguf_path)
}

async fn run_init_for(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  reference: &ReferenceModel,
) -> Result<Vec<u8>, StepError> {
  let model_arg = format!("{}:{}", reference.repo, reference.filename);
  let mut cmd = tokio::process::Command::new(&plan.llamastash_path);
  cmd
    .arg("--quiet")
    .arg("init")
    .arg("--recommended")
    .arg("--json")
    .arg("--model")
    .arg(&model_arg);
  // Only thread `--revision` when locked. A placeholder SHA would
  // resolve to the literal string and fail in hf-hub's HEAD request.
  if !model::is_unlocked(reference) {
    cmd.arg("--revision").arg(reference.commit_sha);
  }
  if matches!(plan.mode, UatMode::Warm) {
    cmd.arg("--skip").arg("server");
  }
  guard.configure_command(cmd.as_std_mut());
  run_child_with_timeout(&mut cmd, plan.per_step_timeout).await
}

/// Extract `model.total_bytes` from `init --json` stdout. A missing /
/// non-numeric value returns `None` — the deviation check then
/// silently skips rather than synthesizing a false warning.
fn parse_init_total_bytes(stdout: &[u8]) -> Option<u64> {
  let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
  v.get("model")?.get("total_bytes")?.as_u64()
}

/// Extract the first GGUF path from `init --json` stdout's
/// `model.files[]` array. Returns `None` if the field is missing or
/// the array is empty — the lifecycle treats that as an init-shape
/// regression rather than synthesizing a path.
fn parse_init_gguf_path(stdout: &[u8]) -> Option<PathBuf> {
  let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
  let first = v.get("model")?.get("files")?.as_array()?.first()?;
  let s = first.as_str()?;
  Some(PathBuf::from(s))
}

/// Return a warning string when `actual_bytes` deviates from
/// `expected_bytes` by more than ±10%. The threshold matches the
/// envelope documented on `ReferenceModel::expected_size_bytes`.
fn check_size_deviation(expected_bytes: u64, actual_bytes: u64) -> Option<String> {
  if expected_bytes == 0 {
    return None;
  }
  let diff = actual_bytes.abs_diff(expected_bytes);
  // Integer arithmetic: diff * 10 vs expected — avoids floating-point
  // edge cases on small models. 10% threshold = diff / expected > 0.1
  // ⇔ diff * 10 > expected.
  if diff.saturating_mul(10) > expected_bytes {
    return Some(format!(
      "model download size {actual_bytes} B deviates >10% from expected {expected_bytes} B (Δ={diff} B)"
    ));
  }
  None
}

/// Helper for the per-step subprocess sites: applies `kill_on_drop` so
/// a cancelled future, a timeout, or a SIGINT-cancelled `tokio::select!`
/// always reaps the child instead of orphaning it.
fn finalize_uat_command(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
  cmd
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
}

/// Step 3. Register the just-downloaded GGUF with the sandbox daemon
/// so a real engine child is `Ready` before `smoke_chat` probes it.
/// `llamastash start --json` returns as soon as the supervisor is
/// *spawned* (`compose_and_spawn` returns post-spawn, before the
/// supervisor transitions Launching → Loading → Ready). We therefore
/// poll `status --json` ourselves after start returns and only declare
/// this step pass once `models[0].state == "ready"`.
async fn step_start_model(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  report: &mut UatReport,
  gguf_path: &std::path::Path,
) -> Result<(), StepError> {
  let started = Instant::now();
  // Force a catalog rescan that includes the GGUF's parent. The
  // daemon takes its catalog snapshot at startup, BEFORE `init` pulls
  // the reference model into the sandbox HF cache; without
  // `--model-path` the resolver can't see the just-downloaded file
  // and `llamastash start` exits 66 ("no model matches") even though
  // the absolute path is valid on disk.
  let scan_dir = gguf_path
    .parent()
    .unwrap_or_else(|| std::path::Path::new("."));
  let mut cmd = tokio::process::Command::new(&plan.llamastash_path);
  cmd
    .arg("--quiet")
    .arg("start")
    .arg("--json")
    .arg("--mode")
    .arg("chat")
    .arg("--model-path")
    .arg(scan_dir)
    .arg(gguf_path);
  guard.configure_command(cmd.as_std_mut());
  let stdout = run_child_with_timeout(&mut cmd, plan.per_step_timeout)
    .await
    .map_err(|mut e| {
      // Preserve TIMEOUT classification; anything else collapses to
      // start_model_failed so the workflow's rolling-issue triage
      // groups start failures together.
      if !matches!(e.classification, FailureClass::Timeout) {
        e.classification = FailureClass::StartModelFailed;
      }
      e
    })?;
  // The shape's stability is enforced by start's own --json contract
  // tests, so a non-JSON or shape-shifted output is not a step fail —
  // the daemon already accepted the start.
  let parsed: serde_json::Value =
    serde_json::from_slice(&stdout).unwrap_or(serde_json::Value::Null);
  // Best-effort PID into the guard so a mid-test SIGINT can reap the
  // child before tempdir teardown — step_stop, when it runs, remains
  // the authoritative shutdown.
  if let Some(pid) = parsed.get("pid").and_then(|v| v.as_i64()) {
    if let Ok(pid32) = i32::try_from(pid) {
      if pid32 > 0 {
        guard.child_pid_handle().store(pid32, Ordering::SeqCst);
      }
    }
  }
  // Wait for the supervisor to actually reach Ready before we hand
  // off to smoke_chat. `llamastash start` returns post-spawn, so the
  // engine child may still be in Launching/Loading when start exits 0;
  // without this poll, smoke_chat hits the port before anything's bound
  // and fails with `connection refused`.
  wait_for_ready(plan, guard, READY_POLL_BUDGET).await?;
  report.step_mut(StepName::StartModel).pass(
    started.elapsed(),
    Some(serde_json::json!({
      "name": parsed.get("name").and_then(|v| v.as_str()).unwrap_or(""),
      "port": parsed.get("port").and_then(|v| v.as_u64()).unwrap_or(0),
      "pid":  parsed.get("pid").and_then(|v| v.as_i64()).unwrap_or(0),
    })),
  );
  Ok(())
}

/// Poll `status --json` until `models[0].state == "ready"`,
/// or fail when the supervisor transitions to a terminal error
/// state, or the budget expires. Used by `step_start_model` to bridge
/// the gap between `compose_and_spawn` returning post-spawn and the
/// engine child actually binding its port.
async fn wait_for_ready(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  budget: Duration,
) -> Result<(), StepError> {
  let started = Instant::now();
  let interval = Duration::from_millis(250);
  let mut last_state = "unknown".to_string();
  loop {
    let elapsed = started.elapsed();
    if elapsed >= budget {
      return Err(StepError {
        message: format!(
          "start_model: supervisor did not reach `ready` within {budget:?} (last state: {last_state})"
        ),
        exit_code: TIMEOUT_CODE,
        classification: FailureClass::Timeout,
        duration: elapsed,
      });
    }
    let status = match fetch_status_json(plan, guard).await {
      Ok(v) => v,
      Err(e) => {
        // status probe failing mid-readiness-poll is informational;
        // the next tick may succeed. Surface only if the budget runs
        // out (handled by the loop guard).
        last_state = format!("status_probe_error: {}", e.message);
        tokio::time::sleep(interval).await;
        continue;
      }
    };
    let first = status
      .get("models")
      .and_then(|v| v.as_array())
      .and_then(|a| a.first());
    let state_label = first.and_then(parse_model_state).unwrap_or("unknown");
    last_state = state_label.to_string();
    match state_label {
      "ready" => return Ok(()),
      "error" => {
        let cause = first
          .and_then(parse_model_error_cause)
          .unwrap_or("(no cause reported)");
        return Err(StepError {
          message: format!("start_model: supervisor entered `error` state: {cause}"),
          exit_code: 1,
          classification: FailureClass::StartModelFailed,
          duration: started.elapsed(),
        });
      }
      "stopped" | "stopping" => {
        return Err(StepError {
          message: format!("start_model: supervisor reached `{state_label}` before becoming ready"),
          exit_code: 1,
          classification: FailureClass::StartModelFailed,
          duration: started.elapsed(),
        });
      }
      _ => {
        // "launching" / "loading" / "unknown" — keep waiting.
        tokio::time::sleep(interval).await;
      }
    }
  }
}

/// Wall-clock budget for the supervisor-Ready poll. Generous because
/// a cold-VRAM load of a tiny reference model can still take a few
/// seconds on a busy box; bounded so a wedged supervisor doesn't
/// burn the per-step timeout silently.
const READY_POLL_BUDGET: Duration = Duration::from_secs(60);

fn parse_model_state(model: &serde_json::Value) -> Option<&str> {
  model.get("state").and_then(|v| v.as_str()).or_else(|| {
    model
      .get("state")
      .and_then(|v| v.get("state"))
      .and_then(|v| v.as_str())
  })
}

fn parse_model_error_cause(model: &serde_json::Value) -> Option<&str> {
  model.get("cause").and_then(|v| v.as_str()).or_else(|| {
    model
      .get("state")
      .and_then(|v| v.get("cause"))
      .and_then(|v| v.as_str())
  })
}

/// Step 4. Wraps `status --json` + an HTTP POST to the model's
/// OpenAI-compatible endpoint. Refused (verdict=fail) on:
///
/// * status lookup failure.
/// * Empty `models[]` (start_model didn't actually leave one running).
/// * HTTP non-2xx or empty choices.
async fn step_smoke_chat(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  report: &mut UatReport,
) -> Result<(), StepError> {
  let started = Instant::now();
  let status_json = fetch_status_json(plan, guard).await?;
  let (model_name, port) =
    parse_first_running_model(&status_json).map_err(|message| StepError {
      message,
      exit_code: SMOKE_PARSE_ERROR_CODE,
      classification: FailureClass::SmokeParse,
      duration: started.elapsed(),
    })?;
  // Track the daemon-managed child PID into the guard so a mid-step
  // SIGINT or panic exits through Drop with enough info to reap the
  // child before tempdir teardown. step_stop, when it runs, remains
  // the authoritative shutdown — this is the fallback path.
  if let Some(pid) = parse_first_running_pid(&status_json) {
    guard.child_pid_handle().store(pid, Ordering::SeqCst);
  }
  let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
  let body = serde_json::json!({
    "model": model_name,
    "messages": [{ "role": "user", "content": "Say hi." }],
    "max_tokens": 16,
    "stream": false,
  });
  let client = reqwest::Client::builder()
    .timeout(Duration::from_secs(30))
    .build()
    .map_err(|e| StepError {
      message: format!("smoke: reqwest build failed: {e}"),
      exit_code: SMOKE_HTTP_ERROR_CODE,
      classification: FailureClass::SmokeHttp,
      duration: started.elapsed(),
    })?;
  let resp = client
    .post(&url)
    .json(&body)
    .send()
    .await
    .map_err(|e| StepError {
      message: format!("smoke: POST {url} failed: {e}"),
      exit_code: SMOKE_HTTP_ERROR_CODE,
      classification: FailureClass::SmokeHttp,
      duration: started.elapsed(),
    })?;
  let http_status = resp.status();
  let text = resp.text().await.unwrap_or_default();
  if !http_status.is_success() {
    return Err(StepError {
      message: format!(
        "smoke: HTTP {http_status}; body (first 200 chars): {}",
        text.chars().take(200).collect::<String>()
      ),
      exit_code: SMOKE_HTTP_ERROR_CODE,
      classification: FailureClass::SmokeHttp,
      duration: started.elapsed(),
    });
  }
  let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| StepError {
    message: format!("smoke: response was not JSON: {e}"),
    exit_code: SMOKE_PARSE_ERROR_CODE,
    classification: FailureClass::SmokeParse,
    duration: started.elapsed(),
  })?;
  let content = parsed
    .get("choices")
    .and_then(|c| c.get(0))
    .and_then(|c| c.get("message"))
    .and_then(|m| m.get("content"))
    .and_then(|s| s.as_str())
    .unwrap_or_default();
  if content.is_empty() {
    return Err(StepError {
      message: "smoke: response had no chat content".to_string(),
      exit_code: SMOKE_PARSE_ERROR_CODE,
      classification: FailureClass::SmokeParse,
      duration: started.elapsed(),
    });
  }
  report.step_mut(StepName::SmokeChat).pass(
    started.elapsed(),
    Some(serde_json::json!({
      "model": model_name,
      "port": port,
      "tokens_observed": content.split_whitespace().count(),
    })),
  );
  Ok(())
}

/// Bounded budget for `status --json`. A wedged daemon should fail
/// loudly inside this window rather than hang the whole UAT until the
/// workflow's 30-minute timeout fires.
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

async fn fetch_status_json(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
) -> Result<serde_json::Value, StepError> {
  let mut cmd = tokio::process::Command::new(&plan.llamastash_path);
  cmd.arg("--quiet").arg("status").arg("--json");
  guard.configure_command(cmd.as_std_mut());
  let output = match tokio::time::timeout(
    STATUS_PROBE_TIMEOUT,
    finalize_uat_command(&mut cmd).output(),
  )
  .await
  {
    Ok(Ok(o)) => o,
    Ok(Err(e)) => {
      return Err(StepError {
        message: format!("smoke: spawning `status --json` failed: {e}"),
        exit_code: SMOKE_STATUS_ERROR_CODE,
        classification: FailureClass::SmokeStatus,
        duration: Duration::ZERO,
      })
    }
    Err(_) => {
      return Err(StepError {
        message: format!("smoke: `status --json` exceeded {STATUS_PROBE_TIMEOUT:?}; killed"),
        exit_code: TIMEOUT_CODE,
        classification: FailureClass::Timeout,
        duration: STATUS_PROBE_TIMEOUT,
      })
    }
  };
  if !output.status.success() {
    return Err(StepError {
      message: format!(
        "smoke: `status --json` exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
      ),
      exit_code: output.status.code().unwrap_or(SMOKE_STATUS_ERROR_CODE),
      classification: FailureClass::SmokeStatus,
      duration: Duration::ZERO,
    });
  }
  serde_json::from_slice(&output.stdout).map_err(|e| StepError {
    message: format!("smoke: status --json was not JSON: {e}"),
    exit_code: SMOKE_PARSE_ERROR_CODE,
    classification: FailureClass::SmokeParse,
    duration: Duration::ZERO,
  })
}

/// Best-effort extract of `models[0].pid` for guard-side cleanup. A
/// missing / non-numeric pid returns `None`: the worst case is that
/// step_stop remains the only cleanup path — never a hard failure.
fn parse_first_running_pid(status_json: &serde_json::Value) -> Option<i32> {
  status_json
    .get("models")?
    .as_array()?
    .first()?
    .get("pid")?
    .as_i64()
    .and_then(|v| i32::try_from(v).ok())
    .filter(|p| *p > 0)
}

fn parse_first_running_model(status_json: &serde_json::Value) -> Result<(String, u16), String> {
  let models = status_json
    .get("models")
    .and_then(|v| v.as_array())
    .ok_or_else(|| "status JSON missing `models[]`".to_string())?;
  let first = models
    .first()
    .ok_or_else(|| "status JSON has empty `models[]`".to_string())?;
  // The daemon's `status --json` emits `models[].id` as a
  // `ModelId` object (`{"path": "...", "header_blake3": "..."}` — see
  // `src/gguf/identity.rs`). We derive the model-name passed to the
  // engine from the file stem so it reads as a friendly identifier; the
  // engine's `/v1/chat/completions` accepts any string in the `model`
  // body field, so this is purely cosmetic.
  let path = first
    .get("id")
    .and_then(|v| v.get("path"))
    .and_then(|s| s.as_str())
    .ok_or_else(|| "first model missing `id.path`".to_string())?;
  let name = std::path::Path::new(path)
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or(path)
    .to_string();
  let port = first
    .get("port")
    .and_then(|p| p.as_u64())
    .ok_or_else(|| "first model missing `port`".to_string())?;
  let port: u16 = port
    .try_into()
    .map_err(|_| format!("port `{port}` does not fit a u16"))?;
  Ok((name, port))
}

/// Step 5. Run regardless of step 3/4 outcomes so a start_model or
/// smoke-chat failure doesn't leak the started model. `--all --yes`
/// is used so we don't depend on knowing the model name at this
/// point — every daemon-owned child is shut down.
async fn step_stop(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  report: &mut UatReport,
) -> Result<(), StepError> {
  let started = Instant::now();
  let mut cmd = tokio::process::Command::new(&plan.llamastash_path);
  cmd.arg("--quiet").arg("stop").arg("--all").arg("--yes");
  guard.configure_command(cmd.as_std_mut());
  match run_child_with_timeout(&mut cmd, Duration::from_secs(30)).await {
    Ok(_) => {
      report
        .step_mut(StepName::Stop)
        .pass(started.elapsed(), None);
      Ok(())
    }
    Err(mut e) => {
      // Keep Timeout as Timeout; otherwise tag StopFailed so the
      // workflow's rolling-issue triage groups stop failures together.
      if !matches!(e.classification, FailureClass::Timeout) {
        e.classification = FailureClass::StopFailed;
      }
      report.step_mut(StepName::Stop).fail(
        started.elapsed(),
        Some(serde_json::json!({"reason": e.message.clone()})),
      );
      Err(e)
    }
  }
}

/// Step 6. `doctor --json` enumerates findings (orphan PIDs, stale
/// lockfiles, missing baseline). The run doesn't gate the verdict on
/// findings: a finding is informational. The report does record the
/// finding count so the maintainer can scan-down to investigate.
async fn step_doctor_postrun(
  plan: &LifecyclePlan,
  guard: &TempdirGuard,
  report: &mut UatReport,
) -> Result<(), StepError> {
  let started = Instant::now();
  let mut cmd = tokio::process::Command::new(&plan.llamastash_path);
  cmd.arg("--quiet").arg("doctor").arg("--json");
  guard.configure_command(cmd.as_std_mut());
  let output = match finalize_uat_command(&mut cmd).output().await {
    Ok(o) => o,
    Err(e) => {
      let err = StepError {
        message: format!("doctor_postrun: spawn failed: {e}"),
        exit_code: 1,
        classification: FailureClass::DoctorPostrunFailed,
        duration: started.elapsed(),
      };
      report.step_mut(StepName::DoctorPostrun).fail(
        err.duration,
        Some(serde_json::json!({"error": err.message.clone()})),
      );
      return Err(err);
    }
  };
  if !output.status.success() {
    let err = StepError {
      message: format!(
        "doctor_postrun: exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
      ),
      exit_code: output.status.code().unwrap_or(1),
      classification: FailureClass::DoctorPostrunFailed,
      duration: started.elapsed(),
    };
    report.step_mut(StepName::DoctorPostrun).fail(
      err.duration,
      Some(serde_json::json!({"error": err.message.clone()})),
    );
    return Err(err);
  }
  let parsed: serde_json::Value =
    serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
  let finding_count = parsed
    .get("findings")
    .and_then(|f| f.as_array())
    .map(|a| a.len())
    .unwrap_or(0);
  report.step_mut(StepName::DoctorPostrun).pass(
    started.elapsed(),
    Some(serde_json::json!({"finding_count": finding_count})),
  );
  Ok(())
}

/// Synthetic exit codes for steps that don't spawn a process (or
/// where the failure happens before / after the subprocess returns).
/// Crate-private — they are an internal protocol between the
/// orchestrator and the JSON report's `failure_summary.exit_code`,
/// documented for agent consumers in
/// `docs/testing/hardware-uat.md` §UAT synthetic exit codes.
/// `124` (timeout) and `130` (SIGINT) follow shell conventions for
/// scripted callers that branch on the exit code.
pub(crate) const PREFLIGHT_MISMATCH_CODE: i32 = 10;
pub(crate) const SMOKE_HTTP_ERROR_CODE: i32 = 11;
pub(crate) const SMOKE_PARSE_ERROR_CODE: i32 = 12;
pub(crate) const SMOKE_STATUS_ERROR_CODE: i32 = 13;
pub(crate) const TIMEOUT_CODE: i32 = 124;
pub(crate) const SIGINT_CODE: i32 = 130;

/// Step-failure carrier. Each step returns this on the unhappy path;
/// `record_failure` lifts it into the report's `failure_summary`.
#[derive(Debug, Clone)]
pub struct StepError {
  pub message: String,
  pub exit_code: i32,
  pub classification: FailureClass,
  pub duration: Duration,
}

fn record_failure(report: &mut UatReport, step: StepName, err: &StepError) {
  report.step_mut(step).fail(
    err.duration,
    Some(serde_json::json!({"error": err.message.clone()})),
  );
  report.failure_summary = Some(FailureSummary {
    step,
    classification: err.classification,
    exit_code: err.exit_code,
    message: err.message.clone(),
  });
}

/// Spawn a child, wait for it to exit, fail if its exit code is
/// non-zero, OR fail with a synthetic "timeout" message if it
/// exceeds the timeout. `kill_on_drop` ensures the child dies when
/// the timeout branch returns and the `Child` is dropped — UAT
/// children are expected to exit cleanly, and a stuck child should
/// fail loudly rather than hang the whole UAT.
async fn run_child_with_timeout(
  cmd: &mut tokio::process::Command,
  timeout: Duration,
) -> Result<Vec<u8>, StepError> {
  let started = Instant::now();
  let child = finalize_uat_command(cmd).spawn().map_err(|e| StepError {
    message: format!("spawn failed: {e}"),
    exit_code: 1,
    classification: FailureClass::Other,
    duration: started.elapsed(),
  })?;
  match tokio::time::timeout(timeout, child.wait_with_output()).await {
    Ok(Ok(output)) => check_status(&output, started.elapsed()),
    Ok(Err(e)) => Err(StepError {
      message: format!("wait failed: {e}"),
      exit_code: 1,
      classification: FailureClass::Other,
      duration: started.elapsed(),
    }),
    Err(_) => Err(StepError {
      message: format!("child exceeded {timeout:?}; killed"),
      exit_code: TIMEOUT_CODE,
      classification: FailureClass::Timeout,
      duration: started.elapsed(),
    }),
  }
}

fn check_status(output: &std::process::Output, elapsed: Duration) -> Result<Vec<u8>, StepError> {
  if output.status.success() {
    return Ok(output.stdout.clone());
  }
  let code = output.status.code().unwrap_or(1);
  let tail = String::from_utf8_lossy(&output.stderr);
  let snippet: String = tail
    .chars()
    .rev()
    .take(400)
    .collect::<String>()
    .chars()
    .rev()
    .collect();
  Err(StepError {
    message: format!("exit {code}: {snippet}"),
    exit_code: code,
    classification: FailureClass::Other,
    duration: elapsed,
  })
}

/// Map a child `init` exit code onto a structured classification.
/// `init` sets process-level codes from `exit_codes.rs` (72-74
/// inclusive cover its three failure modes); other codes fall back to
/// `InitOther` so the report doesn't claim a more specific class than
/// the orchestrator can defend.
fn classify_init_exit(code: i32) -> FailureClass {
  match code {
    73 => FailureClass::InitDownload,
    72 | 74 => FailureClass::InitInstall,
    TIMEOUT_CODE => FailureClass::Timeout,
    _ => FailureClass::InitOther,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_first_running_model_extracts_id_path_and_port() {
    // Mirrors the shape emitted by `src/ipc/methods.rs` for
    // status.models[] — `id` is a ModelId object (path + header
    // BLAKE3), not a flat string.
    let v = serde_json::json!({
      "models": [{
        "id": {
          "path": "/tmp/hf/hub/.../qwen2.5-0.5b.gguf",
          "header_blake3": "0".repeat(64)
        },
        "port": 8081
      }]
    });
    let (name, port) = parse_first_running_model(&v).unwrap();
    assert_eq!(name, "qwen2.5-0.5b");
    assert_eq!(port, 8081);
  }

  #[test]
  fn parse_first_running_model_errors_when_id_path_missing() {
    let v = serde_json::json!({"models": [{"id": {}, "port": 8081}]});
    let err = parse_first_running_model(&v).unwrap_err();
    assert!(err.contains("missing `id.path`"), "{err}");
  }

  #[test]
  fn parse_first_running_model_errors_on_empty_models() {
    let v = serde_json::json!({"models": []});
    let err = parse_first_running_model(&v).unwrap_err();
    assert!(err.contains("empty"), "{err}");
  }

  #[test]
  fn parse_first_running_model_errors_when_models_missing() {
    let v = serde_json::json!({});
    let err = parse_first_running_model(&v).unwrap_err();
    assert!(err.contains("missing"), "{err}");
  }

  #[test]
  fn parse_first_running_model_errors_on_oversize_port() {
    let v = serde_json::json!({
      "models": [{
        "id": {"path": "/m.gguf", "header_blake3": "0".repeat(64)},
        "port": 70000u64
      }]
    });
    let err = parse_first_running_model(&v).unwrap_err();
    assert!(err.contains("does not fit"), "{err}");
  }

  #[test]
  fn gpu_device_count_for_each_variant() {
    assert_eq!(gpu_device_count(&GpuInfo::CpuOnly), 0);
    assert_eq!(
      gpu_device_count(&GpuInfo::AppleMetal {
        total_memory_bytes: 1
      }),
      1
    );
    assert_eq!(gpu_device_count(&GpuInfo::Nvidia { devices: vec![] }), 0);
  }

  #[tokio::test]
  async fn run_child_with_timeout_succeeds_on_zero_exit() {
    let mut cmd = tokio::process::Command::new("true");
    let r = run_child_with_timeout(&mut cmd, Duration::from_secs(5)).await;
    assert!(r.is_ok(), "{r:?}");
  }

  #[tokio::test]
  async fn run_child_with_timeout_surfaces_nonzero_exit() {
    let mut cmd = tokio::process::Command::new("false");
    let err = run_child_with_timeout(&mut cmd, Duration::from_secs(5))
      .await
      .unwrap_err();
    assert_eq!(err.exit_code, 1);
  }

  #[tokio::test]
  async fn run_child_with_timeout_kills_on_timeout() {
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("10");
    let err = run_child_with_timeout(&mut cmd, Duration::from_millis(300))
      .await
      .unwrap_err();
    assert_eq!(err.exit_code, TIMEOUT_CODE);
    assert_eq!(err.classification, FailureClass::Timeout);
    assert!(err.message.contains("exceeded"), "{}", err.message);
  }

  #[test]
  fn parse_first_running_pid_extracts_positive_pid() {
    let v = serde_json::json!({"models": [{"name": "m", "port": 8081, "pid": 12345}]});
    assert_eq!(parse_first_running_pid(&v), Some(12345));
  }

  #[test]
  fn parse_first_running_pid_none_on_missing_or_invalid() {
    assert_eq!(parse_first_running_pid(&serde_json::json!({})), None);
    assert_eq!(
      parse_first_running_pid(&serde_json::json!({"models": []})),
      None
    );
    assert_eq!(
      parse_first_running_pid(&serde_json::json!({"models": [{"port": 1}]})),
      None
    );
    assert_eq!(
      parse_first_running_pid(&serde_json::json!({"models": [{"pid": 0}]})),
      None
    );
    assert_eq!(
      parse_first_running_pid(&serde_json::json!({"models": [{"pid": -42}]})),
      None
    );
  }

  #[test]
  fn classify_init_exit_maps_documented_codes() {
    assert_eq!(classify_init_exit(72), FailureClass::InitInstall);
    assert_eq!(classify_init_exit(73), FailureClass::InitDownload);
    assert_eq!(classify_init_exit(74), FailureClass::InitInstall);
    assert_eq!(classify_init_exit(TIMEOUT_CODE), FailureClass::Timeout);
    assert_eq!(classify_init_exit(1), FailureClass::InitOther);
  }

  #[test]
  fn check_size_deviation_passes_within_band() {
    let expected = 400 * 1024 * 1024_u64;
    assert!(check_size_deviation(expected, expected).is_none());
    assert!(check_size_deviation(expected, expected + expected / 20).is_none()); // +5%
    assert!(check_size_deviation(expected, expected - expected / 20).is_none());
    // -5%
  }

  #[test]
  fn check_size_deviation_warns_outside_band() {
    let expected = 400 * 1024 * 1024_u64;
    // +15% over the envelope.
    let actual_high = expected + (expected * 15 / 100);
    let w = check_size_deviation(expected, actual_high).expect("warn");
    assert!(w.contains("deviates"), "{w}");
    // -15% under the envelope.
    let actual_low = expected - (expected * 15 / 100);
    assert!(check_size_deviation(expected, actual_low).is_some());
  }

  #[test]
  fn check_size_deviation_short_circuits_on_zero_expected() {
    // expected=0 is a misconfiguration the deviation gate cannot
    // helpfully comment on; silently skip rather than synthesize a
    // 100% deviation warning.
    assert!(check_size_deviation(0, 1234).is_none());
  }

  #[test]
  fn parse_init_total_bytes_reads_model_total_bytes() {
    let stdout = serde_json::json!({"model": {"total_bytes": 420_000_000_u64}});
    let bytes = parse_init_total_bytes(stdout.to_string().as_bytes()).expect("present");
    assert_eq!(bytes, 420_000_000);
  }

  #[test]
  fn parse_init_total_bytes_returns_none_for_missing_or_unparseable() {
    assert_eq!(parse_init_total_bytes(b"not json"), None);
    assert_eq!(parse_init_total_bytes(b"{}"), None);
    assert_eq!(parse_init_total_bytes(b"{\"model\":{}}"), None);
  }

  #[test]
  fn parse_init_gguf_path_reads_first_files_entry() {
    let stdout = serde_json::json!({
      "model": {
        "files": ["/tmp/hf/hub/models--foo--bar/snapshots/abc/model.gguf"],
      }
    });
    let path = parse_init_gguf_path(stdout.to_string().as_bytes()).expect("present");
    assert_eq!(
      path,
      PathBuf::from("/tmp/hf/hub/models--foo--bar/snapshots/abc/model.gguf")
    );
  }

  #[test]
  fn parse_init_gguf_path_returns_none_for_missing_or_empty() {
    assert_eq!(parse_init_gguf_path(b"not json"), None);
    assert_eq!(parse_init_gguf_path(b"{}"), None);
    assert_eq!(parse_init_gguf_path(b"{\"model\":{}}"), None);
    assert_eq!(parse_init_gguf_path(b"{\"model\":{\"files\":[]}}"), None);
    // Non-string entry (e.g. someone serialized as an object) is also
    // unusable — return None rather than coercing to a Debug string.
    assert_eq!(
      parse_init_gguf_path(b"{\"model\":{\"files\":[{\"path\":\"/x\"}]}}"),
      None
    );
  }

  #[test]
  fn parse_model_state_accepts_flat_status_json_shape() {
    let row = serde_json::json!({"state": "ready"});
    assert_eq!(parse_model_state(&row), Some("ready"));
    assert_eq!(parse_model_error_cause(&row), None);
  }

  #[test]
  fn parse_model_state_still_accepts_legacy_nested_shape() {
    let row = serde_json::json!({"state": {"state": "error", "cause": "oom"}});
    assert_eq!(parse_model_state(&row), Some("error"));
    assert_eq!(parse_model_error_cause(&row), Some("oom"));
  }
}
