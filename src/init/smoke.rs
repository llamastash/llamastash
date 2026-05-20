//! Smoke launch (R70 / R71 / R79) + TUI handoff.
//!
//! v2 ships two layered checks:
//! - **Phase 1 — pure-function dry-run**: estimate peak memory at the
//!   chosen ctx (using Unit 6's `estimate_peak_bytes`) and compare
//!   against the effective ceiling for the host. Surfaces a tight-fit
//!   warning when peak is within 10% of the safety ceiling.
//! - **`--version` probe**: spawn the installed binary with `env_clear()`
//!   and a minimal env (PATH / HOME / USER / LANG) so `LLAMA_ARG_*`
//!   and `HF_TOKEN` can't leak into the child. A non-zero exit fails
//!   smoke with `INIT_SMOKE_FAILED = 74`.
//!
//! Phase 2 (daemon-mediated `/health` + `/v1/chat/completions` probe)
//! is intentionally deferred: it requires daemon stop+restart
//! plumbing that lives across `src/daemon/mod.rs` and `src/cli/daemon.rs`,
//! and the failure-mode tree is large enough that v2 ships the
//! detection-only path while v2.1 lands the full probe. Phase 1 +
//! `--version` covers the common-case "I installed the wrong variant"
//! / "I downloaded a corrupt binary" failures; the missing piece is
//! the genuine OOM-at-load case which Unit 6's filter already aims
//! to prevent.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::init::detection::HardwareSnapshot;
use crate::init::recommender::estimate_peak_bytes;

/// Smoke phase outcome — `Ok(SmokeReport)` means the binary at least
/// runs and the memory plan looks reasonable; `Err(SmokeFailure)`
/// maps to `INIT_SMOKE_FAILED = 74` at the wizard layer.
#[derive(Debug, Clone)]
pub struct SmokeReport {
  pub ok: bool,
  pub warning: Option<SmokeWarning>,
  pub binary_version: Option<String>,
  pub peak_estimate_bytes: u64,
  pub effective_ceiling_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmokeWarning {
  /// Phase 1: estimated peak is within 10% of the safety ceiling.
  /// Wizard prompts for confirm under interactive mode; auto-yes
  /// under `--yes`.
  VramTight { peak_bytes: u64, ceiling_bytes: u64 },
  /// Phase 1: no GPU detected; user is running CPU only. Surface
  /// so the handoff message can suggest a smaller model if the user
  /// picked something heavy.
  CpuOnly,
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum SmokeFailure {
  #[error("`llama-server --version` exited with status {0}; the binary likely doesn't match this host (arch/variant mismatch?)")]
  VersionProbeNonZero(i32),
  #[error(
    "`llama-server --version` did not run within {0:?}; the binary may be hanging on startup"
  )]
  VersionProbeTimeout(Duration),
  #[error("could not spawn `{path}`: {error}")]
  Spawn { path: String, error: String },
  #[error("estimated peak {peak_bytes} bytes exceeds effective ceiling {ceiling_bytes} bytes; load would OOM")]
  PhaseOneOom { peak_bytes: u64, ceiling_bytes: u64 },
}

/// Phase 1 dry-run. Pure function — no I/O, no spawn.
pub fn phase_one(
  hardware: &HardwareSnapshot,
  weights_bytes: u64,
  ctx: u32,
) -> Result<Option<SmokeWarning>, SmokeFailure> {
  let peak = estimate_peak_bytes(weights_bytes, ctx);
  let ceiling = hardware
    .vram_bytes
    .map(|v| (v as f64 * crate::init::recommender::SAFETY_MARGIN) as u64);
  if let Some(ceiling_bytes) = ceiling {
    if peak > ceiling_bytes {
      return Err(SmokeFailure::PhaseOneOom {
        peak_bytes: peak,
        ceiling_bytes,
      });
    }
    let margin = ceiling_bytes.saturating_sub(peak);
    if margin < (ceiling_bytes / 10) {
      return Ok(Some(SmokeWarning::VramTight {
        peak_bytes: peak,
        ceiling_bytes,
      }));
    }
    return Ok(None);
  }
  // No VRAM: emit the CPU-only warning so the handoff hint kicks in.
  Ok(Some(SmokeWarning::CpuOnly))
}

/// Run `llama-server --version` with `env_clear()` + a minimal allow-
/// list. Returns the trimmed version string on success.
///
/// Spawn-poll-drain mechanics are shared with the GPU probes and
/// `brew install` via [`crate::util::process::run_with_drain_and_timeout`].
pub fn version_probe(binary: &Path, timeout: Duration) -> Result<String, SmokeFailure> {
  let envs: Vec<(OsString, OsString)> = ["PATH", "HOME", "USER", "LANG"]
    .iter()
    .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
    .collect();
  let mut cmd = Command::new(binary);
  cmd.arg("--version").env_clear();
  for (k, v) in &envs {
    cmd.env(k, v);
  }
  let out = crate::util::process::run_with_drain_and_timeout(cmd, timeout).map_err(|e| {
    use crate::util::process::RunError;
    match e {
      RunError::Spawn(e) => SmokeFailure::Spawn {
        path: binary.display().to_string(),
        error: e.to_string(),
      },
      RunError::Timeout { after } => SmokeFailure::VersionProbeTimeout(after),
      RunError::Wait(e) => SmokeFailure::Spawn {
        path: binary.display().to_string(),
        error: e.to_string(),
      },
    }
  })?;
  if !out.status.success() {
    return Err(SmokeFailure::VersionProbeNonZero(
      out.status.code().unwrap_or(-1),
    ));
  }
  let stdout = String::from_utf8_lossy(&out.stdout);
  let stderr = String::from_utf8_lossy(&out.stderr);
  let combined = format!("{stdout}{stderr}");
  Ok(extract_version(&combined))
}

fn extract_version(output: &str) -> String {
  // llama-server --version output format (as of llama.cpp b5037+) is
  //   version: 5037 (b00d09c)
  //   built with cc (...) for x86_64-pc-linux-gnu
  //
  // Older bottle builds and some forks emit a `bNNNN` token directly
  // without the `version:` prefix. Try, in order:
  //   1. The token after `version:` plus a parenthesised hash if present
  //      ("5037 (b00d09c)" → "build 5037 (b00d09c)")
  //   2. A standalone `bNNNN` token (legacy format)
  //   3. The first non-empty line, verbatim
  // Step 3 guarantees we always return something useful instead of an
  // empty string or just `version:`.
  for line in output.lines() {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("version:") {
      let rest = rest.trim();
      if !rest.is_empty() {
        return format!("build {rest}");
      }
    }
  }
  for line in output.lines() {
    for tok in line.split_whitespace() {
      let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
      if t.len() >= 4 && t.starts_with('b') && t[1..].chars().all(|c| c.is_ascii_digit()) {
        return t.to_string();
      }
    }
  }
  output
    .lines()
    .find(|l| !l.trim().is_empty())
    .map(|l| l.trim().to_string())
    .unwrap_or_else(|| "unknown".to_string())
}

/// Default version-probe timeout. Modest — `--version` should return
/// in <100 ms even on slow disks; anything beyond 5 s strongly
/// suggests the binary is broken.
pub const DEFAULT_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Compose phase-1 + `--version` probe into a single smoke pass.
/// Returns a `SmokeReport` the wizard threads into its summary;
/// failures map to `INIT_SMOKE_FAILED` at the CLI boundary.
pub fn run_phase_one_and_version(
  binary: &Path,
  hardware: &HardwareSnapshot,
  weights_bytes: u64,
  ctx: u32,
) -> Result<SmokeReport, SmokeFailure> {
  log::debug!(
    "smoke: phase-1 inputs: binary={}, weights_bytes={}, ctx={}, vram_bytes={:?}",
    binary.display(),
    weights_bytes,
    ctx,
    hardware.vram_bytes
  );
  let warning = phase_one(hardware, weights_bytes, ctx)?;
  log::debug!("smoke: phase-1 result: warning={warning:?}");
  log::debug!(
    "smoke: --version probe: spawning `{} --version` (timeout {:?})",
    binary.display(),
    DEFAULT_VERSION_PROBE_TIMEOUT
  );
  let version = version_probe(binary, DEFAULT_VERSION_PROBE_TIMEOUT)?;
  log::debug!("smoke: --version probe returned: {version:?}");
  let ceiling = hardware
    .vram_bytes
    .map(|v| (v as f64 * crate::init::recommender::SAFETY_MARGIN) as u64);
  let peak = estimate_peak_bytes(weights_bytes, ctx);
  log::debug!("smoke: peak_estimate={peak}, effective_ceiling={ceiling:?}");
  Ok(SmokeReport {
    ok: true,
    warning,
    binary_version: Some(version),
    peak_estimate_bytes: peak,
    effective_ceiling_bytes: ceiling,
  })
}

/// Render the handoff line the wizard prints after a successful run.
/// Interactive runs render this then ask "Launch TUI now? [Y/n]" (UX
/// handled in Unit 10's `print_handoff`); non-interactive runs just
/// print it as the last line.
pub fn render_handoff_line() -> String {
  "Run `llamadash` to enter the TUI, or `llamadash list` to see discovered models.".to_string()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::{GpuDevice, GpuInfo};
  use crate::init::detection::{CpuArch, OsFamily};

  fn nvidia(vram_gb: f64) -> HardwareSnapshot {
    let bytes = (vram_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    HardwareSnapshot {
      gpu: GpuInfo::Nvidia {
        devices: vec![GpuDevice {
          name: "test".into(),
          total_memory_bytes: bytes,
          used_memory_bytes: 0,
          utilization_pct: None,
          temperature_c: None,
        }],
      },
      vram_bytes: Some(bytes),
      gpu_device_count: 1,
      ram_total_bytes: 64 * 1024 * 1024 * 1024,
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  fn cpu(ram_gb: f64) -> HardwareSnapshot {
    HardwareSnapshot {
      gpu: GpuInfo::CpuOnly,
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: (ram_gb * 1024.0 * 1024.0 * 1024.0) as u64,
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  #[test]
  fn phase_one_passes_comfortable_fit() {
    let hw = nvidia(24.0);
    let result = phase_one(&hw, 4_700_000_000, 4096).unwrap();
    assert!(result.is_none(), "comfortable fit should have no warning");
  }

  #[test]
  fn phase_one_warns_when_within_10pct_of_ceiling() {
    // 24 GiB → ceiling ~21.6 GiB. Pick weights that produce a peak
    // within the 10% margin (peak ≥ ceiling - ceiling/10 ≈ 19.44 GiB).
    // peak(w, 16384) = w × 1.2 + w × 0.15 × 4 = w × 1.8 → target
    // w ≥ 19.44 / 1.8 GiB ≈ 10.8 GiB. Use 11.7 GiB so we're
    // comfortably inside the warning band.
    let hw = nvidia(24.0);
    let weights = (11.7 * 1024.0 * 1024.0 * 1024.0) as u64;
    let result = phase_one(&hw, weights, 16384).unwrap();
    assert!(
      matches!(result, Some(SmokeWarning::VramTight { .. })),
      "expected VramTight, got {result:?}"
    );
  }

  #[test]
  fn phase_one_oom_when_peak_exceeds_ceiling() {
    let hw = nvidia(8.0);
    // A 13B Q4 at 16k won't fit in 8 GB.
    let err = phase_one(&hw, 8_000_000_000, 16384).unwrap_err();
    assert!(matches!(err, SmokeFailure::PhaseOneOom { .. }));
  }

  #[test]
  fn phase_one_cpu_only_emits_cpu_warning() {
    let hw = cpu(16.0);
    let result = phase_one(&hw, 1_500_000_000, 4096).unwrap();
    assert_eq!(result, Some(SmokeWarning::CpuOnly));
  }

  #[test]
  fn extract_version_parses_llama_cpp_modern_format() {
    // Current `llama-server --version` output as of llama.cpp b5037+.
    let out = "version: 5037 (b00d09c)\nbuilt with cc (Debian 12.2.0-14) 12.2.0 for x86_64-pc-linux-gnu\n";
    assert_eq!(extract_version(out), "build 5037 (b00d09c)");
  }

  #[test]
  fn extract_version_finds_bnnnn_token_legacy_format() {
    // Older bottle builds and forks may not emit `version:` prefix.
    let out = "llama.cpp b9219 build_type=cpu\n";
    assert_eq!(extract_version(out), "b9219");
  }

  #[test]
  fn extract_version_falls_back_to_first_non_empty_line() {
    // Fallback returns the whole first non-empty line rather than the
    // first whitespace-delim token — gives the user something
    // recognisable instead of a fragment.
    let out = "\nllama-server build 1.2.3\nbuilt with ...\n";
    assert_eq!(extract_version(out), "llama-server build 1.2.3");
  }

  #[test]
  fn extract_version_returns_unknown_when_output_is_empty() {
    assert_eq!(extract_version(""), "unknown");
    assert_eq!(extract_version("\n\n  \n"), "unknown");
  }

  #[test]
  fn version_probe_returns_error_when_binary_missing() {
    let result = version_probe(
      Path::new("/nonexistent/path/to/llama-server"),
      Duration::from_secs(1),
    );
    assert!(matches!(result, Err(SmokeFailure::Spawn { .. })));
  }

  #[test]
  fn version_probe_runs_on_a_real_executable() {
    // `/bin/echo` is the standard always-present executable on a
    // POSIX host; it'll exit 0 with no `bNNNN` token in the output,
    // exercising the fallback path of `extract_version`.
    #[cfg(unix)]
    {
      let echo = std::path::Path::new("/bin/echo");
      if echo.exists() {
        let result = version_probe(echo, Duration::from_secs(2));
        assert!(
          result.is_ok(),
          "echo --version should exit 0, got {result:?}"
        );
      }
    }
  }

  #[test]
  fn version_probe_captures_stdout_for_bnnnn_extraction() {
    // Regression: the old code path used try_wait() then
    // wait_with_output() which dropped stdout on the floor for
    // already-reaped children. This test asserts the version
    // extractor sees an actual stdout-emitted bNNNN token, which
    // requires the reader-thread draining to be wired correctly.
    #[cfg(unix)]
    {
      // Use `/bin/sh -c 'echo version b9999'` via a wrapper script
      // path. We can't directly target /bin/sh because version_probe
      // hard-codes the `--version` arg. Instead, write a tiny shell
      // script to a temp file and call that.
      use std::os::unix::fs::PermissionsExt;
      let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
      let dir = std::env::temp_dir().join(format!(
        "llamadash-version-probe-{}-{nanos}",
        std::process::id()
      ));
      std::fs::create_dir_all(&dir).unwrap();
      let script = dir.join("fake-llama-server");
      std::fs::write(&script, "#!/bin/sh\necho 'version: b9999 cpu'\n").unwrap();
      std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
      let version = version_probe(&script, Duration::from_secs(2)).expect("probe");
      assert_eq!(version, "build b9999 cpu");
      std::fs::remove_dir_all(&dir).ok();
    }
  }

  #[test]
  fn version_probe_strips_hf_token_from_child_env() {
    // R70 (env_clear allowlist) — HF_TOKEN must not appear in the
    // child's environment. Smoke test: spawn a script that echoes
    // its HF_TOKEN env var; assert the captured output shows the
    // var is unset.
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
      let dir = std::env::temp_dir().join(format!(
        "llamadash-env-probe-{}-{nanos}",
        std::process::id()
      ));
      std::fs::create_dir_all(&dir).unwrap();
      let script = dir.join("env-probe");
      std::fs::write(
        &script,
        // Print 'leaked' if any of these are set in the child env.
        "#!/bin/sh\n\
         if [ -n \"$HF_TOKEN\" ] || [ -n \"$HUGGING_FACE_HUB_TOKEN\" ] || \
            [ -n \"$LLAMA_ARG_HOST\" ] || [ -n \"$LLAMA_ARG_CTX\" ]; then \
           echo leaked; else echo b1 clean; fi\n",
      )
      .unwrap();
      std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
      // Set the env vars in the parent — they must NOT propagate to the
      // child because version_probe uses env_clear().
      std::env::set_var("HF_TOKEN", "hf_secret_value");
      std::env::set_var("LLAMA_ARG_HOST", "0.0.0.0");
      let result = version_probe(&script, Duration::from_secs(2));
      std::env::remove_var("HF_TOKEN");
      std::env::remove_var("LLAMA_ARG_HOST");
      let version = result.expect("probe");
      // Exact-string check is brittle; the contract is "no leak", so
      // assert positively (script's clean-env output appears) and
      // negatively (the leaked sentinel never does).
      assert!(
        version.contains("clean"),
        "env_clear allowlist must strip HF_TOKEN / LLAMA_ARG_*; got {version:?}"
      );
      assert!(
        !version.contains("leaked"),
        "expected no env leakage; got {version:?}"
      );
      std::fs::remove_dir_all(&dir).ok();
    }
  }

  #[test]
  fn render_handoff_line_mentions_tui_and_list() {
    let s = render_handoff_line();
    assert!(s.contains("llamadash"));
    assert!(s.contains("TUI") || s.contains("list"));
  }
}
