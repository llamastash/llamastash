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
  name = "llamatui",
  version,
  about = "Fast keyboard-driven TUI + CLI for local llama.cpp models",
  long_about = None,
  before_help = BANNER,
)]
pub struct Cli {
  /// Path to a YAML config file (overrides `LLAMATUI_CONFIG`).
  #[arg(long, value_name = "PATH", global = true)]
  pub config: Option<PathBuf>,

  /// Path to the `llama-server` binary (overrides `LLAMATUI_LLAMA_SERVER`).
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
  #[arg(short, long, global = true, action = ArgAction::SetTrue)]
  pub verbose: bool,

  #[command(subcommand)]
  pub command: Option<Command>,
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
  /// TODO(v2-R46): the in-app pull worker is deferred to v2 (see plan
  /// `Scope Boundaries` / `v2 deferrals`). The subcommand surface is
  /// kept compiled so callers see a stable shape, but it is hidden
  /// from `--help` and the dispatcher exits with `unimplemented!`.
  #[command(hide = true)]
  Pull(PullArgs),
  /// Mark, unmark, and list favorite models.
  Favorites(FavoritesArgs),
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
  },
  /// Delete a saved preset.
  Delete { name: String },
  /// Print a saved preset's parameters.
  Show { name: String },
}

#[derive(Args, Debug)]
pub struct PullArgs {
  #[command(subcommand)]
  pub action: PullAction,
}

#[derive(Subcommand, Debug)]
pub enum PullAction {
  /// Start a new pull from `HuggingFace`.
  Start {
    /// `HuggingFace` repo id, optionally with `:filename.gguf` to pin a single file.
    repo: String,
    /// Fire-and-forget mode: return immediately. The pull job id is emitted to stdout
    /// (JSON when `--json` is set) so the caller can poll `llamatui pull status <id>`.
    #[arg(long)]
    background: bool,
    /// Emit structured JSON output (job id + progress) instead of a human-readable stream.
    #[arg(long)]
    json: bool,
  },
  /// Poll the status of a previously-started pull.
  Status {
    /// Pull job id returned by `pull start`.
    job_id: String,
    #[arg(long)]
    json: bool,
  },
  /// Cancel a running pull. Partial files are cleaned up.
  Cancel {
    /// Pull job id.
    job_id: String,
  },
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
  },
  /// Remove a favorite.
  Remove {
    /// Model reference.
    model: String,
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

  fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("llamatui").chain(args.iter().copied()))
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
      "/tmp/llamatui-test-state",
      "--socket-path",
      "/tmp/llamatui-test-state/daemon.sock",
    ]);
    match cli_with_paths.command {
      Some(Command::Daemon(DaemonAction::Start {
        detach,
        state_dir,
        socket_path,
      })) => {
        assert!(!detach);
        assert_eq!(state_dir, Some(PathBuf::from("/tmp/llamatui-test-state")));
        assert_eq!(
          socket_path,
          Some(PathBuf::from("/tmp/llamatui-test-state/daemon.sock"))
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
    let result = Cli::try_parse_from(["llamatui", "daemon"]);
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
    let result = Cli::try_parse_from(["llamatui", "stop", "42", "--all"]);
    assert!(result.is_err());
  }

  #[test]
  fn stop_requires_target_or_all() {
    // `llamatui stop` with neither a positional target nor --all must error
    // at parse time. Without the ArgGroup, clap would accept this silently
    // and the handler would have no idea what to stop.
    let no_args = Cli::try_parse_from(["llamatui", "stop"]);
    assert!(no_args.is_err(), "stop without target or --all must error");

    let just_yes = Cli::try_parse_from(["llamatui", "stop", "--yes"]);
    assert!(
      just_yes.is_err(),
      "stop --yes without target or --all must error"
    );

    // Either of the valid forms succeeds.
    assert!(Cli::try_parse_from(["llamatui", "stop", "42"]).is_ok());
    assert!(Cli::try_parse_from(["llamatui", "stop", "--all"]).is_ok());
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
        PresetsAction::Delete { name } => assert_eq!(name, "old-preset"),
        other => panic!("expected Delete, got {other:?}"),
      },
      other => panic!("expected presets delete, got {other:?}"),
    }

    let show = parse(&["presets", "qwen-coder", "show", "coding"]);
    match show.command {
      Some(Command::Presets(args)) => match args.action {
        PresetsAction::Show { name } => assert_eq!(name, "coding"),
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
  fn pull_start_parses() {
    let cli = parse(&[
      "pull",
      "start",
      "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
      "--background",
      "--json",
    ]);
    match cli.command {
      Some(Command::Pull(args)) => match args.action {
        PullAction::Start {
          repo,
          background,
          json,
        } => {
          assert_eq!(repo, "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF");
          assert!(background);
          assert!(json);
        }
        other => panic!("expected PullAction::Start, got {other:?}"),
      },
      other => panic!("expected Pull, got {other:?}"),
    }
  }

  #[test]
  fn pull_status_and_cancel_parse() {
    let status_cli = parse(&["pull", "status", "job-abc", "--json"]);
    match status_cli.command {
      Some(Command::Pull(args)) => match args.action {
        PullAction::Status { job_id, json } => {
          assert_eq!(job_id, "job-abc");
          assert!(json);
        }
        other => panic!("expected PullAction::Status, got {other:?}"),
      },
      other => panic!("expected Pull, got {other:?}"),
    }

    let cancel_cli = parse(&["pull", "cancel", "job-abc"]);
    match cancel_cli.command {
      Some(Command::Pull(args)) => match args.action {
        PullAction::Cancel { job_id } => assert_eq!(job_id, "job-abc"),
        other => panic!("expected PullAction::Cancel, got {other:?}"),
      },
      other => panic!("expected Pull, got {other:?}"),
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
        FavoritesAction::Add { model } => assert_eq!(model, "qwen-coder"),
        other => panic!("expected FavoritesAction::Add, got {other:?}"),
      },
      other => panic!("expected Favorites, got {other:?}"),
    }

    let remove_cli = parse(&["favorites", "remove", "qwen-coder"]);
    match remove_cli.command {
      Some(Command::Favorites(args)) => match args.action {
        FavoritesAction::Remove { model } => assert_eq!(model, "qwen-coder"),
        other => panic!("expected FavoritesAction::Remove, got {other:?}"),
      },
      other => panic!("expected Favorites, got {other:?}"),
    }
  }

  #[test]
  fn unknown_reasoning_value_errors() {
    let result = Cli::try_parse_from(["llamatui", "start", "x", "--reasoning", "maybe"]);
    assert!(result.is_err());
  }

  #[test]
  fn version_flag_works() {
    let result = Cli::try_parse_from(["llamatui", "--version"]);
    // clap returns an "error" with exit kind DisplayVersion for --version.
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    let rendered = err.to_string();
    assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
  }

  #[test]
  fn help_flag_lists_every_user_facing_subcommand() {
    let result = Cli::try_parse_from(["llamatui", "--help"]);
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    let rendered = err.to_string();
    // `pull` is intentionally excluded: it's hidden via `#[command(hide
    // = true)]` until R46 lands in v2. The subcommand still parses (see
    // `pull_start_parses` and `pull_status_and_cancel_parse`) so the
    // shape stays stable, but `--help` must not advertise it.
    for sub in [
      "daemon",
      "list",
      "start",
      "stop",
      "status",
      "logs",
      "presets",
      "favorites",
    ] {
      assert!(
        rendered.contains(sub),
        "help output should list `{sub}` subcommand, got: {rendered}"
      );
    }
  }

  #[test]
  fn pull_subcommand_is_hidden_from_help_pending_r46() {
    let result = Cli::try_parse_from(["llamatui", "--help"]);
    let err = result.unwrap_err();
    let rendered = err.to_string();
    // Top-level help omits hidden subcommands. Be precise about what we
    // assert — the word "pull" can appear inside other text (e.g.
    // `pull` showing up in long flag descriptions); we assert on the
    // canonical clap subcommand-line shape `  pull   ...`.
    let lines_with_pull_as_subcommand: Vec<&str> = rendered
      .lines()
      .filter(|l| l.trim_start().starts_with("pull "))
      .collect();
    assert!(
      lines_with_pull_as_subcommand.is_empty(),
      "pull must stay hidden from --help while R46 is deferred to v2: {lines_with_pull_as_subcommand:?}"
    );
  }
}
