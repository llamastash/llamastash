//! Command-line argument schema (clap derive).
//!
//! Top-level args are `global = true` so every subcommand inherits them, and
//! a missing subcommand routes to the TUI. The shape of each subcommand
//! mirrors the agent-facing surface defined in
//! `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md` —
//! handlers are stubbed until their respective implementation units land.

use std::{ffi::OsString, net::IpAddr, path::PathBuf};

use clap::{builder::Styles, ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::banner::BANNER;

/// `--help` color scheme. Mirrors the CLI's semantic palette (green
/// success / cyan accents) so styled help reads like the rest of the
/// tool. clap only emits these escapes when its `ColorChoice` resolves
/// to on — `ColorChoice::Auto` already honors `NO_COLOR` and a non-TTY
/// stdout, and `main` flips it to `Never` under `--no-colors`, so piped
/// help stays byte-stable plain text.
fn help_styles() -> Styles {
  use clap::builder::styling::{AnsiColor, Effects};
  Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser, Debug)]
#[command(
  name = "llamastash",
  version,
  about = "Fast keyboard-driven local-LLM launcher: TUI + CLI for llama.cpp, with a pluggable backend seam",
  long_about = None,
  before_help = BANNER,
  styles = help_styles(),
)]
pub struct Cli {
  /// Path to a YAML config file (overrides `LLAMASTASH_CONFIG`).
  #[arg(long, value_name = "PATH", global = true)]
  pub config: Option<PathBuf>,

  /// Path to the `llama-server` binary (overrides `LLAMASTASH_LLAMA_SERVER`).
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
  /// see <https://no-color.org>) or when stdout is not a terminal
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

  /// Opt into terminal mouse capture for the TUI so a left-click on
  /// the Models list, the right pane, or a tab label moves focus /
  /// switches tab. Takes precedence over `mouse_focus` in
  /// `config.yaml`. Off by default — capturing the mouse pre-empts
  /// the terminal's native click-and-drag text selection.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  pub mouse_focus: bool,

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
  if w < 60 || h < 20 {
    return Err(format!(
      "render size `{raw}` is too small; minimum is 60x20"
    ));
  }
  Ok((w, h))
}

/// Whether a `LLAMASTASH_OFFLINE=<value>` should enable offline mode.
/// Mirrors `init::fetch::offline_requested`'s truthy set so the flag and
/// the helper agree.
fn offline_env_truthy(value: &str) -> bool {
  matches!(
    value.trim().to_ascii_lowercase().as_str(),
    "1" | "true" | "yes"
  )
}

/// Normalize `LLAMASTASH_OFFLINE` *before* clap parses argv.
///
/// The three `--offline` flags bind this env var, but clap strict-parses it
/// as a boolean (only `true` / `false`), so the documented `LLAMASTASH_OFFLINE=1`
/// — and `=0`, and an empty value — would otherwise abort every `init` /
/// `pull` / `recommend` run with `error: invalid value '1' for '--offline'`.
/// Translate a truthy value to the literal `true` clap accepts and treat
/// everything else (`0`, empty, junk) as unset, matching the documented
/// "empty value does not enable" convention.
pub fn normalize_offline_env() {
  if let Ok(v) = std::env::var("LLAMASTASH_OFFLINE") {
    if offline_env_truthy(&v) {
      std::env::set_var("LLAMASTASH_OFFLINE", "true");
    } else {
      std::env::remove_var("LLAMASTASH_OFFLINE");
    }
  }
}

/// Parse argv with the `--help` color policy applied.
///
/// `--help` / `--version` are resolved *inside* clap, before any
/// `colors::init` runs, so the color decision for styled help has to be
/// wired here. Default `ColorChoice::Auto` already silences color when
/// `NO_COLOR` is set or stdout isn't a TTY; passing `--no-colors`
/// (anywhere in argv, since it's a global flag) flips clap to
/// `ColorChoice::Never` so the third off-condition reaches help too.
pub fn parse_cli() -> Result<Cli, clap::Error> {
  use clap::{ColorChoice, CommandFactory, FromArgMatches};
  let no_colors = std::env::args_os().any(|a| a == "--no-colors");
  let choice = if no_colors {
    ColorChoice::Never
  } else {
    ColorChoice::Auto
  };
  let matches = Cli::command().color(choice).try_get_matches()?;
  Cli::from_arg_matches(&matches)
}

#[derive(Subcommand, Debug)]
pub enum Command {
  /// Open the active config file or inspect its bindings.
  Config(ConfigArgs),
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
  /// MVP shape (v2-R65): `llamastash pull <hf-repo>` downloads every
  /// GGUF in the repo (or a single shard set when the repo ships
  /// multi-shard files) into the canonical HF cache layout
  /// (`~/.cache/huggingface/hub/models--<owner>--<repo>/...`) so the
  /// next `llamastash list` rescan finds it. `--json` emits the
  /// summary; otherwise progress streams to stderr.
  Pull(PullArgs),
  /// Run the first-time setup / maintenance wizard.
  Init(InitArgs),
  /// Read-only diagnostic — compares current detection against the
  /// recorded `init_snapshot` baseline.
  Doctor(DoctorArgs),
  /// Mark, unmark, and list favorite models.
  Favorites(FavoritesArgs),
  /// List and download the best-fit model for this hardware. Shortcut for
  /// `init --only models` that lets users grab a
  /// recommended GGUF without walking through the full first-run
  /// wizard.
  Recommend(RecommendArgs),
  /// Inspect the last successful `start_model` params for one or
  /// every catalog model. Surfaces the daemon's `last_params_list`
  /// IPC so agents can answer "how did I launch this model last
  /// time" without going through the TUI.
  LastParams(LastParamsArgs),
  /// Show everything LlamaStash knows about a single model: full
  /// path, on-disk size (summed across shards for split GGUFs),
  /// parsed GGUF metadata, the yaml + built-in `arch_defaults` that
  /// would feed a launch, and the last `start_model` params for this
  /// file. Reuses the catalog/resolver flow that powers `/v1/models`,
  /// `/api/show`, and `start`, so a name that works on one surface
  /// works here.
  Show(ShowArgs),
  /// Maintainer-only hardware UAT lifecycle. Hidden from --help on
  /// every release binary; reachable only when the crate is built
  /// with `--features uat`.
  #[cfg(feature = "uat")]
  #[command(hide = true)]
  Uat(UatArgs),
}

#[derive(Args, Debug, Default)]
pub struct ConfigArgs {
  #[command(subcommand)]
  pub action: Option<ConfigAction>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
  /// Print the active keybinding overrides as YAML.
  Bindings,
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
  /// Start the daemon (no-op if already running). Detaches into the
  /// background by default; pass `--foreground` (or `-f`) to keep the
  /// daemon attached to the terminal (e.g. for `systemd` / supervisor
  /// wrappers that own stdout/stderr).
  Start {
    /// Keep the daemon attached to the controlling terminal instead of
    /// detaching into the background. Use this when a process
    /// supervisor (systemd, runit, foreman, container `CMD`) owns the
    /// lifecycle and needs to see stdout/stderr directly.
    #[arg(long, short = 'f')]
    foreground: bool,
    /// Internal hand-off: state directory to use instead of XDG defaults.
    /// `start_detached` propagates this to the re-exec'd child so tests
    /// and alternate deployments can drive the daemon at a custom path.
    /// Hidden from `--help` because end users should reach for the
    /// config file or XDG env vars instead.
    #[arg(long, value_name = "PATH", hide = true)]
    state_dir: Option<PathBuf>,
    /// TCP port the OpenAI-compat proxy listener binds on
    /// `127.0.0.1`. Overrides `proxy.port` from the config file and
    /// the `--ollama-compat`-derived default. Default port is `11435`
    /// (`11434` with `--ollama-compat`); the listener scans up to
    /// `11440` for a free slot. Use `0` to bind an ephemeral port —
    /// the actual address is reported via `llamastash status`.
    #[arg(long, value_name = "PORT")]
    proxy_port: Option<u16>,
    /// Enable Ollama drop-in mode for this daemon process. `GET /`
    /// returns `"Ollama is running"` so the official `ollama` CLI
    /// (and other Ollama-Go-based clients) recognise the proxy; the
    /// default port shifts to `11434`. OR-ed with `proxy.ollama_compat`
    /// in `config.yaml` and the `LLAMASTASH_OLLAMA_COMPAT` env var
    /// (any of the three turns it on).
    #[arg(long)]
    ollama_compat: bool,
    /// Disable the family-MRU fallback. When a requested model fails
    /// to auto-start, the proxy normally serves the request from
    /// another Ready supervisor (with `x-llamastash-fallback-reason`
    /// stamped on the response). Pass this flag to make the proxy
    /// return a 503 `launch_failed` instead. OR-ed with
    /// `proxy.fallback_enabled: false` in `config.yaml` and the
    /// `LLAMASTASH_NO_PROXY_FALLBACK` env var — any of the three
    /// disables the fallback.
    #[arg(long)]
    no_proxy_fallback: bool,
    /// Address the OpenAI-compat proxy listener binds. Default
    /// `127.0.0.1` (loopback only). Pass a routable address
    /// (`0.0.0.0`, a specific NIC IP, or an IPv6 address like `::`) to
    /// expose the proxy on the LAN. Overrides `proxy.host` in
    /// `config.yaml` and the `LLAMASTASH_PROXY_HOST` env var
    /// (precedence: CLI > env > config). A non-loopback bind requires a
    /// bearer key: llamastash auto-generates and prints one on first
    /// use unless you pass `--insecure-no-auth`. Only the proxy is
    /// exposed — the control plane and `llama-server` children stay
    /// loopback.
    #[arg(long, value_name = "IP")]
    proxy_host: Option<IpAddr>,
    /// Bind a non-loopback `--proxy-host` with NO authentication. By
    /// default llamastash refuses to expose the proxy on the LAN
    /// without a bearer key; this flag opts out of that safety check
    /// and serves the proxy unauthenticated. Anyone who can reach the
    /// address can drive your models. Only use it on a trusted,
    /// firewalled network. A loud warning prints regardless.
    #[arg(long)]
    insecure_no_auth: bool,
    /// Enable the opt-in **experimental** Lemonade (`lemond`) backend for
    /// this daemon: run Lemonade discovery and supervise/route to the
    /// `lemond` umbrella. OR-ed with `lemonade.enabled: true` in
    /// `config.yaml` and the `LLAMASTASH_LEMONADE` env var
    /// (`1`/`true`/`yes`/`on`) — any of the three turns it on. Experimental:
    /// behaviour and config may change. llamastash never installs `lemond`;
    /// set it up manually (see `docs/lemonade-setup.md`).
    #[arg(long)]
    lemonade: bool,
    /// Force-enable the ds4 (DwarfStar) direct backend for DeepSeek V4 GGUFs,
    /// overriding `ds4.enabled: false`. ds4 is otherwise **on by default**
    /// whenever `ds4-server` is found (on `PATH` or via `ds4.binary`). OR-ed
    /// with `LLAMASTASH_DS4=1`. llamastash never installs `ds4-server`; build
    /// it and point `ds4.binary` at it (see `docs/usage.md`).
    #[arg(long)]
    ds4: bool,
    /// Start the daemon even if an *indicated* backend can't initialize —
    /// the `llama-server` binary isn't found, or the Lemonade umbrella port
    /// is already taken / `lemond` is missing. Without this, `daemon start`
    /// fails fast with an error rather than coming up silently degraded. With
    /// it, the daemon starts anyway and the failed backend is simply
    /// unavailable (surfaced in `status` and the TUI server-info section).
    #[arg(long)]
    force: bool,
  },
  /// Stop the running daemon. Running models keep running.
  Stop {
    /// Bypass the IPC `shutdown` call and signal the daemon by PID
    /// instead. Useful when `runtime.json` is missing (e.g. a stale
    /// process from an older version) so the IPC channel can't
    /// negotiate a graceful shutdown. Walks: read `daemon.pid`, send
    /// `SIGTERM`, wait briefly for exit. Falls back automatically
    /// when the regular path detects a stale daemon, so the flag is
    /// mostly an escape hatch for scripts.
    #[arg(long, short = 'f')]
    force: bool,
  },
  /// Print daemon PID, uptime, and connected-client count.
  Status {
    /// Emit the raw `version` IPC response as pretty-printed JSON
    /// instead of the human key/value block. Byte-stable contract:
    /// agents that previously parsed `daemon status` as JSON (before
    /// the v0.0.2 kv-block rewrite) should now pass `--json` to keep
    /// the same shape.
    #[arg(long)]
    json: bool,
  },
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
  /// Model reference: name substring, absolute path, or canonical
  /// model id. Optional: when omitted on an interactive TTY (and
  /// `--json` is not set), `start` opens a cliclack picker over the
  /// catalog. Non-interactive callers (CI / piped / `--json`) must
  /// pass an explicit reference.
  pub model: Option<String>,
  /// Saved preset to load before applying overrides. The reserved value
  /// `auto` launches pure-fit (skips the model's `default:` preset and
  /// last-used params), the clean way to ignore prior launch state.
  #[arg(long, value_name = "NAME")]
  pub preset: Option<String>,
  /// Context length override. A token count pins `-c`; the literal
  /// `auto` delegates the window to llama-server's `--fit` (the knob's
  /// Auto state).
  #[arg(long, value_name = "TOKENS|auto", value_parser = parse_ctx_arg)]
  pub ctx: Option<CtxArg>,
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
  /// Advanced launch params, generated from the typed-knob spec table
  /// (`--n-gpu-layers`, `--device`, `--tensor-split`, `--flash-attn`,
  /// …). See `crate::cli::knob_flags`; the same knobs are also
  /// reachable via the trailing `-- <raw>` passthrough.
  #[command(flatten)]
  pub knobs: crate::cli::knob_flags::KnobFlags,
  /// Extra flags forwarded verbatim to `llama-server` after `--`.
  /// Also accepts any typed knob (e.g. `-ngl 99`) for parity with the
  /// inline flags above.
  #[arg(last = true, value_name = "ARG")]
  pub extra: Vec<OsString>,
  /// Backend to run this model on. `auto` (default) picks by model
  /// identity; override with a backend id (`llamacpp` or another installed
  /// backend) to force one per launch. Validated against the live registry.
  #[arg(long, value_parser = parse_backend_id)]
  pub backend: Option<String>,
  /// Server (a specific build/binary of a backend, e.g. `llamacpp·vulkan`) to
  /// launch on. Determines which binary spawns and, when `--backend` is unset,
  /// which backend runs it. Ids come from `status` (`servers`). A `--device`
  /// selector already implies its owning server, so `--server` is for picking a
  /// build with no device pin.
  #[arg(long)]
  pub server: Option<String>,
  /// MTP (multi-token prediction) speculative decoding: `auto` (default —
  /// enable when the model is MTP-capable), `on` (force; warns + skips if not
  /// capable), or `off`. Launch-only, no config-file entry.
  #[arg(long, value_name = "auto|on|off", value_parser = parse_mtp_enable)]
  pub mtp: Option<crate::launch::params::MtpEnable>,
  /// MTP draft-token count → `--spec-draft-n-max`. Only takes effect when MTP
  /// resolves on; unset uses llama.cpp's default (3).
  #[arg(long, value_name = "N")]
  pub spec_draft_n_max: Option<u32>,

  /// Emit JSON instead of human-readable success prose.
  /// Emit JSON instead of human-readable success prose. Stable
  /// shape: `{ "name", "launch_id", "port", "pid", "preset",
  /// "path" }`. With `--wait`, also carries `state` and `resolved_ctx`.
  #[arg(long)]
  pub json: bool,
  /// Block until the model finishes loading (reaches Ready or Error),
  /// then report the resolved context window `--fit` chose. Default is
  /// fire-and-forget: `start` returns as soon as the daemon accepts the
  /// launch, while the model is still loading. Useful for big models
  /// where you want the resolved ctx without a follow-up `status`.
  #[arg(long)]
  pub wait: bool,
}

/// Validate a `--backend <id>` value against the live registry (plus `auto`),
/// returning the id verbatim — it's sent to the daemon, which parses it into a
/// [`crate::launch::params::BackendChoice`]. Registry-driven: no backend is
/// named here and adding one needs no edit. A bad value is a clap usage error
/// (exit 64) listing the valid ids.
fn parse_backend_id(s: &str) -> Result<String, String> {
  use crate::backend::Backend;
  let mut valid: Vec<&str> = vec!["auto"];
  valid.extend(crate::backend::Backends::all().iter().map(|b| b.id()));
  if valid.contains(&s) {
    Ok(s.to_string())
  } else {
    Err(format!(
      "unknown backend `{s}`; valid: {}",
      valid.join(", ")
    ))
  }
}

/// Parse a `--mtp <auto|on|off>` value into an [`MtpEnable`]. A bad value is a
/// clap usage error (exit 64).
fn parse_mtp_enable(s: &str) -> Result<crate::launch::params::MtpEnable, String> {
  crate::launch::params::MtpEnable::from_token(s)
    .ok_or_else(|| format!("invalid --mtp value `{s}`; expected auto, on, or off"))
}

#[derive(Args, Debug)]
pub struct StopArgs {
  /// Launch id (`L3`), port, or `ext-<pid>` to stop. Optional: when
  /// omitted (and `--all` is not set) on an interactive TTY, `stop`
  /// opens a cliclack picker over running supervisors. Non-interactive
  /// callers must pass an explicit target or `--all`.
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
pub struct ShowArgs {
  /// Model reference: substring of name, absolute path, or canonical
  /// model id. Same matcher `start` and `/v1/...` use, so any name
  /// that works on one surface works here.
  pub model: String,
  /// Emit the composite envelope as JSON instead of the human block.
  /// Stable agent contract: every key is always present even when
  /// `null` so callers can pin field paths.
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
  /// Disable outbound network. Equivalent to `LLAMASTASH_OFFLINE=1`.
  /// Pull always requires network so this exits with `PULL_FAILED`
  /// (69) up-front; honored for parity with `init --offline`.
  #[arg(long, env = "LLAMASTASH_OFFLINE")]
  pub offline: bool,
  /// Do not fetch companion files (mmproj projector, MTP draft head)
  /// alongside the model. By default `pull` grabs one companion per kind so a
  /// multimodal / speculative-decoding model arrives ready to launch.
  #[arg(long, conflicts_with = "all_companions")]
  pub no_companions: bool,
  /// Fetch **every** companion variant (all projector precisions / all draft
  /// heads), not just one per kind. Useful to keep multiple mmproj precisions
  /// on disk.
  #[arg(long)]
  pub all_companions: bool,
}

#[derive(Args, Debug)]
pub struct InitArgs {
  /// Use hardware-aware defaults for every step; do not prompt. Pair
  /// with `--json` for agent consumption. Integrity-check failures
  /// still abort with the documented exit codes — they are never
  /// silently downgraded.
  ///
  /// `global = true` so it works on either side of an `init <step>`
  /// subcommand (`init --recommended models` and `init models
  /// --recommended` both parse).
  #[arg(long, action = ArgAction::SetTrue, global = true)]
  pub recommended: bool,
  /// Hidden alias for `--recommended` kept for agent/script
  /// compatibility.
  #[arg(long, hide = true, global = true)]
  pub yes: bool,
  /// Emit a single structured summary at completion. Per-step progress
  /// goes to stderr (only in `--verbose`). Mutually compatible with
  /// `--recommended`.
  #[arg(long, global = true)]
  pub json: bool,
  /// Disable outbound network. Steps that require network are skipped
  /// with actionable hints. Equivalent to `LLAMASTASH_OFFLINE=1`.
  #[arg(long, env = "LLAMASTASH_OFFLINE", global = true)]
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
  /// Pin the HuggingFace revision (commit SHA, branch, or tag) used
  /// when downloading the model. Honored on the `--model owner/repo`
  /// paste branch where the maintainer wants byte-stable input; the
  /// recommender branch ignores it because curated picks are
  /// branch-tracked. Empty values rejected at parse time.
  #[arg(long, value_name = "SHA", value_parser = parse_revision)]
  pub revision: Option<String>,

  /// Skip the post-init handoff into the interactive TUI. By default,
  /// a successful interactive (non-`--json`, TTY) init prompts to
  /// launch the TUI in the same process; with `--recommended`, the
  /// TUI launches without prompting. This flag bypasses both.
  #[arg(long, action = ArgAction::SetTrue, global = true)]
  pub no_tui: bool,

  /// Pre-answer the integrations picker. Comma-separated tool ids:
  /// `opencode`, `aider`, `continue`, `zed`, `pi`, `env-sh`. Use
  /// `none` to skip the step entirely (equivalent to
  /// `--skip integrations`). Without this flag and without
  /// `--recommended` / `--json`, the interactive multiselect runs;
  /// the latter two suppress the picker — pass `--integrations` to
  /// opt in non-interactively.
  #[arg(
    long,
    value_name = "TOOLS",
    value_delimiter = ',',
    action = ArgAction::Append,
  )]
  pub integrations: Vec<String>,

  /// Run a single wizard step as a subcommand instead of the full
  /// flow. `init server|models|config|integrations` is sugar for
  /// `init --only <step>`, with the step's pre-answer flag carried on
  /// the subcommand itself (e.g. `init server --install gh-releases`).
  /// Bare `init` (no subcommand) runs every step, honoring the
  /// `--only` / `--skip` aliases. See [`InitArgs::fold_step`].
  #[command(subcommand)]
  pub step: Option<InitStepCommand>,
}

/// First-class subcommands for the individual `init` steps. Each
/// variant maps 1:1 to an [`InitStep`] and carries only that step's
/// pre-answer override; [`InitArgs::fold_step`] collapses the chosen
/// variant back into the flat `--only` shape the wizard consumes, so
/// the orchestration in `init::wizard` never has to learn about the
/// subcommand surface.
#[derive(Subcommand, Debug)]
pub enum InitStepCommand {
  /// Install `llama-server` only (`init --only server`).
  Server(InitServerArgs),
  /// Recommend + download a model only (`init --only models`).
  Models(InitModelsArgs),
  /// Write `config.yaml` only (`init --only config`).
  Config(InitConfigArgs),
  /// Patch supported AI dev tools only (`init --only integrations`).
  Integrations(InitIntegrationsArgs),
}

/// Pre-answer flags for `init server`. Mirrors the parent `--install`
/// flag so the override reads naturally on the subcommand.
#[derive(Args, Debug, Default)]
pub struct InitServerArgs {
  /// Pre-answer the install-method prompt. Same grammar as
  /// `init --install` (`brew`, `gh-releases`, `existing`,
  /// `custom:<PATH>`).
  #[arg(long, value_name = "CHOICE", value_parser = parse_install_override)]
  pub install: Option<InstallOverride>,
}

/// Pre-answer flags for `init models`. Mirrors the parent `--model`
/// and `--revision` flags.
#[derive(Args, Debug, Default)]
pub struct InitModelsArgs {
  /// Pre-answer the model-pick prompt (`recommended`, `none`, or
  /// `<owner>/<repo>`).
  #[arg(long, value_name = "CHOICE", value_parser = parse_model_override)]
  pub model: Option<ModelOverride>,
  /// Pin the HuggingFace revision used on the `--model owner/repo`
  /// paste branch.
  #[arg(long, value_name = "SHA", value_parser = parse_revision)]
  pub revision: Option<String>,
}

/// Pre-answer flags for `init config`. Mirrors the parent
/// `--config-step` flag.
#[derive(Args, Debug, Default)]
pub struct InitConfigArgs {
  /// Pre-answer the config-write confirm (`write`, `skip`).
  #[arg(long = "config-step", value_name = "CHOICE", value_enum)]
  pub config_choice: Option<ConfigOverride>,
}

/// Pre-answer flags for `init integrations`. Mirrors the parent
/// `--integrations` flag.
#[derive(Args, Debug, Default)]
pub struct InitIntegrationsArgs {
  /// Comma-separated tool ids to patch (`opencode`, `aider`,
  /// `continue`, `zed`, `pi`, `env-sh`). `none` skips the step.
  #[arg(long, value_name = "TOOLS", value_delimiter = ',', action = ArgAction::Append)]
  pub integrations: Vec<String>,
}

impl InitArgs {
  /// Collapse an `init <step>` subcommand invocation into the flat
  /// flag shape the wizard already understands: pin `only` to the
  /// chosen step, clear `skip`, and copy the subcommand's pre-answer
  /// override onto the parent (the subcommand value wins over a
  /// parent flag of the same name). Bare `init` (no subcommand) is
  /// returned unchanged, so `--only` / `--skip` keep working as
  /// aliases.
  pub fn fold_step(mut self) -> InitArgs {
    let Some(step) = self.step.take() else {
      return self;
    };
    match step {
      InitStepCommand::Server(a) => {
        self.only = vec![InitStep::Server];
        if a.install.is_some() {
          self.install = a.install;
        }
      }
      InitStepCommand::Models(a) => {
        self.only = vec![InitStep::Models];
        if a.model.is_some() {
          self.model = a.model;
        }
        if a.revision.is_some() {
          self.revision = a.revision;
        }
      }
      InitStepCommand::Config(a) => {
        self.only = vec![InitStep::Config];
        if a.config_choice.is_some() {
          self.config_choice = a.config_choice;
        }
      }
      InitStepCommand::Integrations(a) => {
        self.only = vec![InitStep::Integrations];
        if !a.integrations.is_empty() {
          self.integrations = a.integrations;
        }
      }
    }
    self.skip = Vec::new();
    self
  }
}

/// Reject empty `--revision` values up-front so a downstream hf-hub
/// call doesn't silently collapse to the default branch. Non-empty
/// strings are passed through verbatim; hf-hub validates them when
/// resolving the repo (and surfaces a transport error for unknown
/// SHAs / tags via the existing `INIT_DOWNLOAD_FAILED` exit code).
pub fn parse_revision(raw: &str) -> Result<String, String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Err("`--revision <SHA>` requires a non-empty value".into());
  }
  if trimmed.chars().any(char::is_whitespace) {
    return Err(format!(
      "invalid value `{raw}` — revision must not contain whitespace"
    ));
  }
  if trimmed.chars().any(char::is_control) {
    return Err(format!(
      "invalid value `{raw:?}` — revision must not contain control characters"
    ));
  }
  Ok(trimmed.to_string())
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
/// (step 6) always run; the four middle steps are the units of
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
  /// Patch supported AI dev tools (OpenCode, Aider, Continue, Zed,
  /// pi.dev) and emit `~/.config/llamastash/env.sh`. Off in
  /// `--recommended` / `--json` unless `--integrations` is supplied.
  Integrations,
}

/// Convert the `recommend` subcommand's args into the equivalent
/// `init --only models` invocation. The picker (top-10 ranked
/// candidates from `init::recommender`) is shown interactively
/// unless the caller passes `--model` to short-circuit it. We do
/// NOT set `recommended: true` here — that would auto-pick the
/// top entry and skip the prompt, which is `init --recommended`'s
/// job, not `recommend`'s. The user-facing contract for `recommend`
/// is "let me choose from the top picks for my hardware".
pub fn recommend_to_init_args(args: RecommendArgs) -> InitArgs {
  InitArgs {
    recommended: false,
    yes: false,
    json: args.json,
    offline: args.offline,
    only: vec![InitStep::Models],
    skip: Vec::new(),
    install: None,
    model: args.model,
    config_choice: None,
    revision: args.revision,
    no_tui: true,
    integrations: Vec::new(),
    step: None,
  }
}

#[derive(Args, Debug)]
pub struct RecommendArgs {
  /// Emit a structured JSON summary on completion. Mirrors
  /// `init --only models --json` so agents and scripts can consume
  /// the same shape.
  #[arg(long)]
  pub json: bool,
  /// Disable outbound network. Recommend always needs network to
  /// fetch the model snapshot and download weights, so this exits
  /// with `INIT_ABORTED` up-front — kept for parity with
  /// `init --offline`.
  #[arg(long, env = "LLAMASTASH_OFFLINE")]
  pub offline: bool,
  /// Pre-answer the model-pick. Defaults to `recommended` (the
  /// recommender's top pick). Pass `<owner>/<repo>` to override with
  /// a specific HuggingFace repo, or `none` to skip the download
  /// (useful for `--json` dry-runs).
  #[arg(long, value_name = "CHOICE", value_parser = parse_model_override)]
  pub model: Option<ModelOverride>,
  /// Pin the HuggingFace revision (commit SHA, branch, or tag) used
  /// when downloading. Honored only on `--model owner/repo` paste
  /// branch; recommender picks are branch-tracked.
  #[arg(long, value_name = "SHA", value_parser = parse_revision)]
  pub revision: Option<String>,
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

/// `start --ctx` value: a concrete token count or the `auto` knob
/// state. A custom parser (not `ValueEnum`) because the value is either
/// a free integer or the literal `auto`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtxArg {
  /// Delegate the context window to `--fit`.
  Auto,
  /// Pin the context window to this many tokens.
  Value(u32),
}

fn parse_ctx_arg(s: &str) -> Result<CtxArg, String> {
  if s.eq_ignore_ascii_case("auto") {
    return Ok(CtxArg::Auto);
  }
  s.parse::<u32>()
    .map(CtxArg::Value)
    .map_err(|_| format!("expected a token count or `auto`, got `{s}`"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum LaunchMode {
  Chat,
  Embedding,
  Rerank,
}

impl LaunchMode {
  /// Wire-shape label sent to the daemon's `start_model` / `presets_save`
  /// methods. Identical to `clap`'s `rename_all = "lower"` form, but
  /// callers don't have to round-trip through clap's value-enum machinery.
  pub fn as_label(self) -> &'static str {
    match self {
      Self::Chat => "chat",
      Self::Embedding => "embedding",
      Self::Rerank => "rerank",
    }
  }
}

/// Convert the clap value-enum into the `clap`-free domain enum at the
/// CLI boundary. The conversion lives here (cli → launch) so `launch`
/// never depends "up" on the CLI args layer.
impl From<LaunchMode> for crate::launch::mode::LaunchMode {
  fn from(m: LaunchMode) -> Self {
    match m {
      LaunchMode::Chat => crate::launch::mode::LaunchMode::Chat,
      LaunchMode::Embedding => crate::launch::mode::LaunchMode::Embedding,
      LaunchMode::Rerank => crate::launch::mode::LaunchMode::Rerank,
    }
  }
}

/// `llamastash uat` arguments. Only compiled when
/// the `uat` Cargo feature is enabled — the release binary on
/// crates.io and Homebrew bottles never carries this subcommand.
///
/// Global `--quiet` is consumed from the top-level `Cli`; declaring
/// a UAT-local `--quiet` would trip clap's debug-assert refusing
/// duplicate long names across disjoint subcommand scopes (same
/// rationale documented on `InitArgs::config_choice` for
/// `--config-step`).
#[cfg(feature = "uat")]
#[derive(Args, Debug)]
pub struct UatArgs {
  /// GPU backend the host is expected to probe as. Restricted to the
  /// canonical `GpuInfo`
  /// discriminant spellings so a backend mismatch in the report's
  /// `backend.expected` vs `backend.detected` block is unambiguous.
  /// `metal` is not an accepted alias — use `apple_metal`.
  #[arg(long = "host-backend", value_enum, value_name = "BACKEND")]
  pub host_backend: UatBackend,
  /// Optional runtime backend under test. Use this when the machine
  /// probes as one backend (for example `amd`) but the selected
  /// `llama-server` binary intentionally exercises another runtime path
  /// (for example a Vulkan build). Records extra metadata in the report
  /// without falsifying the pre-flight hardware probe.
  #[arg(long = "runtime-backend", value_enum, value_name = "BACKEND")]
  pub runtime_backend: Option<UatBackend>,
  /// `warm` (default) skips the `llama-server` install path on the
  /// assumption that the binary is already on PATH and the reference
  /// GGUF is in the HF cache. `cold` exercises the full install +
  /// pull path and is the per-minor-release coverage gate.
  #[arg(long, value_enum, value_name = "MODE", default_value_t = UatMode::Warm)]
  pub mode: UatMode,
  /// Where to write the structured JSON report. `-` redirects to
  /// stdout; mutually exclusive with the global `--quiet`
  /// (enforced at handle-time). When omitted, the report is emitted
  /// to stdout in TTY-pretty form only.
  #[arg(long, value_name = "PATH")]
  pub report_out: Option<PathBuf>,
  /// Skip the HuggingFace download and use an existing local GGUF
  /// file for the start_model / smoke_chat steps. Useful on machines
  /// without internet access, behind restrictive proxies, or for
  /// faster iteration during development.
  #[arg(long = "local-gguf", value_name = "GGUF_PATH")]
  pub local_gguf: Option<PathBuf>,
}

/// GPU backend names used by UAT. Spellings mirror the `GpuInfo`
/// tagged-union discriminants in `src/gpu/mod.rs` so the report's
/// `backend.expected` / `backend.detected` comparison is direct.
#[cfg(feature = "uat")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum UatBackend {
  Nvidia,
  Amd,
  AppleMetal,
  Vulkan,
  CpuOnly,
}

/// UAT execution mode. `Warm` (default) is the fast per-backend gate;
/// `Cold` exercises the full install + pull path.
#[cfg(feature = "uat")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum UatMode {
  Warm,
  Cold,
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
        InitStep::Integrations => "integrations",
      })
      .collect()
  }

  fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("llamastash").chain(args.iter().copied()))
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
  fn help_carries_styles_and_plain_form_has_no_escapes() {
    use clap::CommandFactory;
    // The custom `Styles` are wired: the styled (ANSI) rendering of
    // help carries escape sequences for the section headers / literals.
    // `ColorChoice` decides whether clap *emits* this form at the
    // stream boundary (proven byte-stable in the piped E2E check); here
    // we prove the styling exists to be emitted at all.
    let help = Cli::command().render_help();
    let ansi = help.ansi().to_string();
    assert!(
      ansi.contains('\u{1b}'),
      "styled help should carry ANSI escapes"
    );
    // The plain `Display` form (what clap writes under `Never` / a
    // non-TTY stdout) has no escapes, so piped help stays byte-stable.
    let plain = help.to_string();
    assert!(
      !plain.contains('\u{1b}'),
      "plain help must have no ANSI escapes: {plain:?}"
    );
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
  fn config_bindings_parses_as_a_config_subcommand() {
    let cli = parse(&["config", "bindings"]);

    assert!(matches!(
      cli.command,
      Some(Command::Config(ConfigArgs {
        action: Some(ConfigAction::Bindings)
      }))
    ));
  }

  #[test]
  fn mouse_focus_flag_parses_and_defaults_off() {
    // Default off so the existing copy-friendly UX is preserved when
    // nothing opts in.
    let baseline = parse(&[]);
    assert!(!baseline.mouse_focus);
    // Flag flips it on at the TUI entry — the dispatcher ORs this
    // with `config.mouse_focus` so either source is sufficient.
    let on = parse(&["--mouse-focus"]);
    assert!(on.mouse_focus);
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
    let result =
      Cli::try_parse_from(["llamastash", "init", "--only", "server", "--skip", "config"]);
    assert!(result.is_err(), "--only and --skip must conflict");
  }

  #[test]
  fn init_recommended_json_offline_flags_parse() {
    let cli = parse(&["init", "--recommended", "--json", "--offline"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.recommended);
        assert!(!args.yes);
        assert!(args.json);
        assert!(args.offline);
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_recommended_parses() {
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
  fn init_help_lists_recommended_and_not_yes() {
    let rendered = Cli::try_parse_from(["llamastash", "init", "--help"])
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
    assert!(!rendered.contains("--yes"), "help must not list --yes");
    assert!(Cli::try_parse_from(["llamastash", "init", "--yes"]).is_ok());
  }

  #[test]
  fn init_yes_json_offline_flags_parse() {
    let cli = parse(&["init", "--yes", "--json", "--offline"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.yes);
        assert!(!args.recommended);
        assert!(args.json);
        assert!(args.offline);
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_recommended_and_yes_are_combinable() {
    let cli = parse(&["init", "--recommended", "--yes"]);
    match cli.command {
      Some(Command::Init(args)) => {
        assert!(args.recommended);
        assert!(args.yes);
      }
      other => panic!("expected init, got {other:?}"),
    }
  }

  /// Extract `InitArgs` from a parsed `init` invocation, panicking on
  /// any other command. Keeps the subcommand-fold tests terse.
  fn init_args(args: &[&str]) -> InitArgs {
    match parse(args).command {
      Some(Command::Init(a)) => a,
      other => panic!("expected init, got {other:?}"),
    }
  }

  #[test]
  fn init_step_subcommand_folds_to_only() {
    // `init <step>` is sugar for `init --only <step>` — fold_step must
    // pin `only` to that single step and clear `skip`.
    for (argv, expected) in [
      (vec!["init", "server"], InitStep::Server),
      (vec!["init", "models"], InitStep::Models),
      (vec!["init", "config"], InitStep::Config),
      (vec!["init", "integrations"], InitStep::Integrations),
    ] {
      let folded = init_args(&argv).fold_step();
      assert_eq!(
        steps(&folded.only),
        steps(&[expected]),
        "{argv:?} must fold to only=[{expected:?}]"
      );
      assert!(folded.skip.is_empty(), "{argv:?} must clear skip");
      assert!(folded.step.is_none(), "fold must consume the subcommand");
    }
  }

  #[test]
  fn init_bare_without_subcommand_is_unchanged_by_fold() {
    // No subcommand → `--only` / `--skip` aliases must survive the fold
    // untouched.
    let only = init_args(&["init", "--only", "server,config"]).fold_step();
    assert_eq!(steps(&only.only), vec!["server", "config"]);
    let skip = init_args(&["init", "--skip", "models"]).fold_step();
    assert_eq!(steps(&skip.skip), vec!["models"]);
    assert!(skip.only.is_empty());
  }

  #[test]
  fn init_step_carries_pre_answer_override() {
    let server = init_args(&["init", "server", "--install", "gh-releases"]).fold_step();
    assert_eq!(server.install, Some(InstallOverride::GhReleases));
    assert_eq!(steps(&server.only), vec!["server"]);

    let model = init_args(&["init", "models", "--model", "none"]).fold_step();
    assert_eq!(model.model, Some(ModelOverride::None));

    let revision = init_args(&["init", "models", "--revision", "abc123"]).fold_step();
    assert_eq!(revision.revision.as_deref(), Some("abc123"));

    let config = init_args(&["init", "config", "--config-step", "write"]).fold_step();
    assert_eq!(config.config_choice, Some(ConfigOverride::Write));

    let integrations =
      init_args(&["init", "integrations", "--integrations", "opencode,aider"]).fold_step();
    assert_eq!(integrations.integrations, vec!["opencode", "aider"]);
  }

  #[test]
  fn init_global_flags_parse_on_either_side_of_subcommand() {
    // `--json` / `--recommended` are `global = true`, so they bind
    // whether they appear before or after the step subcommand.
    for argv in [
      vec!["init", "--json", "models"],
      vec!["init", "models", "--json"],
    ] {
      let folded = init_args(&argv).fold_step();
      assert!(folded.json, "{argv:?} must set --json");
      assert_eq!(steps(&folded.only), vec!["models"]);
    }
    let folded = init_args(&["init", "server", "--recommended"]).fold_step();
    assert!(folded.recommended);
    assert_eq!(steps(&folded.only), vec!["server"]);
  }

  #[test]
  fn init_step_subcommand_overrides_only_flag() {
    // A stray `--only` alongside the subcommand must not win — the
    // subcommand is authoritative.
    let folded = init_args(&["init", "--only", "config", "server"]).fold_step();
    assert_eq!(steps(&folded.only), vec!["server"]);
  }

  #[test]
  fn init_unknown_step_subcommand_rejected() {
    let result = Cli::try_parse_from(["llamastash", "init", "frobnicate"]);
    assert!(result.is_err(), "unknown init subcommand must be rejected");
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
    let result = Cli::try_parse_from(["llamastash", "init", "--install", "custom:"]);
    assert!(result.is_err());
  }

  #[test]
  fn init_install_unknown_value_rejected() {
    let result = Cli::try_parse_from(["llamastash", "init", "--install", "frobnicate"]);
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
    let result = Cli::try_parse_from(["llamastash", "init", "--model", "invalid-no-slash"]);
    assert!(result.is_err());
  }

  #[test]
  fn init_model_with_whitespace_is_rejected() {
    let result = Cli::try_parse_from(["llamastash", "init", "--model", "owner / repo"]);
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
    let result = Cli::try_parse_from(["llamastash", "pull"]);
    assert!(result.is_err(), "pull requires the repo positional");
  }

  #[test]
  fn daemon_subcommands_parse() {
    // Bare `daemon start` parses with `foreground = false` — the
    // default flips to detached so the prompt comes back on its own.
    let cli_start = parse(&["daemon", "start"]);
    match cli_start.command {
      Some(Command::Daemon(DaemonAction::Start {
        foreground,
        state_dir,
        proxy_port,
        ollama_compat,
        no_proxy_fallback,
        proxy_host,
        insecure_no_auth,
        lemonade,
        ds4,
        force,
      })) => {
        assert!(!foreground);
        assert!(state_dir.is_none());
        assert!(proxy_port.is_none());
        assert!(!ollama_compat);
        assert!(!no_proxy_fallback);
        assert!(proxy_host.is_none());
        assert!(!insecure_no_auth);
        assert!(!lemonade);
        assert!(!ds4);
        assert!(!force);
      }
      other => panic!("expected daemon start, got {other:?}"),
    }

    // `--foreground` (and its `-f` alias) flips the bool back on.
    let cli_fg = parse(&["daemon", "start", "--foreground"]);
    assert!(matches!(
      cli_fg.command,
      Some(Command::Daemon(DaemonAction::Start {
        foreground: true,
        ..
      }))
    ));
    let cli_fg_short = parse(&["daemon", "start", "-f"]);
    assert!(matches!(
      cli_fg_short.command,
      Some(Command::Daemon(DaemonAction::Start {
        foreground: true,
        ..
      }))
    ));

    let cli_with_paths = parse(&[
      "daemon",
      "start",
      "--state-dir",
      "/tmp/llamastash-test-state",
    ]);
    match cli_with_paths.command {
      Some(Command::Daemon(DaemonAction::Start {
        foreground,
        state_dir,
        proxy_port,
        ollama_compat,
        no_proxy_fallback,
        proxy_host,
        insecure_no_auth,
        ..
      })) => {
        assert!(!foreground);
        assert_eq!(state_dir, Some(PathBuf::from("/tmp/llamastash-test-state")));
        assert!(proxy_port.is_none());
        assert!(!ollama_compat);
        assert!(!no_proxy_fallback);
        assert!(proxy_host.is_none());
        assert!(!insecure_no_auth);
      }
      other => panic!("expected daemon start with paths, got {other:?}"),
    }

    let cli_with_proxy_port = parse(&["daemon", "start", "--proxy-port", "8080"]);
    match cli_with_proxy_port.command {
      Some(Command::Daemon(DaemonAction::Start {
        foreground,
        state_dir,
        proxy_port,
        ollama_compat,
        no_proxy_fallback,
        proxy_host,
        insecure_no_auth,
        ..
      })) => {
        assert!(!foreground);
        assert!(state_dir.is_none());
        assert_eq!(proxy_port, Some(8080));
        assert!(!ollama_compat);
        assert!(!no_proxy_fallback);
        assert!(proxy_host.is_none());
        assert!(!insecure_no_auth);
      }
      other => panic!("expected daemon start --proxy-port 8080, got {other:?}"),
    }

    // --proxy-port 0 binds an ephemeral port (status surface reports
    // the actual bound port). Accepted as a deliberate dev-only knob.
    let cli_ephemeral = parse(&["daemon", "start", "--proxy-port", "0"]);
    match cli_ephemeral.command {
      Some(Command::Daemon(DaemonAction::Start { proxy_port, .. })) => {
        assert_eq!(proxy_port, Some(0));
      }
      other => panic!("expected daemon start --proxy-port 0, got {other:?}"),
    }

    // --ollama-compat alone flips the bool; the daemon's
    // `build_options` resolves the effective port from it.
    let cli_compat = parse(&["daemon", "start", "--ollama-compat"]);
    match cli_compat.command {
      Some(Command::Daemon(DaemonAction::Start {
        ollama_compat,
        proxy_port,
        ..
      })) => {
        assert!(ollama_compat);
        assert!(proxy_port.is_none());
      }
      other => panic!("expected daemon start --ollama-compat, got {other:?}"),
    }

    // --proxy-host parses an IP (LAN opt-in) and --insecure-no-auth
    // flips the auth waiver; build_options applies precedence + the
    // key-provision / backstop logic downstream.
    let cli_lan = parse(&[
      "daemon",
      "start",
      "--proxy-host",
      "0.0.0.0",
      "--insecure-no-auth",
    ]);
    match cli_lan.command {
      Some(Command::Daemon(DaemonAction::Start {
        proxy_host,
        insecure_no_auth,
        ..
      })) => {
        assert_eq!(proxy_host, Some("0.0.0.0".parse().unwrap()));
        assert!(insecure_no_auth);
      }
      other => panic!("expected daemon start --proxy-host, got {other:?}"),
    }

    // --no-proxy-fallback flips the disable bool; build_options OR-merges
    // it with the config + env so any of the three turns the family-MRU
    // fallback off.
    let cli_no_fallback = parse(&["daemon", "start", "--no-proxy-fallback"]);
    match cli_no_fallback.command {
      Some(Command::Daemon(DaemonAction::Start {
        no_proxy_fallback, ..
      })) => {
        assert!(no_proxy_fallback);
      }
      other => panic!("expected daemon start --no-proxy-fallback, got {other:?}"),
    }

    // --lemonade flips the opt-in enable bool; build_options OR-merges it
    // with the config + env.
    let cli_lemonade = parse(&["daemon", "start", "--lemonade"]);
    match cli_lemonade.command {
      Some(Command::Daemon(DaemonAction::Start { lemonade, .. })) => {
        assert!(lemonade);
      }
      other => panic!("expected daemon start --lemonade, got {other:?}"),
    }

    let cli_stop = parse(&["daemon", "stop"]);
    assert!(matches!(
      cli_stop.command,
      Some(Command::Daemon(DaemonAction::Stop { force: false }))
    ));
    let cli_stop_force = parse(&["daemon", "stop", "--force"]);
    assert!(matches!(
      cli_stop_force.command,
      Some(Command::Daemon(DaemonAction::Stop { force: true }))
    ));
    let cli_stop_force_short = parse(&["daemon", "stop", "-f"]);
    assert!(matches!(
      cli_stop_force_short.command,
      Some(Command::Daemon(DaemonAction::Stop { force: true }))
    ));

    let cli_status = parse(&["daemon", "status"]);
    assert!(matches!(
      cli_status.command,
      Some(Command::Daemon(DaemonAction::Status { json: false }))
    ));
    let cli_status_json = parse(&["daemon", "status", "--json"]);
    assert!(matches!(
      cli_status_json.command,
      Some(Command::Daemon(DaemonAction::Status { json: true }))
    ));
  }

  #[test]
  fn daemon_without_action_is_an_error() {
    let result = Cli::try_parse_from(["llamastash", "daemon"]);
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
        assert_eq!(args.model.as_deref(), Some("qwen-coder"));
        assert_eq!(args.preset.as_deref(), Some("coding"));
        assert_eq!(args.ctx, Some(CtxArg::Value(32768)));
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
    let result = Cli::try_parse_from(["llamastash", "stop", "42", "--all"]);
    assert!(result.is_err());
  }

  #[test]
  fn stop_no_args_opens_picker_at_runtime() {
    // After the picker landed, `llamastash stop` is allowed at the
    // parse layer — the handler opens a cliclack picker over running
    // launches when both `<target>` and `--all` are absent. The
    // picker itself refuses non-TTY / `--json` contexts so a piped
    // caller still gets an actionable error, but that's runtime
    // policy, not a clap constraint.
    assert!(Cli::try_parse_from(["llamastash", "stop"]).is_ok());
    assert!(Cli::try_parse_from(["llamastash", "stop", "--yes"]).is_ok());

    // The explicit forms still parse cleanly.
    assert!(Cli::try_parse_from(["llamastash", "stop", "42"]).is_ok());
    assert!(Cli::try_parse_from(["llamastash", "stop", "--all"]).is_ok());
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
  fn offline_env_truthy_matches_documented_values() {
    // The documented `LLAMASTASH_OFFLINE=1` and friends enable offline;
    // `0`, empty, and junk do not (mirrors `offline_requested`).
    for v in ["1", "true", "TRUE", " yes ", "Yes"] {
      assert!(offline_env_truthy(v), "`{v}` should be truthy");
    }
    for v in ["0", "false", "no", "", " ", "off", "2", "enabled"] {
      assert!(!offline_env_truthy(v), "`{v}` should be falsy");
    }
  }

  #[test]
  fn parse_render_size_accepts_canonical_form() {
    assert_eq!(parse_render_size("120x40").unwrap(), (120, 40));
    // 60x20 is the compact floor — pane geometry hides the right
    // pane by default and the list shrinks to marker + Name.
    assert_eq!(parse_render_size("60x20").unwrap(), (60, 20));
  }

  #[test]
  fn parse_render_size_rejects_too_small_or_malformed() {
    assert!(parse_render_size("garbage").is_err());
    assert!(parse_render_size("100").is_err());
    assert!(parse_render_size("axb").is_err());
    // Below the minimum that still lets the layout split into the
    // title row + info row + body without collapsing into nonsense.
    assert!(parse_render_size("20x5").is_err());
    // 59x20 is one short of the compact floor — sub-60 widths
    // render the "too small" placeholder, so the parser rejects them
    // to keep `--render --render-size` honest.
    assert!(parse_render_size("59x20").is_err());
    // 60x19 is one short on the height axis.
    assert!(parse_render_size("60x19").is_err());
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
  fn presets_save_parses_ctx() {
    let cli = parse(&["presets", "qwen-coder", "save", "pinned", "--ctx", "32768"]);
    match cli.command {
      Some(Command::Presets(args)) => match args.action {
        PresetsAction::Save { ctx, .. } => assert_eq!(ctx, Some(32_768)),
        other => panic!("expected Save, got {other:?}"),
      },
      other => panic!("expected Presets, got {other:?}"),
    }
  }

  #[test]
  fn presets_save_rejects_port() {
    // Config presets carry no port (per-launch, auto-assigned), so the
    // flag was removed — clap must reject it now.
    let result = Cli::try_parse_from([
      "llamastash",
      "presets",
      "qwen-coder",
      "save",
      "pinned",
      "--port",
      "41150",
    ]);
    assert!(
      result.is_err(),
      "--port must no longer parse on presets save"
    );
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
    let result = Cli::try_parse_from(["llamastash", "start", "x", "--reasoning", "maybe"]);
    assert!(result.is_err());
  }

  #[test]
  fn version_flag_works() {
    let result = Cli::try_parse_from(["llamastash", "--version"]);
    // clap returns an "error" with exit kind DisplayVersion for --version.
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    let rendered = err.to_string();
    assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
  }

  #[test]
  fn parse_revision_accepts_typical_sha() {
    assert_eq!(
      parse_revision("abc1234deadbeef").unwrap(),
      "abc1234deadbeef"
    );
  }

  #[test]
  fn parse_revision_trims_surrounding_whitespace() {
    // `clap` strips outer whitespace from argv but env-derived values
    // or future scripted callers may not — trim defensively so a
    // stray newline doesn't make hf-hub probe a non-existent ref.
    assert_eq!(parse_revision("  abc1234  ").unwrap(), "abc1234");
  }

  #[test]
  fn parse_revision_rejects_empty() {
    assert!(parse_revision("").is_err());
    assert!(parse_revision("   ").is_err());
  }

  #[test]
  fn parse_revision_rejects_interior_whitespace() {
    assert!(parse_revision("abc 123").is_err());
    assert!(parse_revision("abc\t123").is_err());
  }

  #[test]
  fn parse_revision_rejects_control_chars() {
    assert!(parse_revision("abc\x00123").is_err());
    assert!(parse_revision("abc\x1b[31m").is_err());
  }

  #[test]
  fn help_flag_lists_every_user_facing_subcommand() {
    let result = Cli::try_parse_from(["llamastash", "--help"]);
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

  #[cfg(feature = "uat")]
  #[test]
  fn uat_default_parses() {
    let cli = parse(&["uat", "--host-backend", "nvidia"]);
    match cli.command {
      Some(Command::Uat(args)) => {
        assert_eq!(args.host_backend, UatBackend::Nvidia);
        assert_eq!(args.runtime_backend, None);
        assert_eq!(args.mode, UatMode::Warm);
        assert!(args.report_out.is_none());
      }
      other => panic!("expected Uat, got {other:?}"),
    }
  }

  #[cfg(feature = "uat")]
  #[test]
  fn uat_quiet_is_the_global_flag() {
    // No UAT-local `--quiet` is declared; the top-level global one
    // applies. Refusing to duplicate the long name matches the same
    // clap debug-assert rationale documented on `--config-step`.
    let cli = parse(&[
      "--quiet",
      "uat",
      "--host-backend",
      "nvidia",
      "--mode",
      "cold",
      "--report-out",
      "/tmp/r.json",
    ]);
    assert!(cli.quiet);
    match cli.command {
      Some(Command::Uat(args)) => {
        assert_eq!(args.host_backend, UatBackend::Nvidia);
        assert_eq!(args.runtime_backend, None);
        assert_eq!(args.mode, UatMode::Cold);
        assert_eq!(args.report_out, Some(PathBuf::from("/tmp/r.json")));
      }
      other => panic!("expected Uat, got {other:?}"),
    }
  }

  #[cfg(feature = "uat")]
  #[test]
  fn uat_accepts_every_canonical_backend() {
    for (raw, expected) in [
      ("nvidia", UatBackend::Nvidia),
      ("amd", UatBackend::Amd),
      ("apple_metal", UatBackend::AppleMetal),
      ("vulkan", UatBackend::Vulkan),
      ("cpu_only", UatBackend::CpuOnly),
    ] {
      match parse(&["uat", "--host-backend", raw]).command {
        Some(Command::Uat(args)) => assert_eq!(args.host_backend, expected),
        other => panic!("expected Uat for host-backend={raw}, got {other:?}"),
      }
    }
  }

  #[cfg(feature = "uat")]
  #[test]
  fn uat_accepts_runtime_backend_override() {
    match parse(&[
      "uat",
      "--host-backend",
      "amd",
      "--runtime-backend",
      "vulkan",
    ])
    .command
    {
      Some(Command::Uat(args)) => {
        assert_eq!(args.host_backend, UatBackend::Amd);
        assert_eq!(args.runtime_backend, Some(UatBackend::Vulkan));
      }
      other => panic!("expected Uat with runtime-backend, got {other:?}"),
    }
  }

  #[cfg(feature = "uat")]
  #[test]
  fn uat_rejects_metal_alias() {
    // `metal` is the user-friendly shorthand but it does not match the
    // `GpuInfo::AppleMetal` discriminant — refusing it at parse time
    // keeps the report's `backend.expected` vs `backend.detected`
    // comparison unambiguous on Apple Silicon hosts.
    let result = Cli::try_parse_from(["llamastash", "uat", "--host-backend", "metal"]);
    assert!(
      result.is_err(),
      "`--host-backend metal` must be refused; use apple_metal"
    );
  }

  #[cfg(not(feature = "uat"))]
  #[test]
  fn uat_subcommand_absent_without_feature() {
    // Build invariant: no UAT entry point when the
    // feature is off. clap rejects the subcommand at parse time.
    let result = Cli::try_parse_from(["llamastash", "uat", "--host-backend", "nvidia"]);
    assert!(
      result.is_err(),
      "`uat` must not parse without `--features uat`"
    );
  }
}
