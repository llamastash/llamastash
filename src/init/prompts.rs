//! Wizard-facing prompt wrapper.
//!
//! Each picker check, in order:
//!   1. The corresponding per-step value flag → return it immediately.
//!   2. `is_recommended()` (either `--recommended` or hidden alias
//!      `--yes`) → return the supplied default.
//!   3. stdout is not a TTY → return the default and emit one stderr
//!      warning so headless callers know defaults were used.
//!   4. otherwise → cliclack prompt.
//!
//! The cliclack branch runs synchronously inside
//! `tokio::task::spawn_blocking` so the wizard's async runtime stays
//! responsive while the user reads the prompt.
//!
//! The wizard's step functions delegate to these helpers; raw
//! `args.recommended` / `args.yes` reads live in `is_recommended`
//! only.

use std::io::IsTerminal;

use crate::cli::cli_args::{ConfigOverride, InitArgs, InstallOverride, ModelOverride};
use crate::cli::exit_codes::{CliExit, INIT_ABORTED};
use crate::gpu::{GpuDevice, GpuInfo};
use crate::init::benchmark::ModelEntry;
use crate::init::detection::{BinaryPresence, CpuArch, HardwareSnapshot, OsFamily};
use crate::init::fetch::FetchClient;
use crate::init::hf_api::{self, format_param_count, HfSearchResult, HfSortKey};
use crate::init::install::InstallChoice;
use crate::init::recommender::{Recommendation, RecommendationKind};
use crate::init::wizard::InitSummary;

/// Canonical "use derived defaults" predicate. The wizard reads this
/// once at the top of `run` and threads the boolean into each step;
/// no other site reads `args.recommended` / `args.yes` directly.
pub fn is_recommended(args: &InitArgs) -> bool {
  args.recommended || args.yes
}

/// Wraps a cliclack spinner so wizard steps can give the user
/// running narration ("Installing llama.cpp via Homebrew…",
/// "Downloaded foo.gguf") without writing two code paths for the
/// TTY and non-TTY cases.
///
/// In TTY mode this drives an animated cliclack spinner. In non-TTY
/// mode (piped output, CI) the spinner would be silent, so we fall
/// back to `cliclack::log::info` / `log::success` / `log::error`
/// which emit themed but static lines to stderr. Either way the
/// user (or the script reading the logs) sees a clear "started X →
/// finished X" pair instead of a long unexplained pause.
enum StepProgressInner {
  /// TTY-attached: animate a cliclack spinner.
  Spinner(cliclack::ProgressBar),
  /// Non-TTY but human-facing: emit themed static lines via
  /// `cliclack::log` so the script reading the logs still sees
  /// "started → finished" pairs.
  Log,
  /// `--json` mode: callers don't want any narration on stderr.
  Quiet,
}

pub struct StepProgress {
  inner: StepProgressInner,
}

impl StepProgress {
  /// Start a step. Pass the present-tense label the user should see
  /// while the work is running (e.g. `"Installing llama.cpp via
  /// Homebrew"`).
  pub fn start(label: impl Into<String>) -> Self {
    let label = label.into();
    if std::io::stderr().is_terminal() {
      let bar = cliclack::spinner();
      bar.start(label);
      Self {
        inner: StepProgressInner::Spinner(bar),
      }
    } else {
      let _ = cliclack::log::info(label);
      Self {
        inner: StepProgressInner::Log,
      }
    }
  }

  /// No-op variant for `--json` mode. Every method becomes a noop so
  /// the wizard can keep a single call shape regardless of human-vs-
  /// machine output.
  pub fn quiet() -> Self {
    Self {
      inner: StepProgressInner::Quiet,
    }
  }

  /// `start` if `emit` is true, otherwise `quiet`. Convenience for
  /// wizard sites that already have an `args.json` boolean in scope.
  pub fn start_if(emit: bool, label: impl Into<String>) -> Self {
    if emit {
      Self::start(label)
    } else {
      Self::quiet()
    }
  }

  /// Update the in-progress label without finishing the step. Useful
  /// for multi-phase work ("Downloading… → Verifying… → Extracting…")
  /// where each sub-phase is too short to merit its own spinner.
  pub fn update(&self, msg: impl Into<String>) {
    match &self.inner {
      StepProgressInner::Spinner(b) => b.set_message(msg.into()),
      StepProgressInner::Log => {
        let _ = cliclack::log::info(msg.into());
      }
      StepProgressInner::Quiet => {}
    }
  }

  /// Mark the step done with a success message and surrender the
  /// spinner (a `StepProgress` cannot be reused once stopped).
  pub fn success(self, msg: impl Into<String>) {
    match self.inner {
      StepProgressInner::Spinner(b) => b.stop(msg.into()),
      StepProgressInner::Log => {
        let _ = cliclack::log::success(msg.into());
      }
      StepProgressInner::Quiet => {}
    }
  }

  /// Mark the step done with a failure message. The wizard usually
  /// follows this immediately with a `return Err(...)` carrying the
  /// matching exit code.
  pub fn fail(self, msg: impl Into<String>) {
    match self.inner {
      StepProgressInner::Spinner(b) => b.error(msg.into()),
      StepProgressInner::Log => {
        let _ = cliclack::log::error(msg.into());
      }
      StepProgressInner::Quiet => {}
    }
  }
}

/// Render the dry-run diff with light YAML syntax coloring so the
/// preview the user confirms doesn't look like a wall of plain text.
/// Marker (`+` / `~`) is colored, dotted key paths are bold-cyan,
/// values are left in their canonical form (already redacted by
/// `config_writer` when the path matches the secrets list). Colors
/// are produced via `console::style`, so `--no-colors` / `NO_COLOR`
/// / non-TTY downgrade to plain text automatically.
pub fn render_diff_preview(diff: &[crate::init::config_writer::RedactedDiffEntry]) -> String {
  let mut out = String::new();
  out.push_str(&console::style("config diff (preview):").bold().to_string());
  out.push('\n');
  if diff.is_empty() {
    out.push_str(&console::style("  (no changes)").dim().to_string());
    return out;
  }
  for entry in diff {
    let marker = match entry.kind {
      "added" => console::style("+").green().bold().to_string(),
      "changed" => console::style("~").yellow().bold().to_string(),
      _ => console::style(" ").to_string(),
    };
    let path = console::style(&entry.path).cyan().bold().to_string();
    let value = entry.value_yaml.trim_end();
    if value.contains('\n') {
      out.push_str(&format!("  {marker} {path}:\n"));
      for line in value.lines() {
        out.push_str(&format!("      {line}\n"));
      }
    } else {
      out.push_str(&format!("  {marker} {path}: {value}\n"));
    }
  }
  // Trim only the trailing newline so the cliclack panel that hosts
  // this string doesn't double-space.
  if out.ends_with('\n') {
    out.pop();
  }
  out
}

/// What `pick_model` returns. Mirrors the recommender's outcome but
/// adds the `Skip` variant for `--model none`. `ModelEntry` does not
/// implement `PartialEq` so this enum doesn't either — callers use
/// pattern matching to branch.
//
// `large_enum_variant`: ModelEntry is ~272 bytes after the
// schema additions; the next-largest variant is 24 bytes. Boxing
// would force every call site to dereference for no measurable win
// (pick_model is invoked once per init wizard run). Allow the size
// asymmetry instead.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ModelChoice {
  /// Use this curated entry from the snapshot.
  Curated(ModelEntry),
  /// Download an HF repo id the user pasted (or supplied via
  /// `--model owner/repo`).
  Paste(String),
  /// Skip the model-download step entirely.
  Skip,
}

/// Render the cliclack intro panel with the detected-hardware line.
/// JSON callers bypass this (the wizard's `print_handoff` already
/// returns early on `--json`).
pub fn intro(hardware: &HardwareSnapshot) {
  let _ = cliclack::intro(console::style("llamastash init").bold().to_string());
  let line = hardware_line(hardware);
  let _ = cliclack::log::info(line);
}

/// Render the cliclack outro panel with the summary's headline plus
/// the "what landed" lines from the existing `print_handoff` body.
///
/// The multi-line "what landed" block goes through `cliclack::note`
/// so every line picks up the bordered-panel chrome (otherwise only
/// the first line gets the `└  ` prefix and the rest escape the
/// visual frame). A single-line `cliclack::outro` closes the session.
pub fn outro(summary: &InitSummary) {
  let mut body = String::new();
  body.push_str(&format!("steps_ran:     {:?}", summary.steps_ran));
  if !summary.steps_skipped.is_empty() {
    body.push_str(&format!("\nsteps_skipped: {:?}", summary.steps_skipped));
  }
  if let Some(install) = &summary.install {
    body.push_str(&format!(
      "\ninstall:       {} → {}",
      install.method,
      install.path.display()
    ));
  }
  if let Some(model) = &summary.model {
    if !model.repo.is_empty() {
      body.push_str(&format!(
        "\nmodel:         {} ({:.1} MiB across {} file(s))",
        model.repo,
        model.total_bytes as f64 / (1024.0 * 1024.0),
        model.files.len()
      ));
    }
  }
  if let Some(cfg) = &summary.config {
    body.push_str(&format!(
      "\nconfig:        wrote {} bytes to {}",
      cfg.written_bytes,
      cfg.path.display()
    ));
  }
  if let Some(int) = &summary.integrations {
    if !int.applied.is_empty() {
      body.push_str("\nintegrations:");
      for tool in &int.applied {
        body.push_str(&format!(
          "\n  • {} → {}",
          tool.display_name,
          tool.path.display()
        ));
      }
      // The two sourceable-env writers each dropped a snippet — surface
      // the command for whichever was applied. The OpenAI `env.sh` goes
      // in the shell rc; the Claude Code snippet is sourced per-shell
      // (kept out of Claude Code's global settings so bare `claude`
      // stays on Anthropic).
      if let Some(env) = int.applied.iter().find(|t| t.id == "env-sh") {
        body.push_str(&format!(
          "\n  ▸ run: source {}  (add this line to your shell rc)",
          env.path.display()
        ));
      }
      if let Some(req) = &int.env_requirement {
        body.push_str(&format!(
          "\n  ! {}",
          crate::init::wizard::render_env_requirement(req)
        ));
      }
      if let Some(cc) = int.applied.iter().find(|t| t.id == "claude-code") {
        body.push_str(&format!(
          "\n  ▸ Claude Code (local proxy, this shell only): source {} && claude",
          cc.path.display()
        ));
      }
    }
    if !int.failed.is_empty() {
      body.push_str("\nintegrations failed:");
      for t in &int.failed {
        body.push_str(&format!("\n  ⚠ {}: {}", t.id, t.error));
      }
    }
  }
  let _ = cliclack::note("init summary", body);
  let _ = cliclack::outro(
    "Next: run `llamastash` to enter the TUI, or `llamastash list` to see discovered models.",
  );
}

/// Pre-formatted "detected: …" block used by both `intro` and the
/// non-TTY warning path's context message. Three lines: GPU, CPU,
/// system. Each segment elides cleanly when its data isn't available.
fn hardware_line(hw: &HardwareSnapshot) -> String {
  let mut lines: Vec<String> = Vec::with_capacity(3);
  lines.push(format!("gpu: {}", format_gpu_segment(hw)));
  lines.push(format!("cpu: {}", format_cpu_segment(hw)));
  lines.push(format!("sys: {}", format_system_segment(hw)));
  lines.join("\n")
}

/// Classification verdict suffix for the init banner — `(unified,
/// inferred)` / `(unified)` / `(discrete)` — sourced from the same
/// [`GpuInfo::uma_class_source`] that drives the `status` and `doctor`
/// GPU lines, so all three name the verdict identically. Empty (no
/// suffix) when the vendor-unknown Vulkan path can't classify.
fn verdict_suffix(gpu: &GpuInfo) -> String {
  gpu
    .uma_class_source()
    .map(|s| format!(" ({})", s.label()))
    .unwrap_or_default()
}

fn format_gpu_segment(hw: &HardwareSnapshot) -> String {
  match &hw.gpu {
    GpuInfo::CpuOnly => "(none — CPU only)".to_string(),
    GpuInfo::Nvidia { devices } | GpuInfo::Amd { devices } => {
      let vendor = match &hw.gpu {
        GpuInfo::Nvidia { .. } => "NVIDIA",
        GpuInfo::Amd { .. } => "AMD",
        _ => unreachable!(),
      };
      let name_segment = format_device_name(devices, vendor);
      let mem = format_gib(hw.vram_bytes.unwrap_or(0));
      // Verdict vocabulary matches `status` / `doctor`: a unified APU
      // reads `(unified, inferred)`, a discrete card `(discrete)` — never
      // the bare `VRAM` word that re-introduced the 2x-memory confusion.
      format!("{name_segment} · {mem}{}", verdict_suffix(&hw.gpu))
    }
    GpuInfo::AppleMetal {
      total_memory_bytes: _,
    } => {
      let mem = format_gib(hw.vram_bytes.unwrap_or(0));
      format!("Apple Silicon · {mem}{}", verdict_suffix(&hw.gpu))
    }
    GpuInfo::Unknown { devices } => {
      // Vendor unknown (Vulkan fallback): no discrete-vs-unified verdict,
      // so show the raw pool size without claiming either.
      let name_segment = format_device_name(devices, "GPU (vendor unknown)");
      let mem = match hw.vram_bytes {
        Some(b) => format!(" · {}", format_gib(b)),
        None => String::new(),
      };
      format!("{name_segment}{mem}")
    }
    GpuInfo::Multi { devices } => {
      // Show per-backend breakdown
      let mut nvidia = vec![];
      let mut amd = vec![];
      let mut unknown = vec![];
      for d in devices {
        match d.backend.as_str() {
          "nvidia" => nvidia.push(d),
          "amd" => amd.push(d),
          _ => unknown.push(d),
        }
      }
      let parts: Vec<String> = vec![
        if !nvidia.is_empty() {
          let names = nvidia
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
          Some(format!("NVIDIA · {}", names))
        } else {
          None
        },
        if !amd.is_empty() {
          let names = amd
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
          Some(format!("AMD · {}", names))
        } else {
          None
        },
        if !unknown.is_empty() {
          let names = unknown
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
          Some(format!("Unknown · {}", names))
        } else {
          None
        },
      ]
      .into_iter()
      .flatten()
      .collect();
      let mem = hw
        .vram_bytes
        .map(|b| format!(" · {}{}", format_gib(b), verdict_suffix(&hw.gpu)))
        .unwrap_or_default();
      if parts.is_empty() {
        "(none)".to_string()
      } else {
        format!("{}{}", parts.join(" + "), mem)
      }
    }
  }
}

fn format_device_name(devices: &[GpuDevice], vendor_fallback: &str) -> String {
  let count = devices.len();
  let first_name = devices
    .first()
    .map(|d| d.name.trim())
    .filter(|n| !n.is_empty());
  match (count, first_name) {
    (0, _) => vendor_fallback.to_string(),
    (1, Some(name)) => format!("{vendor_fallback} {name}"),
    (1, None) => vendor_fallback.to_string(),
    (n, Some(name)) => format!("{n}× {vendor_fallback} {name}"),
    (n, None) => format!("{n}× {vendor_fallback}"),
  }
}

fn format_cpu_segment(hw: &HardwareSnapshot) -> String {
  let brand = if hw.cpu_brand.is_empty() {
    format!("{:?} CPU", hw.cpu_arch)
  } else {
    hw.cpu_brand.clone()
  };
  let mut parts: Vec<String> = vec![brand];
  if hw.cpu_cores > 0 {
    parts.push(format!("{} cores", hw.cpu_cores));
  }
  if !hw.cpu_features.is_empty() {
    parts.push(hw.cpu_features.join(" "));
  }
  parts.join(" · ")
}

fn format_system_segment(hw: &HardwareSnapshot) -> String {
  // `MEM*` on unified hosts (the GPU draws from this pool); `MEM` on
  // discrete. Matches the TUI host pane and doctor hardware section.
  let mem_label = if hw.gpu.is_unified() { "MEM*" } else { "MEM" };
  let ram = format!("{} {mem_label}", format_gib(hw.ram_total_bytes));
  let mut parts: Vec<String> = vec![ram];
  if hw.disk_free_bytes > 0 {
    parts.push(format!("{} disk free", format_gib(hw.disk_free_bytes)));
  }
  parts.push(format!("{}/{}", os_short(hw.os), arch_short(hw.cpu_arch)));
  parts.join(" · ")
}

fn format_gib(bytes: u64) -> String {
  // Delegate to the one canonical formatter so the init banner, doctor,
  // the recommender, status, and the TUI all print the same unit.
  crate::init::detection::fmt_gib(bytes)
}

pub fn os_short(os: OsFamily) -> &'static str {
  match os {
    OsFamily::Linux => "linux",
    OsFamily::MacOs => "macos",
    OsFamily::Windows => "windows",
    OsFamily::Other => "other",
  }
}

pub fn arch_short(arch: CpuArch) -> &'static str {
  match arch {
    CpuArch::X86_64 => "x86_64",
    CpuArch::Arm64 => "arm64",
    CpuArch::Other => "other",
  }
}

/// Resolve the install-method choice. Returns immediately if the
/// override flag is set, the wizard is in recommended mode, or
/// stdout is not a terminal. Otherwise prompts via cliclack.
pub async fn pick_install_method(
  args: &InitArgs,
  default: InstallChoice,
  existing: &BinaryPresence,
) -> Result<InstallChoice, CliExit> {
  if let Some(override_value) = &args.install {
    return install_override_to_choice(override_value.clone(), existing);
  }
  if is_recommended(args) {
    return Ok(default);
  }
  if !stdout_is_terminal() {
    // Silent fallback — wizard::run emits the single consolidated
    // non-TTY warning before the first picker runs.
    return Ok(default);
  }
  let (initial_idx, items) = build_install_items(&default, existing, brew_offer_available());
  let items_for_thread = items.clone();
  let chosen_idx = tokio::task::spawn_blocking(move || {
    let mut select = cliclack::select::<usize>("Install method").initial_value(initial_idx);
    for (i, (_pick, label, hint)) in items_for_thread.iter().enumerate() {
      select = select.item(i, label.clone(), hint.clone());
    }
    select.interact()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: install prompt: {e}")))?;
  let (pick, _, _) = items
    .into_iter()
    .nth(chosen_idx)
    .ok_or_else(|| CliExit::new(INIT_ABORTED, "init: install pick index out of range"))?;
  match pick {
    InstallPick::Resolved(choice) => Ok(choice),
    InstallPick::PromptCustomPath => prompt_custom_path().await,
  }
}

/// Cliclack input that collects an absolute path to a user-built or
/// otherwise-installed `llama-server` binary. Validation only checks the
/// shape (non-empty, absolute) so the dedicated `install_from_custom_path`
/// step still owns existence / digest / runnability checks — the same
/// errors surface either way.
async fn prompt_custom_path() -> Result<InstallChoice, CliExit> {
  let entered: String = tokio::task::spawn_blocking(|| {
    cliclack::input("Path to existing llama-server binary")
      .placeholder("/absolute/path/to/llama-server")
      .validate(|s: &String| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
          return Err("path is required");
        }
        if !std::path::Path::new(trimmed).is_absolute() {
          return Err("path must be absolute");
        }
        Ok(())
      })
      .interact()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: custom path prompt: {e}")))?;
  Ok(InstallChoice::CustomPath(std::path::PathBuf::from(
    entered.trim(),
  )))
}

/// Picker outcome that distinguishes "I have a resolved `InstallChoice`"
/// from "user picked the Custom path… sentinel and we need to prompt
/// for a path next." Kept private — callers see only `InstallChoice`.
#[derive(Debug, Clone)]
enum InstallPick {
  Resolved(InstallChoice),
  PromptCustomPath,
}

/// Resolve the model-pick choice. Override / recommended / non-TTY
/// short-circuits mirror the install picker.
///
/// `ModelOverride::Recommended` and the recommended-mode short-circuit
/// both resolve to the top curated recommendation, so this fn handles
/// the override + recommended-mode + non-TTY branches uniformly.
pub async fn pick_model(
  args: &InitArgs,
  fetch: &FetchClient,
  recs: &[Recommendation],
) -> Result<ModelChoice, CliExit> {
  let curated_default = recs.iter().find_map(|r| match &r.kind {
    RecommendationKind::Curated { entry } => Some(entry.clone()),
    _ => None,
  });
  let curated_or_skip = || match curated_default.clone() {
    Some(entry) => ModelChoice::Curated(entry),
    None => ModelChoice::Skip,
  };
  if let Some(override_value) = &args.model {
    return Ok(match override_value {
      ModelOverride::None => ModelChoice::Skip,
      ModelOverride::Paste(repo) => ModelChoice::Paste(repo.clone()),
      ModelOverride::Recommended => curated_or_skip(),
    });
  }
  if is_recommended(args) {
    return Ok(curated_or_skip());
  }
  if !stdout_is_terminal() {
    // Silent fallback — wizard::run emits the single consolidated
    // non-TTY warning before the first picker runs.
    return Ok(curated_or_skip());
  }
  // Filter OnDisk entries out of the interactive list — they
  // represent "you already have this" rather than a download action.
  // Selecting one used to silently map to `Skip` (a confusing UX);
  // the recommender's analysis output still shows them, but the
  // wizard prompt only offers actionable choices.
  let owned_recs: Vec<Recommendation> = recs
    .iter()
    .filter(|r| !matches!(r.kind, RecommendationKind::OnDisk { .. }))
    .cloned()
    .collect();
  if owned_recs.is_empty() {
    return Ok(curated_or_skip());
  }
  let initial_idx: usize = owned_recs
    .iter()
    .position(|r| matches!(r.kind, RecommendationKind::Curated { .. }))
    .unwrap_or(0);
  // Search hits the network, so only offer it when egress is allowed.
  let online = !fetch.is_offline();
  // Re-show the model list after the search sub-flow's "back" so the
  // user isn't trapped in search if they change their mind.
  loop {
    match show_model_menu(owned_recs.clone(), initial_idx, online).await? {
      ModelMenuOutcome::Pick(RecommendationKind::Curated { entry }) => {
        return Ok(ModelChoice::Curated(entry))
      }
      ModelMenuOutcome::Pick(RecommendationKind::Escape) => {
        return Ok(ModelChoice::Paste(prompt_paste_repo().await?))
      }
      // OnDisk variants were filtered out above; any other pick is a
      // no-op that falls through to Skip.
      ModelMenuOutcome::Pick(_) | ModelMenuOutcome::Skip => return Ok(ModelChoice::Skip),
      ModelMenuOutcome::Search => {
        if let Some(repo) = search_and_pick(fetch).await? {
          return Ok(ModelChoice::Paste(repo));
        }
        // "back" → fall through to re-show the model list.
      }
    }
  }
}

/// What the model-list prompt resolved to. `Pick` carries the chosen
/// recommendation's kind so the caller branches without re-indexing.
// `Pick` embeds the ~1.4 KB `RecommendationKind`; built once per prompt
// and dropped immediately, so accept the asymmetry (as `RecommendationKind`
// itself does) rather than box it.
#[allow(clippy::large_enum_variant)]
enum ModelMenuOutcome {
  Pick(RecommendationKind),
  Search,
  Skip,
}

/// Render the "Pick a model" cliclack list: recommendations, an
/// optional "Search HuggingFace" item (online only), and "Skip".
async fn show_model_menu(
  recs: Vec<Recommendation>,
  initial_idx: usize,
  online: bool,
) -> Result<ModelMenuOutcome, CliExit> {
  let n = recs.len();
  let search_idx = online.then_some(n);
  let skip_idx = if online { n + 1 } else { n };
  tokio::task::spawn_blocking(move || {
    let mut select = cliclack::select("Pick a model").initial_value(initial_idx);
    for (i, r) in recs.iter().enumerate() {
      let (label, hint) = render_recommendation(r);
      select = select.item(i, label, hint);
    }
    if let Some(si) = search_idx {
      select = select.item(
        si,
        "Search HuggingFace by name…".to_string(),
        "type a query, pick from results".to_string(),
      );
    }
    select = select.item(
      skip_idx,
      "Skip — don't download a model".to_string(),
      "use `llamastash pull` later".to_string(),
    );
    let idx = select.interact()?;
    let outcome = if Some(idx) == search_idx {
      ModelMenuOutcome::Search
    } else if idx == skip_idx {
      ModelMenuOutcome::Skip
    } else {
      match recs.into_iter().nth(idx) {
        Some(r) => ModelMenuOutcome::Pick(r.kind),
        None => ModelMenuOutcome::Skip,
      }
    };
    Ok::<_, std::io::Error>(outcome)
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: model prompt: {e}")))
}

/// The existing "paste an owner/repo slug" prompt. Shared by the
/// `Escape` menu item.
async fn prompt_paste_repo() -> Result<String, CliExit> {
  tokio::task::spawn_blocking(|| {
    cliclack::input("Paste an HF repo id")
      .placeholder("owner/repo")
      .validate(|s: &String| validate_repo_slug(s))
      .interact()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: paste prompt: {e}")))
}

/// `owner/repo` slug validator, shared by the paste prompt and the
/// search query (which forbids the slash a query wouldn't carry).
fn validate_repo_slug(s: &str) -> Result<(), &'static str> {
  if !s.contains('/') {
    return Err("expected `owner/repo`");
  }
  if s.chars().any(char::is_whitespace) {
    return Err("must not contain whitespace");
  }
  // Control characters (incl. null bytes) flow into filesystem paths +
  // URLs downstream; reject at the validator boundary, not after they
  // cause a confusing OS-level error.
  if s.chars().any(char::is_control) {
    return Err("must not contain control characters");
  }
  Ok(())
}

/// What the search results prompt resolved to.
enum SearchPick {
  Repo(String),
  Again,
  Back,
}

/// Search HuggingFace by name and let the user pick a repo. Returns
/// `Some(repo_id)` once a result is chosen (the caller wraps it as
/// `ModelChoice::Paste`, reusing the slug download path), or `None` to
/// return to the model list. Reuses `hf_api::search`; only the cliclack
/// rendering is local.
async fn search_and_pick(fetch: &FetchClient) -> Result<Option<String>, CliExit> {
  let mut query = prompt_search_query(None).await?;
  loop {
    // The wizard is bootstrapping a first model for the default backend, so
    // GGUF is the right scope here even though the TUI browser is wider.
    let results = match hf_api::search(
      fetch,
      &query,
      HfSortKey::Downloads,
      None,
      hf_api::WeightFormatFilter::GgufOnly,
    )
    .await
    {
      Ok(page) => page.results,
      Err(err) => {
        note_search_error(&err).await?;
        query = prompt_search_query(Some(&query)).await?;
        continue;
      }
    };
    match pick_search_result(&query, results).await? {
      SearchPick::Repo(repo) => return Ok(Some(repo)),
      SearchPick::Again => query = prompt_search_query(Some(&query)).await?,
      SearchPick::Back => return Ok(None),
    }
  }
}

/// Prompt for a free-text search query, pre-filled with `initial` when
/// re-searching.
async fn prompt_search_query(initial: Option<&str>) -> Result<String, CliExit> {
  let initial = initial.map(str::to_string);
  tokio::task::spawn_blocking(move || {
    let mut input = cliclack::input("Search HuggingFace").placeholder("e.g. qwen3 coder");
    if let Some(seed) = initial {
      input = input.default_input(&seed);
    }
    input
      .validate(|s: &String| {
        if s.trim().is_empty() {
          return Err("enter a search term");
        }
        Ok(())
      })
      .interact::<String>()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: search prompt: {e}")))
}

/// Render the search results as a cliclack list (plus "search again" /
/// "back"). Each row's label is the repo id; the hint carries the
/// reused params / size / downloads summary.
async fn pick_search_result(
  query: &str,
  results: Vec<HfSearchResult>,
) -> Result<SearchPick, CliExit> {
  let query = query.to_string();
  tokio::task::spawn_blocking(move || {
    let n = results.len();
    let again_idx = n;
    let back_idx = n + 1;
    let title = if results.is_empty() {
      format!("No GGUF matches for `{query}`")
    } else {
      format!("Results for `{query}`")
    };
    let mut select = cliclack::select(title);
    for (i, r) in results.iter().enumerate() {
      select = select.item(i, r.repo_id.clone(), search_result_hint(r));
    }
    select = select.item(again_idx, "↻ Search again".to_string(), String::new());
    select = select.item(back_idx, "← Back to model list".to_string(), String::new());
    let idx = select.interact()?;
    let pick = if idx == again_idx {
      SearchPick::Again
    } else if idx == back_idx {
      SearchPick::Back
    } else {
      match results.into_iter().nth(idx) {
        Some(r) => SearchPick::Repo(r.repo_id),
        None => SearchPick::Back,
      }
    };
    Ok::<_, std::io::Error>(pick)
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: results prompt: {e}")))
}

/// One-line metadata hint for a search result row: params, approximate
/// download size, downloads. Reuses [`format_param_count`]; sizes/counts
/// are local display helpers.
fn search_result_hint(r: &HfSearchResult) -> String {
  let mut parts: Vec<String> = Vec::new();
  if let Some(p) = r.param_count() {
    parts.push(format_param_count(p));
  }
  if let Some(b) = r.download_size_bytes() {
    parts.push(format_gib(b));
  }
  if let Some(d) = r.downloads {
    parts.push(format!("↓{}", short_count(d)));
  }
  parts.join(" · ")
}

/// K/M/B short count for popularity totals.
fn short_count(n: u64) -> String {
  match n {
    0..=999 => n.to_string(),
    1_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0),
    1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
    _ => format!("{:.1}B", n as f64 / 1_000_000_000.0),
  }
}

/// Surface a search failure as a cliclack note so the user can retry
/// rather than have the wizard abort on a transient network error.
async fn note_search_error(err: &crate::init::fetch::FetchError) -> Result<(), CliExit> {
  let msg = format!("search failed: {err}");
  tokio::task::spawn_blocking(move || cliclack::log::warning(msg))
    .await
    .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
    .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: search note: {e}")))
}

/// Resolve the config-write confirm. Override / recommended /
/// non-TTY short-circuits mirror the install picker. The diff
/// preview is the same text the wizard already builds — it's
/// printed before the prompt in the cliclack branch only.
pub async fn confirm_config_write(args: &InitArgs, diff_render: &str) -> Result<bool, CliExit> {
  if let Some(choice) = args.config_choice {
    return Ok(matches!(choice, ConfigOverride::Write));
  }
  if is_recommended(args) {
    return Ok(true);
  }
  if !stdout_is_terminal() {
    // Refuse to silently write config in non-interactive mode without
    // explicit consent. Agents piping stdout get a clear actionable
    // error rather than an unexpected config write whose path was not
    // chosen via flag.
    return Err(CliExit::new(
      INIT_ABORTED,
      "init: config-write step needs explicit consent in non-interactive mode; \
       pass `--recommended`, `--config-step write`, or `--config-step skip`"
        .to_string(),
    ));
  }
  let diff_owned = diff_render.to_string();
  let confirmed = tokio::task::spawn_blocking(move || {
    if !diff_owned.is_empty() {
      let _ = cliclack::log::info(diff_owned);
    }
    cliclack::confirm("Write config?")
      .initial_value(true)
      .interact()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: config prompt: {e}")))?;
  Ok(confirmed)
}

/// Resolve whether `init` should hand off into the interactive TUI
/// when the wizard completes successfully. Short-circuits keep
/// non-interactive callers (agents piping `--json`, headless CI,
/// users who passed `--no-tui`) on the legacy "print outro and exit"
/// path; `--recommended` auto-launches without prompting; everything
/// else gets a single Y/n cliclack confirm with default Y.
pub async fn confirm_tui_handoff(args: &InitArgs) -> Result<bool, CliExit> {
  if args.no_tui || args.json {
    return Ok(false);
  }
  if !stdout_is_terminal() {
    return Ok(false);
  }
  if is_recommended(args) {
    return Ok(true);
  }
  let confirmed = tokio::task::spawn_blocking(|| {
    cliclack::confirm("Launch the TUI now?")
      .initial_value(true)
      .interact()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: handoff prompt: {e}")))?;
  Ok(confirmed)
}

/// cliclack multiselect over the registered external-tool patchers.
/// Returns the picked patcher ids (caller resolves them back via
/// [`crate::init::external::patcher_by_id`]). Non-TTY skips the
/// picker and returns an empty list — the wizard's `--integrations`
/// flag is the non-interactive opt-in.
pub async fn pick_integrations() -> Result<Vec<String>, CliExit> {
  if !stdout_is_terminal() {
    return Ok(Vec::new());
  }
  let items: Vec<(String, &'static str, String)> = crate::init::external::all_patchers()
    .iter()
    .map(|p| {
      let hint = p
        .default_path()
        .map(|pp| pp.display().to_string())
        .unwrap_or_else(|| "<no home>".into());
      (p.id().to_string(), p.display_name(), hint)
    })
    .collect();
  if items.is_empty() {
    return Ok(Vec::new());
  }
  let picked: Vec<String> = tokio::task::spawn_blocking(move || {
    let mut select = cliclack::multiselect::<String>(
      "Patch AI tool configs to point at llamastash? (space to toggle)",
    )
    .required(false);
    for (id, label, hint) in &items {
      select = select.item(id.clone(), *label, hint.clone());
    }
    select.interact()
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: integrations prompt: {e}")))?;
  Ok(picked)
}

fn install_override_to_choice(
  override_value: InstallOverride,
  existing: &BinaryPresence,
) -> Result<InstallChoice, CliExit> {
  match override_value {
    InstallOverride::Brew => Ok(InstallChoice::Brew),
    InstallOverride::GhReleases => Ok(InstallChoice::GhReleases),
    InstallOverride::Custom(path) => Ok(InstallChoice::CustomPath(path)),
    InstallOverride::Existing => match existing.resolved_path.clone() {
      Some(path) => Ok(InstallChoice::CustomPath(path)),
      None => Err(CliExit::new(
        INIT_ABORTED,
        "init: `--install existing` supplied but no existing llama-server binary was detected"
          .to_string(),
      )),
    },
  }
}

/// Whether the Homebrew install option should be offered in the
/// interactive picker. Homebrew only exists on macOS/Linux, so it's
/// never offered on Windows; on macOS/Linux it's offered only when
/// `brew` is actually resolvable on PATH (a `which` lookup) so the
/// wizard never lists a method the host can't run.
fn brew_offer_available() -> bool {
  !cfg!(target_os = "windows") && which::which("brew").is_ok()
}

/// Build the cliclack-select items list for the install prompt and
/// pick which index should be the initial cursor position. Returns
/// `(initial_index, items)` where each item is
/// `(pick, label, hint)`. The cliclack `Select` is keyed by `usize`
/// index because `InstallPick` does not implement `Eq`. The trailing
/// "Custom path…" sentinel lets users adopt a self-built or otherwise-
/// installed `llama-server` interactively (mirrors the `--install
/// custom:PATH` CLI override).
///
/// `brew_available` gates the Homebrew item: Homebrew is a macOS/Linux
/// package manager, so we never list it on Windows, and even on
/// macOS/Linux we only offer it when `brew` is actually on PATH —
/// listing an install method the host can't run would just dead-end the
/// wizard. Compute it via [`brew_offer_available`] at the call site.
fn build_install_items(
  default: &InstallChoice,
  existing: &BinaryPresence,
  brew_available: bool,
) -> (usize, Vec<(InstallPick, String, String)>) {
  let mut items: Vec<(InstallPick, String, String)> = vec![(
    InstallPick::Resolved(InstallChoice::GhReleases),
    "GitHub Releases".into(),
    "verified asset for this host".into(),
  )];
  if brew_available {
    items.push((
      InstallPick::Resolved(InstallChoice::Brew),
      "Homebrew".into(),
      "brew install --quiet llama.cpp".into(),
    ));
  }
  if let Some(path) = &existing.resolved_path {
    items.push((
      InstallPick::Resolved(InstallChoice::CustomPath(path.clone())),
      format!("Use existing binary at {}", path.display()),
      format!("detected via {:?}", existing.source),
    ));
  }
  items.push((
    InstallPick::PromptCustomPath,
    "Custom path…".into(),
    "point at a self-built or pre-installed llama-server".into(),
  ));
  let initial = items
    .iter()
    .position(|(pick, _, _)| match pick {
      InstallPick::Resolved(c) => install_choice_matches_default(c, default),
      InstallPick::PromptCustomPath => false,
    })
    .unwrap_or(0);
  (initial, items)
}

/// `InstallChoice` lacks `PartialEq`. Helper compares by variant +
/// path payload so the install-prompt's initial cursor lands on the
/// derived default when possible.
fn install_choice_matches_default(candidate: &InstallChoice, default: &InstallChoice) -> bool {
  match (candidate, default) {
    (InstallChoice::Brew, InstallChoice::Brew) => true,
    (InstallChoice::GhReleases, InstallChoice::GhReleases) => true,
    (InstallChoice::CustomPath(a), InstallChoice::CustomPath(b)) => a == b,
    _ => false,
  }
}

fn render_recommendation(r: &Recommendation) -> (String, String) {
  match &r.kind {
    RecommendationKind::Curated { entry } => (
      format!("{} ({})", entry.id, entry.quant),
      r.justification.clone(),
    ),
    RecommendationKind::OnDisk {
      path, architecture, ..
    } => (
      format!(
        "On-disk: {}",
        path
          .file_name()
          .and_then(|s| s.to_str())
          .unwrap_or("<unknown>")
      ),
      architecture
        .clone()
        .unwrap_or_else(|| "unknown arch".into()),
    ),
    RecommendationKind::Escape => ("Paste an HF repo id…".into(), String::new()),
  }
}

/// Single TTY check shared by every picker. Reads stdout (not stderr
/// or `console::user_attended()`) so it matches the off-condition
/// used by `cli::colors::init` — an agent piping stdout but leaving
/// stderr attached gets the same answer from both the color policy
/// and the prompt fallback path.
///
/// `LLAMASTASH_ASSUME_NON_TTY=1` forces a `false` return regardless of
/// the real fd state. Cargo's libtest captures stdout at the print
/// layer, not the file-descriptor layer, so an interactive
/// `cargo test` leaves the binary's fd 1 attached to the user's
/// terminal. Tests that exercise the non-TTY branches set this env
/// var so they don't fall through into a blocking cliclack prompt.
fn stdout_is_terminal() -> bool {
  if std::env::var_os("LLAMASTASH_ASSUME_NON_TTY").is_some_and(|v| v == "1") {
    return false;
  }
  std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::*;
  use crate::cli::cli_args::{ConfigOverride, InitArgs, InstallOverride, ModelOverride};
  use crate::init::detection::BinarySource;

  fn empty_args() -> InitArgs {
    InitArgs {
      recommended: false,
      yes: false,
      json: false,
      offline: false,
      only: Vec::new(),
      skip: Vec::new(),
      install: None,
      model: None,
      config_choice: None,
      revision: None,
      no_tui: true,
      integrations: Vec::new(),
      step: None,
    }
  }

  fn base_hw() -> HardwareSnapshot {
    HardwareSnapshot {
      gpu: GpuInfo::CpuOnly,
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  #[test]
  fn hardware_line_renders_three_segments() {
    let line = hardware_line(&base_hw());
    let segments: Vec<&str> = line.split('\n').collect();
    assert_eq!(segments.len(), 3, "expected 3 lines, got {segments:?}");
    assert!(segments[0].starts_with("gpu: "));
    assert!(segments[1].starts_with("cpu: "));
    assert!(segments[2].starts_with("sys: "));
  }

  #[test]
  fn hardware_line_surfaces_gpu_device_name() {
    let mut hw = base_hw();
    hw.gpu = GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "GeForce RTX 4090".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    };
    hw.vram_bytes = Some(24 * 1024 * 1024 * 1024);
    hw.gpu_device_count = 1;
    let line = hardware_line(&hw);
    assert!(
      line.contains("NVIDIA GeForce RTX 4090"),
      "expected NVIDIA + device name, got {line:?}"
    );
    // Verdict vocabulary now matches `status` / `doctor`: a discrete
    // card reads `(discrete)`, not the bare `VRAM` word.
    assert!(
      line.contains("24.0 GiB (discrete)"),
      "expected discrete verdict, got {line:?}"
    );
  }

  #[test]
  fn hardware_line_apple_metal_reports_unified() {
    let mut hw = base_hw();
    hw.os = OsFamily::MacOs;
    hw.cpu_arch = CpuArch::Arm64;
    let bytes = 64u64 * 1024 * 1024 * 1024;
    hw.gpu = GpuInfo::AppleMetal {
      total_memory_bytes: bytes,
    };
    hw.vram_bytes = Some((bytes as f64 * 0.75) as u64);
    hw.gpu_device_count = 1;
    let line = hardware_line(&hw);
    assert!(line.contains("Apple Silicon"));
    assert!(line.contains("unified"));
    assert!(line.contains("macos/arm64"));
  }

  #[test]
  fn hardware_line_multi_gpu_counts() {
    let mut hw = base_hw();
    let device = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 0,
      utilization_pct: None,
      temperature_c: None,
      ..Default::default()
    };
    hw.gpu = GpuInfo::Nvidia {
      devices: vec![device.clone(), device],
    };
    hw.vram_bytes = Some(24 * 1024 * 1024 * 1024);
    hw.gpu_device_count = 2;
    let line = hardware_line(&hw);
    assert!(
      line.contains("2× NVIDIA RTX 4090"),
      "expected multi-GPU prefix, got {line:?}"
    );
  }

  #[test]
  fn hardware_line_cpu_only_says_so() {
    let line = hardware_line(&base_hw());
    assert!(line.contains("(none — CPU only)"));
    assert!(!line.contains("VRAM"));
  }

  #[test]
  fn hardware_line_elides_disk_when_zero_and_includes_when_nonzero() {
    let mut hw = base_hw();
    let without_disk = hardware_line(&hw);
    assert!(!without_disk.contains("disk free"));
    hw.disk_free_bytes = 100 * 1024 * 1024 * 1024;
    let with_disk = hardware_line(&hw);
    assert!(with_disk.contains("100.0 GiB disk free"));
  }

  #[test]
  fn hardware_line_uses_brand_when_present_and_arch_when_not() {
    let mut hw = base_hw();
    let bare = hardware_line(&hw);
    assert!(bare.contains("cpu: X86_64 CPU"));
    hw.cpu_brand = "AMD Ryzen AI MAX+ 395".into();
    hw.cpu_cores = 16;
    hw.cpu_features = vec!["AVX2".into(), "AVX-512".into()];
    let branded = hardware_line(&hw);
    assert!(branded.contains("AMD Ryzen AI MAX+ 395"));
    assert!(branded.contains("16 cores"));
    assert!(branded.contains("AVX2 AVX-512"));
  }

  #[test]
  fn render_diff_preview_marks_added_and_changed_paths() {
    use crate::init::config_writer::RedactedDiffEntry;
    let entries = vec![
      RedactedDiffEntry {
        path: "llama_server_path".into(),
        kind: "added",
        value_yaml: "/opt/llama-server".into(),
      },
      RedactedDiffEntry {
        path: "port_range.start".into(),
        kind: "changed",
        value_yaml: "50000".into(),
      },
    ];
    let rendered = render_diff_preview(&entries);
    let plain = console::strip_ansi_codes(&rendered);
    assert!(plain.contains("config diff (preview):"));
    assert!(plain.contains("+ llama_server_path: /opt/llama-server"));
    assert!(plain.contains("~ port_range.start: 50000"));
  }

  #[test]
  fn render_diff_preview_handles_empty() {
    let rendered = render_diff_preview(&[]);
    let plain = console::strip_ansi_codes(&rendered);
    assert!(plain.contains("(no changes)"));
  }

  fn no_existing_binary() -> BinaryPresence {
    BinaryPresence {
      resolved_path: None,
      source: BinarySource::None,
    }
  }

  fn existing_binary(p: &str) -> BinaryPresence {
    BinaryPresence {
      resolved_path: Some(PathBuf::from(p)),
      source: BinarySource::CommonLocation,
    }
  }

  #[test]
  fn is_recommended_reads_recommended_field() {
    let mut args = empty_args();
    args.recommended = true;
    assert!(is_recommended(&args));
  }

  #[test]
  fn is_recommended_reads_yes_alias() {
    let mut args = empty_args();
    args.yes = true;
    assert!(is_recommended(&args));
  }

  #[test]
  fn is_recommended_false_for_default_args() {
    assert!(!is_recommended(&empty_args()));
  }

  #[test]
  fn install_override_brew_short_circuits() {
    let result = install_override_to_choice(InstallOverride::Brew, &no_existing_binary());
    assert!(matches!(result, Ok(InstallChoice::Brew)));
  }

  #[test]
  fn install_override_gh_releases_short_circuits() {
    let result = install_override_to_choice(InstallOverride::GhReleases, &no_existing_binary());
    assert!(matches!(result, Ok(InstallChoice::GhReleases)));
  }

  #[test]
  fn install_override_custom_carries_path() {
    let result = install_override_to_choice(
      InstallOverride::Custom(PathBuf::from("/opt/llama-server")),
      &no_existing_binary(),
    );
    match result {
      Ok(InstallChoice::CustomPath(p)) => assert_eq!(p, PathBuf::from("/opt/llama-server")),
      other => panic!("expected CustomPath, got {other:?}"),
    }
  }

  #[test]
  fn install_override_existing_uses_detected_path() {
    let result = install_override_to_choice(
      InstallOverride::Existing,
      &existing_binary("/usr/local/bin/llama-server"),
    );
    match result {
      Ok(InstallChoice::CustomPath(p)) => {
        assert_eq!(p, PathBuf::from("/usr/local/bin/llama-server"))
      }
      other => panic!("expected CustomPath, got {other:?}"),
    }
  }

  #[test]
  fn install_override_existing_errors_when_no_binary_detected() {
    let result = install_override_to_choice(InstallOverride::Existing, &no_existing_binary());
    match result {
      Err(exit) => assert_eq!(exit.code, INIT_ABORTED),
      other => panic!("expected Err(INIT_ABORTED), got {other:?}"),
    }
  }

  #[test]
  fn search_result_hint_joins_available_metadata() {
    use crate::init::hf_api::HfGgufMeta;
    let mut r = HfSearchResult {
      repo_id: "owner/Model-GGUF".into(),
      downloads: Some(1_234_567),
      likes: None,
      last_modified: None,
      pipeline_tag: None,
      tags: vec![],
      gguf: Some(HfGgufMeta {
        total: Some(8_030_261_248),
        total_file_size: Some(5_732_991_008),
      }),
    };
    // params · size · downloads, in order, only the parts present.
    assert_eq!(search_result_hint(&r), "8B · 5.3 GiB · ↓1.2M");
    r.gguf = None;
    r.downloads = None;
    assert_eq!(search_result_hint(&r), "", "no metadata → empty hint");
  }

  #[tokio::test]
  async fn pick_model_paste_override_carries_repo() {
    let mut args = empty_args();
    args.model = Some(ModelOverride::Paste("owner/repo".into()));
    let result = pick_model(&args, &FetchClient::offline(), &[])
      .await
      .expect("override should not fail");
    match result {
      ModelChoice::Paste(s) => assert_eq!(s, "owner/repo"),
      other => panic!("expected Paste, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn pick_model_none_override_returns_skip() {
    let mut args = empty_args();
    args.model = Some(ModelOverride::None);
    let result = pick_model(&args, &FetchClient::offline(), &[])
      .await
      .expect("override should not fail");
    assert!(matches!(result, ModelChoice::Skip));
  }

  #[tokio::test]
  async fn pick_model_recommended_override_with_empty_recs_returns_skip() {
    let mut args = empty_args();
    args.model = Some(ModelOverride::Recommended);
    let result = pick_model(&args, &FetchClient::offline(), &[])
      .await
      .expect("override should not fail");
    assert!(matches!(result, ModelChoice::Skip));
  }

  #[tokio::test]
  async fn pick_model_recommended_mode_with_empty_recs_returns_skip() {
    let mut args = empty_args();
    args.recommended = true;
    let result = pick_model(&args, &FetchClient::offline(), &[])
      .await
      .expect("recommended mode should not fail without recs");
    assert!(matches!(result, ModelChoice::Skip));
  }

  #[tokio::test]
  async fn confirm_config_write_override_write_returns_true() {
    // The override arm at the top of `confirm_config_write` resolves
    // before any await/cliclack interaction, so this test works under
    // cargo test's non-TTY default without touching a terminal.
    let mut args = empty_args();
    args.config_choice = Some(ConfigOverride::Write);
    let confirmed = confirm_config_write(&args, "")
      .await
      .expect("override should not fail");
    assert!(confirmed, "ConfigOverride::Write must resolve to true");
  }

  #[tokio::test]
  async fn confirm_config_write_override_skip_returns_false() {
    let mut args = empty_args();
    args.config_choice = Some(ConfigOverride::Skip);
    let confirmed = confirm_config_write(&args, "")
      .await
      .expect("override should not fail");
    assert!(!confirmed, "ConfigOverride::Skip must resolve to false");
  }

  #[tokio::test]
  async fn confirm_config_write_recommended_returns_true() {
    let mut args = empty_args();
    args.recommended = true;
    let confirmed = confirm_config_write(&args, "")
      .await
      .expect("recommended should not fail");
    assert!(confirmed, "recommended mode must accept the write");
  }

  #[tokio::test]
  async fn confirm_config_write_non_tty_without_consent_errors() {
    // With neither `--recommended` nor `--config-step` set, the wizard
    // must refuse rather than silently auto-write (closes the
    // regression that ADV-1 caught). Force non-TTY explicitly — an
    // interactive `cargo test` leaves the binary's stdout attached to
    // the user's terminal, so the natural fd check would fall through
    // into a blocking cliclack prompt.
    std::env::set_var("LLAMASTASH_ASSUME_NON_TTY", "1");
    let args = empty_args();
    let result = confirm_config_write(&args, "").await;
    match result {
      Err(exit) => {
        assert_eq!(exit.code, INIT_ABORTED, "must abort with INIT_ABORTED");
        assert!(
          exit.to_string().contains("explicit consent"),
          "error must explain how to provide consent, got `{}`",
          exit
        );
      }
      Ok(_) => panic!("non-TTY without consent must not silently auto-accept"),
    }
  }

  #[tokio::test]
  async fn pick_install_method_install_override_beats_recommended() {
    // W3 contract: an explicit `--install` overrides recommended-mode
    // adoption. The picker's arm-1 (override) fires before the arm-2
    // (recommended → default) check.
    let mut args = empty_args();
    args.recommended = true;
    args.install = Some(InstallOverride::Brew);
    let result = pick_install_method(&args, InstallChoice::GhReleases, &no_existing_binary())
      .await
      .expect("override should not fail");
    assert!(
      matches!(result, InstallChoice::Brew),
      "got {result:?}, expected Brew"
    );
  }

  #[tokio::test]
  async fn pick_install_method_recommended_short_circuits_to_default() {
    let mut args = empty_args();
    args.recommended = true;
    let result = pick_install_method(&args, InstallChoice::Brew, &no_existing_binary())
      .await
      .expect("recommended should not fail");
    assert!(
      matches!(result, InstallChoice::Brew),
      "got {result:?}, expected the supplied default Brew"
    );
  }

  #[test]
  fn build_install_items_always_includes_custom_path_sentinel() {
    let (_, items) = build_install_items(&InstallChoice::GhReleases, &no_existing_binary(), true);
    let last = items.last().expect("items must not be empty");
    assert!(
      matches!(last.0, InstallPick::PromptCustomPath),
      "Custom path sentinel must be last item, got {:?}",
      last.0
    );
    assert_eq!(last.1, "Custom path…");
  }

  #[test]
  fn build_install_items_with_existing_binary_still_appends_custom_path_sentinel() {
    let (_, items) = build_install_items(
      &InstallChoice::GhReleases,
      &existing_binary("/opt/llama-server"),
      true,
    );
    let last = items.last().expect("items must not be empty");
    assert!(
      matches!(last.0, InstallPick::PromptCustomPath),
      "Custom path sentinel must be last item even when an existing binary is detected"
    );
    // Detected-binary item still present as one of the resolved picks.
    let has_existing = items.iter().any(|(p, _, _)| {
      matches!(p, InstallPick::Resolved(InstallChoice::CustomPath(path)) if path == &PathBuf::from("/opt/llama-server"))
    });
    assert!(has_existing, "detected-binary pick must still be offered");
  }

  fn has_brew_item(items: &[(InstallPick, String, String)]) -> bool {
    items
      .iter()
      .any(|(p, _, _)| matches!(p, InstallPick::Resolved(InstallChoice::Brew)))
  }

  #[test]
  fn build_install_items_includes_homebrew_when_brew_available() {
    let (_, items) = build_install_items(&InstallChoice::GhReleases, &no_existing_binary(), true);
    assert!(
      has_brew_item(&items),
      "Homebrew must be offered when brew is available"
    );
  }

  #[test]
  fn build_install_items_omits_homebrew_when_brew_unavailable() {
    let (_, items) = build_install_items(&InstallChoice::GhReleases, &no_existing_binary(), false);
    assert!(
      !has_brew_item(&items),
      "Homebrew must not be offered when brew is unavailable (e.g. Windows, or brew not on PATH)"
    );
    // GH Releases + the Custom path… sentinel still stand.
    assert!(items
      .iter()
      .any(|(p, _, _)| matches!(p, InstallPick::Resolved(InstallChoice::GhReleases))));
    assert!(matches!(
      items.last().expect("items must not be empty").0,
      InstallPick::PromptCustomPath
    ));
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn brew_offer_unavailable_on_windows() {
    assert!(
      !brew_offer_available(),
      "Homebrew is macOS/Linux-only and must never be offered on Windows"
    );
  }

  #[tokio::test]
  async fn pick_install_method_non_tty_falls_back_silently() {
    // With neither override nor recommended set, the picker silently
    // returns the default — the consolidated warning (#30) lives in
    // wizard::run, not here. Force non-TTY explicitly so the test
    // still exercises the fallback when run interactively (cargo's
    // libtest captures at the print layer, not fd 1).
    std::env::set_var("LLAMASTASH_ASSUME_NON_TTY", "1");
    let args = empty_args();
    let result = pick_install_method(&args, InstallChoice::GhReleases, &no_existing_binary())
      .await
      .expect("non-TTY fallback should not fail");
    assert!(
      matches!(result, InstallChoice::GhReleases),
      "got {result:?}, expected GhReleases default"
    );
  }
}
