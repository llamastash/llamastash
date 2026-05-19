//! Command-line argument schema (clap derive).
//!
//! Top-level args are `global = true` so every subcommand inherits them, and
//! a missing subcommand routes to the TUI. The shape of each subcommand
//! mirrors the agent-facing surface defined in
//! `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md` (R35) —
//! handlers are stubbed until their respective implementation units land.

use std::{ffi::OsString, path::PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::banner::BANNER;

#[derive(Parser, Debug)]
#[command(
  name = "llamadash",
  version,
  about = "Fast keyboard-driven TUI + CLI for local llama.cpp models",
  long_about = None,
  before_help = BANNER,
)]
pub struct Cli {
  /// Path to a YAML config file (overrides `LLAMADASH_CONFIG`).
  #[arg(long, value_name = "PATH", global = true)]
  pub config: Option<PathBuf>,

  /// Path to the `llama-server` binary (overrides `LLAMADASH_LLAMA_SERVER`).
  #[arg(long, value_name = "PATH", global = true)]
  pub llama_server: Option<PathBuf>,

  /// Extra directory to scan for GGUF models. Repeatable.
  #[arg(short = 'p', long = "model-path", value_name = "DIR", global = true)]
  pub model_paths: Vec<PathBuf>,

  /// Disable filesystem scanning of default and configured roots.
  #[arg(long, global = true)]
  pub no_scan: bool,

  /// Fail fast if the daemon is not already running, instead of auto-spawning it.
  /// Useful for scripted/agent environments that want deterministic failure.
  #[arg(long, global = true)]
  pub no_spawn: bool,

  /// Verbose logging (Debug level instead of Info).
  #[arg(short, long, global = true, action = ArgAction::SetTrue, conflicts_with = "quiet")]
  pub verbose: bool,

  /// Suppress success prose on mutating commands (`start`, `stop`,
  /// `favorites add/remove`, `presets save/delete`). Errors still
  /// print. Useful for scripts that branch on exit code alone.
  #[arg(short = 'q', long, global = true, action = ArgAction::SetTrue)]
  pub quiet: bool,

  /// Disable ANSI color output. Color is also disabled automatically
  /// when `NO_COLOR` is set in the environment (any non-empty value;
  /// see https://no-color.org) or when stdout is not a terminal
  /// (piped / redirected). `--json` output is never colored.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  pub no_colors: bool,

  /// Render one frame of the TUI to stdout as plain text and exit
  /// instead of entering the interactive loop. Connects to (or auto-
  /// spawns) the daemon, primes `list_models` + `status`, draws one
  /// frame against `ratatui`'s headless `TestBackend`, and prints the
  /// resulting cell grid. The same path the e2e render test uses, so
  /// agents and CI can sanity-check the UI without a terminal.
  ///
  /// Only honoured when no subcommand is given (i.e. the TUI entry).
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  pub render: bool,

  /// Optional `WIDTHxHEIGHT` (e.g. `120x40`) for `--render`. Defaults
  /// to `120x40` which matches the e2e test fixture geometry.
  #[arg(long, value_name = "WxH", global = true)]
  pub render_size: Option<String>,

  #[command(subcommand)]
  pub command: Option<Command>,
}

/// Parse a `WIDTHxHEIGHT` string into a `(u16, u16)` tuple. Returns
/// `Err` with a user-facing message on malformed input.
pub fn parse_render_size(raw: &str) -> Result<(u16, u16), String> {
  let (w, h) = raw
    .split_once('x')
    .ok_or_else(|| format!("expected `WxH`, got `{raw}`"))?;
  let w: u16 = w
    .trim()
    .parse()
    .map_err(|_| format!("invalid width in `{raw}`"))?;
  let h: u16 = h
    .trim()
    .parse()
    .map_err(|_| format!("invalid height in `{raw}`"))?;
  if w < 40 || h < 10 {
    return Err(format!(
      "render size `{raw}` is too small; minimum is 40x10"
    ));
  }
  Ok((w, h))
}

#[derive(Subcommand, Debug)]
pub enum Command {
  /// Manage the background daemon (auto-spawned on attach).
  #[command(subcommand)]
  Daemon(DaemonAction),
  /// List discovered models.
  List(ListArgs),
  /// Start a model.
  Start(StartArgs),
  /// Stop a running model.
  Stop(StopArgs),
  /// Show daemon and running-model status.
  Status(StatusArgs),
  /// Tail or follow a running model's log.
  Logs(LogsArgs),
  /// Manage named launch presets for a model.
  Presets(PresetsArgs),
  /// Pull a GGUF from `HuggingFace`.
  ///
  /// MVP shape (v2-R65): `llamadash pull <hf-repo>` downloads every
  /// GGUF in the repo (or a single shard set when the repo ships
  /// multi-shard files) into the canonical HF cache layout
  /// (`~/.cache/huggingface/hub/models--<owner>--<repo>/...`) so the
  /// next `llamadash list` rescan finds it. `--json` emits the
  /// summary; otherwise progress streams to stderr.
  Pull(PullArgs),
  /// Run the first-time setup / maintenance wizard (v2-R48).
  Init(InitArgs),
  /// Read-only diagnostic — compares current detection against the
  /// recorded `init_snapshot` baseline (v2-R74).
  Doctor(DoctorArgs),
  /// Mark, unmark, and list favorite models.
  Favorites(FavoritesArgs),
  /// Inspect the last successful `start_model` params for one or
  /// every catalog model. Surfaces the daemon's `last_params_list`
  /// IPC so agents can answer "how did I launch this model last
  /// time" without going through the TUI.
  LastParams(LastParamsArgs),
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
  /// Start the daemon (no-op if already running).
  Start {
    /// Background the daemon by detaching from the controlling terminal.
    #[arg(long)]
    detach: bool,
    /// Internal hand-off: state directory to use instead of XDG defaults.
    /// `start_detached` propagates this to the re-exec'd child so tests
    /// and alternate deployments can drive the daemon at a custom path.
    /// Hidden from `--help` because end users should reach for the
    /// config file or XDG env vars instead.
    #[arg(long, value_name = "PATH", hide = true)]
    state_dir: Option<PathBuf>,
    /// Internal hand-off: socket path to bind instead of the
    /// platform-default runtime socket. See `state-dir` for rationale.
    #[arg(long, value_name = "PATH", hide = true)]
    socket_path: Option<PathBuf>,
  },
  /// Stop the running daemon. Running models keep running.
  Stop,
  /// Print daemon PID, uptime, and connected-client count.
  Status,
}

#[derive(Args, Debug)]
pub struct ListArgs {
  /// Emit JSON instead of the human-readable table.
  #[arg(long)]
  pub json: bool,
  /// Filter substring matched against name, path, arch, and quant.
  #[arg(long, value_name = "PATTERN")]
  pub filter: Option<String>,
}

#[derive(Args, Debug)]
pub struct StartArgs {
  /// Model reference: name substring, absolute path, or canonical model id.
  pub model: String,
  /// Saved preset to load before applying overrides.
  #[arg(long, value_name = "NAME")]
  pub preset: Option<String>,
  /// Context length override.
  #[arg(long, value_name = "TOKENS")]
  pub ctx: Option<u32>,
  /// Pin the listening port (otherwise auto-allocated from the config range).
  #[arg(long, value_name = "PORT")]
  pub port: Option<u16>,
  /// Enable or disable reasoning (bundles `--reasoning-format deepseek --jinja`
  /// + smoke-test `<think>` collapse). Advanced panel can unbundle.
  #[arg(long, value_enum)]
  pub reasoning: Option<ReasoningFlag>,
  /// Force the launch mode. `None` means "infer from GGUF metadata; error
  /// out if the GGUF mode hint is `Unknown`" — handlers must NOT silently
  /// default to `chat` when this is `None`.
  #[arg(long, value_enum)]
  pub mode: Option<LaunchMode>,
  /// Extra flags forwarded verbatim to `llama-server` after `--`.
  #[arg(last = true, value_name = "ARG")]
  pub extra: Vec<OsString>,
  /// Emit JSON instead of human-readable success prose. Stable
  /// shape: `{ "name", "launch_id", "port", "pid", "preset",
  /// "path" }`.
  #[arg(long)]
  pub json: bool,
}

#[derive(Args, Debug)]
pub struct StopArgs {
  /// Model id or port to stop. Required unless `--all` is set.
  #[arg(required_unless_present = "all")]
  pub target: Option<String>,
  /// Stop every model owned by this daemon.
  #[arg(long, conflicts_with = "target")]
  pub all: bool,
  /// Skip the confirmation prompt.
  #[arg(short, long)]
  pub yes: bool,
  /// Seconds the daemon waits between SIGTERM and SIGKILL. Mirrors
  /// the IPC parameter; defaults to 5.
  #[arg(long, value_name = "SECONDS")]
  pub grace_secs: Option<u64>,
  /// Emit JSON instead of human-readable success prose. Stable
  /// shape: `{ "stopped": [...], "count": N }` for `--all`, a
  /// single-row object for one-shot stop.
  #[arg(long)]
  pub json: bool,
}

#[derive(Args, Debug)]
pub struct LastParamsArgs {
  /// Optional model id, path, or name substring. When omitted, all
  /// recorded last-params rows are returned.
  pub target: Option<String>,
  /// Emit JSON instead of the human-readable table.
  #[arg(long)]
  pub json: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
  /// Optional model id or port to scope the response to a single model.
  pub target: Option<String>,
  /// Emit JSON instead of the human-readable status block.
  #[arg(long)]
  pub json: bool,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
  /// Model id or port whose log to tail.
  pub target: String,
  /// Follow the log instead of printing the current tail.
  #[arg(short, long)]
  pub follow: bool,
  /// Number of trailing lines to print before following.
  #[arg(short = 'n', long, value_name = "N")]
  pub lines: Option<u32>,
  /// Emit JSON Lines (one object per poll) instead of raw text.
  /// One-shot `logs` emits a single `{ "launch_id": "...", "lines":
  /// [...] }` object; `--follow --json` emits one object per
  /// refresh containing only the newly-arrived lines.
  #[arg(long)]
  pub json: bool,
}

#[derive(Args, Debug)]
pub struct PresetsArgs {
  /// Model reference: name substring, absolute path, or canonical model id.
  pub model: String,
  #[command(subcommand)]
  pub action: PresetsAction,
}

#[derive(Subcommand, Debug)]
pub enum PresetsAction {
  /// List saved presets for this model.
  List {
    /// Emit JSON instead of the human-readable table.
    #[arg(long)]
    json: bool,
  },
  /// Save the current/passed launch params under `name`.
  Save {
    name: String,
    #[arg(long, value_name = "TOKENS")]
    ctx: Option<u32>,
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
    #[arg(long, value_enum)]
    reasoning: Option<ReasoningFlag>,
    #[arg(long, value_enum)]
    mode: Option<LaunchMode>,
    #[arg(last = true, value_name = "ARG")]
    extra: Vec<OsString>,
    /// Emit JSON. `{ "action": "save", "name": "...", "replaced":
    /// bool }`.
    #[arg(long)]
    json: bool,
  },
  /// Delete a saved preset.
  Delete {
    name: String,
    /// Emit JSON. `{ "action": "delete", "name": "...", "deleted":
    /// bool }`.
    #[arg(long)]
    json: bool,
  },
  /// Print a saved preset's parameters.
  Show {
    name: String,
    #[arg(long)]
    json: bool,
  },
}

#[derive(Args, Debug)]
pub struct PullArgs {
  /// `HuggingFace` repo id (`owner/repo`), optionally with a
  /// `:filename.gguf` suffix to pin one file (defaults to all `.gguf`
  /// in the repo). Files land in the canonical HF cache layout that
  /// discovery already scans.
  pub repo: String,
  /// Emit a structured JSON summary on success instead of a
  /// human-readable stream. Shape:
  /// `{repo, revision, files: [...absolute paths], total_bytes}`.
  /// Per-shard progress JSON in `--verbose` mode is not yet
  /// implemented — v2 emits a single summary line on success;
  /// download progress goes to stderr unstructured. v2.1 backlog
  /// tracks line-buffered progress events for long multi-shard
  /// pulls.
  #[arg(long)]
  pub json: bool,
  /// Disable outbound network. Equivalent to `LLAMADASH_OFFLINE=1`.
  /// Pull always requires network so this exits with `PULL_FAILED`
  /// (69) up-front; honored for parity with `init --offline`.
  #[arg(long, env = "LLAMADASH_OFFLINE")]
  pub offline: bool,
}

#[derive(Args, Debug)]
pub struct InitArgs {
  /// Use hardware-aware defaults for every step; do not prompt. Pair
  /// with `--json` for agent consumption. Integrity-check failures
  /// still abort with the documented exit codes — they are never
  /// silently downgraded.
  #[arg(long, action = ArgAction::SetTrue)]
  pub recommended: bool,
  /// Backward-compat alias for `--recommended`. Hidden from `--help`
  /// because new invocations should reach for `--recommended`, but
  /// preserved indefinitely so existing scripts and agents keep
  /// working without a deprecation warning at runtime.
  #[arg(long, hide = true)]
  pub yes: bool,
  /// Emit a single structured summary at completion. Per-step progress
  /// goes to stderr (only in `--verbose`). Mutually compatible with
  /// `--recommended`.
  #[arg(long)]
  pub json: bool,
  /// Disable outbound network. Steps that require network are skipped
  /// with actionable hints. Equivalent to `LLAMADASH_OFFLINE=1`.
  #[arg(long, env = "LLAMADASH_OFFLINE")]
  pub offline: bool,
  /// Run only these step(s). Repeatable; values are comma-separable.
  /// Mutually exclusive with `--skip`.
  #[arg(
    long,
    value_name = "STEP",
    value_delimiter = ',',
    action = ArgAction::Append,
    conflicts_with = "skip",
    value_enum,
  )]
  pub only: Vec<InitStep>,
  /// Skip these step(s). Repeatable; values are comma-separable.
  /// Mutually exclusive with `--only`.
  #[arg(
    long,
    value_name = "STEP",
    value_delimiter = ',',
    action = ArgAction::Append,
    conflicts_with = "only",
    value_enum,
  )]
  pub skip: Vec<InitStep>,
  /// Pre-answer the install-method prompt. Accepted values:
  /// `brew`, `gh-releases`, `existing`, `custom:<PATH>` (relative
  /// paths are accepted at parse time; runtime integrity checks
  /// decide if they are usable). When supplied for a step that
  /// `--skip` excludes, the wizard emits a stderr warning and
  /// proceeds.
  #[arg(long, value_name = "CHOICE", value_parser = parse_install_override)]
  pub install: Option<InstallOverride>,
  /// Pre-answer the model-pick prompt. Accepted values:
  /// `recommended`, `none`, `<owner>/<repo>` (HuggingFace repo id).
  /// When supplied for a step that `--skip` excludes, the wizard
  /// emits a stderr warning and proceeds.
  #[arg(long, value_name = "CHOICE", value_parser = parse_model_override)]
  pub model: Option<ModelOverride>,
  /// Pre-answer the config-write confirm. Accepted values: `write`,
  /// `skip`. When supplied for a step that `--skip` excludes, the
  /// wizard emits a stderr warning and proceeds.
  ///
  /// Long flag is `--config-step` (not `--config`) because the
  /// top-level `--config <PATH>` is `global = true` and clap's debug
  /// assert refuses two args with the same long name even on disjoint
  /// subcommand scopes.
  #[arg(long = "config-step", value_name = "CHOICE", value_enum)]
  pub config_choice: Option<ConfigOverride>,
}

/// Per-step override for `--install`. When set, the wizard's
/// install-method prompt is suppressed and the override value is
/// used directly. The `Custom` variant carries the user-supplied
/// path; the wizard's existing `is_safe_to_adopt` integrity check
/// runs at step time, not at parse time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOverride {
  Brew,
  GhReleases,
  Existing,
  Custom(PathBuf),
}

/// Per-step override for `--model`. `Recommended` selects the
/// top recommender pick; `None` skips the model-download step
/// entirely; `Paste` carries an `<owner>/<repo>` HF id that is
/// downloaded directly (bypassing the recommender).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelOverride {
  Recommended,
  None,
  Paste(String),
}

/// Per-step override for `--config`. Maps to the wizard's
/// config-write confirm: `Write` accepts, `Skip` declines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum ConfigOverride {
  Write,
  Skip,
}

/// Parse the `--install` value into an [`InstallOverride`].
///
/// Shape-only validation: `custom:` paths are not stat'd here — the
/// wizard's runtime integrity check (`is_safe_to_adopt`) is the
/// authoritative gate. Relative paths are accepted so callers can
/// pass them through.
pub fn parse_install_override(raw: &str) -> Result<InstallOverride, String> {
  match raw {
    "brew" => Ok(InstallOverride::Brew),
    "gh-releases" => Ok(InstallOverride::GhReleases),
    "existing" => Ok(InstallOverride::Existing),
    other => {
      if let Some(path) = other.strip_prefix("custom:") {
        if path.is_empty() {
          return Err("`--install custom:<PATH>` requires a non-empty path after the colon".into());
        }
        Ok(InstallOverride::Custom(PathBuf::from(path)))
      } else {
        Err(format!(
          "invalid value `{raw}` — possible values: brew, gh-releases, existing, custom:<PATH>"
        ))
      }
    }
  }
}

/// Parse the `--model` value into a [`ModelOverride`]. Strings
/// other than `recommended` / `none` are validated as a HF repo
/// id (`<owner>/<repo>`, both halves non-empty, no whitespace).
pub fn parse_model_override(raw: &str) -> Result<ModelOverride, String> {
  match raw {
    "recommended" => Ok(ModelOverride::Recommended),
    "none" => Ok(ModelOverride::None),
    other => {
      if other.chars().any(char::is_whitespace) {
        return Err(format!(
          "invalid value `{other}` — HF repo id must not contain whitespace"
        ));
      }
      // Control characters (incl. null bytes) flow downstream into
      // filesystem paths and URLs; reject at parse time so a malformed
      // value can't truncate a path on the FFI boundary.
      if other.chars().any(char::is_control) {
        return Err(format!(
          "invalid value `{other:?}` — HF repo id must not contain control characters"
        ));
      }
      let mut parts = other.split('/');
      let owner = parts.next().unwrap_or("");
      let repo = parts.next().unwrap_or("");
      let extra = parts.next();
      if owner.is_empty() || repo.is_empty() || extra.is_some() {
        return Err(format!(
          "invalid value `{other}` — expected `recommended`, `none`, or `<owner>/<repo>`"
        ));
      }
      Ok(ModelOverride::Paste(other.to_string()))
    }
  }
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
  /// Emit structured JSON findings instead of the human-readable
  /// list. Stable shape:
  /// `{schema_version, findings: [{id, severity, message, fix_hint,
  /// safe_to_log}], baseline: {snapshot_bundle_date, init_date}}`.
  ///
  /// `doctor` always exits `0` — findings are informative, not a
  /// failure signal. Agents should branch on a non-empty `findings`
  /// array (or filter for `severity == "error"`) to escalate, not on
  /// the exit code. This makes `doctor` safe to run unconditionally
  /// from health-check loops without `set -e` blowing up.
  #[arg(long)]
  pub json: bool,
}

/// One of the wizard's optional steps. Detection (step 1) and handoff
/// (step 6) always run; the three middle steps are the units of
/// `--only`/`--skip` scoping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum InitStep {
  /// Install `llama-server` (step 2).
  Server,
  /// Recommender pick + GGUF download (steps 3 + 5).
  Models,
  /// Write `config.yaml` (step 4).
  Config,
}

#[derive(Args, Debug)]
pub struct FavoritesArgs {
  #[command(subcommand)]
  pub action: FavoritesAction,
}

#[derive(Subcommand, Debug)]
pub enum FavoritesAction {
  /// List the user's favorites.
  List {
    #[arg(long)]
    json: bool,
  },
  /// Mark a model as a favorite.
  Add {
    /// Model reference: name substring, absolute path, or canonical model id.
    model: String,
    /// Emit JSON instead of human-readable prose. Stable shape:
    /// `{ "action": "add", "model": "...", "added": bool,
    ///    "already_present": bool }`.
    #[arg(long)]
    json: bool,
  },
  /// Remove a favorite.
  Remove {
    /// Model reference.
    model: String,
    /// Emit JSON. Mirrors `Add` with `removed`/`already_absent`.
    #[arg(long)]
    json: bool,
  },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum ReasoningFlag {
  On,
  Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum LaunchMode {
  Chat,
  Embedding,
  Rerank,
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Pretty-prints a `Vec<InitStep>` so assertions in `cli_init_parse.rs`
  /// and the inline tests below share the same canonical form.
  fn steps(v: &[InitStep]) -> Vec<&'static str> {
    v.iter()
      .map(|s| match s {
        InitStep::Server => "server",
        InitStep::Models => "models",
        InitStep::Config => "config",
      })
      .collect()
  }

  fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("llamadash").chain(args.iter().copied()))
      .expect("argv should parse")
  }

  #[test]
  fn parses_with_no_subcommand() {
    let cli = parse(&[]);
    assert!(cli.command.is_none());
    assert!(!cli.no_scan);
    assert!(!cli.verbose);
  }

  #[test]
  fn model_path_is_repeatable() {
    let cli = parse(&["--model-path", "/a", "--model-path", "/b", "-p", "/c"]);
    assert_eq!(
      cli.model_paths,
      vec![
        PathBuf::from("/a"),
        PathBuf::from("/b"),
        PathBuf::from("/c"),
      ]
    );
  }

  #[test]
  fn global_flags_work_before_and_after_subcommand() {
    let before = parse(&["--no-scan", "list"]);
    let after = parse(&["list", "--no-scan"]);
    assert!(before.no_scan);
    assert!(after.no_scan);
    assert!(matches!(before.command, Some(Command::List(_))));
    assert!(matches!(after.command, Some(Command::List(_))));
  }

  #[test]
  fn init_parses_with_no_flags() {
    let cli = parse(&["init"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(!args.yes && !args.json && !args.offline);
        assert!(args.only.is_empty());
        assert!(args.skip.is_empty());
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_only_comma_separated_and_repeatable() {
    let cli_comma = parse(&["init", "--only", "server,config"]);
    let cli_repeat = parse(&["init", "--only", "server", "--only", "config"]);
    for cli in [cli_comma, cli_repeat] {
      match cli.command {
        Some(Command::Init(args)) => {
          assert_eq!(steps(&args.only), vec!["server", "config"]);
          assert!(args.skip.is_empty());
        }
        other => panic!("expected init, got {other:?}"),
      }
    }
  }

  #[test]
  fn init_only_and_skip_are_mutually_exclusive() {
    let result = Cli::try_parse_from(["llamadash", "init", "--only", "server", "--skip", "config"]);
    assert!(result.is_err(), "--only and --skip must conflict");
  }

  #[test]
  fn init_yes_json_offline_flags_parse() {
    let cli = parse(&["init", "--yes", "--json", "--offline"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.yes);
        assert!(args.json);
        assert!(args.offline);
        assert!(!args.recommended);
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_recommended_parses_independently_of_yes() {
    let cli = parse(&["init", "--recommended"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.recommended);
        assert!(!args.yes);
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_recommended_and_yes_are_combinable() {
    // Both flags coexist with no mutex; the wizard reads
    // `args.recommended || args.yes` so either flag short-circuits.
    let cli = parse(&["init", "--recommended", "--yes"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.recommended);
        assert!(args.yes);
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_yes_is_hidden_from_help_but_still_parses() {
    let rendered = Cli::try_parse_from(["llamadash", "init", "--help"])
      .unwrap_err()
      .to_string();
    assert!(
      rendered.contains("--recommended"),
      "help must list --recommended"
    );
    assert!(rendered.contains("--install"), "help must list --install");
    assert!(rendered.contains("--model"), "help must list --model");
    assert!(
      rendered.contains("--config-step"),
      "help must list --config-step"
    );
    assert!(!rendered.contains("--yes"), "help must hide --yes");
    // Still parseable.
    assert!(Cli::try_parse_from(["llamadash", "init", "--yes"]).is_ok());
  }

  #[test]
  fn init_install_override_value_enum_variants() {
    for (raw, expected) in [
      ("brew", InstallOverride::Brew),
      ("gh-releases", InstallOverride::GhReleases),
      ("existing", InstallOverride::Existing),
    ] {
      let cli = parse(&["init", "--install", raw]);
      match cli.command {
        Some(Command::Init(args)) => assert_eq!(args.install, Some(expected)),
        other => panic!("expected init, got {other:?}"),
      }
    }
  }

  #[test]
  fn init_install_custom_absolute_path() {
    let cli = parse(&["init", "--install", "custom:/usr/local/bin/llama-server"]);
    match cli.command {
      Some(Command::Init(args)) => assert_eq!(
        args.install,
        Some(InstallOverride::Custom(PathBuf::from(
          "/usr/local/bin/llama-server"
        )))
      ),
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_install_custom_relative_path_is_accepted() {
    // Relative paths are accepted at parse time; runtime
    // `is_safe_to_adopt` decides if they are usable.
    let cli = parse(&["init", "--install", "custom:relative/path/llama-server"]);
    match cli.command {
      Some(Command::Init(args)) => assert_eq!(
        args.install,
        Some(InstallOverride::Custom(PathBuf::from(
          "relative/path/llama-server"
        )))
      ),
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_install_custom_empty_path_rejected_at_parse_time() {
    let result = Cli::try_parse_from(["llamadash", "init", "--install", "custom:"]);
    assert!(result.is_err());
  }

  #[test]
  fn init_install_unknown_value_rejected() {
    let result = Cli::try_parse_from(["llamadash", "init", "--install", "frobnicate"]);
    let err = result.expect_err("frobnicate must be rejected");
    let msg = err.to_string();
    assert!(
      msg.contains("brew") && msg.contains("gh-releases") && msg.contains("custom:<PATH>"),
      "error must list valid choices, got: {msg}"
    );
  }

  #[test]
  fn init_model_override_enum_variants() {
    for (raw, expected) in [
      ("recommended", ModelOverride::Recommended),
      ("none", ModelOverride::None),
    ] {
      let cli = parse(&["init", "--model", raw]);
      match cli.command {
        Some(Command::Init(args)) => assert_eq!(args.model, Some(expected)),
        other => panic!("expected init, got {other:?}"),
      }
    }
  }

  #[test]
  fn init_model_paste_owner_repo() {
    let cli = parse(&["init", "--model", "bartowski/Llama-3.2-3B-GGUF"]);
    match cli.command {
      Some(Command::Init(args)) => assert_eq!(
        args.model,
        Some(ModelOverride::Paste(
          "bartowski/Llama-3.2-3B-GGUF".to_string()
        ))
      ),
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_model_without_slash_is_rejected() {
    let result = Cli::try_parse_from(["llamadash", "init", "--model", "invalid-no-slash"]);
    assert!(result.is_err());
  }

  #[test]
  fn init_model_with_whitespace_is_rejected() {
    let result = Cli::try_parse_from(["llamadash", "init", "--model", "owner / repo"]);
    assert!(result.is_err());
  }

  #[test]
  fn init_config_step_write_and_skip() {
    for (raw, expected) in [
      ("write", ConfigOverride::Write),
      ("skip", ConfigOverride::Skip),
    ] {
      let cli = parse(&["init", "--config-step", raw]);
      match cli.command {
        Some(Command::Init(args)) => assert_eq!(args.config_choice, Some(expected)),
        other => panic!("expected init, got {other:?}"),
      }
    }
  }

  #[test]
  fn init_all_new_flags_combinable_in_one_invocation() {
    let cli = parse(&[
      "init",
      "--recommended",
      "--install",
      "brew",
      "--model",
      "bartowski/Llama-3.2-3B-GGUF",
      "--config-step",
      "write",
    ]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.recommended);
        assert_eq!(args.install, Some(InstallOverride::Brew));
        assert_eq!(
          args.model,
          Some(ModelOverride::Paste(
            "bartowski/Llama-3.2-3B-GGUF".to_string()
          ))
        );
        assert_eq!(args.config_choice, Some(ConfigOverride::Write));
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_config_step_coexists_with_global_config() {
    // `--config <PATH>` is the global YAML-config-file flag on `Cli`;
    // `--config-step <CHOICE>` answers the wizard's config-write
    // confirm. The long names differ to satisfy clap's per-command
    // uniqueness assertion.
    let cli = parse(&["--config", "/tmp/my.yaml", "init", "--config-step", "skip"]);
    assert_eq!(cli.config, Some(PathBuf::from("/tmp/my.yaml")));
    match cli.command {
      Some(Command::Init(args)) => {
        assert_eq!(args.config_choice, Some(ConfigOverride::Skip));
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn parse_install_override_round_trip() {
    assert_eq!(
      parse_install_override("brew").unwrap(),
      InstallOverride::Brew
    );
    assert!(parse_install_override("custom:").is_err());
    assert!(parse_install_override("garbage").is_err());
  }

  #[test]
  fn parse_model_override_round_trip() {
    assert_eq!(
      parse_model_override("recommended").unwrap(),
      ModelOverride::Recommended
    );
    assert_eq!(parse_model_override("none").unwrap(), ModelOverride::None);
    assert!(parse_model_override("a/b/c").is_err());
    assert!(parse_model_override("/repo").is_err());
    assert!(parse_model_override("owner/").is_err());
  }

  #[test]
  fn doctor_parses_with_and_without_json() {
    let cli_plain = parse(&["doctor"]);
    let cli_json = parse(&["doctor", "--json"]);
    match cli_plain.command {
      Some(Command::Doctor(args)) => assert!(!args.json),
      other => panic!("expected doctor, got {other:?}"),
    }
    match cli_json.command {
      Some(Command::Doctor(args)) => assert!(args.json),
      other => panic!("expected doctor, got {other:?}"),
    }
  }

  #[test]
  fn pull_parses_positional_repo() {
    let cli = parse(&["pull", "HuggingFaceH4/zephyr-7b-beta-gguf"]);
    match cli.command {
      Some(Command::Pull(args)) => {
        assert_eq!(args.repo, "HuggingFaceH4/zephyr-7b-beta-gguf");
        assert!(!args.json);
      }
      other => panic!("expected pull, got {other:?}"),
    }
    let cli_json = parse(&["pull", "owner/repo:weights.gguf", "--json"]);
    match cli_json.command {
      Some(Command::Pull(args)) => {
        assert_eq!(args.repo, "owner/repo:weights.gguf");
        assert!(args.json);
      }
      other => panic!("expected pull, got {other:?}"),
    }
  }

  #[test]
  fn pull_requires_repo() {
    let result = Cli::try_parse_from(["llamadash", "pull"]);
    assert!(result.is_err(), "pull requires the repo positional");
  }

  #[test]
  fn daemon_subcommands_parse() {
    let cli_start = parse(&["daemon", "start", "--detach"]);
    match cli_start.command {
      Some(Command::Daemon(DaemonAction::Start {
        detach,
        state_dir,
        socket_path,
      })) => {
        assert!(detach);
        assert!(state_dir.is_none());
        assert!(socket_path.is_none());
      }
      other => panic!("expected daemon start --detach, got {other:?}"),
    }

    let cli_with_paths = parse(&[
      "daemon",
      "start",
      "--state-dir",
      "/tmp/llamadash-test-state",
      "--socket-path",
      "/tmp/llamadash-test-state/daemon.sock",
    ]);
    match cli_with_paths.command {
      Some(Command::Daemon(DaemonAction::Start {
        detach,
        state_dir,
        socket_path,
      })) => {
        assert!(!detach);
        assert_eq!(state_dir, Some(PathBuf::from("/tmp/llamadash-test-state")));
        assert_eq!(
          socket_path,
          Some(PathBuf::from("/tmp/llamadash-test-state/daemon.sock"))
        );
      }
      other => panic!("expected daemon start with paths, got {other:?}"),
    }

    let cli_stop = parse(&["daemon", "stop"]);
    assert!(matches!(
      cli_stop.command,
      Some(Command::Daemon(DaemonAction::Stop))
    ));

    let cli_status = parse(&["daemon", "status"]);
    assert!(matches!(
      cli_status.command,
      Some(Command::Daemon(DaemonAction::Status))
    ));
  }

  #[test]
  fn daemon_without_action_is_an_error() {
    let result = Cli::try_parse_from(["llamadash", "daemon"]);
    assert!(result.is_err(), "daemon subcommand requires an action");
  }

  #[test]
  fn start_accepts_full_launch_surface() {
    let cli = parse(&[
      "start",
      "qwen-coder",
      "--preset",
      "coding",
      "--ctx",
      "32768",
      "--port",
      "41150",
      "--reasoning",
      "on",
      "--mode",
      "chat",
      "--",
      "--threads",
      "8",
    ]);
    match cli.command {
      Some(Command::Start(args)) => {
        assert_eq!(args.model, "qwen-coder");
        assert_eq!(args.preset.as_deref(), Some("coding"));
        assert_eq!(args.ctx, Some(32768));
        assert_eq!(args.port, Some(41150));
        assert_eq!(args.reasoning, Some(ReasoningFlag::On));
        assert_eq!(args.mode, Some(LaunchMode::Chat));
        assert_eq!(
          args.extra,
          vec![OsString::from("--threads"), OsString::from("8")]
        );
      }
      other => panic!("expected Start, got {other:?}"),
    }
  }

  #[test]
  fn stop_all_conflicts_with_target() {
    let result = Cli::try_parse_from(["llamadash", "stop", "42", "--all"]);
    assert!(result.is_err());
  }

  #[test]
  fn stop_requires_target_or_all() {
    // `llamadash stop` with neither a positional target nor --all must error
    // at parse time. Without the ArgGroup, clap would accept this silently
    // and the handler would have no idea what to stop.
    let no_args = Cli::try_parse_from(["llamadash", "stop"]);
    assert!(no_args.is_err(), "stop without target or --all must error");

    let just_yes = Cli::try_parse_from(["llamadash", "stop", "--yes"]);
    assert!(
      just_yes.is_err(),
      "stop --yes without target or --all must error"
    );

    // Either of the valid forms succeeds.
    assert!(Cli::try_parse_from(["llamadash", "stop", "42"]).is_ok());
    assert!(Cli::try_parse_from(["llamadash", "stop", "--all"]).is_ok());
  }

  #[test]
  fn presets_list_delete_show_parse() {
    let list = parse(&["presets", "qwen-coder", "list"]);
    match list.command {
      Some(Command::Presets(args)) => {
        assert_eq!(args.model, "qwen-coder");
        assert!(matches!(args.action, PresetsAction::List { json: false }));
      }
      other => panic!("expected presets list, got {other:?}"),
    }

    let list_json = parse(&["presets", "qwen-coder", "list", "--json"]);
    match list_json.command {
      Some(Command::Presets(args)) => {
        assert!(matches!(args.action, PresetsAction::List { json: true }));
      }
      other => panic!("expected presets list --json, got {other:?}"),
    }

    let delete = parse(&["presets", "qwen-coder", "delete", "old-preset"]);
    match delete.command {
      Some(Command::Presets(args)) => match args.action {
        PresetsAction::Delete { name, .. } => assert_eq!(name, "old-preset"),
        other => panic!("expected Delete, got {other:?}"),
      },
      other => panic!("expected presets delete, got {other:?}"),
    }

    let show = parse(&["presets", "qwen-coder", "show", "coding"]);
    match show.command {
      Some(Command::Presets(args)) => match args.action {
        PresetsAction::Show { name, .. } => assert_eq!(name, "coding"),
        other => panic!("expected Show, got {other:?}"),
      },
      other => panic!("expected presets show, got {other:?}"),
    }
  }

  #[test]
  fn global_flags_capture_values() {
    let cli = parse(&[
      "--verbose",
      "--config",
      "/tmp/my.yaml",
      "--llama-server",
      "/usr/local/bin/llama-server",
      "list",
    ]);
    assert!(cli.verbose);
    assert_eq!(cli.config, Some(PathBuf::from("/tmp/my.yaml")));
    assert_eq!(
      cli.llama_server,
      Some(PathBuf::from("/usr/local/bin/llama-server"))
    );
    assert!(matches!(cli.command, Some(Command::List(_))));
  }

  #[test]
  fn list_supports_json_and_filter() {
    let cli = parse(&["list", "--json", "--filter", "qwen"]);
    match cli.command {
      Some(Command::List(args)) => {
        assert!(args.json);
        assert_eq!(args.filter.as_deref(), Some("qwen"));
      }
      other => panic!("expected List, got {other:?}"),
    }
  }

  #[test]
  fn presets_save_parses_with_extra_args() {
    let cli = parse(&[
      "presets",
      "qwen-coder",
      "save",
      "long-ctx",
      "--ctx",
      "131072",
      "--",
      "--flash-attn",
    ]);
    match cli.command {
      Some(Command::Presets(args)) => {
        assert_eq!(args.model, "qwen-coder");
        match args.action {
          PresetsAction::Save {
            name, ctx, extra, ..
          } => {
            assert_eq!(name, "long-ctx");
            assert_eq!(ctx, Some(131_072));
            assert_eq!(extra, vec![OsString::from("--flash-attn")]);
          }
          other => panic!("expected Save, got {other:?}"),
        }
      }
      other => panic!("expected Presets, got {other:?}"),
    }
  }

  #[test]
  fn logs_follow_and_tail_lines() {
    let cli = parse(&["logs", "model-abc", "-f", "-n", "200"]);
    match cli.command {
      Some(Command::Logs(args)) => {
        assert_eq!(args.target, "model-abc");
        assert!(args.follow);
        assert_eq!(args.lines, Some(200));
      }
      other => panic!("expected Logs, got {other:?}"),
    }
  }

  #[test]
  fn logs_accepts_port_as_target() {
    let cli = parse(&["logs", "41150"]);
    match cli.command {
      Some(Command::Logs(args)) => assert_eq!(args.target, "41150"),
      other => panic!("expected Logs, got {other:?}"),
    }
  }

  #[test]
  fn no_spawn_global_flag_parses() {
    let with_flag = parse(&["--no-spawn", "status"]);
    assert!(with_flag.no_spawn);
    let without_flag = parse(&["status"]);
    assert!(!without_flag.no_spawn);
  }

  #[test]
  fn render_flag_and_size_parse_globally() {
    let with_flag = parse(&["--render", "--render-size", "100x30"]);
    assert!(with_flag.render);
    assert_eq!(with_flag.render_size.as_deref(), Some("100x30"));
    let without_flag = parse(&[]);
    assert!(!without_flag.render);
    assert!(without_flag.render_size.is_none());
  }

  #[test]
  fn parse_render_size_accepts_canonical_form() {
    assert_eq!(parse_render_size("120x40").unwrap(), (120, 40));
    assert_eq!(parse_render_size("80x20").unwrap(), (80, 20));
  }

  #[test]
  fn parse_render_size_rejects_too_small_or_malformed() {
    assert!(parse_render_size("garbage").is_err());
    assert!(parse_render_size("100").is_err());
    assert!(parse_render_size("axb").is_err());
    // Below the minimum that still lets the layout split into the
    // title row + info row + body without collapsing into nonsense.
    assert!(parse_render_size("20x5").is_err());
  }

  #[test]
  fn status_accepts_optional_target() {
    let scoped = parse(&["status", "model-abc", "--json"]);
    match scoped.command {
      Some(Command::Status(args)) => {
        assert_eq!(args.target.as_deref(), Some("model-abc"));
        assert!(args.json);
      }
      other => panic!("expected Status, got {other:?}"),
    }

    let unscoped = parse(&["status"]);
    match unscoped.command {
      Some(Command::Status(args)) => {
        assert!(args.target.is_none());
        assert!(!args.json);
      }
      other => panic!("expected Status, got {other:?}"),
    }
  }

  #[test]
  fn presets_save_accepts_port() {
    let cli = parse(&[
      "presets",
      "qwen-coder",
      "save",
      "pinned",
      "--ctx",
      "32768",
      "--port",
      "41150",
    ]);
    match cli.command {
      Some(Command::Presets(args)) => match args.action {
        PresetsAction::Save { port, ctx, .. } => {
          assert_eq!(port, Some(41150));
          assert_eq!(ctx, Some(32_768));
        }
        other => panic!("expected Save, got {other:?}"),
      },
      other => panic!("expected Presets, got {other:?}"),
    }
  }

  #[test]
  fn favorites_subcommands_parse() {
    let list_cli = parse(&["favorites", "list", "--json"]);
    match list_cli.command {
      Some(Command::Favorites(args)) => match args.action {
        FavoritesAction::List { json } => assert!(json),
        other => panic!("expected FavoritesAction::List, got {other:?}"),
      },
      other => panic!("expected Favorites, got {other:?}"),
    }

    let add_cli = parse(&["favorites", "add", "qwen-coder"]);
    match add_cli.command {
      Some(Command::Favorites(args)) => match args.action {
        FavoritesAction::Add { model, .. } => assert_eq!(model, "qwen-coder"),
        other => panic!("expected FavoritesAction::Add, got {other:?}"),
      },
      other => panic!("expected Favorites, got {other:?}"),
    }

    let remove_cli = parse(&["favorites", "remove", "qwen-coder"]);
    match remove_cli.command {
      Some(Command::Favorites(args)) => match args.action {
        FavoritesAction::Remove { model, .. } => assert_eq!(model, "qwen-coder"),
        other => panic!("expected FavoritesAction::Remove, got {other:?}"),
      },
      other => panic!("expected Favorites, got {other:?}"),
    }
  }

  #[test]
  fn unknown_reasoning_value_errors() {
    let result = Cli::try_parse_from(["llamadash", "start", "x", "--reasoning", "maybe"]);
    assert!(result.is_err());
  }

  #[test]
  fn version_flag_works() {
    let result = Cli::try_parse_from(["llamadash", "--version"]);
    // clap returns an "error" with exit kind DisplayVersion for --version.
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    let rendered = err.to_string();
    assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
  }

  #[test]
  fn help_flag_lists_every_user_facing_subcommand() {
    let result = Cli::try_parse_from(["llamadash", "--help"]);
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    let rendered = err.to_string();
    // `pull` is now a first-class subcommand (R65 graduated in v2).
    // `init` and `doctor` ship in v2 alongside it.
    for sub in [
      "daemon",
      "list",
      "start",
      "stop",
      "status",
      "logs",
      "presets",
      "favorites",
      "last-params",
      "pull",
      "init",
      "doctor",
    ] {
      assert!(
        rendered.contains(sub),
        "help output should list `{sub}` subcommand, got: {rendered}"
      );
    }
  }
}
