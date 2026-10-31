//! `llamastash session <tool> [MODEL]` — one coding tool, one model, one run.
//!
//! The tool's per-run settings go in as env vars, CLI flags, or a generated
//! file only that run loads, then the process is replaced by the tool. No
//! tool's global config is read-modified-written here: that is what
//! `integrations` does, and it changes what the tool does in every later run.
//!
//! Model resolution follows the proxy's rules (a `proxy.aliases` name that owns
//! no model, then a model reference, then a live launch), so the same string
//! works on every surface. A model that is not running is started and waited
//! for, which gives the real context window and surfaces a load failure before
//! the tool opens.

use std::borrow::Cow;

use clap::ValueEnum;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::cli::api_key::KEYLESS_STUB;
use crate::cli::cli_args::{Cli, LaunchMode, SessionArgs, SessionTool};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{
  CliExit, CliResult, BINARY_NOT_FOUND, DAEMON_UNREACHABLE, LAUNCH_FAILED, MODEL_NOT_FOUND,
  UNKNOWN, USAGE,
};
use crate::cli::resolve::{
  fetch_catalog, fetch_status, resolve_model_or_launch, CatalogRow, RunningRow, StatusSnapshot,
};
use crate::cli::start::{poll_until_settled, start_row_with_preset};
use crate::config::Config;
use crate::init::external::proxy_url;
use crate::init::external::{all_patchers, PatchContext, PatchModel, SessionLaunch, ToolPatcher};
use crate::ipc::Client;
use crate::launch::resolve::{
  join_named_reference, name_matches, published_ids_for, resolve_exact_reference,
};
use crate::proxy::alias::AliasTable;

/// The states a launch can be handed to a tool from: anything the proxy can
/// route a request for.
const LIVE_STATES: [&str; 3] = ["launching", "loading", "ready"];

pub async fn handle(args: SessionArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let catalog = fetch_catalog(&mut client).await?;
  let status = fetch_status(&mut client).await?;
  // The tool's traffic has to reach a proxy that is actually serving, so this
  // is settled before anything is started or written. `proxy_base_url` is the one
  // rule for which address that is (a port that moved, a `--proxy-port` launch).
  let proxy_base_url = proxy_base_url(&status)?;
  // A real bearer key is the only secret in this launch, so it is the only value
  // `--dry-run` masks. The stub a keyless proxy ignores is safe to show.
  let secret = config.proxy.effective_api_key();
  let api_key = proxy_api_key(config);

  let tool = match args.tool {
    Some(tool) => tool,
    None => pick_tool(args.json).await?,
  };
  // The tool binary is looked up before a model is picked or started: a tool
  // that is not installed should cost no question and no load, and the program
  // depends only on the tool. A dry run carries the failure instead of raising
  // it, because it describes a command it does not need to run.
  let program = match lookup_program(tool) {
    Err(e) if !args.dry_run => return Err(e),
    found => found,
  };
  let mut target = resolve_target(&catalog, &status.models, &args, config).await?;
  let mut launch = describe(
    tool,
    &target,
    &proxy_base_url,
    &api_key,
    Path::new(NO_FILES_DIR),
  )?;
  // Only a tool that generates a file needs the cache dir, and only a launch
  // that carries one says so. Resolving it up front would fail a `claude` or
  // `aider` session — which write nothing — on a machine with no cache dir.
  // A second describe with the real dir gives pi its file path; the first pass
  // only answers whether one is needed.
  let files_dir = if launch.files.is_empty() {
    PathBuf::from(NO_FILES_DIR)
  } else {
    let dir = session_files_dir()?;
    launch = describe(tool, &target, &proxy_base_url, &api_key, &dir)?;
    dir
  };
  if args.dry_run {
    return emit_dry_run(&args, tool, &target, &launch, secret.as_deref());
  }
  let program = program?;
  // Everything that can fail is checked before the process is handed over: once
  // the tool is exec'd there is nothing of ours left to report a late failure.
  settle(&mut client, &catalog, config, &mut target).await?;
  launch = describe(tool, &target, &proxy_base_url, &api_key, &files_dir)?;
  write_session_files(&launch)?;
  if !cli.quiet {
    eprintln!("{}", target.note(tool.as_str()));
  }
  run(program, launch, &args.tool_args)
}

/// Stand-in for a launch that writes no file. Only a tool that generates one
/// (pi) ever has its real path read, and by then the real dir is in hand.
const NO_FILES_DIR: &str = "";

/// The address a client must send model traffic to: the one the running proxy
/// listens on, which is not always the configured one (a port that moved past a
/// busy one, a `daemon start --proxy-port`).
pub(crate) fn proxy_base_url(status: &StatusSnapshot) -> Result<String, CliExit> {
  proxy_url::from_proxy_block(&status.proxy).ok_or_else(|| {
    let state = status
      .proxy
      .get("status")
      .and_then(Value::as_str)
      .unwrap_or("unknown");
    CliExit::new(
      DAEMON_UNREACHABLE,
      format!(
        "the proxy is not listening ({state}); this command routes its model traffic through it"
      ),
    )
  })
}

/// The bearer key to send the proxy, or the stub a keyless proxy ignores but
/// clients that require a non-empty key still need.
pub(crate) fn proxy_api_key(config: &Config) -> String {
  config
    .proxy
    .effective_api_key()
    .unwrap_or_else(|| KEYLESS_STUB.to_string())
}

/// The tool's binary on `PATH`. The patcher names the program without a model in
/// the way, so this can run before the pickers and before anything is started.
fn lookup_program(tool: SessionTool) -> Result<PathBuf, CliExit> {
  let program = patcher_for(tool)
    .and_then(|p| p.session_program())
    .ok_or_else(|| no_launch(tool))?;
  which::which(program)
    .map_err(|_| CliExit::new(BINARY_NOT_FOUND, format!("`{program}` not found on PATH")))
}

/// The tool's launch for this target: what to exec, with what.
fn describe(
  tool: SessionTool,
  target: &Target,
  proxy_base_url: &str,
  api_key: &str,
  files_dir: &Path,
) -> Result<SessionLaunch, CliExit> {
  let patcher = patcher_for(tool).ok_or_else(|| no_launch(tool))?;
  let ctx = PatchContext {
    proxy_base_url: proxy_base_url.to_string(),
    api_key: api_key.to_string(),
    models: vec![target.patch_model()],
  };
  patcher
    .session(&ctx, files_dir)
    .ok_or_else(|| no_launch(tool))
}

fn no_launch(tool: SessionTool) -> CliExit {
  CliExit::new(
    USAGE,
    format!("`{}` has no per-run launch", tool.patcher_id()),
  )
}

/// The model the tool is pointed at, and the launch behind it (none for a
/// `--dry-run` of a model that is not running).
struct Target {
  row: CatalogRow,
  /// The launch name the reference asked for. Kept even when no launch exists:
  /// a dry run has to print the id the real run will use.
  name: Option<String>,
  launch: Option<RunningRow>,
  /// The id to hand the tool: what `/v1/models` publishes, plus `@<name>` when
  /// the launch is or will be named. Without the suffix the proxy would
  /// auto-start a second, unnamed copy of the same model.
  id: String,
  context_window: Option<u64>,
}

impl Target {
  fn patch_model(&self) -> PatchModel {
    let mut model = PatchModel::from_catalog_row(&self.row, self.id.clone());
    // The launch's own window, not the trained one: a tool sizes its history
    // against this and overflowing the server costs the whole session.
    model.context_window = self.context_window;
    model
  }

  /// The one line printed on stderr before the tool takes the process over.
  fn note(&self, tool: &str) -> String {
    let Some(launch) = &self.launch else {
      return format!("{tool} → {}", self.id);
    };
    let ctx = self
      .context_window
      .map(|c| format!(", ctx {c}"))
      .unwrap_or_default();
    format!("{tool} → {} ({}{ctx})", self.id, launch.launch_id)
  }
}

/// Resolve what to run. Reads only: nothing is started here, and nothing is
/// written.
async fn resolve_target(
  catalog: &[CatalogRow],
  running: &[RunningRow],
  args: &SessionArgs,
  config: &Config,
) -> Result<Target, CliExit> {
  let (row, name, launch) = match args.model.as_deref() {
    None => match latest_chat_launch(running) {
      Some(found) => {
        let name = found.name.clone();
        (catalog_row(catalog, &found.model_path)?, name, Some(found))
      }
      // Nothing live to point at, so ask for a model to start.
      None => (pick_model_to_start(catalog, args.json).await?, None, None),
    },
    Some(reference) => {
      let aliases = AliasTable::from_config(&config.proxy.aliases);
      let reference = alias_target(&aliases, catalog, reference);
      let (row, name) = resolve_model_or_launch(catalog, running, &reference)?;
      let launch = launch_by_identity(running, reference.as_ref(), &row.path)
        .or_else(|| matching_launch(running, &row.path, name.as_deref()));
      (row, name, launch)
    }
  };
  let mut target = Target {
    row,
    name,
    launch,
    id: String::new(),
    context_window: None,
  };
  fill(&mut target, catalog, config);
  if let Some(kind) = non_chat_mode(&target) {
    return Err(CliExit::new(
      USAGE,
      format!(
        "`{}` is {kind} model; session tools need a chat model",
        target.row.name()
      ),
    ));
  }
  Ok(target)
}

/// Start the model when nothing serves it yet, and wait for the launch to be
/// usable. Called after the tool binary is found and after `--dry-run` has
/// returned, so a dry run never reaches this.
async fn settle(
  client: &mut Client,
  catalog: &[CatalogRow],
  config: &Config,
  target: &mut Target,
) -> Result<(), CliExit> {
  if target.launch.as_ref().is_some_and(|l| l.state == "ready") {
    return Ok(());
  }
  let poll = match target.launch.as_ref() {
    Some(live) => poll_until_settled(client, Some(&live.launch_id), &target.row.path).await?,
    None => {
      // `@<name>` is both the address and the preset to launch with; anything
      // else takes the model's own default preset, as `start` does. The mode is
      // stated because a session can only be chat: a model with no mode hint
      // would otherwise hit `start`'s "pass --mode" error, and this command has
      // no such flag.
      let resp = start_row_with_preset(
        client,
        &target.row,
        target.name.as_deref(),
        Some(LaunchMode::Chat),
      )
      .await?;
      let launch_id = resp.get("launch_id").and_then(Value::as_str);
      poll_until_settled(client, launch_id, &target.row.path).await?
    }
  };
  let settled = poll.ok_or_else(|| {
    let which = match target.launch.as_ref() {
      Some(live) => format!("{} ({})", target.row.name(), live.launch_id),
      None => target.row.name(),
    };
    CliExit::new(
      LAUNCH_FAILED,
      format!("{which} did not become ready in time"),
    )
  })?;
  if settled.state != "ready" {
    return Err(launch_failed(&settled));
  }
  target.launch = Some(settled);
  fill(target, catalog, config);
  Ok(())
}

fn launch_failed(launch: &RunningRow) -> CliExit {
  CliExit::new(
    LAUNCH_FAILED,
    format!(
      "launch {} is {}: {}",
      launch.launch_id,
      launch.state,
      launch.state_cause.as_deref().unwrap_or("no cause reported")
    ),
  )
}

/// The id and the window to hand the tool, from whatever is known about the
/// launch: the live launch's own name and window when there is one, else the
/// reference's name and the window the preset `start` would use. A dry run of a
/// model that is not up has to print what the real run will.
fn fill(target: &mut Target, catalog: &[CatalogRow], config: &Config) {
  let published = published_id(catalog, &target.row);
  let name = target
    .launch
    .as_ref()
    .and_then(|l| l.name.clone())
    .or_else(|| target.name.clone());
  target.id = match name {
    Some(name) => join_named_reference(&published, &name),
    None => published,
  };
  target.context_window = declared_ctx(
    target.launch.as_ref(),
    &target.row,
    preset_ctx(config, catalog, &target.row, target.name.as_deref()),
  );
}

pub(crate) fn published_id(catalog: &[CatalogRow], row: &CatalogRow) -> String {
  published_ids_for(catalog, std::slice::from_ref(row))
    .into_iter()
    .next()
    .unwrap_or_else(|| row.public_id())
}

/// The context the preset `start` would launch this row with: the one named by
/// the reference, else the model's own `default:`. `None` when neither pins a
/// context, or when the default is the `auto` sentinel.
fn preset_ctx(
  config: &Config,
  catalog: &[CatalogRow],
  row: &CatalogRow,
  name: Option<&str>,
) -> Option<u64> {
  let effective = crate::launch::presets::effective_presets(
    &row.name(),
    &row.path,
    row.arch.as_deref(),
    &config.presets,
    catalog,
  );
  let wanted = name.or(effective.default.as_deref())?;
  let preset = effective.presets.get(wanted)?;
  preset
    .params
    .knobs
    .u32(crate::launch::knobs::kid("ctx-size"))
    .or(preset.params.ctx)
    .map(u64::from)
}

fn catalog_row(catalog: &[CatalogRow], path: &str) -> Result<CatalogRow, CliExit> {
  catalog
    .iter()
    .find(|c| c.path == path)
    .cloned()
    .ok_or_else(|| {
      CliExit::new(
        MODEL_NOT_FOUND,
        format!("the running model is not in the catalog: {path}"),
      )
    })
}

/// The most recently started live chat launch. Launch ids are never reused, so
/// the highest `L<n>` is the newest start.
fn latest_chat_launch(running: &[RunningRow]) -> Option<RunningRow> {
  running
    .iter()
    .filter(|r| r.mode == "chat" && LIVE_STATES.contains(&r.state.as_str()))
    .max_by_key(|r| launch_number(r))
    .cloned()
}

/// The live launch a resolved model plus optional name means. A named reference
/// needs that name; a plain one prefers an unnamed launch of that model, else
/// its latest launch.
fn matching_launch(running: &[RunningRow], path: &str, name: Option<&str>) -> Option<RunningRow> {
  let live: Vec<&RunningRow> = running
    .iter()
    .filter(|r| r.model_path == path && LIVE_STATES.contains(&r.state.as_str()))
    .collect();
  if let Some(want) = name {
    return live
      .into_iter()
      .find(|r| name_matches(r.name.as_deref(), want))
      .cloned();
  }
  let unnamed: Vec<&RunningRow> = live.iter().copied().filter(|r| r.name.is_none()).collect();
  let pool = if unnamed.is_empty() { &live } else { &unnamed };
  pool
    .iter()
    .copied()
    .max_by_key(|r| launch_number(r))
    .cloned()
}

fn launch_number(row: &RunningRow) -> u64 {
  row
    .launch_id
    .strip_prefix('L')
    .and_then(|n| n.parse().ok())
    .unwrap_or(0)
}

/// The live launch a reference names outright: `L4` or `41100`. Those are
/// identities, so matching on them cannot drift to another launch of the same
/// model the way a path-and-name search can. `path` is the model the reference
/// resolved to, which keeps a model actually named `L4` from answering for the
/// launch of that id.
fn launch_by_identity(running: &[RunningRow], reference: &str, path: &str) -> Option<RunningRow> {
  running
    .iter()
    .find(|r| {
      r.model_path == path
        && (r.launch_id.eq_ignore_ascii_case(reference) || r.port.to_string() == reference)
    })
    .cloned()
}

/// `Some(…)` for a target that cannot carry a chat session, phrased for the
/// error: "is an embedding model". The embedding decision is the patchers' own,
/// made on the same `PatchModel` they get (id with the `@<name>` suffix and
/// all), so a target that passes this gate is one they will take. The rerank
/// hint is checked here because `is_embed` says nothing about rerank models.
fn non_chat_mode(target: &Target) -> Option<&'static str> {
  not_chat(&target.row, &target.patch_model().id)
}

/// The same rule for a row on its own, before a launch or preset is known, which
/// is all the model picker has to judge. `id` is the id the tool will be handed.
pub(crate) fn not_chat(row: &CatalogRow, id: &str) -> Option<&'static str> {
  if PatchModel::from_catalog_row(row, id.to_string()).is_embed {
    return Some("an embedding");
  }
  match row.mode_hint.as_deref() {
    Some("rerank") => Some("a rerank"),
    Some("image") => Some("an image"),
    Some("tts") => Some("a text-to-speech"),
    Some("stt") => Some("a speech-to-text"),
    _ => None,
  }
}

/// The reference to resolve. A `proxy.aliases` name answers only when it owns no
/// model outright, which is the proxy's own precedence.
fn alias_target<'a>(
  aliases: &'a AliasTable,
  catalog: &[CatalogRow],
  reference: &'a str,
) -> Cow<'a, str> {
  let Some(target) = aliases.target(reference) else {
    return reference.into();
  };
  match resolve_exact_reference(catalog, reference) {
    Ok(_) => reference.into(),
    Err(_) => target.into(),
  }
}

/// The window to hand the tool: what the engine settled on, else what the launch
/// asked for, else what the preset pins, else the trained window.
fn declared_ctx(launch: Option<&RunningRow>, row: &CatalogRow, preset: Option<u64>) -> Option<u64> {
  launch
    .and_then(|l| l.resolved_ctx.map(u64::from))
    .or_else(|| {
      launch
        .and_then(|l| l.params.as_ref())
        .and_then(|p| p.get("ctx"))
        .and_then(Value::as_u64)
    })
    .or(preset)
    .or(row.native_ctx)
}

fn patcher_for(tool: SessionTool) -> Option<Box<dyn ToolPatcher>> {
  let id = tool.patcher_id();
  all_patchers().into_iter().find(|p| p.id() == id)
}

/// The tool when none was typed. A caller that cannot answer a prompt gets the
/// choices spelled out instead of a prompt it cannot see.
async fn pick_tool(json: bool) -> Result<SessionTool, CliExit> {
  if !crate::cli::picker::interactive(json) {
    let tools: Vec<&str> = SessionTool::value_variants()
      .iter()
      .map(|t| t.as_str())
      .collect();
    return Err(CliExit::new(
      USAGE,
      format!("no tool given; pass one of: {}", tools.join(", ")),
    ));
  }
  let tools: &[SessionTool] = SessionTool::value_variants();
  let items = tools
    .iter()
    .map(|t| {
      let hint = patcher_for(*t)
        .map(|p| p.display_name().to_string())
        .unwrap_or_default();
      (t.as_str().to_string(), hint)
    })
    .collect();
  let idx = crate::cli::picker::select_one("Pick a tool".to_string(), 0, items)
    .await
    .map_err(|e| e.into_exit(USAGE, "session picker join", "session picker"))?;
  Ok(*tools.get(idx).expect("select returns an index it offered"))
}

/// The model picker for a session with no MODEL and nothing running.
async fn pick_model_to_start(catalog: &[CatalogRow], json: bool) -> Result<CatalogRow, CliExit> {
  if !crate::cli::picker::interactive(json) {
    return Err(CliExit::new(
      MODEL_NOT_FOUND,
      "no chat model is running; start one or pass a MODEL",
    ));
  }
  let rows = chat_rows(catalog);
  if rows.is_empty() {
    return Err(CliExit::new(
      MODEL_NOT_FOUND,
      "no chat model is running and the catalog has none to start",
    ));
  }
  crate::cli::picker::pick_catalog_row(&rows, "session", "Pick a model to start").await
}

/// Catalog rows a session can use. Without this the picker would offer rows the
/// gate refuses as soon as one is picked.
fn chat_rows(catalog: &[CatalogRow]) -> Vec<CatalogRow> {
  catalog
    .iter()
    .filter(|r| not_chat(r, &published_id(catalog, r)).is_none())
    .cloned()
    .collect()
}

/// Where a tool's generated per-run files go. One file per distinct content, so
/// concurrent sessions never overwrite each other.
fn session_files_dir() -> Result<PathBuf, CliExit> {
  crate::util::paths::cache_dir()
    .map(|d| d.join("sessions"))
    .ok_or_else(|| CliExit::new(UNKNOWN, "no cache directory available for session files"))
}

fn write_session_files(launch: &SessionLaunch) -> Result<(), CliExit> {
  for file in &launch.files {
    let dir = file.path.parent().unwrap_or(Path::new("."));
    crate::util::atomic_write::write_secure(
      dir,
      "session-",
      &file.path,
      file.content.as_bytes(),
      Some(0o600),
    )
    .map_err(|e| {
      CliExit::new(
        UNKNOWN,
        format!("could not write {}: {e}", file.path.display()),
      )
    })?;
  }
  Ok(())
}

/// Only the bearer key is a secret, so only a value equal to it is masked. The
/// stub a keyless proxy ignores is not one, and with no key configured nothing is
/// masked at all: a dry run you cannot paste back is useless.
fn shown_value(secret: Option<&str>, value: &str) -> String {
  if secret == Some(value) {
    "***".to_string()
  } else {
    value.to_string()
  }
}

/// `--dry-run` output: one shell-quoted line, or the contract's JSON object.
fn emit_dry_run(
  args: &SessionArgs,
  tool: SessionTool,
  target: &Target,
  launch: &SessionLaunch,
  secret: Option<&str>,
) -> CliResult {
  let body = if args.json {
    let mut env = Map::new();
    for (name, value) in &launch.env {
      env.insert(name.clone(), json!(shown_value(secret, value)));
    }
    crate::cli::output::pretty_json(&json!({
      "tool": tool.as_str(),
      "model": target.id,
      "launch_id": target.launch.as_ref().map(|l| l.launch_id.clone()),
      "running": target.launch.is_some(),
      "context_window": target.context_window,
      "program": launch.program,
      "args": launch.args,
      "env": env,
      "env_remove": launch.env_remove,
      "files": launch.files,
      "tool_args": args.tool_args,
    }))
  } else {
    shell_line(launch, &args.tool_args, secret)
  };
  println!("{body}");
  Ok(())
}

/// The launch as one line a POSIX shell runs the same way `session` does. A var
/// the launch removes goes through `env -u`, or a pasted line would hand the
/// tool whatever the user's shell has set.
fn shell_line(launch: &SessionLaunch, tool_args: &[String], secret: Option<&str>) -> String {
  let mut line = String::new();
  if !launch.env_remove.is_empty() {
    line.push_str("env");
    for name in &launch.env_remove {
      line.push_str(&format!(" -u {name}"));
    }
    line.push(' ');
  }
  for (name, value) in &launch.env {
    line.push_str(&format!(
      "{name}={} ",
      shell_quote(&shown_value(secret, value))
    ));
  }
  line.push_str(launch.program);
  for arg in launch.args.iter().chain(tool_args.iter()) {
    line.push(' ');
    line.push_str(&shell_quote(arg));
  }
  line
}

/// Single-quote a token the way a POSIX shell needs it, so the `--dry-run` line
/// can be pasted back.
fn shell_quote(value: &str) -> String {
  format!("'{}'", value.replace('\'', r"'\''"))
}

/// Hand the process to the tool. On Unix the child replaces us, so signals and
/// the terminal go straight to it and nothing of ours stays resident; elsewhere
/// it is spawned and waited for, and its exit code becomes ours.
fn run(program: PathBuf, launch: SessionLaunch, tool_args: &[String]) -> CliResult {
  let mut cmd = std::process::Command::new(&program);
  cmd.args(&launch.args).args(tool_args);
  for (name, value) in &launch.env {
    cmd.env(name, value);
  }
  for name in &launch.env_remove {
    cmd.env_remove(name);
  }
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    // Only returns when the exec itself failed.
    let err = cmd.exec();
    Err(CliExit::new(
      BINARY_NOT_FOUND,
      format!("could not start `{}`: {err}", launch.program),
    ))
  }
  #[cfg(not(unix))]
  match cmd.status() {
    Ok(status) => Err(CliExit::code_only(status.code().unwrap_or(UNKNOWN))),
    Err(e) => Err(CliExit::new(
      BINARY_NOT_FOUND,
      format!("could not start `{}`: {e}", launch.program),
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A running row with the fields a session looks at; the rest are what a
  /// `ready` row always carries.
  fn live(launch_id: &str, path: &str, mode: &str, name: Option<&str>) -> RunningRow {
    RunningRow {
      launch_id: launch_id.to_string(),
      model_path: path.to_string(),
      id: None,
      port: 41100,
      mode: mode.to_string(),
      name: name.map(str::to_string),
      preset: None,
      state: "ready".into(),
      state_cause: None,
      pid: Some(1),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
      ctx_clamped: false,
      preset_count: 0,
      preset_default: None,
      backend: None,
      stop_grace_secs: 0,
      request_stats: None,
    }
  }

  fn chat(path: &str) -> CatalogRow {
    let mut row = CatalogRow::for_resolution(path.into(), None, None);
    row.mode_hint = Some("chat".into());
    row
  }

  /// A target the way `resolve_target` builds one, so the id and window come out
  /// through the same `fill` the command uses.
  fn target(
    catalog: &[CatalogRow],
    row: CatalogRow,
    name: Option<&str>,
    launch: Option<RunningRow>,
  ) -> Target {
    let mut t = Target {
      row,
      name: name.map(str::to_string),
      launch,
      id: String::new(),
      context_window: None,
    };
    fill(&mut t, catalog, &Config::default());
    t
  }

  /// A config whose `chat.gguf` has one preset, `coder`, pinning a 64k window.
  fn presets_config(default: Option<&str>) -> Config {
    use crate::config::{ConfigPresetBlock, PresetBody};
    let body = PresetBody {
      knobs: crate::knobset! { ctx: 65_536 },
      ..PresetBody::default()
    };
    let block = ConfigPresetBlock {
      default: default.map(str::to_string),
      entries: [("coder".to_string(), body)].into_iter().collect(),
    };
    Config {
      presets: [("chat.gguf".to_string(), block)].into_iter().collect(),
      ..Config::default()
    }
  }

  #[test]
  fn every_session_tool_maps_to_a_patcher_with_a_launch() {
    use clap::ValueEnum;
    let ctx = PatchContext::fixture(&["qwen3-coder-30b"]);
    for tool in SessionTool::value_variants() {
      let patcher =
        patcher_for(*tool).unwrap_or_else(|| panic!("{} is not a patcher", tool.as_str()));
      assert!(
        patcher.session(&ctx, Path::new("/sessions")).is_some(),
        "{} has no per-run launch",
        tool.as_str()
      );
    }
  }

  #[test]
  fn the_latest_start_wins_when_no_model_is_given() {
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", None),
      {
        let mut loading = live("L5", "/m/chat.gguf", "chat", Some("coder"));
        loading.state = "loading".into();
        loading
      },
      live("L7", "/m/embed.gguf", "embedding", None),
    ];
    assert_eq!(
      latest_chat_launch(&running)
        .expect("a chat launch")
        .launch_id,
      "L5",
      "the highest launch number, even while it is still loading"
    );
  }

  #[test]
  fn an_embedding_only_daemon_has_nothing_to_hand_a_tool() {
    let running = vec![
      live("L2", "/m/embed.gguf", "embedding", None),
      live("L3", "/m/rerank.gguf", "rerank", None),
    ];
    assert!(latest_chat_launch(&running).is_none());
  }

  #[test]
  fn a_plain_reference_prefers_the_unnamed_launch() {
    let catalog = vec![chat("/m/chat.gguf")];
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", Some("coder")),
      live("L3", "/m/chat.gguf", "chat", None),
    ];
    let unnamed = matching_launch(&running, "/m/chat.gguf", None).expect("unnamed launch");
    assert_eq!(unnamed.launch_id, "L3");
    assert_eq!(
      target(&catalog, catalog[0].clone(), None, Some(unnamed)).id,
      "chat",
      "an unnamed launch keeps the plain published id"
    );
  }

  #[test]
  fn only_named_launches_fall_to_the_latest_one_and_carry_the_name() {
    let catalog = vec![chat("/m/chat.gguf")];
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", Some("coder")),
      live("L4", "/m/chat.gguf", "chat", Some("fast")),
    ];
    assert_eq!(
      matching_launch(&running, "/m/chat.gguf", None)
        .expect("latest launch")
        .launch_id,
      "L4"
    );
    let coder = matching_launch(&running, "/m/chat.gguf", Some("coder")).expect("named launch");
    assert_eq!(
      target(&catalog, catalog[0].clone(), None, Some(coder)).id,
      "chat@coder",
      "without the suffix the proxy would start a second, unnamed copy"
    );
    assert!(
      matching_launch(&running, "/m/chat.gguf", Some("gone")).is_none(),
      "a name with no live launch is not running"
    );
  }

  #[test]
  fn a_reference_to_a_live_launch_id_is_that_exact_launch() {
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", Some("coder")),
      live("L9", "/m/chat.gguf", "chat", None),
    ];
    // Both are the same model, so a path-and-name search would answer L9 for an
    // unnamed `L2`.
    assert_eq!(
      launch_by_identity(&running, "L2", "/m/chat.gguf")
        .expect("L2")
        .launch_id,
      "L2",
      "a launch id is an identity, not a model reference"
    );
    assert_eq!(
      launch_by_identity(&running, "41100", "/m/chat.gguf")
        .expect("by port")
        .launch_id,
      "L2",
      "a port is one too"
    );
    assert_eq!(
      launch_by_identity(&running, "l2", "/m/chat.gguf")
        .expect("lower case id")
        .launch_id,
      "L2",
      "the id is matched the way `stop` matches it, case included"
    );
    assert!(launch_by_identity(&running, "chat.gguf", "/m/chat.gguf").is_none());
    assert!(
      launch_by_identity(&running, "L2", "/m/other.gguf").is_none(),
      "the identity only answers for the model it belongs to"
    );
  }

  #[test]
  fn a_dry_run_of_a_model_that_is_not_up_keeps_the_name_and_the_preset_window() {
    let catalog = vec![chat("/m/chat.gguf")];
    let mut named = Target {
      row: catalog[0].clone(),
      name: Some("coder".into()),
      launch: None,
      id: String::new(),
      context_window: None,
    };
    fill(&mut named, &catalog, &presets_config(None));
    assert_eq!(named.id, "chat@coder", "the id the real run will use");
    assert_eq!(
      named.context_window,
      Some(65_536),
      "the window the preset pins, not the trained one"
    );

    // With no name in the reference, `start` would use the model's own
    // `default:` preset, so that is the window a dry run reports.
    let mut plain = Target {
      row: catalog[0].clone(),
      name: None,
      launch: None,
      id: String::new(),
      context_window: None,
    };
    fill(&mut plain, &catalog, &presets_config(Some("coder")));
    assert_eq!(plain.context_window, Some(65_536));
    assert_eq!(plain.id, "chat");
    // A live launch wins over both: what the engine serves is the answer.
    let mut live_row = live("L1", "/m/chat.gguf", "chat", None);
    live_row.resolved_ctx = Some(8192);
    named.launch = Some(live_row);
    named.name = None;
    fill(&mut named, &catalog, &presets_config(None));
    assert_eq!(named.context_window, Some(8192));
  }

  #[test]
  fn the_model_picker_offers_only_chat_rows() {
    let mut embed = chat("/m/e.gguf");
    embed.mode_hint = Some("embedding".into());
    let mut rerank = chat("/m/r.gguf");
    rerank.mode_hint = Some("rerank".into());
    // No hint at all, named like an embedder: the picker uses the same rule the
    // gate does, so it never offers a row that would be refused after the pick.
    let mut hintless = chat("/m/nomic-embed.gguf");
    hintless.mode_hint = None;
    let catalog = vec![chat("/m/chat.gguf"), embed, rerank, hintless];
    assert_eq!(
      chat_rows(&catalog)
        .iter()
        .map(|r| r.name())
        .collect::<Vec<_>>(),
      vec!["chat.gguf"],
      "an embedding or rerank row is not a session target"
    );
  }

  #[test]
  fn a_model_that_cannot_chat_is_refused_as_a_usage_error() {
    let catalog = vec![chat("/m/x.gguf")];
    let gated = |hint: Option<&str>, name: Option<&str>| {
      let mut row = chat("/m/x.gguf");
      row.mode_hint = hint.map(str::to_string);
      target(&catalog, row, name, None)
    };
    assert_eq!(
      non_chat_mode(&gated(Some("embedding"), None)),
      Some("an embedding")
    );
    assert_eq!(
      non_chat_mode(&gated(Some("rerank"), None)),
      Some("a rerank")
    );
    for (hint, label) in [
      ("image", "an image"),
      ("tts", "a text-to-speech"),
      ("stt", "a speech-to-text"),
    ] {
      assert_eq!(non_chat_mode(&gated(Some(hint), None)), Some(label));
    }
    assert_eq!(non_chat_mode(&gated(None, None)), None);
    assert_eq!(non_chat_mode(&gated(Some("chat"), None)), None);
    // A GGUF with no mode hint is called embedding by the patchers' own id rule,
    // and on the id with the launch name on it, which is the one they are given.
    assert_eq!(
      non_chat_mode(&gated(None, Some("embed-test"))),
      Some("an embedding"),
      "the same id the patcher refuses is the id the gate refuses"
    );
    assert_eq!(
      non_chat_mode(&gated(Some("unknown"), Some("embed-test"))),
      Some("an embedding"),
      "an `unknown` hint is no hint, so the id still decides"
    );
    assert_eq!(
      non_chat_mode(&gated(Some("unknown"), None)),
      None,
      "and does not make a plain model an embedder"
    );
    assert_eq!(
      non_chat_mode(&gated(None, Some("coder"))),
      None,
      "a name that says nothing does not trip it"
    );
    assert_eq!(
      launch_failed(&live("L1", "/m/x.gguf", "chat", None)).code,
      LAUNCH_FAILED
    );
    assert_eq!(no_launch(SessionTool::Claude).code, USAGE);
  }

  #[test]
  fn a_model_named_like_an_alias_wins_over_the_alias() {
    let catalog = vec![chat("/m/qwen.gguf")];
    let aliases = AliasTable::from_config(&crate::config::ProxyAliases::from_pairs(vec![(
      "qwen".to_string(),
      "Other-Model".to_string(),
    )]));
    assert_eq!(
      alias_target(&aliases, &catalog, "qwen"),
      "qwen",
      "a model that owns the name answers first, same rule as the proxy"
    );
    assert_eq!(alias_target(&aliases, &catalog, "gemma"), "gemma");
  }

  #[test]
  fn an_alias_that_names_no_model_answers_with_its_target() {
    let catalog = vec![chat("/m/qwen.gguf")];
    let aliases = AliasTable::from_config(&crate::config::ProxyAliases::from_pairs(vec![(
      "small".to_string(),
      "gemma-3-1b-it-Q4_K_M".to_string(),
    )]));
    assert_eq!(
      alias_target(&aliases, &catalog, "small"),
      "gemma-3-1b-it-Q4_K_M"
    );
  }

  #[test]
  fn the_context_window_prefers_what_the_engine_settled_on() {
    let mut row = chat("/m/chat.gguf");
    row.native_ctx = Some(131_072);
    let mut launch = live("L1", "/m/chat.gguf", "chat", None);
    assert_eq!(
      declared_ctx(Some(&launch), &row, None),
      Some(131_072),
      "catalog figure"
    );
    launch.params = Some(json!({ "ctx": 32768 }));
    assert_eq!(
      declared_ctx(Some(&launch), &row, None),
      Some(32768),
      "what the launch asked for"
    );
    launch.resolved_ctx = Some(8192);
    assert_eq!(
      declared_ctx(Some(&launch), &row, Some(65_536)),
      Some(8192),
      "what the engine served"
    );
    assert_eq!(
      declared_ctx(None, &row, None),
      Some(131_072),
      "no launch at all"
    );
    assert_eq!(
      declared_ctx(None, &row, Some(65_536)),
      Some(65_536),
      "a preset outranks the trained window when nothing is running"
    );
  }

  #[test]
  fn the_context_window_of_a_running_launch_reaches_the_model() {
    let catalog = vec![{
      let mut row = chat("/m/chat.gguf");
      row.native_ctx = Some(131_072);
      row
    }];
    let mut launch = live("L3", "/m/chat.gguf", "chat", Some("coder"));
    launch.resolved_ctx = Some(32_768);
    let t = target(&catalog, catalog[0].clone(), None, Some(launch));
    let model = t.patch_model();
    assert_eq!(model.id, "chat@coder");
    assert_eq!(model.context_window, Some(32_768));
    assert_eq!(t.note("claude"), "claude → chat@coder (L3, ctx 32768)");
  }

  #[test]
  fn only_the_bearer_key_is_masked_in_a_dry_run() {
    assert_eq!(shown_value(Some("s3cr3t"), "s3cr3t"), "***");
    assert_eq!(
      shown_value(Some("s3cr3t"), "http://127.0.0.1:11435"),
      "http://127.0.0.1:11435",
      "the base URL has to survive being pasted back"
    );
    assert_eq!(shown_value(Some("s3cr3t"), KEYLESS_STUB), KEYLESS_STUB);
    assert_eq!(shown_value(None, "s3cr3t"), "s3cr3t");
  }

  #[test]
  fn the_shell_line_quotes_everything_that_needs_it() {
    assert_eq!(shell_quote("plain"), "'plain'");
    assert_eq!(shell_quote("qwen'3"), r"'qwen'\''3'");
    assert_eq!(shell_quote("a b"), "'a b'");
  }

  #[test]
  fn a_removed_var_stays_removed_when_the_line_is_pasted() {
    let launch = SessionLaunch {
      program: "claude",
      args: Vec::new(),
      env: vec![("ANTHROPIC_AUTH_TOKEN".into(), "s3cr3t".into())],
      env_remove: vec!["ANTHROPIC_API_KEY".into()],
      files: Vec::new(),
    };
    assert_eq!(
      shell_line(&launch, &["-p".into(), "hi".into()], Some("s3cr3t")),
      "env -u ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN='***' claude '-p' 'hi'"
    );
    let keeps_all = SessionLaunch {
      env_remove: Vec::new(),
      ..launch
    };
    assert_eq!(
      shell_line(&keeps_all, &[], None),
      "ANTHROPIC_AUTH_TOKEN='s3cr3t' claude"
    );
  }
}
