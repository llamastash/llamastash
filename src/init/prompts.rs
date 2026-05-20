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
use crate::init::benchmark::ModelEntry;
use crate::init::detection::{BinaryPresence, HardwareSnapshot};
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
pub fn outro(summary: &InitSummary) {
  let mut body = String::new();
  body.push_str(&format!("steps_ran: {:?}\n", summary.steps_ran));
  if !summary.steps_skipped.is_empty() {
    body.push_str(&format!("steps_skipped: {:?}\n", summary.steps_skipped));
  }
  if let Some(install) = &summary.install {
    body.push_str(&format!(
      "install: {} → {}\n",
      install.method,
      install.path.display()
    ));
  }
  if let Some(model) = &summary.model {
    if !model.repo.is_empty() {
      body.push_str(&format!(
        "model: {} ({:.1} MiB across {} file(s))\n",
        model.repo,
        model.total_bytes as f64 / (1024.0 * 1024.0),
        model.files.len()
      ));
    }
  }
  if let Some(cfg) = &summary.config {
    body.push_str(&format!(
      "config: wrote {} bytes to {}\n",
      cfg.written_bytes,
      cfg.path.display()
    ));
  }
  body.push_str(
    "Next: run `llamastash` to enter the TUI, or `llamastash list` to see discovered models.",
  );
  let _ = cliclack::outro(body);
}

/// Pre-formatted "detected: …" line used by both `intro` and the
/// non-TTY warning path's context message.
fn hardware_line(hw: &HardwareSnapshot) -> String {
  let vram = match hw.vram_bytes {
    Some(b) => format!("{:.1} GB VRAM", b as f64 / 1_073_741_824.0),
    None => "no GPU".to_string(),
  };
  let ram = format!("{:.0} GB RAM", hw.ram_total_bytes as f64 / 1_073_741_824.0);
  format!(
    "detected: {} · {ram} · {vram} · {:?}/{:?}",
    hw.gpu.label(),
    hw.os,
    hw.cpu_arch
  )
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
  let (initial_idx, items) = build_install_items(&default, existing);
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
pub async fn pick_model(args: &InitArgs, recs: &[Recommendation]) -> Result<ModelChoice, CliExit> {
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
  // Return the chosen Recommendation directly from the blocking task
  // so the index never crosses the spawn_blocking boundary back to
  // an unrelated slice. Removes the "two parallel slices must stay
  // in sync" hazard the prior code had.
  let chosen: Option<Recommendation> = tokio::task::spawn_blocking(move || {
    let mut select = cliclack::select("Pick a model").initial_value(initial_idx);
    for (i, r) in owned_recs.iter().enumerate() {
      let (label, hint) = render_recommendation(r);
      select = select.item(i, label, hint);
    }
    let idx = select.interact()?;
    Ok::<_, std::io::Error>(owned_recs.into_iter().nth(idx))
  })
  .await
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
  .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: model prompt: {e}")))?;
  match chosen.map(|r| r.kind) {
    Some(RecommendationKind::Curated { entry }) => Ok(ModelChoice::Curated(entry)),
    Some(RecommendationKind::Escape) => {
      let repo: String = tokio::task::spawn_blocking(|| {
        cliclack::input("Paste an HF repo id")
          .placeholder("owner/repo")
          .validate(|s: &String| {
            if !s.contains('/') {
              return Err("expected `owner/repo`");
            }
            if s.chars().any(char::is_whitespace) {
              return Err("must not contain whitespace");
            }
            // Control characters (incl. null bytes) flow into
            // filesystem paths + URLs downstream; reject at the
            // validator boundary, not after they cause a confusing
            // OS-level error.
            if s.chars().any(char::is_control) {
              return Err("must not contain control characters");
            }
            Ok(())
          })
          .interact()
      })
      .await
      .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: prompt join failed: {e}")))?
      .map_err(|e| CliExit::new(INIT_ABORTED, format!("init: paste prompt: {e}")))?;
      Ok(ModelChoice::Paste(repo))
    }
    // OnDisk variants were filtered out above; treat any residual
    // None (empty selection, or unexpected variant) as Skip.
    _ => Ok(ModelChoice::Skip),
  }
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

/// Build the cliclack-select items list for the install prompt and
/// pick which index should be the initial cursor position. Returns
/// `(initial_index, items)` where each item is
/// `(pick, label, hint)`. The cliclack `Select` is keyed by `usize`
/// index because `InstallPick` does not implement `Eq`. The trailing
/// "Custom path…" sentinel lets users adopt a self-built or otherwise-
/// installed `llama-server` interactively (mirrors the `--install
/// custom:PATH` CLI override).
fn build_install_items(
  default: &InstallChoice,
  existing: &BinaryPresence,
) -> (usize, Vec<(InstallPick, String, String)>) {
  let mut items: Vec<(InstallPick, String, String)> = vec![
    (
      InstallPick::Resolved(InstallChoice::GhReleases),
      "GitHub Releases".into(),
      "verified asset for this host".into(),
    ),
    (
      InstallPick::Resolved(InstallChoice::Brew),
      "Homebrew".into(),
      "brew install --quiet llama.cpp".into(),
    ),
  ];
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
fn stdout_is_terminal() -> bool {
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
    }
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

  #[tokio::test]
  async fn pick_model_paste_override_carries_repo() {
    let mut args = empty_args();
    args.model = Some(ModelOverride::Paste("owner/repo".into()));
    let result = pick_model(&args, &[])
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
    let result = pick_model(&args, &[])
      .await
      .expect("override should not fail");
    assert!(matches!(result, ModelChoice::Skip));
  }

  #[tokio::test]
  async fn pick_model_recommended_override_with_empty_recs_returns_skip() {
    let mut args = empty_args();
    args.model = Some(ModelOverride::Recommended);
    let result = pick_model(&args, &[])
      .await
      .expect("override should not fail");
    assert!(matches!(result, ModelChoice::Skip));
  }

  #[tokio::test]
  async fn pick_model_recommended_mode_with_empty_recs_returns_skip() {
    let mut args = empty_args();
    args.recommended = true;
    let result = pick_model(&args, &[])
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
    // cargo test is always non-TTY; with neither `--recommended` nor
    // `--config-step` set, the wizard must refuse rather than silently
    // auto-write (closes the regression that ADV-1 caught).
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
    let (_, items) = build_install_items(&InstallChoice::GhReleases, &no_existing_binary());
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

  #[tokio::test]
  async fn pick_install_method_non_tty_falls_back_silently() {
    // cargo test is always non-TTY. With neither override nor
    // recommended set, the picker silently returns the default —
    // the consolidated warning (#30) lives in wizard::run, not here.
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
