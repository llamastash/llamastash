//! `llamastash start <model-ref>` — launch a model.
//!
//! Resolution order (per the plan):
//! 1. resolve `<model-ref>` against the catalog (substring / path / id),
//! 2. layer the named preset's params (if `--preset NAME`) onto the
//!    daemon's last_params for this model,
//! 3. apply per-invocation overrides (`--ctx`, `--port`, `--reasoning`,
//!    `--mode`, and the trailing `-- <extra>` args),
//! 4. send `start_model` to the daemon and report the result.
//!
//! Mode resolution is strict: when the catalog reports `mode_hint =
//! unknown` and the user didn't pass `--mode`, we error out rather
//! than silently default to chat. The plan's `cli_args::StartArgs`
//! comment is the authority. A mode the hint merely implies is validated
//! here but not *sent* — see `resolve_mode`.

use std::ffi::OsString;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, CtxArg, LaunchMode as CliLaunchMode, ReasoningFlag, StartArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{
  CliExit, CliResult, BINARY_NOT_FOUND, LAUNCH_FAILED, MODEL_NOT_FOUND, USAGE,
};
use crate::cli::resolve::{fetch_catalog, resolve_model_with_candidates, CatalogRow, ResolveError};
use crate::cli::tail_args::parse_tail_args;
use crate::config::Config;
use crate::ipc::Client;
use crate::launch::knobs::{Concept, KnobSet};

pub async fn handle(args: StartArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let rows = fetch_catalog(&mut client).await?;

  // A launch file supplies both the model reference and the preset, so it
  // replaces the `--preset` → `presets_show` round trip rather than adding a
  // path beside it.
  let from_file = match args.model.as_deref() {
    Some(m) if crate::cli::launch_file::is_launch_file(m) => Some(crate::cli::launch_file::load(
      std::path::Path::new(m),
      args.preset.as_deref(),
    )?),
    _ => None,
  };

  let row = match (&from_file, args.model.as_deref()) {
    (Some(sel), _) => select_start_row(&rows, &args, &sel.model_key)?,
    (None, Some(m)) => select_start_row(&rows, &args, m)?,
    (None, None) => crate::cli::picker::pick_catalog_row(&rows, args.json).await?,
  };

  // The preset that actually applied. A launch file's `default:` picks one
  // without `--preset` ever being typed, and the success line must say so.
  let applied_preset: Option<String> = match &from_file {
    Some(sel) => Some(sel.preset_name.clone()),
    None => args.preset.clone(),
  };

  // Launch selection (drives daemon-side default-preset + last_params
  // inheritance). `--preset auto` is the reserved "pure fit" choice (no
  // preset fetch); a named `--preset` is an explicit baseline; a plain
  // `start` makes no selection, so the daemon applies the model's `default:`.
  let preset_is_auto = args.preset.as_deref() == Some(crate::launch::presets::AUTO_DEFAULT);
  let selection = match (&from_file, args.preset.as_deref()) {
    // A launch file's preset is a self-contained baseline.
    (Some(_), _) => "explicit",
    (None, Some(p)) if p == crate::launch::presets::AUTO_DEFAULT => "auto",
    (None, Some(_)) => "explicit",
    (None, None) => "default",
  };

  // Preset baseline → IPC params. `presets_show` returns the saved
  // params so the daemon doesn't have to re-resolve the model id.
  let mut params = match (&from_file, args.preset.as_ref()) {
    (Some(sel), _) => partial_params_from_launch(
      &crate::launch::presets::materialize_preset(
        &sel.preset_name,
        &sel.body,
        std::path::PathBuf::from(&row.path),
      )
      .params,
    ),
    (None, Some(preset_name)) if !preset_is_auto => {
      fetch_preset_params(&mut client, &row.path, preset_name).await?
    }
    _ => PartialParams::default(),
  };

  // Mode, resolved after the preset fetch so a `mode:` pin is in hand — a
  // preset that says `mode: embedding` has to reach the launch, not just the
  // config file. `None` means nothing chose one and the daemon decides.
  let mode = resolve_mode(&row, args.mode, params.mode.as_deref())?;

  match args.ctx {
    // A pinned count rides the top-level `ctx` (emitted inline as `-c`).
    Some(CtxArg::Value(n)) => params.ctx = Some(n),
    // `auto` sets the knob's Auto state so `--fit` governs the window;
    // it must not also set top-level ctx (that would pin `-c`).
    Some(CtxArg::Auto) => {
      if let Some(def) = crate::launch::knobs::def_for_backend_concept(
        args
          .backend
          .as_deref()
          .unwrap_or(crate::backend::DEFAULT_BACKEND_ID),
        Concept::ContextLength,
      ) {
        params.knobs.set_auto(def.knob_id());
      }
    }
    None => {}
  }
  if let Some(port) = args.port {
    params.port = Some(port);
  }
  if let Some(r) = args.reasoning {
    params.reasoning = Some(matches!(r, ReasoningFlag::On));
  }
  // MTP overrides layer over any preset baseline, like `--ctx` / `--reasoning`.
  if let Some(m) = args.mtp {
    params.mtp = Some(m);
  }
  if let Some(n) = args.mtp_draft_n {
    params.mtp_draft_n = Some(n);
  }
  let (cli_knobs, cli_extras) = parse_cli_knobs(&args.knobs.tokens, &args.extra)?;
  // Layer per-invocation overrides onto the preset baseline instead of
  // replacing it — a CLI `--threads` must not wipe a preset's other
  // knobs.
  params.knobs.overlay(&cli_knobs);
  // Only replace preset extras when the invocation supplied some; an
  // inline-only launch keeps the preset's passthrough flags.
  if !cli_extras.is_empty() {
    params.extras = cli_extras;
  }

  // Warning surfaces (kept best-effort; never block the launch):
  // ctx > native max should advise rather than refuse, per R12.
  // Gated on `!args.json` so the warning text doesn't mix into the
  // structured stderr stream agents read when capturing both streams.
  if !args.json {
    if let (Some(ctx), Some(native)) = (params.ctx, row.native_ctx) {
      if (ctx as u64) > native {
        eprintln!(
          "{}",
          crate::cli::colors::warning(&format!(
            "--ctx {ctx} exceeds native context length {native} for {}; the supervisor will still try",
            row.name()
          ))
        );
      }
    }
  }

  // An explicit flag beats the preset's pin; the preset beats nothing at all.
  let backend = args.backend.as_deref().or(params.backend.as_deref());
  let server = args.server.as_deref().or(params.server.as_deref());
  let payload = build_payload(&row.path, mode, &params, backend, server, selection);
  let resp = client
    .call("start_model", Some(payload))
    .await
    .map_err(|e| map_start_error(e, &row))?;
  if args.wait {
    return wait_and_emit(
      &mut client,
      applied_preset.as_deref(),
      &row,
      &resp,
      args.json,
      cli.quiet,
    )
    .await;
  }
  emit_response(applied_preset.as_deref(), &row, &resp, args.json, cli.quiet);
  Ok(())
}

/// `--wait`: poll `status` until the just-started launch reaches a
/// terminal-ish state (Ready / Error / Stopped) or the budget runs out,
/// then report the resolved context window. The daemon's own probe budget
/// (size-scaled) guarantees a stuck load eventually flips to Error, so the
/// 15-minute ceiling here is only a safety net for pathological cases.
async fn wait_and_emit(
  client: &mut Client,
  preset: Option<&str>,
  row: &CatalogRow,
  resp: &Value,
  json: bool,
  quiet: bool,
) -> CliResult {
  use crate::cli::resolve::{fetch_status, running_index};

  let launch_id = resp
    .get("launch_id")
    .and_then(Value::as_str)
    .map(str::to_string);
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
  // Final running row once terminal, or None if we time out / can't find it.
  let mut settled: Option<crate::cli::resolve::RunningRow> = None;
  // `resolved_ctx` is stamped by a separate recorder a beat *after* the
  // Ready transition, so a row can read `ready` with no ctx yet. Once we
  // first see Ready, give the recorder this grace window to land the
  // actuals before settling — otherwise `--wait` prints `ctx=—`.
  let mut ready_since: Option<std::time::Instant> = None;
  let actuals_grace = std::time::Duration::from_secs(5);
  while std::time::Instant::now() < deadline {
    let snap = fetch_status(client).await?;
    let index = running_index(&snap.models);
    // Prefer the launch_id match; fall back to the model path (a daemon
    // build without launch_id on the row still resolves by path).
    let found = snap
      .models
      .iter()
      .find(|m| Some(m.launch_id.as_str()) == launch_id.as_deref())
      .cloned()
      .or_else(|| index.get(&row.path).cloned());
    if let Some(r) = found {
      match r.state.as_str() {
        // Error / Stopped are terminal immediately — no actuals to wait on.
        "error" | "stopped" => {
          settled = Some(r);
          break;
        }
        // Ready settles once the resolved ctx is stamped, or after the
        // grace window elapses (a build whose `/props` omits it, etc.).
        "ready" => {
          let since = *ready_since.get_or_insert_with(std::time::Instant::now);
          if r.resolved_ctx.is_some() || since.elapsed() >= actuals_grace {
            settled = Some(r);
            break;
          }
        }
        // launching / loading → keep polling.
        _ => {}
      }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
  }

  let failed = matches!(settled.as_ref().map(|r| r.state.as_str()), Some("error"));
  if json {
    let mut body = json!({
      "name": row.name(),
      "launch_id": resp.get("launch_id"),
      "port": resp.get("port"),
      "pid": resp.get("pid"),
      "preset": preset,
      "path": row.path,
      "state": settled.as_ref().map(|r| r.state.clone()),
      "resolved_ctx": settled.as_ref().and_then(|r| r.resolved_ctx),
      "ctx_clamped": settled.as_ref().map(|r| r.ctx_clamped).unwrap_or(false),
    });
    if let Some(cause) = settled.as_ref().and_then(|r| r.state_cause.clone()) {
      body["cause"] = Value::String(cause);
    }
    // The daemon omits `layer_sources` when empty, so this is absent on a
    // pure-fit launch and present otherwise.
    if let Some(sources) = resp.get("layer_sources") {
      body["layer_sources"] = sources.clone();
    }
    println!("{}", crate::cli::output::pretty_json(&body));
  } else if !quiet {
    // Headline first (reuses the standard "started ..." prose), then the
    // readiness follow-up.
    emit_response(preset, row, resp, false, false);
    print_wait_followup(settled.as_ref());
  }
  if failed {
    // The launch was accepted but the model never came up; reflect that
    // in the exit code so scripts can branch on it.
    return Err(CliExit::code_only(LAUNCH_FAILED));
  }
  Ok(())
}

/// One-line readiness summary appended under the `start` headline when
/// `--wait` settles. Covers Ready (with resolved ctx + clamp note),
/// Error (with cause), and the timeout fallthrough.
fn print_wait_followup(settled: Option<&crate::cli::resolve::RunningRow>) {
  use crate::cli::colors;
  let arrow = colors::dim("→");
  match settled {
    Some(r) if r.state == "ready" => {
      let ctx = match r.resolved_ctx {
        Some(c) if r.ctx_clamped => {
          format!("{c} {}", colors::dim("(clamped to fit-ctx floor)"))
        }
        Some(c) => c.to_string(),
        None => "—".into(),
      };
      println!("{} {arrow} ctx={ctx}", colors::success("ready"), ctx = ctx,);
    }
    Some(r) if r.state == "error" => {
      let cause = r.state_cause.as_deref().unwrap_or("unknown");
      println!("{} {arrow} {cause}", colors::error("failed"));
    }
    Some(r) => {
      // Stopped before we observed Ready (raced with an external stop).
      println!("{} {arrow} {}", colors::dim("settled"), r.state);
    }
    None => {
      println!(
        "{} {arrow} still loading; check `llamastash status`",
        colors::dim("waiting timed out"),
      );
    }
  }
}

fn select_start_row(
  rows: &[CatalogRow],
  args: &StartArgs,
  model: &str,
) -> Result<CatalogRow, CliExit> {
  match resolve_model_with_candidates(rows, model) {
    Ok(row) => Ok(row),
    Err(ResolveError::Empty) => Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty model reference; supply a name substring, absolute path, or short id",
    )),
    Err(ResolveError::None) => {
      if let Some(path) = direct_path_candidate(model, args)? {
        return Ok(direct_catalog_row(
          path,
          args
            .mode
            .expect("direct_path_candidate requires explicit mode"),
        ));
      }
      Err(CliExit::new(
        MODEL_NOT_FOUND,
        format!("no model matches `{model}` ({} known)", rows.len()),
      ))
    }
    Err(ResolveError::Many(candidates)) => {
      let names: Vec<String> = candidates.iter().map(|r| r.name()).collect();
      Err(CliExit::new(
        MODEL_NOT_FOUND,
        format!(
          "`{model}` matches {} models: {}\nrefine the reference (full path or unique substring) and retry",
          candidates.len(),
          names.join(", ")
        ),
      ))
    }
  }
}

fn direct_path_candidate(model: &str, args: &StartArgs) -> Result<Option<PathBuf>, CliExit> {
  let path = PathBuf::from(model);
  if !path.is_absolute() {
    return Ok(None);
  }
  if !path.exists() {
    return Ok(None);
  }
  // A weight file, or a directory some backend claims as a model of its own.
  // The `is_file` assumption predates directory-shaped models: the daemon
  // accepts them over IPC (that is what `synthetic_identity` is for), so
  // refusing here left such a model with no launch route at all, reported as
  // "no model matches" rather than "directories are unsupported".
  if !path.is_file() && crate::backend::synthetic_identity_for_path(&path).is_none() {
    return Ok(None);
  }
  if args.mode.is_none() {
    return Err(CliExit::new(
      USAGE,
      format!(
        "absolute path `{}` bypasses catalog discovery; pass --mode chat|embedding|rerank",
        path.display()
      ),
    ));
  }
  Ok(Some(path))
}

fn direct_catalog_row(path: PathBuf, mode: CliLaunchMode) -> CatalogRow {
  let parent = path
    .parent()
    .map(|p| p.display().to_string())
    .unwrap_or_default();
  CatalogRow {
    path: path.display().to_string(),
    model_id: None,
    parent,
    source: "direct_path".into(),
    arch: None,
    quant: None,
    native_ctx: None,
    mode_hint: Some(mode.as_label().to_string()),
    parameter_label: None,
    weights_bytes: None,
    display_label: None,
    parse_error: None,
    split_siblings: Vec::new(),
    has_chat_template: false,
    has_reasoning_hint: false,
    tokenizer_kind: None,
    total_parameters: None,
    backend: None,
    supported_backends: Vec::new(),
    multimodal: None,
    mtp: None,
  }
}

#[derive(Debug, Default, Clone)]
struct PartialParams {
  ctx: Option<u32>,
  port: Option<u16>,
  reasoning: Option<bool>,
  knobs: KnobSet,
  extras: Vec<String>,
  mtp: Option<crate::launch::params::MtpEnable>,
  mtp_draft_n: Option<u32>,
  /// Serving mode a preset pins. `Some("chat")` is *not* a pin: a preset
  /// stores a mode only when it differs from chat, so a materialised `chat`
  /// means the preset said nothing and the catalog hint still decides.
  mode: Option<String>,
  /// Backend and server a preset pins. Launch *identity*, not tuning: they
  /// say which engine and which build run, so a preset that carries them and
  /// a launch that ignores them reproduce different runs. An explicit CLI
  /// flag still wins.
  backend: Option<String>,
  server: Option<String>,
}

/// Canonicalise a mode string onto the wire label, or `None` when it names no
/// mode we serve.
fn mode_label(s: &str) -> Option<&'static str> {
  match s {
    "chat" => Some("chat"),
    "embedding" => Some("embedding"),
    "rerank" => Some("rerank"),
    _ => None,
  }
}

/// The mode to put on the wire, or `None` to let the daemon resolve one.
///
/// Only a real *choice* is sent: an explicit `--mode`, or a preset's `mode:`
/// pin. A mode the catalog hint merely implies is deliberately **not** sent —
/// it is the model's own default, which sits below the user's config in the
/// precedence order, and sending it as though it were a choice is what let it
/// shadow a `default:` preset's pin on every plain `start`. The daemon reads
/// the same hint off the header it already parses, so nothing is lost.
///
/// The `unknown` hint still errors here rather than server-side: the fix is a
/// flag the user types, so the message belongs on the surface they typed at.
fn resolve_mode(
  row: &CatalogRow,
  override_mode: Option<CliLaunchMode>,
  preset_mode: Option<&str>,
) -> Result<Option<&'static str>, CliExit> {
  if let Some(m) = override_mode {
    return Ok(Some(m.as_label()));
  }
  // A managed-multiplexer row is served by an umbrella that picks the recipe
  // from the model name; llama.cpp launch modes don't apply, so don't force
  // `--mode` and don't refuse a row whose hint we cannot read.
  if crate::backend::is_managed_multiplexer(crate::cli::output::backend_for_source(&row.source)) {
    return Ok(None);
  }
  // A preset's pin outranks the hint. `chat` is not a pin -- see
  // `PartialParams::mode`.
  if let Some(m) = preset_mode.filter(|m| *m != "chat").and_then(mode_label) {
    return Ok(Some(m));
  }
  match row.mode_hint.as_deref() {
    // A hint the daemon will read for itself.
    Some("chat") | Some("embedding") | Some("rerank") => Ok(None),
    Some("unknown") | None => Err(CliExit::new(
      USAGE,
      format!(
        "model `{name}` has no mode hint; pass `--mode chat|embedding|rerank` to disambiguate",
        name = row.name(),
      ),
    )),
    Some(other) => Err(CliExit::new(
      USAGE,
      format!("unrecognised mode hint `{other}` from daemon; please file a bug"),
    )),
  }
}

async fn fetch_preset_params(
  client: &mut Client,
  model_path: &str,
  preset_name: &str,
) -> Result<PartialParams, CliExit> {
  let body = client
    .call(
      "presets_show",
      Some(json!({"model_path": model_path, "name": preset_name})),
    )
    .await
    .map_err(CliExit::from_client_error)?;
  let preset = body.get("preset");
  if preset.map(Value::is_null).unwrap_or(true) {
    return Err(CliExit::new(
      USAGE,
      format!("preset `{preset_name}` not found for {model_path}"),
    ));
  }
  Ok(partial_params_from_preset(preset.unwrap()))
}

/// A materialized preset's params, taken through the same projection a
/// `presets_show` baseline takes.
///
/// Routed through the wire shape on purpose: it is the one definition of
/// `LaunchParams` → `PartialParams`, so a launch file and `--preset <name>`
/// cannot resolve the same preset differently.
fn partial_params_from_launch(p: &crate::launch::params::LaunchParams) -> PartialParams {
  partial_params_from_preset(&json!({ "params": p.to_wire() }))
}

/// Map one `presets_show` preset object onto the launch params `start` sends.
///
/// Split out of the wire call so it is testable without a daemon: this is
/// where a preset stops being config and becomes a launch, and every field it
/// forgets is a preset that silently reproduces a different run.
fn partial_params_from_preset(preset: &Value) -> PartialParams {
  let p = preset.get("params").cloned().unwrap_or(Value::Null);
  let knobs: KnobSet = p
    .get("knobs")
    .and_then(|v| serde_json::from_value(v.clone()).ok())
    .unwrap_or_default();
  let extras = p
    .get("extras")
    .and_then(Value::as_array)
    .map(|a| {
      a.iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
    })
    .unwrap_or_default();
  PartialParams {
    ctx: p.get("ctx").and_then(Value::as_u64).map(|n| n as u32),
    port: p.get("port").and_then(Value::as_u64).map(|n| n as u16),
    reasoning: p.get("reasoning").and_then(Value::as_bool),
    knobs,
    extras,
    mtp: p
      .get("mtp")
      .and_then(|v| serde_json::from_value(v.clone()).ok()),
    mtp_draft_n: p
      .get("mtp_draft_n")
      .and_then(Value::as_u64)
      .map(|n| n as u32),
    mode: p.get("mode").and_then(Value::as_str).map(str::to_string),
    backend: p.get("backend").and_then(Value::as_str).map(str::to_string),
    server: p.get("server").and_then(Value::as_str).map(str::to_string),
  }
}

/// Resolve the per-invocation knob overrides from the two CLI surfaces:
/// the generated inline flags (`--threads 8`, captured as canonical
/// tokens by `crate::cli::knob_flags`) and the trailing `-- <raw>`
/// passthrough. Both feed the single [`parse_tail_args`] validator;
/// passthrough tokens come last so a flag set on both wins from `--`
/// (last-occurrence-wins). Returns the parsed knobs plus any
/// unrecognised tokens to forward verbatim as `extras`.
fn parse_cli_knobs(
  knob_tokens: &[OsString],
  extra: &[OsString],
) -> Result<(KnobSet, Vec<String>), CliExit> {
  if knob_tokens.is_empty() && extra.is_empty() {
    return Ok((KnobSet::new(), Vec::new()));
  }
  let mut combined: Vec<OsString> = knob_tokens.to_vec();
  combined.extend(extra.iter().cloned());
  let (knobs, extras) = parse_tail_args(&combined)?;
  let extras = extras
    .into_iter()
    .map(|s| s.to_string_lossy().into_owned())
    .collect();
  Ok((knobs, extras))
}

fn build_payload(
  model_path: &str,
  mode: Option<&str>,
  p: &PartialParams,
  backend: Option<&str>,
  server: Option<&str>,
  selection: &str,
) -> Value {
  let mut obj = serde_json::Map::new();
  obj.insert(
    "model_path".into(),
    Value::String(PathBuf::from(model_path).display().to_string()),
  );
  // Omitted when nothing chose one, so the daemon's own rungs (a `default:`
  // preset's pin, then the model's header hint) decide.
  if let Some(m) = mode {
    obj.insert("mode".into(), Value::String(m.to_string()));
  }
  // Drives whether the daemon applies the model's `default:` preset +
  // last_params inheritance. `default` (no selection) is the common case.
  obj.insert("selection".into(), Value::String(selection.to_string()));
  // Per-model backend override. Omitted when unset so the daemon
  // applies its default (`Auto` → identity rule).
  if let Some(b) = backend {
    obj.insert("backend".into(), Value::String(b.to_string()));
  }
  // Chosen server (build/binary). Omitted when unset. Drives the launch binary
  // and, when `backend` is unset, the backend.
  if let Some(s) = server {
    obj.insert("server".into(), Value::String(s.to_string()));
  }
  if let Some(ctx) = p.ctx {
    obj.insert("ctx".into(), Value::from(ctx));
  }
  if let Some(port) = p.port {
    obj.insert("port".into(), Value::from(port));
  }
  if let Some(r) = p.reasoning {
    obj.insert("reasoning".into(), Value::from(r));
  }
  if !p.knobs.is_empty() {
    obj.insert(
      "knobs".into(),
      serde_json::to_value(&p.knobs).expect("crate::launch::knobs::KnobSet serialises cleanly"),
    );
  }
  if !p.extras.is_empty() {
    obj.insert(
      "extras".into(),
      Value::Array(p.extras.iter().cloned().map(Value::String).collect()),
    );
  }
  // MTP intent + draft-token count. Omitted when unset so the daemon inherits
  // (default preset / last_params) and falls to `Auto`.
  if let Some(m) = p.mtp {
    obj.insert("mtp".into(), Value::String(m.label().to_string()));
  }
  if let Some(n) = p.mtp_draft_n {
    obj.insert("mtp_draft_n".into(), Value::from(n));
  }
  Value::Object(obj)
}

fn map_start_error(e: crate::ipc::ClientError, row: &CatalogRow) -> CliExit {
  use crate::backend::Backend;
  use crate::ipc::protocol::ErrorCode;
  use crate::ipc::ClientError;
  match e {
    ClientError::Remote(err) => {
      // Daemon distinguishes "binary missing" via the launch
      // environment guard; surface that as BINARY_NOT_FOUND so
      // scripts can react. The binary name is the default backend's *primary*
      // process marker (`llama-server`) so this site names no backend; guard
      // the empty-marker case so `contains("")` never matches everything. Only
      // the primary is matched — an alternate marker short enough to be a
      // substring of an unrelated message would misclassify the error.
      // The daemon tags a refusal it raised because the model's backend is off
      // or its launcher is absent. An environment problem, not a usage one —
      // the flags were fine — so it gets the environment code rather than
      // landing in the `InvalidParams` → USAGE bucket below.
      let backend_unavailable = err
        .data
        .as_ref()
        .and_then(|d| d.get("cause"))
        .and_then(Value::as_str)
        == Some("backend_unavailable");
      let lower = err.message.to_lowercase();
      let marker = crate::backend::default_backend()
        .process_markers()
        .first()
        .copied()
        .unwrap_or("");
      if backend_unavailable {
        CliExit::new(BINARY_NOT_FOUND, err.message)
      } else if lower.contains("launch environment")
        || (!marker.is_empty() && lower.contains(marker))
      {
        CliExit::new(
          BINARY_NOT_FOUND,
          format!(
            "daemon could not launch {name}: {msg}\nhint: pass --llama-server <path> or set LLAMASTASH_LLAMA_SERVER",
            name = row.name(),
            msg = err.message,
          ),
        )
      } else if err.code == ErrorCode::InvalidParams.as_i32() {
        // The daemon rejected the request before spawning (bad server id,
        // bad port, ctx out of range, …). That is a usage error, not a
        // supervisor failure — map it to USAGE so scripts branch correctly.
        CliExit::new(USAGE, err.message)
      } else {
        CliExit::new(
          LAUNCH_FAILED,
          format!("start_model failed for {}: {}", row.name(), err.message),
        )
      }
    }
    other => CliExit::from_client_error(other),
  }
}

fn emit_response(preset: Option<&str>, row: &CatalogRow, resp: &Value, json: bool, quiet: bool) {
  let port = resp.get("port").and_then(Value::as_u64);
  let lid = resp.get("launch_id").and_then(Value::as_str);
  let pid = resp.get("pid").and_then(Value::as_u64);
  if json {
    let mut body = serde_json::json!({
      "name": row.name(),
      "launch_id": lid,
      "port": port,
      "pid": pid,
      "preset": preset,
      "path": row.path,
    });
    // The daemon omits `layer_sources` when empty, so this is absent on a
    // pure-fit launch and present otherwise.
    if let Some(sources) = resp.get("layer_sources") {
      body["layer_sources"] = sources.clone();
    }
    println!("{}", crate::cli::output::pretty_json(&body));
    return;
  }
  if quiet {
    return;
  }
  let preset_label = preset
    .map(|p| format!(" (preset: {p})"))
    .unwrap_or_default();
  // The headline ("started <name> ...") keeps the standard green
  // success style. Trailing tokens (launch_id / port / pid) pick up
  // semantic value colors so the actionable IDs stand out against the
  // green prose.
  use crate::cli::colors;
  let head = colors::success(&format!("started {name}{preset_label}", name = row.name()));
  let lid_token = lid
    .map(colors::launch_id)
    .unwrap_or_else(|| colors::dim("?"));
  let port_token = port
    .map(|p| colors::port(p as u16))
    .unwrap_or_else(|| colors::dim("?"));
  let pid_token = pid
    .map(|p| console::style(p.to_string()).bold().to_string())
    .unwrap_or_else(|| colors::dim("?"));
  println!(
    "{head} {arrow} launch_id={lid_token} port={port_token} pid={pid_token}",
    arrow = colors::dim("→"),
  );
  // Non-fatal advisories (dropped knobs, deepseek4 KV-blind note,
  // ssd_streaming bypass). Human output only — `--json` shape is untouched.
  if let Some(ws) = resp.get("warnings").and_then(Value::as_array) {
    for w in ws.iter().filter_map(Value::as_str) {
      println!("  {}", colors::warning(w));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::resolve::CatalogRow;

  /// A preset that pins `backend` / `server` has to launch on them.
  ///
  /// These are launch *identity*, not tuning: they say which engine and which
  /// build run. `PartialParams` carried neither, so `start --preset` rebuilt
  /// the preset without them and launched on whatever `auto` picked, while
  /// still printing the preset's name. Verified on real builds: a preset
  /// pinning the second of two llama.cpp servers launched on the first.
  #[test]
  fn a_presets_pinned_backend_and_server_survive_the_rebuild() {
    let preset = serde_json::json!({
      "params": {
        "ctx": 2048,
        "backend": "some-engine",
        "server": "build-two",
        "knobs": {},
        "extras": [],
      }
    });
    let p = partial_params_from_preset(&preset);
    assert_eq!(p.backend.as_deref(), Some("some-engine"));
    assert_eq!(p.server.as_deref(), Some("build-two"));
    assert_eq!(p.ctx, Some(2048), "tuning still comes through");

    // A preset that pins neither leaves both to the identity rule.
    let bare = serde_json::json!({ "params": { "knobs": {}, "extras": [] } });
    let b = partial_params_from_preset(&bare);
    assert_eq!(b.backend, None);
    assert_eq!(b.server, None);
  }

  /// An explicit flag beats the preset's pin, the preset beats nothing.
  ///
  /// The same last-wins order the knob flags follow, kept here so the two
  /// identity fields cannot drift from it.
  #[test]
  fn an_explicit_identity_flag_wins_over_the_presets_pin() {
    let preset = serde_json::json!({
      "params": { "backend": "from-preset", "server": "srv-preset",
                  "knobs": {}, "extras": [] }
    });
    let p = partial_params_from_preset(&preset);

    let flag_backend: Option<String> = Some("from-flag".into());
    let flag_server: Option<String> = None;
    assert_eq!(
      flag_backend.as_deref().or(p.backend.as_deref()),
      Some("from-flag"),
      "an explicit --backend wins"
    );
    assert_eq!(
      flag_server.as_deref().or(p.server.as_deref()),
      Some("srv-preset"),
      "no --server falls back to the pin"
    );
  }

  fn row(mode_hint: Option<&str>) -> CatalogRow {
    CatalogRow {
      path: "/m/qwen.gguf".into(),
      model_id: Some("deadbeef".into()),
      parent: "/m".into(),
      source: "user".into(),
      arch: Some("qwen2".into()),
      quant: Some("Q4_K".into()),
      native_ctx: Some(8192),
      mode_hint: mode_hint.map(str::to_string),
      parameter_label: Some("7B".into()),
      weights_bytes: Some(4_200_000_000),
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  #[test]
  fn explicit_mode_wins_even_when_hint_present() {
    let r = row(Some("chat"));
    assert_eq!(
      resolve_mode(&r, Some(CliLaunchMode::Embedding), None).unwrap(),
      Some("embedding")
    );
  }

  /// Every field a preset can carry has to survive the launch-file route too.
  ///
  /// The projection is shared with `--preset`, so a field dropped on either
  /// side means a launch file and the same saved preset resolve differently.
  #[test]
  fn partial_params_from_launch_carries_every_preset_field() {
    use crate::config::PresetBody;
    use crate::launch::knobs::KnobSet;

    let mut knobs = KnobSet::new();
    for (name, value) in [
      ("n-gpu-layers", "99"),
      ("ctx-size", "4096"),
      ("mode", "embedding"),
      ("mtp", "true"),
      ("mtp-draft-n", "4"),
      ("reasoning", "true"),
    ] {
      assert!(knobs.set_by_name(name, value), "{name} is declared");
    }
    let body = PresetBody {
      knobs,
      extras: Some(vec!["--rope-freq-base".into(), "1000000".into()]),
      backend: Some(crate::backend::DEFAULT_BACKEND_ID.to_string()),
      server: Some("llamacpp-vulkan".into()),
    };
    let named =
      crate::launch::presets::materialize_preset("fast", &body, PathBuf::from("/m/a.gguf"));
    let p = partial_params_from_launch(&named.params);

    assert_eq!(p.ctx, Some(4096));
    assert_eq!(
      p.knobs.u32(crate::launch::knobs::kid("n-gpu-layers")),
      Some(99),
      "a plain tuning knob stays in the knob set"
    );
    assert_eq!(p.extras, vec!["--rope-freq-base", "1000000"]);
    assert_eq!(
      p.backend.as_deref(),
      Some(crate::backend::DEFAULT_BACKEND_ID)
    );
    assert_eq!(p.server.as_deref(), Some("llamacpp-vulkan"));
    assert_eq!(p.mode.as_deref(), Some("embedding"));
    assert_eq!(p.mtp, Some(crate::launch::params::MtpEnable::On));
    assert_eq!(p.mtp_draft_n, Some(4));
    assert_eq!(p.reasoning, Some(true));
  }

  #[test]
  fn a_presets_pinned_mode_beats_the_catalog_hint() {
    let r = row(Some("chat"));
    assert_eq!(
      resolve_mode(&r, None, Some("embedding")).unwrap(),
      Some("embedding")
    );
  }

  #[test]
  fn a_presets_pinned_mode_answers_an_unknown_hint() {
    // Without the pin this is the `pass --mode` usage error.
    let r = row(Some("unknown"));
    assert_eq!(
      resolve_mode(&r, None, Some("rerank")).unwrap(),
      Some("rerank")
    );
  }

  #[test]
  fn an_explicit_mode_flag_beats_the_presets_pin() {
    let r = row(Some("chat"));
    assert_eq!(
      resolve_mode(&r, Some(CliLaunchMode::Rerank), Some("embedding")).unwrap(),
      Some("rerank")
    );
  }

  #[test]
  fn a_materialised_chat_is_not_treated_as_a_pin() {
    // A preset stores a mode only when it is non-chat, so `chat` off the wire
    // means "the preset said nothing". Nothing chose, so nothing is sent and
    // the daemon reads the same hint off the header it already parsed.
    for hint in ["embedding", "chat", "rerank"] {
      let r = row(Some(hint));
      assert_eq!(
        resolve_mode(&r, None, Some("chat")).unwrap(),
        None,
        "{hint}"
      );
      assert_eq!(resolve_mode(&r, None, None).unwrap(), None, "{hint}");
    }
  }

  #[test]
  fn a_payload_with_no_chosen_mode_omits_the_field() {
    // The daemon distinguishes "no mode" from "chat"; sending the hint as if
    // it were a choice is what shadowed a preset's pin.
    let v = build_payload(
      "/m/a.gguf",
      None,
      &PartialParams::default(),
      None,
      None,
      "default",
    );
    assert!(v.get("mode").is_none(), "{v}");
  }

  #[test]
  fn a_preset_mode_survives_the_rebuild() {
    let preset = serde_json::json!({
      "name": "emb",
      "params": { "model_path": "/m/a.gguf", "mode": "embedding" },
    });
    assert_eq!(
      partial_params_from_preset(&preset).mode.as_deref(),
      Some("embedding")
    );
  }

  #[test]
  fn missing_hint_without_override_errors_with_usage() {
    let r = row(None);
    let err = resolve_mode(&r, None, None).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--mode"));
  }

  #[test]
  fn unknown_hint_without_override_errors() {
    let r = row(Some("unknown"));
    assert!(resolve_mode(&r, None, None).is_err());
  }

  #[test]
  fn build_payload_includes_backend_override_when_set() {
    let p = PartialParams::default();
    let v = build_payload(
      "/m/a.gguf",
      Some("chat"),
      &p,
      Some("llamacpp"),
      None,
      "explicit",
    );
    assert_eq!(v["backend"], serde_json::json!("llamacpp"));
    assert_eq!(v["selection"], serde_json::json!("explicit"));
  }

  #[test]
  fn build_payload_omits_empty_backend_knobs() {
    let v = build_payload(
      "/m/a.gguf",
      Some("chat"),
      &PartialParams::default(),
      None,
      None,
      "default",
    );
    assert!(v.get("backend_knobs").is_none());
  }

  fn osvec(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
  }

  #[test]
  fn cli_knobs_inline_and_passthrough_combine_passthrough_wins() {
    // Inline `--threads 4` (from the generated flags) plus a trailing
    // `-- --threads 16` passthrough: the `--` value wins, and an
    // unrecognised passthrough flag routes to extras.
    let (knobs, extras) = parse_cli_knobs(
      &osvec(&["--threads", "4", "--device", "Vulkan0"]),
      &osvec(&["--threads", "16", "--rope-freq-base", "10000"]),
    )
    .unwrap();
    assert_eq!(knobs.u32(crate::launch::knobs::kid("threads")), Some(16));
    assert_eq!(
      knobs.str(crate::launch::knobs::kid("device")),
      Some("Vulkan0")
    );
    assert_eq!(
      extras,
      vec!["--rope-freq-base".to_string(), "10000".to_string()]
    );
  }

  #[test]
  fn cli_knobs_bad_value_is_usage() {
    let err = parse_cli_knobs(&osvec(&["--threads", "xyz"]), &[]).unwrap_err();
    assert_eq!(err.code, USAGE);
    assert!(err.to_string().contains("--threads"), "{err}");
  }

  #[test]
  fn direct_path_candidate_requires_explicit_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.gguf");
    std::fs::write(&path, b"gguf").unwrap();
    let model = path.display().to_string();
    let args = StartArgs {
      model: Some(model.clone()),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: None,
      knobs: crate::cli::knob_flags::KnobFlags::default(),
      extra: vec![],
      backend: None,
      server: None,
      mtp: None,
      mtp_draft_n: None,
      json: false,
      wait: false,
    };
    let err = direct_path_candidate(&model, &args).unwrap_err();
    assert_eq!(err.code, USAGE);
    assert!(err.to_string().contains("pass --mode"));
  }

  #[test]
  fn direct_path_candidate_accepts_existing_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.gguf");
    std::fs::write(&path, b"gguf").unwrap();
    let model = path.display().to_string();
    let args = StartArgs {
      model: Some(model.clone()),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      knobs: crate::cli::knob_flags::KnobFlags::default(),
      extra: vec![],
      backend: None,
      server: None,
      mtp: None,
      mtp_draft_n: None,
      json: false,
      wait: false,
    };
    let resolved = direct_path_candidate(&model, &args).unwrap();
    assert_eq!(resolved, Some(path));
  }

  #[test]
  fn direct_catalog_row_uses_explicit_mode_hint() {
    let row = direct_catalog_row(PathBuf::from("/tmp/m.gguf"), CliLaunchMode::Rerank);
    assert_eq!(row.path, "/tmp/m.gguf");
    assert_eq!(row.mode_hint.as_deref(), Some("rerank"));
    assert_eq!(row.source, "direct_path");
  }

  #[test]
  fn select_start_row_falls_back_to_direct_path_when_catalog_misses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.gguf");
    std::fs::write(&path, b"gguf").unwrap();
    let args = StartArgs {
      model: Some(path.display().to_string()),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      knobs: crate::cli::knob_flags::KnobFlags::default(),
      extra: vec![],
      backend: None,
      server: None,
      mtp: None,
      mtp_draft_n: None,
      json: false,
      wait: false,
    };
    let model = args.model.clone().unwrap();
    let row = select_start_row(&[], &args, &model).unwrap();
    assert_eq!(row.path, path.display().to_string());
    assert_eq!(row.mode_hint.as_deref(), Some("chat"));
  }

  #[test]
  fn select_start_row_prefers_catalog_match_over_direct_path_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.gguf");
    std::fs::write(&path, b"gguf").unwrap();
    let row = CatalogRow {
      path: path.display().to_string(),
      model_id: Some("deadbeef".into()),
      parent: dir.path().display().to_string(),
      source: "user".into(),
      arch: Some("qwen2".into()),
      quant: Some("Q4_K".into()),
      native_ctx: Some(8192),
      mode_hint: Some("embedding".into()),
      parameter_label: Some("7B".into()),
      weights_bytes: Some(123),
      display_label: Some("known-model".into()),
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    };
    let args = StartArgs {
      model: Some(path.display().to_string()),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      knobs: crate::cli::knob_flags::KnobFlags::default(),
      extra: vec![],
      backend: None,
      server: None,
      mtp: None,
      mtp_draft_n: None,
      json: false,
      wait: false,
    };
    let model = args.model.clone().unwrap();
    let selected = select_start_row(std::slice::from_ref(&row), &args, &model).unwrap();
    assert_eq!(selected.display_label.as_deref(), Some("known-model"));
    assert_eq!(selected.mode_hint.as_deref(), Some("embedding"));
  }
}
