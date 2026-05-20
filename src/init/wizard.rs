//! `llamastash init` orchestration (R48 / R49 / R50 / R72 / R76 / R77).
//!
//! Six-step flow:
//!   1. detect (hardware + binary) — always runs.
//!   2. install — `--only` / `--skip` gated.
//!   3. model recommend + download — `--only` / `--skip` gated.
//!   4. config write — `--only` / `--skip` gated.
//!   5. smoke launch — `--only` / `--skip` gated (Unit 12 owns the body).
//!   6. handoff — always runs.
//!
//! `--recommended` (or the hidden `--yes` alias) short-circuits every
//! prompt to its hardware-aware default. Three per-step value flags
//! (`--install`, `--model`, `--config-step`) pre-answer individual
//! prompts without skipping the rest of the wizard. `--json` emits a
//! single summary on completion; per-step progress goes to stderr at
//! `--verbose`. `--offline` constructs an offline `FetchClient`;
//! steps that need network mark themselves "skipped" and the user
//! gets an actionable hint.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::cli::cli_args::{Cli, InitArgs, InitStep};
use crate::cli::colors;
use crate::cli::exit_codes::{CliExit, CliResult, INIT_ABORTED, INIT_SMOKE_FAILED, UNKNOWN};
use crate::config::Config;
use crate::init::benchmark::load_bundled;
use crate::init::detection::{
  detect_binary, detect_hardware, BinaryPresence, DetectBinaryInputs, HardwareSnapshot,
};
use crate::init::fetch::{build_with_offline_check, FetchClient, FetchClientConfig};
use crate::init::install::{
  default_install_method, gh_releases, BinaryInstall, InstallChoice, InstallError,
};
use crate::init::prompts::{self, ModelChoice};
use crate::init::recommender::{recommend, OnDiskModel, RecommendOptions, Recommendation};
use crate::init::snapshot::{self, InstallMethod, ManagedKey};

/// Effective per-step run plan after `--only`/`--skip` resolve. Step 1
/// (detect) and step 6 (handoff) always run, so they're not in the
/// matrix — only the three middle steps gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepPlan {
  pub server: bool,
  pub models: bool,
  pub config: bool,
}

impl StepPlan {
  /// Resolve the plan from `--only` / `--skip` per the matrix in the
  /// plan's "init/doctor mode/flag decision matrix".
  pub fn resolve(only: &[InitStep], skip: &[InitStep]) -> Self {
    if !only.is_empty() {
      let on = |s: InitStep| only.contains(&s);
      return Self {
        server: on(InitStep::Server),
        models: on(InitStep::Models),
        config: on(InitStep::Config),
      };
    }
    if !skip.is_empty() {
      let off = |s: InitStep| skip.contains(&s);
      return Self {
        server: !off(InitStep::Server),
        models: !off(InitStep::Models),
        config: !off(InitStep::Config),
      };
    }
    Self {
      server: true,
      models: true,
      config: true,
    }
  }
}

/// Schema version for `init --json`. Bump on breaking shape changes so
/// agent consumers can version-gate their parsers. Matches the
/// `DoctorReport.schema_version` contract.
pub const INIT_JSON_SCHEMA_VERSION: u32 = 1;

/// `init`'s end-of-run summary the wizard prints (and the `--json`
/// shape downstream agents parse).
///
/// **safe_to_log policy (plan Key Decision §init --json output
/// redaction allowlist):** several fields contain host-specific
/// paths or content digests that are *not* safe to paste into a
/// public bug report:
///
/// - `install.path` (filesystem path to llama-server)
/// - `install.digest` (binary content hash)
/// - `model.files[]` (filesystem paths to GGUFs in the HF cache)
/// - `hardware.os` / `hardware.arch` are fine; `hardware.vram_bytes`
///   is fine.
///
/// `safe_to_log: false` at the top level signals to agent consumers
/// that the JSON contains host-identifying data and must be
/// stripped before public emission. Future v2.1 may add a
/// per-field annotation, but for v2 the document-level flag is the
/// contract.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InitSummary {
  /// Schema version. Currently `1` (R77). Agents should compare
  /// against `INIT_JSON_SCHEMA_VERSION` they were built against.
  pub schema_version: u32,
  /// Document-level safe-to-log classification. See struct doc.
  pub safe_to_log: bool,
  pub steps_ran: Vec<&'static str>,
  pub steps_skipped: Vec<&'static str>,
  pub install: Option<InstallSummary>,
  pub model: Option<ModelSummary>,
  pub config: Option<ConfigSummary>,
  pub smoke: Option<SmokeSummary>,
  pub hardware: HardwareSummary,
  pub offline: bool,
  /// Top-N model recommendations the models step computed against the
  /// active benchmark snapshot. Populated whenever the models step
  /// runs; empty otherwise. Lets `init --only models --json` act as a
  /// "what would you suggest" listing without downloading anything —
  /// makes the output directly comparable to `whichllm --json`.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub recommendations: Vec<Recommendation>,
  /// `Some(true)` when a remote benchmark snapshot fetch succeeded
  /// and verified this run; `Some(false)` when it was attempted and
  /// failed (counter +1); `None` when no attempt was made (offline
  /// mode, or models step skipped). Consumed only by
  /// `persist_init_snapshot` for the `remote_fetch_failures`
  /// counter that backs doctor's `RemoteSnapshotUnreachable`
  /// finding — never emitted in `init --json`.
  #[serde(skip)]
  pub remote_snapshot_attempt: Option<bool>,
}

impl Default for InitSummary {
  fn default() -> Self {
    Self {
      schema_version: INIT_JSON_SCHEMA_VERSION,
      safe_to_log: false,
      steps_ran: Vec::new(),
      steps_skipped: Vec::new(),
      install: None,
      model: None,
      config: None,
      smoke: None,
      hardware: HardwareSummary::default(),
      offline: false,
      recommendations: Vec::new(),
      remote_snapshot_attempt: None,
    }
  }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InstallSummary {
  pub method: String,
  pub path: PathBuf,
  pub digest: String,
  pub version: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelSummary {
  pub repo: String,
  pub files: Vec<PathBuf>,
  pub total_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigSummary {
  pub path: PathBuf,
  pub written_bytes: u64,
  pub managed_keys: Vec<String>,
  /// Per-path digest records (value bytes + wrote_at). Skipped in
  /// `init --json` because per-key digests are *not* in the
  /// safe_to_log allowlist (plan Key Decision §init --json output
  /// redaction allowlist); they are consumed only by
  /// `persist_init_snapshot` to populate `_init_snapshot.json.
  /// managed_keys` per R72.
  #[serde(skip)]
  pub managed_records: Vec<ManagedKey>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SmokeSummary {
  pub ok: bool,
  pub note: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HardwareSummary {
  pub gpu_backend: String,
  pub vram_bytes: Option<u64>,
  pub ram_bytes: u64,
  pub os: String,
  pub arch: String,
}

/// Entry point invoked by `cli::init::handle`. Returns the structured
/// exit code; the dispatcher prints any failure message to stderr.
pub async fn run(args: InitArgs, cli: &Cli, config: &Config) -> CliResult {
  let plan = StepPlan::resolve(&args.only, &args.skip);
  log::debug!(
    "init: plan resolved (server={}, models={}, config={}); flags: recommended={}, offline={}, json={}",
    plan.server,
    plan.models,
    plan.config,
    prompts::is_recommended(&args),
    args.offline,
    args.json,
  );

  // Refuse up-front when `--offline` cannot satisfy `--only models`:
  // step 3 (models) has no offline fallback, and a mid-step abort with
  // `INIT_DOWNLOAD_FAILED` would mis-classify the failure for agent
  // consumers (which is "init aborted before substantive work", not
  // "network op failed during the run"). Plan Unit 10 test scenario.
  if args.offline && plan.models && !plan.server && !plan.config {
    return Err(CliExit::new(
      INIT_ABORTED,
      "init: cannot satisfy `--only models` with `--offline` — disable `--offline` or drop `--only models`".to_string(),
    ));
  }

  // Step 1: detection.
  let hardware = detect_hardware();
  let mut summary = InitSummary {
    hardware: render_hardware(&hardware),
    offline: args.offline,
    ..InitSummary::default()
  };
  summary.steps_ran.push("detect");
  // cliclack intro panel for human-readable runs; JSON callers bypass.
  if !args.json {
    prompts::intro(&hardware);
  }
  // Per-step value flags pointed at a skipped step: emit a single
  // stderr warning and proceed. Keeps `--only`/`--skip` and the
  // override flags' axes independent (W4).
  //
  // Gated on `!args.json` so the human-readable warning text doesn't
  // mix into the structured stderr stream agents capture alongside
  // stdout. The flag still functions as documented; the warning is
  // a human affordance.
  if !args.json {
    warn_on_ignored_step_overrides(&args, &plan);
    // Single consolidated non-TTY warning. Fires once at the top
    // rather than per-picker so an agent piping stdout sees one
    // clear message even when multiple steps fall back to defaults.
    // Suppressed when every in-plan step already has an explicit
    // override or `--recommended` is set.
    let any_step_lacks_override = (plan.server && args.install.is_none())
      || (plan.models && args.model.is_none())
      || (plan.config && args.config_choice.is_none());
    if !prompts::is_recommended(&args)
      && any_step_lacks_override
      && !std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
      eprintln!(
        "{}",
        colors::warning(
          "stdout is not a terminal. Install + model steps will use recommended defaults; \
           the config step needs --config-step write|skip (or --recommended). \
           Pre-answer specific steps with --install / --model / --config-step."
        )
      );
    }
  }

  // Thread the same flag/env/config the daemon does (see
  // `cli/daemon.rs::resolved_inputs`) so `--llama-server <path>` and
  // `LLAMASTASH_LLAMA_SERVER` hint the wizard's existing-install probe.
  // Without this the wizard probes only PATH + common locations and
  // silently misses a binary the user explicitly pointed at.
  let binary = detect_binary(DetectBinaryInputs {
    cli_flag: cli.llama_server.clone(),
    env_var: std::env::var_os("LLAMASTASH_LLAMA_SERVER"),
    config_path: config.llama_server_path.clone(),
  });

  let fetch = match build_with_offline_check(args.offline, FetchClientConfig::default()) {
    Ok(c) => c,
    Err(e) => {
      return Err(CliExit::new(
        INIT_ABORTED,
        format!("init: fetch client: {e}"),
      ))
    }
  };

  // Step 2: install.
  let install: Option<BinaryInstall> = if plan.server {
    summary.steps_ran.push("server");
    match run_install_step(&args, &fetch, &hardware, &binary).await {
      Ok(install) => {
        summary.install = Some(InstallSummary {
          method: install_method_label(install.method).to_string(),
          path: install.path.clone(),
          digest: install.digest.clone(),
          version: install.version.clone(),
        });
        Some(install)
      }
      Err(e) => return Err(e),
    }
  } else {
    summary.steps_skipped.push("server");
    None
  };

  // Step 3 + 5: model pick + download. Combined here per the plan's
  // "steps 3 + 5 (models)" matrix.
  let model_summary: Option<ModelSummary> = if plan.models {
    summary.steps_ran.push("models");
    let outcome = run_models_step(&args, &fetch, &hardware).await?;
    summary.remote_snapshot_attempt = outcome.remote_snapshot_attempt;
    summary.recommendations = outcome.recommendations;
    Some(outcome.model)
  } else {
    summary.steps_skipped.push("models");
    None
  };
  summary.model = model_summary.clone();

  // Step 4: config write.
  if plan.config {
    summary.steps_ran.push("config");
    match run_config_step(&args, install.as_ref(), &hardware).await {
      Ok(Some(c)) => summary.config = Some(c),
      // User declined the confirm or `--config-step skip` was set:
      // record the step as skipped so the summary reflects reality.
      Ok(None) => {
        summary.steps_ran.retain(|s| *s != "config");
        summary.steps_skipped.push("config");
      }
      Err(e) => {
        // Config step failed (e.g. the non-TTY explicit-consent
        // refusal in `confirm_config_write`). The install + model
        // steps already wrote durable state to disk; persist the
        // init snapshot here so `doctor` sees the partial baseline
        // instead of reporting nothing. Without this the binary +
        // downloaded model live on disk but are invisible to
        // subsequent diagnostic runs.
        summary.steps_ran.retain(|s| *s != "config");
        summary.steps_skipped.push("config");
        if let Err(persist_err) = persist_init_snapshot(&hardware, install.as_ref(), &summary) {
          log::warn!(
            "init: failed to persist init_snapshot.json after config-step error: {persist_err}"
          );
        }
        return Err(e);
      }
    }
  } else {
    summary.steps_skipped.push("config");
  }

  // Persist init_snapshot.json. Best-effort: a write failure logs but
  // doesn't abort the run (doctor will rebuild from re-detection).
  if let Err(e) = persist_init_snapshot(&hardware, install.as_ref(), &summary) {
    log::warn!("init: failed to persist init_snapshot.json: {e}");
  }

  // Step 5: smoke launch (Unit 12). Phase 1 + --version probe runs
  // whenever both an install and a downloaded model are present;
  // otherwise we emit an honest "skipped" note.
  let smoke = run_smoke_step(
    install.as_ref(),
    summary.model.as_ref(),
    &hardware,
    !args.json,
  );
  let smoke_ok = smoke.ok;
  let smoke_note = smoke.note.clone();
  summary.smoke = Some(smoke);
  summary.steps_ran.push("smoke");

  // Step 6: handoff. Always render the summary first so agent
  // consumers (and humans) see what landed even when smoke failed —
  // the binary + model + config writes are durable and worth
  // reporting before we exit non-zero.
  summary.steps_ran.push("handoff");
  print_handoff(&summary, args.json);

  // R78: smoke failures map to INIT_SMOKE_FAILED (74) so agents can
  // branch on the exit code without parsing the JSON. The earlier
  // steps (install / download / config) already succeeded if we
  // reached this point — re-running smoke alone is the right
  // remediation, which is what the message hints at.
  if !smoke_ok {
    return Err(CliExit::new(
      INIT_SMOKE_FAILED,
      format!("init smoke: {smoke_note}"),
    ));
  }

  Ok(())
}

fn run_smoke_step(
  install: Option<&BinaryInstall>,
  model: Option<&ModelSummary>,
  hardware: &HardwareSnapshot,
  emit_progress: bool,
) -> SmokeSummary {
  let Some(install) = install else {
    return SmokeSummary {
      ok: true,
      note: "smoke skipped: no install in this run".into(),
    };
  };
  let Some(model) = model else {
    return SmokeSummary {
      ok: true,
      note: "smoke skipped: no model downloaded this run".into(),
    };
  };
  let weights_bytes = model
    .files
    .first()
    .and_then(|p| std::fs::metadata(p).ok())
    .map(|m| m.len())
    .unwrap_or(model.total_bytes);
  log::debug!(
    "init: smoke step inputs: binary={}, weights_bytes={}, ctx={}",
    install.path.display(),
    weights_bytes,
    crate::init::recommender::DEFAULT_CTX
  );
  let binary_label = install
    .path
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("llama-server")
    .to_string();
  let sp = prompts::StepProgress::start_if(
    emit_progress,
    format!("Probing {binary_label}: dry-run memory check (phase-1) + `--version` exec"),
  );
  match crate::init::smoke::run_phase_one_and_version(
    &install.path,
    hardware,
    weights_bytes,
    crate::init::recommender::DEFAULT_CTX,
  ) {
    Ok(report) => {
      let version_str = report
        .binary_version
        .clone()
        .unwrap_or_else(|| "unknown".into());
      let peak_gib = report.peak_estimate_bytes as f64 / 1_073_741_824.0;
      let ceiling_str = report
        .effective_ceiling_bytes
        .map(|c| format!("{:.1} GiB", c as f64 / 1_073_741_824.0))
        .unwrap_or_else(|| "CPU-only".into());
      let note = match report.warning {
        Some(crate::init::smoke::SmokeWarning::VramTight {
          peak_bytes,
          ceiling_bytes,
        }) => format!(
          "tight fit: peak ~{:.1} GiB vs ceiling ~{:.1} GiB (<10% headroom); {binary_label} reports {version_str}",
          peak_bytes as f64 / 1_073_741_824.0,
          ceiling_bytes as f64 / 1_073_741_824.0,
        ),
        Some(crate::init::smoke::SmokeWarning::CpuOnly) => format!(
          "CPU-only host (no VRAM detected); peak estimate ~{peak_gib:.1} GiB, expect lower tok/s than a VRAM-resident run; {binary_label} reports {version_str}"
        ),
        None => format!(
          "phase-1 fits (peak ~{peak_gib:.1} GiB vs ceiling {ceiling_str}); {binary_label} reports {version_str}"
        ),
      };
      log::debug!("init: smoke step succeeded: {note}");
      sp.success(format!("Smoke probe OK — {note}"));
      SmokeSummary { ok: true, note }
    }
    Err(failure) => {
      let note = format!("{failure}");
      log::debug!("init: smoke step failed: {note}");
      sp.fail(format!("Smoke probe failed: {note}"));
      SmokeSummary { ok: false, note }
    }
  }
}

fn install_method_label(method: InstallMethod) -> &'static str {
  match method {
    InstallMethod::GhReleases => "gh_releases",
    InstallMethod::Brew => "brew",
    InstallMethod::CustomPath => "custom_path",
  }
}

fn render_hardware(hw: &HardwareSnapshot) -> HardwareSummary {
  HardwareSummary {
    gpu_backend: hw.gpu.label().to_string(),
    vram_bytes: hw.vram_bytes,
    ram_bytes: hw.ram_total_bytes,
    os: format!("{:?}", hw.os).to_lowercase(),
    arch: format!("{:?}", hw.cpu_arch).to_lowercase(),
  }
}

/// Emit a single stderr line per per-step flag pointed at a step the
/// `--only`/`--skip` plan excludes. The flag is still parsed and
/// recorded; we just tell the user it's a no-op for this run rather
/// than silently dropping it (W4).
fn warn_on_ignored_step_overrides(args: &InitArgs, plan: &StepPlan) {
  if args.install.is_some() && !plan.server {
    eprintln!(
      "{}",
      colors::warning("--install ignored because the server step is skipped")
    );
  }
  if args.model.is_some() && !plan.models {
    eprintln!(
      "{}",
      colors::warning("--model ignored because the models step is skipped")
    );
  }
  if args.config_choice.is_some() && !plan.config {
    eprintln!(
      "{}",
      colors::warning("--config-step ignored because the config step is skipped")
    );
  }
}

async fn run_install_step(
  args: &InitArgs,
  fetch: &FetchClient,
  hardware: &HardwareSnapshot,
  binary: &BinaryPresence,
) -> Result<BinaryInstall, CliExit> {
  let emit_progress = !args.json;
  // In recommended mode, if the user already has a safe-to-adopt binary
  // on PATH or at a common location, prefer adopting it over running a
  // fresh install. Matches the prior `--yes` behavior.
  //
  // `args.install.is_none()` guards the shortcut so an explicit
  // `--install <choice>` always wins over recommended-mode adoption
  // (W3: per-step flags beat `--recommended`).
  if prompts::is_recommended(args) && args.install.is_none() {
    if let Some(path) = binary.resolved_path.clone() {
      if crate::init::install::custom_path::is_safe_to_adopt(&path) {
        let sp = prompts::StepProgress::start_if(
          emit_progress,
          format!("Adopting existing llama-server at {}", path.display()),
        );
        return match crate::init::install::custom_path::install_from_custom_path(&path) {
          Ok(install) => {
            sp.success(format!("Adopted llama-server ({})", install.path.display()));
            Ok(install)
          }
          Err(e) => {
            sp.fail(format!("Could not adopt {}: {e}", path.display()));
            Err(install_err_to_exit(e))
          }
        };
      }
    }
  }
  let default = default_install_method(hardware);
  log::debug!(
    "init: install step (default={:?}, detected_binary={:?})",
    default,
    binary.resolved_path
  );
  let choice = prompts::pick_install_method(args, default, binary).await?;
  log::debug!("init: install method chosen: {choice:?}");
  match choice {
    InstallChoice::Brew => {
      let sp = prompts::StepProgress::start_if(
        emit_progress,
        "Installing llama.cpp via Homebrew (running `brew install --quiet llama.cpp`)",
      );
      match crate::init::install::brew::install_via_brew() {
        Ok(install) => {
          sp.success(format!(
            "Installed llama.cpp via Homebrew → {}",
            install.path.display()
          ));
          Ok(install)
        }
        Err(e) => {
          sp.fail(format!("Homebrew install failed: {e}"));
          Err(install_err_to_exit(e))
        }
      }
    }
    InstallChoice::GhReleases => {
      let install_root = crate::util::paths::state_dir()
        .ok_or_else(|| CliExit::new(INIT_ABORTED, "no state dir"))?
        .join("llama-cpp");
      let sp_query = prompts::StepProgress::start_if(
        emit_progress,
        "Querying GitHub Releases for the latest llama.cpp asset",
      );
      let pick = match gh_releases::fetch_latest_asset(fetch, hardware).await {
        Ok(p) => {
          sp_query.success(format!(
            "Selected GitHub Releases asset `{}` ({})",
            p.asset_name, p.tag
          ));
          p
        }
        Err(e) => {
          sp_query.fail(format!("GitHub Releases query failed: {e}"));
          return Err(install_err_to_exit(e));
        }
      };
      let sp_install = prompts::StepProgress::start_if(
        emit_progress,
        format!("Downloading + verifying + extracting `{}`", pick.asset_name),
      );
      match gh_releases::install_picked(fetch, &pick, &install_root).await {
        Ok(install) => {
          sp_install.success(format!(
            "Installed llama-server at {}",
            install.path.display()
          ));
          Ok(install)
        }
        Err(e) => {
          sp_install.fail(format!("GitHub Releases install failed: {e}"));
          Err(install_err_to_exit(e))
        }
      }
    }
    InstallChoice::CustomPath(p) => {
      let sp = prompts::StepProgress::start_if(
        emit_progress,
        format!("Adopting llama-server binary at {}", p.display()),
      );
      match crate::init::install::custom_path::install_from_custom_path(&p) {
        Ok(install) => {
          sp.success(format!("Adopted llama-server ({})", install.path.display()));
          Ok(install)
        }
        Err(e) => {
          sp.fail(format!("Could not adopt {}: {e}", p.display()));
          Err(install_err_to_exit(e))
        }
      }
    }
  }
}

fn install_err_to_exit(e: InstallError) -> CliExit {
  match e {
    InstallError::ChecksumMismatch { .. } | InstallError::UnsafeArchive { .. } => {
      CliExit::prefix(INIT_ABORTED, "init server", e)
    }
    InstallError::RateLimited { status } => CliExit::new(
      INIT_ABORTED,
      format!(
        "init server: GH Releases API rate-limited (status {status}); \
         retry in an hour or point at an existing binary via --llama-server <path>"
      ),
    ),
    other => CliExit::new(INIT_ABORTED, format!("init server: {other}")),
  }
}

/// Outcome of [`run_models_step`] threaded back up so
/// `persist_init_snapshot` can update the `remote_fetch_failures`
/// counter that backs doctor's `RemoteSnapshotUnreachable` finding.
struct ModelsStepResult {
  model: ModelSummary,
  /// The top-N recommendations the recommender produced this run.
  /// Surfaced into [`InitSummary::recommendations`] so JSON consumers
  /// can read the full ranked list even when a single model was
  /// auto-selected or download was skipped.
  recommendations: Vec<Recommendation>,
  remote_snapshot_attempt: Option<bool>,
}

async fn run_models_step(
  args: &InitArgs,
  fetch: &FetchClient,
  hardware: &HardwareSnapshot,
) -> Result<ModelsStepResult, CliExit> {
  let emit_progress = !args.json;
  let bundled = load_bundled();
  // Try the verified remote tier; on any failure, fall back silently
  // to the bundled snapshot but record the failure so doctor can
  // surface a sustained outage (R74 finding-6). Offline mode never
  // counts as a failure — the user opted out of network.
  let (snapshot, remote_snapshot_attempt) = if fetch.is_offline() {
    (bundled, None)
  } else {
    let sp =
      prompts::StepProgress::start_if(emit_progress, "Fetching latest model benchmark snapshot");
    match crate::init::benchmark::load_remote(fetch, &bundled).await {
      Ok(Some(fresh)) => {
        sp.success(format!(
          "Loaded benchmark snapshot ({} model entries)",
          fresh.models.len()
        ));
        (fresh, Some(true))
      }
      // `Ok(None)` means the bundled snapshot carries no remote_url
      // (e.g. a dev build pointed at a private fork) — not a failure.
      Ok(None) => {
        sp.success(format!(
          "Using bundled benchmark snapshot ({} entries)",
          bundled.models.len()
        ));
        (bundled, None)
      }
      Err(e) => {
        sp.fail(format!(
          "Remote benchmark snapshot unreachable; falling back to bundled ({e})"
        ));
        log::info!("init: remote benchmark snapshot fetch failed (using bundled): {e}");
        (bundled, Some(false))
      }
    }
  };
  let on_disk: Vec<OnDiskModel> = Vec::new();
  let recs = recommend(&snapshot, hardware, &on_disk, &RecommendOptions::default());
  log::debug!(
    "init: models step recommended {} candidate(s) from snapshot of {} entries",
    recs.len(),
    snapshot.models.len()
  );
  // JSON mode without an explicit `--model` override is treated as a
  // listing request: emit the recommendations in the summary and skip
  // the download. Lets `init --only models --json` work as a "what
  // would you suggest" surface comparable to `whichllm --json`,
  // without surprising the caller by pulling 10+ GB.
  if args.json && args.model.is_none() && !prompts::is_recommended(args) {
    return Ok(ModelsStepResult {
      model: ModelSummary {
        repo: String::new(),
        files: Vec::new(),
        total_bytes: 0,
      },
      recommendations: recs,
      remote_snapshot_attempt,
    });
  }
  let choice = prompts::pick_model(args, &recs).await?;
  log::debug!("init: model chosen: {choice:?}");
  // `--revision` only carries through on the paste branch — curated
  // picks are deliberately HEAD-tracked because the benchmark
  // snapshot picks them by repo, not by SHA. Threading `--revision`
  // into curated would silently override the recommender's
  // assumption that "this repo's HEAD is good".
  let is_paste = matches!(choice, ModelChoice::Paste(_));
  let (repo, pinned_filename, estimated_bytes) = match choice {
    ModelChoice::Curated(entry) => (entry.repo, Some(entry.file), Some(entry.weights_bytes)),
    ModelChoice::Paste(raw) => {
      // Parse through `RepoSpec::parse` so the interactive paste
      // accepts the same `owner/repo[:filename.gguf]` syntax as the
      // `llamastash pull` CLI surface. Without this, a colon-suffix
      // is silently treated as part of the repo name and the
      // downstream hf-hub call fails with a confusing error.
      let spec = crate::init::download::RepoSpec::parse(&raw)
        .map_err(|e| CliExit::prefix(INIT_ABORTED, "init paste", e))?;
      (spec.repo_id, spec.pinned_filename, None)
    }
    ModelChoice::Skip => {
      // No model fits, user picked "skip", or `--model none` supplied.
      return Ok(ModelsStepResult {
        model: ModelSummary {
          repo: String::new(),
          files: Vec::new(),
          total_bytes: 0,
        },
        recommendations: recs,
        remote_snapshot_attempt,
      });
    }
  };
  let spec = crate::init::download::RepoSpec {
    repo_id: repo.clone(),
    pinned_filename,
  };
  let progress: Option<std::sync::Arc<dyn crate::init::download::DownloadProgress>> =
    if emit_progress {
      Some(std::sync::Arc::new(WizardDownloadProgress::new(
        repo.clone(),
      )))
    } else {
      None
    };
  let revision = if is_paste {
    args.revision.clone()
  } else {
    None
  };
  let options = crate::init::download::DownloadOptions {
    extension_filter: None,
    estimated_bytes,
    progress,
    revision,
  };
  let result = crate::init::download::run_for_init(&spec, fetch, &options).await?;
  Ok(ModelsStepResult {
    model: ModelSummary {
      repo,
      files: result.paths,
      total_bytes: result.total_bytes,
    },
    recommendations: recs,
    remote_snapshot_attempt,
  })
}

/// Bridges hf-hub's per-file lifecycle (resolved → started → finished)
/// into a single rolling cliclack spinner so the user sees one
/// "Downloading file X of N" line that updates rather than N
/// flickering spinners. The spinner is replaced on each file
/// transition; `Mutex<Option<...>>` accommodates the trait's `&self`
/// receivers without making `StepProgress` itself `Sync`.
struct WizardDownloadProgress {
  repo: String,
  spinner: std::sync::Mutex<Option<prompts::StepProgress>>,
}

impl WizardDownloadProgress {
  fn new(repo: String) -> Self {
    Self {
      repo,
      spinner: std::sync::Mutex::new(None),
    }
  }
}

impl crate::init::download::DownloadProgress for WizardDownloadProgress {
  fn on_files_resolved(&self, files: &[(String, u64)]) {
    let total_mib = files.iter().map(|(_, s)| *s as f64).sum::<f64>() / (1024.0 * 1024.0);
    let _ = cliclack::log::info(format!(
      "Resolved {} file(s) (~{total_mib:.1} MiB) from `{}`",
      files.len(),
      self.repo
    ));
  }
  fn on_file_started(&self, filename: &str, size: u64, index: usize, total: usize) {
    let mib = size as f64 / (1024.0 * 1024.0);
    let sp = prompts::StepProgress::start(format!(
      "Downloading {}/{} `{filename}` (~{mib:.1} MiB)",
      index + 1,
      total
    ));
    *self.spinner.lock().unwrap() = Some(sp);
  }
  fn on_file_finished(&self, filename: &str, index: usize, total: usize) {
    if let Some(sp) = self.spinner.lock().unwrap().take() {
      sp.success(format!("Downloaded {}/{} `{filename}`", index + 1, total));
    }
  }
}

async fn run_config_step(
  args: &InitArgs,
  install: Option<&BinaryInstall>,
  hardware: &HardwareSnapshot,
) -> Result<Option<ConfigSummary>, CliExit> {
  let path =
    crate::util::paths::user_config_file().ok_or_else(|| CliExit::new(UNKNOWN, "no config dir"))?;
  let now = SystemTime::now();
  // Compose the wizard's additions as a serde-derived struct rather
  // than hand-rolling `serde_yaml::Value::String("...".into())` for
  // every key. The serialised top-level mapping becomes our
  // composition list (used below to digest each managed value's
  // canonical YAML bytes for R72).
  let mut bootstrap = InitConfigAdditions::default();
  if let Some(install) = install {
    bootstrap.llama_server_path = Some(install.path.display().to_string());
  }
  if hardware.gpu.is_gpu() {
    bootstrap.arch_defaults.insert(
      "qwen2".to_string(),
      crate::config::ArchDefaults {
        n_gpu_layers: Some(99),
        flash_attn: Some(true),
        ..Default::default()
      },
    );
    bootstrap.arch_defaults.insert(
      "llama".to_string(),
      crate::config::ArchDefaults {
        n_gpu_layers: Some(99),
        ..Default::default()
      },
    );
  }
  let additions_value =
    serde_yaml::to_value(&bootstrap).expect("InitConfigAdditions serialises cleanly");
  // `composed` records `(dotted-path, value)` for every top-level
  // managed entry. R72's user-edit detection compares the digest of
  // canonical YAML *value* bytes — never the key-name string.
  let composed: Vec<(String, serde_yaml::Value)> = match &additions_value {
    serde_yaml::Value::Mapping(m) => m
      .iter()
      .filter_map(|(k, v)| k.as_str().map(|s| (s.to_string(), v.clone())))
      .collect(),
    _ => Vec::new(),
  };
  // Interactive flow: render the diff before asking for confirmation
  // so the user sees what would be written. `--json` mode emits the
  // diff as part of the structured summary, so we skip the stderr
  // preview there. `--recommended` and `--config-step skip` also
  // skip the preview — `confirm_config_write` resolves their answer
  // synchronously and no live prompt is rendered, so the (possibly
  // expensive) filesystem read for `dry_run_diff` would be wasted.
  let skip_preview = args.json
    || prompts::is_recommended(args)
    || matches!(
      args.config_choice,
      Some(crate::cli::cli_args::ConfigOverride::Skip)
    );
  let preview_for_confirm = if skip_preview {
    String::new()
  } else {
    let dry = crate::init::config_writer::dry_run_diff(&path, additions_value.clone())
      .map_err(|e| CliExit::prefix(INIT_ABORTED, "init config", e))?;
    prompts::render_diff_preview(&dry.diff_json)
  };
  let confirmed = prompts::confirm_config_write(args, &preview_for_confirm).await?;
  if !confirmed {
    return Ok(None);
  }
  let options = crate::init::config_writer::WriteOptions {
    // The interactive preview already ran via `dry_run_diff`; the
    // writer shouldn't double-print it.
    show_diff_preview: false,
    verbose: false,
  };
  let result = crate::init::config_writer::write_with_diff(&path, additions_value, options)
    .map_err(|e| CliExit::prefix(INIT_ABORTED, "init config", e))?;
  // Build per-path digest records from the canonical YAML bytes of
  // each composed value. The writer-time diff entries (changed
  // keys only) overlap with `composed` but lose composition-time
  // unchanged-but-still-owned entries, so the composed list is the
  // authoritative source.
  let mut managed_records: Vec<ManagedKey> = composed
    .iter()
    .map(|(p, v)| {
      let bytes = canonical_value_bytes(v);
      ManagedKey::new(p.clone(), &bytes, now)
    })
    .collect();
  // Include any diff-derived paths the composition list didn't
  // already cover (e.g. recursive-merge inserts inside an existing
  // user block). These get an empty-bytes digest because we don't
  // have the composed value handy; mark them with a sentinel digest
  // by hashing the actual diff value_yaml string.
  let composed_paths: std::collections::HashSet<&String> =
    composed.iter().map(|(p, _)| p).collect();
  for entry in &result.diff_json {
    if !composed_paths.contains(&entry.path) {
      // diff_json's value_yaml may be `<redacted>` for secret-bearing
      // paths; that's the value the user would see on disk too, so
      // digesting it still uniquely identifies the wizard-written
      // form (and any real value-edit produces a different digest).
      let bytes = entry.value_yaml.as_bytes();
      managed_records.push(ManagedKey::new(entry.path.clone(), bytes, now));
    }
  }
  let managed_keys: Vec<String> = managed_records.iter().map(|m| m.path.clone()).collect();
  Ok(Some(ConfigSummary {
    path: result.path,
    written_bytes: result.written_bytes,
    managed_keys,
    managed_records,
  }))
}

/// Canonical YAML serialisation of `v` suitable for content-digesting.
/// We use the default block style and trim trailing newline so the
/// same logical value always yields the same byte sequence regardless
/// of how the wizard composed it.
fn canonical_value_bytes(v: &serde_yaml::Value) -> Vec<u8> {
  let s = serde_yaml::to_string(v).unwrap_or_default();
  s.trim_end().as_bytes().to_vec()
}

/// What `run_config_step` composes for the writer. Skipping empty
/// fields keeps the on-disk diff minimal — e.g. a no-GPU host writes
/// neither `arch_defaults` nor any unused leaf inside it. Each
/// `#[serde(skip_serializing_if)]` mirrors the merge semantics
/// `merge_and_write` already honours.
#[derive(Debug, Clone, Default, serde::Serialize)]
struct InitConfigAdditions {
  #[serde(skip_serializing_if = "Option::is_none")]
  llama_server_path: Option<String>,
  #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
  arch_defaults: std::collections::BTreeMap<String, crate::config::ArchDefaults>,
}

/// Advisory flock around `init_snapshot.json` writes so two concurrent
/// `llamastash init` runs can't clobber each other's persisted state.
/// Failure to acquire (unsupported FS, EACCES on the lock file) is
/// non-fatal — the lock is best-effort and the caller proceeds
/// unsynchronised, which is no worse than v2's pre-fix behaviour.
struct SnapshotWriteLock {
  // Held to keep the underlying fd alive — flock releases on close.
  // Allowed because the field is never read directly: its presence
  // is what extends the fd lifetime, and `Drop` on `File` is what
  // releases the flock.
  #[cfg(unix)]
  #[allow(dead_code)]
  file: Option<std::fs::File>,
}

impl SnapshotWriteLock {
  fn acquire(state_dir: &std::path::Path) -> Self {
    #[cfg(unix)]
    {
      let path = state_dir.join("init_snapshot.json.lock");
      // Ensure the state dir exists so the lock file open below
      // doesn't ENOENT on a first-run machine.
      let _ = std::fs::create_dir_all(state_dir);
      match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&path)
      {
        Ok(file) => {
          use std::os::fd::AsRawFd;
          // SAFETY: `flock` is a stable POSIX syscall with no
          // memory-safety implications.
          //
          // `LOCK_NB` (non-blocking) is critical: this function is
          // called from an async context. A blocking `LOCK_EX` would
          // stall the Tokio worker thread if a concurrent `llamastash
          // init` already holds the lock. EWOULDBLOCK / EAGAIN map
          // to "couldn't lock now" which we treat as best-effort and
          // proceed unsynchronised — matching the wider best-effort
          // stance for snapshot writes documented above.
          let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
          if rc == 0 {
            Self { file: Some(file) }
          } else {
            Self { file: None }
          }
        }
        Err(_) => Self { file: None },
      }
    }
    #[cfg(not(unix))]
    {
      Self {}
    }
  }
}

impl Drop for SnapshotWriteLock {
  fn drop(&mut self) {
    // Lock is released when the file descriptor closes; explicit
    // unlock is unnecessary and `Drop` on `File` handles it.
  }
}

fn persist_init_snapshot(
  hardware: &HardwareSnapshot,
  install: Option<&BinaryInstall>,
  summary: &InitSummary,
) -> Result<(), CliExit> {
  let state_dir =
    crate::util::paths::state_dir().ok_or_else(|| CliExit::new(UNKNOWN, "no state dir"))?;
  let now = SystemTime::now();
  // Best-effort advisory lock so two concurrent `llamastash init`
  // processes don't race on the load-modify-save cycle of
  // `init_snapshot.json`. The lock file is per-state-dir, opened
  // with `OpenOptions::create(true).truncate(false)`, and the file
  // descriptor is dropped (releasing the flock) on function exit.
  // Failure to acquire is non-fatal — we proceed unsynchronised,
  // matching the broader "best-effort" stance for snapshot writes.
  let _lock = SnapshotWriteLock::acquire(&state_dir);
  let mut snap = snapshot::load(&state_dir)
    .map_err(|e| CliExit::prefix(UNKNOWN, "load init_snapshot", e))?
    .unwrap_or_default();
  snap.gpu_vendor = Some(hardware.gpu.label().to_string());
  snap.vram_gb = hardware.vram_bytes.map(|b| b as f32 / 1_073_741_824.0);
  snap.gpu_device_count = Some(hardware.gpu_device_count);
  if let Some(install) = install {
    snap.llama_server_version = install.version.clone();
    snap.install_method = Some(install.method);
    snap.llama_server_path = Some(install.path.clone());
    snap.llama_server_digest = Some(install.digest.clone());
  }
  snap.init_date = Some(crate::util::datetime::iso8601(now));
  // Update the remote-snapshot failure counter: reset to 0 on a
  // verified fresh fetch, increment on a verified failure, leave
  // alone when no attempt was made (offline mode / models step
  // skipped). doctor's `RemoteSnapshotUnreachable` (R74) reads
  // this counter once it crosses its threshold.
  match summary.remote_snapshot_attempt {
    Some(true) => snap.remote_fetch_failures = 0,
    Some(false) => snap.remote_fetch_failures = snap.remote_fetch_failures.saturating_add(1),
    None => {}
  }
  // Record the bundle date of the snapshot in effect this run so
  // doctor's `SnapshotStale` finding has a baseline. Best-effort —
  // a successful remote fetch updates this to the fresher date.
  let bundled = crate::init::benchmark::load_bundled();
  if !bundled.bundle_date.is_empty() {
    snap.snapshot_bundle_date = Some(bundled.bundle_date);
  }
  if let Some(ref cfg) = summary.config {
    let mut merged: Vec<ManagedKey> = cfg.managed_records.clone();
    // Preserve any pre-existing managed keys not in this run — a
    // `--only X` partial re-run must not silently drop keys recorded by
    // a previous step's run. R72 contract.
    for prior in snap.managed_keys.iter() {
      if !merged.iter().any(|m| m.path == prior.path) {
        merged.push(prior.clone());
      }
    }
    snap.managed_keys = merged;
  }
  snapshot::save(&state_dir, &snap)
    .map_err(|e| CliExit::prefix(UNKNOWN, "save init_snapshot", e))?;
  Ok(())
}

fn print_handoff(summary: &InitSummary, json: bool) {
  if json {
    println!(
      "{}",
      serde_json::to_string_pretty(summary).unwrap_or_default()
    );
    return;
  }
  prompts::outro(summary);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn step_plan_default_runs_every_step() {
    let plan = StepPlan::resolve(&[], &[]);
    assert!(plan.server && plan.models && plan.config);
  }

  #[test]
  fn step_plan_only_server_runs_only_server() {
    let plan = StepPlan::resolve(&[InitStep::Server], &[]);
    assert!(plan.server);
    assert!(!plan.models);
    assert!(!plan.config);
  }

  #[test]
  fn step_plan_only_server_and_config() {
    let plan = StepPlan::resolve(&[InitStep::Server, InitStep::Config], &[]);
    assert!(plan.server);
    assert!(!plan.models);
    assert!(plan.config);
  }

  #[test]
  fn step_plan_skip_models_runs_other_two() {
    let plan = StepPlan::resolve(&[], &[InitStep::Models]);
    assert!(plan.server);
    assert!(!plan.models);
    assert!(plan.config);
  }

  #[test]
  fn step_plan_only_wins_over_skip_when_both_supplied() {
    // The CLI rejects both flags together; if a programmatic caller
    // supplies both, --only takes precedence (matches the matrix).
    let plan = StepPlan::resolve(&[InitStep::Config], &[InitStep::Server]);
    assert!(!plan.server);
    assert!(!plan.models);
    assert!(plan.config);
  }

  #[test]
  fn install_method_labels_round_trip() {
    assert_eq!(
      install_method_label(InstallMethod::GhReleases),
      "gh_releases"
    );
    assert_eq!(install_method_label(InstallMethod::Brew), "brew");
    assert_eq!(
      install_method_label(InstallMethod::CustomPath),
      "custom_path"
    );
  }

  #[test]
  fn init_date_is_iso8601() {
    let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    assert_eq!(crate::util::datetime::iso8601(t), "2023-11-14T22:13:20Z");
  }

  #[test]
  fn canonical_value_bytes_are_value_not_path() {
    // R72 contract: the digest must be over the value, not the key
    // name — otherwise every key on every run shares the path-bytes
    // digest and user-edit detection is non-functional.
    let path_a = "llama_server_path";
    let path_b = "port_range";
    let v_a = serde_yaml::Value::String("/opt/llama-server".into());
    let v_b = serde_yaml::Value::String("/opt/llama-server".into());
    let now = SystemTime::UNIX_EPOCH;
    let ka = ManagedKey::new(path_a, &canonical_value_bytes(&v_a), now);
    let kb = ManagedKey::new(path_b, &canonical_value_bytes(&v_b), now);
    // Same value → same digest, regardless of path.
    assert_eq!(ka.value_digest, kb.value_digest);
    // Different value → different digest, same path.
    let v_c = serde_yaml::Value::String("/usr/local/bin/llama-server".into());
    let kc = ManagedKey::new(path_a, &canonical_value_bytes(&v_c), now);
    assert_ne!(ka.value_digest, kc.value_digest);
  }

  #[test]
  fn canonical_value_bytes_round_trip_matches_value_yaml() {
    // Sanity-check that the canonicalisation we use is stable across
    // recompositions of the same value. A user-edited config that
    // restores the wizard's exact composed value yields the same
    // digest.
    let mut m = serde_yaml::Mapping::new();
    m.insert(
      serde_yaml::Value::String("n_gpu_layers".into()),
      serde_yaml::Value::Number(99.into()),
    );
    let v1 = serde_yaml::Value::Mapping(m.clone());
    let v2 = serde_yaml::Value::Mapping(m);
    assert_eq!(canonical_value_bytes(&v1), canonical_value_bytes(&v2));
  }
}
