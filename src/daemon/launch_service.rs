//! The daemon-side launch pipeline.
//!
//! `compose_and_spawn` is the one code path that turns a parsed
//! `StartParams` into a running supervised model: input validation →
//! identity / arch resolution → race-safe port reservation → layered
//! knob merge → memory admission → supervisor spawn → registry insert →
//! last-params recorder. The IPC `start_model` handler and the proxy's
//! auto-start path both call it, so the two surfaces can never drift in
//! how a launch is composed. It ends by handing a [`LaunchExec`] to the
//! resolved backend's [`crate::backend::Backend::start`]: a process-per-model
//! backend runs the default supervised spawn (`spawn_supervised`); a
//! managed-multiplexer backend overrides `start` to anchor on its shared
//! umbrella. This path never branches on lifecycle.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backend::identity::ModelIdentity;
use crate::backend::Backend;
use crate::config::MAX_CTX_TOKENS;
use crate::daemon::context::{MethodContext, PersistedState};
use crate::daemon::registry::LaunchId;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::RunningSnapshot;
use crate::daemon::supervisor::{
  spawn as supervisor_spawn, ManagedModel, ManagedSpawn, ManagedState,
};
use crate::gguf::header::{read_path as read_gguf_header, HeaderReadOptions};
use crate::gguf::identity::ModelId;
use crate::ipc::protocol::{ErrorCode, ErrorObject};
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

/// Wire-shape for the `start_model` IPC method. The fields land
/// verbatim from JSON-RPC; `compose_and_spawn` consumes the parsed
/// struct so the proxy's auto-start path can build one
/// directly without going through JSON.
#[derive(Deserialize, Default, Clone)]
pub(crate) struct StartParams {
  /// Absolute path to the GGUF the user wants to launch. We compute
  /// the canonical `ModelId` by reading its header on the daemon
  /// side rather than trusting the caller — keeps the surface
  /// minimal for CLI/TUI clients.
  pub(crate) model_path: PathBuf,
  #[serde(default)]
  pub(crate) mode: Option<LaunchModeWire>,
  #[serde(default)]
  pub(crate) ctx: Option<u32>,
  #[serde(default)]
  pub(crate) port: Option<u16>,
  /// Soft port preference — if the supplied port is free at
  /// reservation time, use it; otherwise allocate from the
  /// configured range. Distinct from `port` which is strict and
  /// errors on conflict. The TUI sets this so a returning user
  /// lands on their previously-bound port without scripted clients
  /// silently losing strict semantics.
  #[serde(default)]
  pub(crate) prefer_port: Option<u16>,
  #[serde(default)]
  pub(crate) reasoning: Option<bool>,
  /// Caller-supplied typed knob overrides. Each `Some` field lands
  /// on the `LayerLabel::User` layer of the resolver, outranking
  /// last_used / arch_default / built-in.
  #[serde(default)]
  pub(crate) knobs: crate::launch::knobs::KnobSet,
  /// Free-form argv tail for `llama-server` flags the typed editor
  /// doesn't model. Appended after the resolved knobs.
  #[serde(default)]
  pub(crate) extras: Vec<String>,
  /// Optional path to a multimodal projector (mmproj) file. When
  /// `None`, the daemon auto-detects by scanning the parent
  /// directory of the model for a `mmproj-<stem>.gguf` companion.
  /// Set to `Some(path)` to override the auto-detection, or leave
  /// as `None` to let the daemon find it automatically.
  #[serde(default)]
  pub(crate) mmproj_path: Option<PathBuf>,
  /// Per-model backend override. `None` / `auto` runs the identity
  /// rule (GGUF → llama.cpp, registry → its backend); an explicit value
  /// forces a backend. Set by `start --backend` and the TUI Launch picker.
  #[serde(default)]
  pub(crate) backend: Option<crate::launch::params::BackendChoice>,
  /// Chosen **server** id (a build/binary of a backend, e.g. `llamacpp·vulkan`).
  /// Set by `start --server` / the TUI server knob. Determines which binary the
  /// launch spawns and, when `backend` is `Auto`, which backend it runs on
  /// (the server's owning backend). `None` = no server pick (default binary).
  #[serde(default)]
  pub(crate) server: Option<String>,
  /// How the caller selected launch params — drives whether the daemon
  /// applies the model's configured `default:` preset and `last_params`
  /// inheritance. Absent on the wire ⇒ `Default` (no selection), which is
  /// what the proxy's `StartParams::default()` auto-start path sends.
  #[serde(default)]
  pub(crate) selection: LaunchSelection,
  /// MTP speculative-decoding intent. `None` ⇒ inherit (default preset /
  /// last_params) or fall to `Auto`; `Some(_)` is an explicit
  /// `--mtp auto|on|off`. Launch-only, no config-file entry (KD2).
  #[serde(default)]
  pub(crate) mtp: Option<crate::launch::params::MtpEnable>,
  /// Tokens to draft per speculation step. `None` ⇒ inherit, or leave the
  /// serving backend on its own default.
  #[serde(default)]
  pub(crate) mtp_draft_n: Option<u32>,
}

/// How a launch chose its parameters. See the resolver rule in
/// `compose_and_spawn`: `Default` applies the effective default
/// (`PresetDefault` → `LastUsed`); `Explicit` means the caller already
/// flattened a named preset / inline flags into `knobs`/`extras` (skip the
/// default layer *and* `last_params` — a preset is self-contained, so a stale
/// `last_params` must not leak in; extras verbatim);
/// `Auto` is pure fit (skip the default layer and `last_params`, no extras).
#[derive(Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaunchSelection {
  #[default]
  Default,
  Explicit,
  Auto,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LaunchModeWire {
  Chat,
  Embedding,
  Rerank,
}

impl From<LaunchModeWire> for LaunchMode {
  fn from(m: LaunchModeWire) -> Self {
    match m {
      LaunchModeWire::Chat => LaunchMode::Chat,
      LaunchModeWire::Embedding => LaunchMode::Embedding,
      LaunchModeWire::Rerank => LaunchMode::Rerank,
    }
  }
}

/// Output of `compose_and_spawn` — everything the caller needs to
/// observe the launch from the outside. The IPC handler projects
/// this onto the JSON-RPC response; the proxy's auto-start path
/// keeps the `ManagedModel` handle so it can poll the state
/// machine without going through the registry snapshot.
pub struct StartedLaunch {
  pub(crate) launch_id: LaunchId,
  pub(crate) model_id: ModelId,
  pub(crate) port: u16,
  pub(crate) model: ManagedModel,
  pub(crate) log_path: PathBuf,
  /// Non-fatal advisories surfaced to the caller (CLI human output / TUI toast):
  /// capability-dropped knobs, backend admission/knob-resolution notes, and the
  /// admission-bypass note. Empty on a clean launch.
  pub(crate) warnings: Vec<String>,
  /// The layer each resolved knob value came from (provenance), surfaced on the
  /// `start_model` IPC response and the CLI `--json` output. Empty when no knob
  /// resolved from a real layer.
  pub(crate) layer_sources:
    std::collections::BTreeMap<crate::launch::knobs::KnobId, crate::launch::params::LayerLabel>,
}

/// Everything a backend's [`crate::backend::Backend::start`] needs to execute a
/// launch, once `compose_and_spawn` has done the backend-agnostic prep
/// (validation, identity, port reservation, layered knob resolution). The
/// backend decides *how* to start — a supervised child process (the default) or
/// a delegation to a managed-multiplexer umbrella — so the caller never branches
/// on lifecycle. Consumed by value.
pub struct LaunchExec {
  /// The fully-resolved launch params (knobs argv-ified from these).
  pub(crate) params: LaunchParams,
  /// The reserved launch-pool port. A process-per-model backend spawns on it; a
  /// managed multiplexer releases it (its umbrella binds a configured port).
  pub(crate) reserved_port: u16,
  /// The device-owning default binary the orchestrator chose; a backend with its
  /// own server overrides via [`crate::backend::Backend::resolve_launch_binary`].
  pub(crate) default_binary: PathBuf,
  /// Size-scaled probe budget.
  pub(crate) probe: crate::daemon::probe::ProbeOptions,
  pub(crate) id: ModelId,
  pub(crate) identity: ModelIdentity,
  pub(crate) log_path: PathBuf,
  pub(crate) mode: LaunchMode,
  pub(crate) origin: crate::daemon::supervisor::LaunchOrigin,
  /// Resolved `general.architecture`, for the admission demand model.
  pub(crate) arch: Option<String>,
  /// Trained context window, for the strict-fit ctx-clamp gate.
  pub(crate) native_ctx: Option<u32>,
  /// Shard-aware total weight bytes (admission + probe scaling).
  pub(crate) total_weight_bytes: u64,
  /// The user-supplied knob deltas to persist in `last_params` (not the full
  /// resolved set — keeps source chips meaningful).
  pub(crate) user_knobs: crate::launch::knobs::KnobSet,
  /// The layer each resolved knob value came from (provenance). Surfaced on the
  /// `start_model` IPC response and the CLI `--json` output; empty when no knob
  /// resolved from a real layer.
  pub(crate) layer_sources:
    std::collections::BTreeMap<crate::launch::knobs::KnobId, crate::launch::params::LayerLabel>,
  /// Native-knob keys the backend auto-resolved this launch — stripped from the
  /// persisted `last_params` so they re-resolve next launch (see
  /// [`crate::backend::Backend::resolve_knobs`]).
  pub(crate) auto_set_knobs: std::collections::BTreeSet<String>,
  /// Native-knob keys the backend declares as here-and-now judgements — also
  /// stripped from the persisted `last_params`, so a one-off `--preset` cannot
  /// silently become permanent (see
  /// [`KnobDef::volatile`](crate::launch::knobs::KnobDef)).
  pub(crate) volatile_knobs: Vec<&'static str>,
  /// Whether this launch bypasses the memory admission gate (streams from disk).
  pub(crate) bypasses_admission: bool,
  /// Advisories accumulated during composition, extended by the execution.
  pub(crate) warnings: Vec<String>,
  /// The backend id this launch resolved to, stamped on the persisted rows.
  pub(crate) resolved_backend_id: String,
}

/// Whether a launch resolves as "pure fit" — skipping the default-preset and
/// `LastUsed` (last_params) layers. `Auto` (explicit `--preset auto`) and
/// `Explicit` (named preset / TUI form, which already flattened a resolved
/// preset into the `User` layer) are pure-fit by construction; a no-selection
/// launch is pure-fit only when its configured default is `auto`.
fn is_pure_fit(selection: LaunchSelection, default_is_auto: bool) -> bool {
  matches!(selection, LaunchSelection::Auto | LaunchSelection::Explicit) || default_is_auto
}
/// Pick the launch binary and stamp the device-derived identity.
///
/// Precedence: an explicit **server** pick (a chosen build/binary) wins
/// outright; else the binary that owns the chosen `--device` selector (the
/// selector came from a specific binary's `--list-devices`, so we must spawn
/// *that* binary or it is invalid); else the default binary. When the device
/// selector resolves to a server, stamp its id (and, when the backend is still
/// `Auto`, its owning backend) so status and Ctrl+P capture report it instead
/// of falling back to the default.
fn pick_launch_binary(
  launch_params: &mut LaunchParams,
  picked_server: Option<&crate::backend::Server>,
  selector: Option<&str>,
  servers: &[crate::backend::Server],
  default_binary: &Path,
) -> PathBuf {
  if let Some(server) = picked_server {
    return server.binary.clone();
  }
  match selector {
    Some(sel) => match servers
      .iter()
      .find(|s| s.devices.iter().any(|d| d.selector == sel))
    {
      Some(srv) => {
        // The device selector came from this server's `--list-devices`, so the
        // launch is really on this build — stamp the id (and, when the backend
        // is still `Auto`, its owning backend) so status and Ctrl+P capture
        // report it instead of falling back to the default.
        launch_params.server = Some(srv.id.clone());
        if launch_params.backend == crate::launch::params::BackendChoice::Auto {
          launch_params.backend = crate::launch::params::BackendChoice::from_id(&srv.backend_id);
        }
        srv.binary.clone()
      }
      None => {
        // Stale persisted selector or the catalog probe failed. Drop the
        // selector so the backend's argv emitter
        // (`src/backend/llama_cpp/compose.rs`) doesn't emit an invalid
        // `--device` the default binary would reject, and spawn the default
        // binary with auto-select.
        log::warn!(
          "device selector {sel:?} not in server catalog; dropping it and spawning default binary {}",
          default_binary.display()
        );
        launch_params.knobs.remove_by_name("device");
        default_binary.to_path_buf()
      }
    },
    None => default_binary.to_path_buf(),
  }
}

/// The one launch-composition pipeline, for callers that already have a
/// parsed [`StartParams`]: the IPC `start_model` handler and the proxy's
/// auto-start path. Performs validation → arch resolve → port
/// reservation → layered knob merge → supervisor spawn → registry insert
/// → last_params recorder, so the two call sites share one code path.
/// Returns the live [`StartedLaunch`] handle on success; the
/// [`ErrorObject`] form on any failure stays JSON-RPC compatible so the
/// IPC handler can forward it verbatim.
pub(crate) async fn compose_and_spawn(
  ctx: &MethodContext,
  parsed: StartParams,
  origin: crate::daemon::supervisor::LaunchOrigin,
) -> Result<StartedLaunch, ErrorObject> {
  // Pure input-validation lives before the daemon's launch-env
  // lookup so a malformed request gives an actionable
  // `InvalidParams` error even on misconfigured daemons.
  if parsed.port.is_some() && parsed.prefer_port.is_some() {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      "set exactly one of `port` (strict) or `prefer_port` (soft preference)",
    ));
  }
  let env = ctx.launch.as_ref().ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InternalError,
      "daemon launch environment not configured (binary / port range / log dir missing)",
    )
  })?;

  // Identity + (for a GGUF) the header-derived launch inputs, from the one
  // resolver every surface shares. A file-less or directory-shaped path is
  // claimed by its backend and gets a synthetic id; a local GGUF gets its real
  // one out of a single header read, so the knob resolver never re-reads it.
  let crate::backend::ResolvedIdentity {
    id,
    identity,
    arch,
    native_ctx,
    mode_hint,
    supported_backends,
    mtp_embedded,
    ..
  } = crate::backend::resolve_identity_for_path(&parsed.model_path, Some(ctx)).map_err(
    |e| match &e {
      // Tagged so the CLI maps it to an environment exit code without
      // string-matching the message. Without the tag it was indistinguishable
      // from a bad flag and came back as a usage error.
      crate::backend::IdentityError::BackendUnavailable { .. } => ErrorObject::with_data(
        ErrorCode::InvalidParams,
        format!("`{}` {e}", parsed.model_path.display()),
        serde_json::json!({ "cause": "backend_unavailable" }),
      ),
      crate::backend::IdentityError::Header(msg) => {
        ErrorObject::new(ErrorCode::InvalidParams, msg.clone())
      }
    },
  )?;

  // Pre-spawn refusal (D-guard): on an auto-routed launch, ask every backend
  // whether it declines this model (e.g. a distributed/split GGUF half a
  // backend recognizes by arch but cannot load alone, wasting a 100 GB+ load).
  // An explicit `--backend` override passes through so the engine can surface
  // its own error. Registry-driven — names no backend.
  if parsed.backend.clone().unwrap_or_default() == crate::launch::params::BackendChoice::Auto {
    if let Some(msg) = crate::backend::refusal_for_auto_launch(arch.as_deref(), &parsed.model_path)
    {
      return Err(ErrorObject::new(ErrorCode::InvalidParams, msg));
    }
  }

  // The model's configured `default:` preset (config-only), resolved
  // server-side so it applies uniformly on CLI plain `start`, the TUI, and
  // proxy auto-start. Only a no-selection launch consults it, so explicit /
  // auto launches skip the preset-store snapshot + catalog projection. Read
  // via the same `effective_presets` the IPC handlers use.
  let is_default_sel = matches!(parsed.selection, LaunchSelection::Default);
  let effective_default = if is_default_sel {
    let store = ctx.presets.snapshot().await;
    let rows = crate::ipc::methods::catalog_rows(ctx).await;
    let key = crate::util::paths::model_file_label(&parsed.model_path);
    let path_str = parsed.model_path.display().to_string();
    Some(crate::launch::presets::effective_presets(
      &key,
      &path_str,
      arch.as_deref(),
      &store,
      &rows,
    ))
  } else {
    None
  };

  // Collapse the launch into one resolution shape. `Auto` (explicit
  // `--preset auto`) and a no-selection launch whose config default is
  // `auto` both mean "pure fit": skip the default-preset and last_params
  // layers entirely. A no-selection launch otherwise applies the effective
  // default (the `PresetDefault` layer when `default:` names a preset, then
  // last_params). An explicit launch carries its own flattened knobs/extras.
  let default_is_auto = effective_default
    .as_ref()
    .is_some_and(|e| e.default_is_auto());
  // A named preset (CLI `start --preset <name>` or a TUI form launch) is
  // self-contained: it must not inherit a stale `last_params`, so `Explicit`
  // skips the `LastUsed` layer just like `Auto`. The inline-flag-only CLI path
  // sends `Default`, so it keeps the `PresetDefault → LastUsed` fallback.
  let pure_fit = is_pure_fit(parsed.selection, default_is_auto);
  let no_selection = is_default_sel && !pure_fit;

  // Mode resolution, in precedence order: the caller's explicit choice > the
  // effective default preset's `mode:` pin > the model's own header hint >
  // chat.
  //
  // The last two rungs live here rather than on each caller on purpose. A
  // hint-derived mode is the *model's* default, which sits below the user's
  // config; callers that resolved the hint themselves and sent it as if it
  // were a choice are exactly how a preset's pin got shadowed on every launch.
  // A caller that genuinely chose (an explicit `--mode`, the proxy raising the
  // mode for the endpoint that triggered the auto-start) still sends one and
  // still wins.
  //
  // A materialised `Chat` on the preset rung is indistinguishable from "unset"
  // (a preset only stores a non-chat mode) and lands on the same place the
  // hint would anyway.
  let mode = parsed
    .mode
    .map(LaunchMode::from)
    .or_else(|| {
      if no_selection {
        effective_default
          .as_ref()
          .and_then(|e| e.default_preset())
          .map(|np| np.params.mode)
      } else {
        None
      }
    })
    .or_else(|| LaunchMode::resolve(None, mode_hint))
    .unwrap_or(LaunchMode::Chat);

  // Reject pinned port values that would corrupt our internal state
  // or require root: any value below 1024 (which also covers 0, the
  // "OS picks for me" sentinel llama-server would pick a port we
  // never track) needs root and is almost certainly a typo / hostile.
  for p in parsed.port.iter().chain(parsed.prefer_port.iter()) {
    if *p < 1024 {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("port {p} is not in the allowed range (>= 1024)"),
      ));
    }
  }
  // Validate ctx token-window bound.
  if let Some(c) = parsed.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }

  // Reject an unknown `--server` id up front — before any port/admission
  // reservation — so a typo errors cleanly instead of silently launching on
  // the default binary with a bogus `params.server` recorded. The catalog
  // fills in the background at boot, so only reject once it is populated; an
  // empty catalog means "not known yet" and keeps the id as a best-effort hint
  // (resolved to the default binary below).
  if let Some(server_id) = &parsed.server {
    let servers = env.servers.read().await;
    if !servers.is_empty() && !servers.iter().any(|s| &s.id == server_id) {
      let valid = servers
        .iter()
        .map(|s| s.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("unknown server `{server_id}`; valid: {valid}"),
      ));
    }
  }

  // Port allocation — race-safe. `reserve_port` is a CAS across
  // `collect_in_use_ports → allocate → reserve` so two concurrent
  // `start_model` calls cannot both walk away with the same port.
  // We must collect the live in-use list before taking the
  // reservation mutex, since `collect_in_use_ports` itself awaits
  // supervisor read locks.
  let live_in_use = collect_in_use_ports(ctx).await;
  let port = if let Some(preferred) = parsed.prefer_port {
    // Soft preference: try the requested port first; on conflict
    // fall back to the auto-allocator so a returning TUI user
    // doesn't fail launches just because their old port is taken.
    match ctx
      .supervisors
      .reserve_port(Some(preferred), &live_in_use, &env.port_range)
      .await
    {
      Ok(p) => p,
      Err(_) => ctx
        .supervisors
        .reserve_port(None, &live_in_use, &env.port_range)
        .await
        .map_err(|e| {
          ErrorObject::new(
            ErrorCode::InternalError,
            format!("port allocation failed: {e}"),
          )
        })?,
    }
  } else {
    ctx
      .supervisors
      .reserve_port(parsed.port, &live_in_use, &env.port_range)
      .await
      .map_err(|e| {
        ErrorObject::new(
          ErrorCode::InternalError,
          format!("port allocation failed: {e}"),
        )
      })?
  };

  // Compose LaunchParams with the layered resolver. Precedence
  // (highest first): caller-supplied `knobs` → daemon's persisted
  // `last_params` for this model → YAML `arch_defaults[architecture]`
  // → built-in `(arch, backend)` table → llama-server's own default.
  let mut launch_params = LaunchParams::new(parsed.model_path.clone(), mode);
  launch_params.port = Some(port);

  // Launch *identity* — which backend, and which of its builds — inherited on
  // a no-selection launch the same way extras and knobs are, so a plain
  // `start` reproduces the run rather than silently dropping back to the
  // priority-default build.
  //
  // This has to settle before backend resolution, because a server pick
  // decides which backend runs. That rules out the backend-matched
  // `last_params` gate the knob layers use (it needs the resolved backend,
  // which needs this) — but the recorded server id names its own backend, so
  // there is nothing to contaminate.
  let identity_default = if matches!(parsed.selection, LaunchSelection::Default) {
    inherited_launch_identity(ctx, &parsed, &identity, arch.as_deref()).await
  } else {
    InheritedIdentity::default()
  };

  // Per-model backend override: `None` keeps the default `Auto`
  // (identity rule); an explicit choice from `start --backend` / the TUI
  // picker overrides it, and a remembered one fills in behind both.
  launch_params.backend = parsed
    .backend
    .clone()
    .or(identity_default.backend)
    .unwrap_or_default();

  // Chosen server (a build/binary of a backend). Resolve it from the catalog
  // once — it drives the launch binary below and, when the backend is still
  // `Auto`, the backend itself (the server's owning backend), so a `--server`
  // pick subsumes backend selection. An unknown id was already rejected before
  // the port reservation; the only `None` that reaches here is the empty-catalog
  // startup race, which falls back to the default binary.
  launch_params.server = parsed.server.clone().or(identity_default.server);
  let picked_server: Option<crate::backend::Server> = match &launch_params.server {
    Some(server_id) => {
      let servers = env.servers.read().await;
      let found = servers.iter().find(|s| &s.id == server_id).cloned();
      if found.is_none() {
        // A typed `--server` was already rejected up front. Reaching here
        // means the id came from a preset or a remembered launch and the
        // build is gone (rebuilt llama.cpp, moved machine) — warn and take
        // the default rather than failing a launch the user did not pin.
        log::warn!("server {server_id:?} not in catalog; using the default binary");
        launch_params.server = None;
      }
      found
    }
    None => None,
  };
  if let Some(server) = &picked_server {
    if launch_params.backend == crate::launch::params::BackendChoice::Auto {
      launch_params.backend = crate::launch::params::BackendChoice::from_id(&server.backend_id);
    }
  }

  // Resolve the backend up front (D-route) so both the launch plan *and* the
  // cross-backend contamination gate below see the same decision. Selection
  // honors the per-model override, then the header-level routing signal (the
  // backend that auto-claimed the header wins when it is available and serves
  // the mode), then the identity rule as fallback. Registry-driven — this site
  // names no backend.
  let inference_backend = crate::backend::resolve_backend_for_launch(
    &identity,
    launch_params.backend.clone(),
    &supported_backends,
    mode,
    ctx,
  );
  let resolved_backend_id = crate::backend::Backend::id(&inference_backend).to_string();

  // The model's last successful launch params + the backend it resolved to.
  // Cloned once here and reused for the last-used knob layer below.
  let last_params_entry = {
    let snap = ctx.state.snapshot().await;
    snap
      .last_params
      .iter()
      .find(|e| e.id == identity)
      .map(|e| (e.params.clone(), e.resolved_backend.clone()))
  };
  // D-contamination: the implicit LastUsed layer + extras inheritance apply
  // only when the stored launch resolved to the *same* backend, so llama.cpp
  // extras (`--rope-freq-base …`) saved before ds4 existed can't poison a ds4
  // spawn (and vice versa). Explicit config (presets, inline extras) is
  // untouched. A legacy row with no tag reads as `llamacpp`.
  let last_params_backend_ok = last_params_entry
    .as_ref()
    .map(|(_, tag)| tag == &resolved_backend_id)
    .unwrap_or(false);
  let last_params = last_params_entry
    .as_ref()
    .filter(|_| last_params_backend_ok)
    .map(|(p, _)| p.clone());

  // Free-form extras (whole-list, no per-flag merge). Explicit inline extras
  // are always honored verbatim. Otherwise a no-selection launch inherits the
  // effective default's extras (the default preset's when `default:` names one
  // with extras, else last_params'); everything else (pure fit, or an
  // explicit preset that carried no extras) gets none. This supersedes the
  // PR #49 origin gate — inheritance is driven by "did the caller make a
  // selection", not Manual-vs-AutoStart, and `auto` is the clean "no inherit"
  // gesture.
  launch_params.extras = if !parsed.extras.is_empty() {
    parsed.extras.iter().cloned().map(OsString::from).collect()
  } else if no_selection {
    effective_default
      .as_ref()
      .and_then(|e| e.default_preset())
      .map(|np| np.params.extras.clone())
      .filter(|e| !e.is_empty())
      .or_else(|| last_params.as_ref().map(|p| p.extras.clone()))
      .unwrap_or_default()
  } else {
    Vec::new()
  };
  // Native knobs (not layered by the typed-knob resolver): explicit inline
  // values win verbatim; else a no-selection relaunch inherits the last-used
  // native knobs — but only through the backend-matched `last_params` gate
  // above (D-contamination), so a ds4 relaunch re-applies its `--power` /
  // `--kv-disk-*` while a cross-backend run inherits nothing. Empty for
  // llama.cpp / Lemonade.
  // Seed the resolved backend's config-derived launch knobs into
  // `backend_knobs`, fresh each launch (config projection, not user intent) —
  // llama.cpp projects `jinja` / `strict_fit` / `fit_ctx_floor` here, so the
  // generic launch path carries no llama.cpp-specific launch scalars. Runs
  // after `backend_knobs` inheritance settles (overwriting any stale inherited
  // value) and before native-knob auto-resolution reads the map.
  inference_backend.seed_launch_knobs(ctx, &mut launch_params);
  // Resolve the multimodal projector: an explicit `mmproj_path` wins;
  // otherwise auto-detect a companion next to the model — unless the
  // caller is already managing the projector through `extras`
  // (`--mmproj` to pin a path, `--no-mmproj` to force text-only), in
  // which case auto-detection would only emit a redundant second flag.
  launch_params.mmproj_path = parsed.mmproj_path.clone().or_else(|| {
    if extras_manage_mmproj(&launch_params.extras) {
      None
    } else {
      crate::discovery::scanner::find_mmproj(&parsed.model_path)
    }
  });
  // MTP intent — same whole-value inheritance as extras: an
  // explicit `--mtp` / `--mtp-draft-n` wins verbatim; else a no-selection
  // launch inherits the default preset's value, then last_params'; else the
  // `MtpEnable::Auto` default. Launch-only, no config-file entry (KD2). The
  // effective *directive* (what argv to emit) is resolved below, once real
  // capability is known.
  launch_params.mtp = parsed.mtp.unwrap_or_else(|| {
    if no_selection {
      effective_default
        .as_ref()
        .and_then(|e| e.default_preset())
        .map(|np| np.params.mtp)
        .or_else(|| last_params.as_ref().map(|p| p.mtp))
        .unwrap_or_default()
    } else {
      crate::launch::params::MtpEnable::default()
    }
  });
  launch_params.mtp_draft_n = parsed.mtp_draft_n.or_else(|| {
    if no_selection {
      effective_default
        .as_ref()
        .and_then(|e| e.default_preset())
        .and_then(|np| np.params.mtp_draft_n)
        .or_else(|| last_params.as_ref().and_then(|p| p.mtp_draft_n))
    } else {
      None
    }
  });

  // Merge the caller's top-level `ctx` and `reasoning` into the
  // User-layer typed knobs so they participate in the resolver chain
  // alongside the other typed fields. The wire payload keeps the
  // top-level fields for backward compat with scripted clients —
  // they're projected onto the typed knob slots here, with explicit
  // `knobs.{ctx,reasoning}` overrides winning if the caller set both.
  let mut user_knobs = parsed.knobs.clone();
  use crate::launch::knobs::{Concept, KnobValue as KV, Scalar};
  if let Some(c) = parsed.ctx {
    if user_knobs
      .by_concept(&resolved_backend_id, Concept::ContextLength)
      .is_none()
    {
      user_knobs.set_by_concept(
        &resolved_backend_id,
        Concept::ContextLength,
        KV::Set(Scalar::U32(c)),
      );
    }
  }
  if let Some(r) = parsed.reasoning {
    if user_knobs.get_by_name("reasoning").is_none() {
      user_knobs.set_by_name("reasoning", r.to_string());
    }
  }

  // Last-used knobs from the snapshot taken above, so a returning user
  // inherits the knobs they last shipped.
  let last_params_knobs = last_params
    .as_ref()
    .map(|p| p.knobs.clone())
    .unwrap_or_default();
  // The default preset's knobs (no-selection + named default only). Built
  // via `preset_body_from_launch_params` so the preset's `ctx`/`reasoning`
  // (held as `LaunchParams` siblings) fold back into the knob set.
  let default_preset_knobs = if no_selection {
    effective_default
      .as_ref()
      .and_then(|e| e.default_preset())
      .map(|np| crate::launch::presets::preset_body_from_launch_params(&np.params).knobs)
  } else {
    None
  };
  let empty_yaml = crate::launch::knobs::KnobSet::new();
  let yaml_knobs = arch
    .as_deref()
    .and_then(|a| env.arch_defaults.get(a))
    .unwrap_or(&empty_yaml);
  let backend = current_backend_flavor(ctx).await;
  let builtin_knobs = match arch.as_deref() {
    Some(a) => crate::launch::defaults_table::lookup(a, backend),
    None => crate::launch::defaults_table::lookup("", backend),
  };
  // Build the precedence chain per resolution shape. `User` always leads.
  // `PresetDefault` (named config default) ranks below User, above LastUsed.
  // `LastUsed` is skipped under pure fit. yaml + built-in share the
  // `ArchDefault` chip — yaml wins per field via precedence order.
  use crate::launch::params::LayerLabel;
  let mut layers: Vec<(LayerLabel, &crate::launch::knobs::KnobSet)> =
    vec![(LayerLabel::User, &user_knobs)];
  if let Some(k) = default_preset_knobs.as_ref() {
    layers.push((LayerLabel::PresetDefault, k));
  }
  if !pure_fit {
    layers.push((LayerLabel::LastUsed, &last_params_knobs));
  }
  layers.push((LayerLabel::ArchDefault, yaml_knobs));
  layers.push((LayerLabel::ArchDefault, &builtin_knobs));
  let mut resolved = crate::launch::knobs::resolve_layered(&resolved_backend_id, &layers);
  // Seed knobs no layer filled per the default launch mode: under
  // `Auto` a layer-less knob delegates to `--fit` (an Auto knob emits
  // nothing, exactly like the unset slot it replaces). The mode is
  // `Config.default_launch_mode` (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`),
  // threaded through `LaunchEnv`.
  crate::launch::knobs::seed_layerless(
    &mut resolved,
    &resolved_backend_id,
    env.default_launch_mode,
  );
  // A knob some layer supplied that this backend cannot honour is dropped and
  // surfaced rather than silently emitted (R6). The whole-map contamination
  // gate the old shape needed is gone: the resolver carries values across a
  // backend switch by concept instead of throwing the lot away.
  for dropped_id in &resolved.dropped {
    log::info!(
      "{resolved_backend_id}: knob `{dropped_id}` is not honoured by this backend; dropped"
    );
  }
  // Project resolved ctx/reasoning back onto the top-level
  // `LaunchParams` fields — `compose` emits them inline (ctx as
  // `-c <N>`, reasoning as the `--jinja --reasoning-format deepseek`
  // bundle).
  // An `Auto` ctx/reasoning collapses to "no inline flag" here
  // (`set_value()` → `None`): `compose` emits nothing and `--fit`
  // governs ctx, the chat template governs reasoning.
  launch_params.ctx = resolved.knobs.u32_by_concept(
    &resolved_backend_id,
    crate::launch::knobs::Concept::ContextLength,
  );
  launch_params.reasoning = resolved
    .knobs
    .get_by_name("reasoning")
    .and_then(|v| v.set_value())
    .and_then(|s| s.as_bool())
    .unwrap_or(false);
  // Provenance for the IPC/CLI response (only knobs a real layer supplied),
  // computed before `resolved.knobs` moves out below.
  let layer_sources = resolved.real_sources(&resolved_backend_id);
  launch_params.knobs = resolved.knobs;
  // Close the `knobs.u32(crate::launch::knobs::kid("ctx-size"))` bypass of `MAX_CTX_TOKENS` (the early check
  // only saw the top-level `parsed.ctx`): validate the *resolved* ctx,
  // which folds in both `parsed.ctx` and a typed `knobs.u32(crate::launch::knobs::kid("ctx-size"))` set via the
  // editor or last-params.
  if let Some(c) = launch_params.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }
  // Leave `device` exactly as the resolver chain set it. When no layer
  // selected one it stays `None`, so the backend's argv emitter
  // (`src/backend/llama_cpp/compose.rs`) emits no `--device` and
  // `llama-server` keeps its default (auto-select / split across every
  // visible GPU) — the documented backwards-compatible behavior.

  // Reject loopback-breaking / auth-bypass extras flags before
  // spawn. `compose` strips defensively too, but failing fast here
  // gives callers a clear error instead of a silently-different argv.
  // Release the reservation first so a retry can re-use the port —
  // otherwise a client that repeatedly submits a banned flag would
  // permanently exhaust the port pool.
  let banned = crate::launch::params::forbidden_in_extras(&launch_params.extras);
  if !banned.is_empty() {
    ctx.supervisors.release_reserved_port(port).await;
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!(
        "extras flags refused (loopback / auth contract): {}",
        banned.join(", ")
      ),
    ));
  }

  // Per-launch log file under cache_dir/logs/<short-id>-<ts>.log.
  let log_path = build_log_path(&env.log_dir, &id);

  // Scale the probe budget by total weight bytes so a slow load
  // (large multipart GGUF, HIP/ROCm upload, cold disk) doesn't trip
  // the default 120 s timeout. The catalog row carries the
  // multipart-aware total via `discovery::shard_sizes`. Fall back to
  // the path's on-disk total when the model isn't in the catalog
  // (direct-path launches that bypass scan).
  let total_weight_bytes = launch_total_bytes(ctx, &launch_params.model_path).await;
  let scaled_probe = env.probe.scale_for_model(total_weight_bytes);

  // Pick the launch binary. Precedence: an explicit **server** pick (a chosen
  // build/binary) wins outright; else the binary that owns the chosen `--device`
  // selector (the selector came from a specific binary's `--list-devices`, so we
  // must spawn *that* binary or it is invalid); else the default binary.
  let selector = launch_params
    .knobs
    .text_by_name("device")
    .filter(|s| !s.is_empty());
  let servers_snapshot = env.servers.read().await;
  let launch_binary = pick_launch_binary(
    &mut launch_params,
    picked_server.as_ref(),
    selector.as_deref(),
    &servers_snapshot[..],
    &env.binary,
  );
  drop(servers_snapshot);

  // `inference_backend` was resolved up front (before the last_params gate).
  // The orchestrator owns the branch on plan shape below.

  // Backend-specific extras denylist, checked once the backend is known: ds4
  // adds `--cors` / `--dist-` on top of the base loopback/auth heads already
  // refused above. Release the port before returning so a retry can reuse it.
  let extra_heads = crate::backend::Backend::forbidden_extra_heads(&inference_backend);
  if !extra_heads.is_empty() {
    let backend_banned =
      crate::launch::params::forbidden_in_extras_ext(&launch_params.extras, extra_heads);
    if !backend_banned.is_empty() {
      ctx.supervisors.release_reserved_port(port).await;
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!(
          "extras flags refused for the {} backend (network / loopback contract): {}",
          crate::backend::Backend::id(&inference_backend),
          backend_banned.join(", ")
        ),
      ));
    }
  }

  // Non-fatal advisories accumulated across composition, surfaced on the
  // `StartedLaunch` (CLI human output / TUI toast).
  let mut warnings: Vec<String> = Vec::new();
  // Resolve the effective MTP directive (KD1 hard gate): fold the user's intent
  // with real capability — the embedded nextn head from the GGUF header, or a
  // separate `mtp-*.gguf` drafter on disk. A force-on non-capable model warns +
  // skips. Only in chat mode: MTP is a token-generation feature, so an
  // embedding / rerank launch never speculates. And the backend defers entirely
  // when the user is already hand-driving speculation through extras (KD3) —
  // asked, not matched here, so the resolution names no backend or flag.
  // Fold the resolved knob into the typed sibling the backends compose from.
  // `parsed.mtp_draft_n` is the wire field (CLI `--mtp-draft-n`); a preset or
  // arch default supplies the same value as a knob instead, and only the
  // resolver knows which layer won. Wire field first, so an explicit flag
  // still beats an inherited layer.
  if launch_params.mtp_draft_n.is_none() {
    launch_params.mtp_draft_n = launch_params
      .knobs
      .get_by_name_for(&resolved_backend_id, "mtp-draft-n")
      .and_then(|v| v.set_value())
      .and_then(|s| s.as_u32());
  }
  let user_drives_speculation =
    crate::backend::Backend::speculation_set_in_extras(&inference_backend, &launch_params.extras);
  launch_params.mtp_directive = if matches!(mode, LaunchMode::Chat) && !user_drives_speculation {
    crate::launch::params::resolve_mtp_directive(
      launch_params.mtp,
      mtp_embedded.is_some(),
      crate::discovery::scanner::find_mtp_head(&parsed.model_path, arch.as_deref()),
      &mut warnings,
    )
  } else {
    None
  };
  // Record what speculation actually resolved to on its own knob, so every
  // surface reading `params.knobs` sees the truth. The knob emits nothing
  // itself (`Emit::Custom` — the backend builds the flags from the directive),
  // so argv is unchanged; without this the running view rendered `inherited`
  // on a launch that was speculating.
  if let Some(def) =
    crate::launch::knobs::def_for_backend(&resolved_backend_id, crate::launch::knobs::kid("mtp"))
  {
    launch_params.knobs.set(
      def.knob_id(),
      crate::launch::knobs::KnobValue::Set(crate::launch::knobs::Scalar::Bool(
        launch_params.mtp_directive.is_some(),
      )),
    );
  }
  // Same for the draft count, which no longer emits on its own: show what the
  // launch is really drafting with, and drop a stale count on a launch that
  // ended up not speculating.
  if let Some(def) = crate::launch::knobs::def_for_backend(
    &resolved_backend_id,
    crate::launch::knobs::kid("mtp-draft-n"),
  ) {
    match launch_params
      .mtp_draft_n
      .filter(|_| launch_params.mtp_directive.is_some())
    {
      Some(n) => launch_params.knobs.set(
        def.knob_id(),
        crate::launch::knobs::KnobValue::Set(crate::launch::knobs::Scalar::U32(n)),
      ),
      None => {
        launch_params.knobs.clear(def.knob_id());
      }
    }
  }
  // Dropped-knob surfacing (R6): typed knobs the user set that the resolved
  // backend can't honor are silently dropped from argv — tell the user which.
  // ds4 honors only `Ctx`, so a `--flash-attn` on a ds4-routed model warns.
  //
  // Against the **user** layer, not the resolved set. The resolved set carries
  // the resolver's own answers (a model-default `reasoning`, an arch default),
  // so every launch on a narrow-capability backend warned that it had dropped a
  // knob the user never touched — noise on a bare `start`, on the surface where
  // a real dropped knob most needs to be noticed.

  // Native-knob auto-resolution: a backend resolves its own **Auto** native
  // knobs from live host context (e.g. enabling disk streaming when residency
  // won't fit), mutating `backend_knobs` in place — the uniform knob
  // auto-behavior, not a special case. A user on/off is left untouched.
  // Registry-driven, so this path names no backend or knob.
  let native_resolution = inference_backend
    .resolve_knobs(ctx, &mut launch_params, total_weight_bytes)
    .await;
  for msg in &native_resolution.warnings {
    log::warn!("{msg}");
  }
  warnings.extend(native_resolution.warnings);
  // A knob combination the engine rejects at startup: refuse here rather than
  // spawning a process that loads the weights and then exits.
  if let Some(msg) = native_resolution.refusal {
    ctx.supervisors.release_reserved_port(port).await;
    return Err(ErrorObject::new(ErrorCode::InvalidParams, msg));
  }
  let auto_set_knobs = native_resolution.auto_set;
  // Whether this launch skips the memory admission gate (streams from disk).
  let bypasses_admission = inference_backend.bypasses_admission(&launch_params);

  // Hand the launch to the resolved backend: it decides *how* to start — a
  // supervised child process (the default `start`) or a delegation to its
  // managed-multiplexer umbrella — so this path never branches on lifecycle and
  // names no backend.
  let exec = LaunchExec {
    params: launch_params,
    reserved_port: port,
    default_binary: launch_binary,
    probe: scaled_probe,
    id,
    identity,
    log_path,
    mode,
    origin,
    arch,
    native_ctx,
    total_weight_bytes,
    user_knobs,
    layer_sources,
    auto_set_knobs,
    volatile_knobs: crate::launch::knobs::volatile_ids(&resolved_backend_id),
    bypasses_admission,
    warnings,
    resolved_backend_id,
  };
  inference_backend.start(ctx, exec).await
}

/// Execute a **process-per-model** launch: the pre-spawn memory admission gate,
/// the supervised spawn, the persisted running snapshot, and the background
/// last-params + admission-settle recorders. This is the backend-agnostic body
/// of a process backend's [`crate::backend::Backend::start`] — a managed
/// multiplexer overrides `start` and never reaches here. `spec` is the argv the
/// backend composed. Consumes `exec` (destructured back into the original local
/// names, so this body is a verbatim lift of the old inline spawn path).
pub(crate) async fn spawn_supervised(
  ctx: &MethodContext,
  exec: LaunchExec,
  spec: crate::backend::ProcessLaunchSpec,
  admission_floor: Option<u32>,
  fit_gate: Option<crate::daemon::supervisor::FitGate>,
) -> Result<StartedLaunch, ErrorObject> {
  let LaunchExec {
    mut warnings,
    params: launch_params,
    reserved_port: port,
    probe: scaled_probe,
    id,
    identity,
    log_path,
    mode,
    origin,
    arch,
    native_ctx: _,
    total_weight_bytes,
    user_knobs,
    layer_sources,
    auto_set_knobs,
    volatile_knobs,
    bypasses_admission,
    resolved_backend_id,
    default_binary: _,
  } = exec;
  let resolved_backend_id = resolved_backend_id.clone();
  let launch_spec = spec;

  // Pre-spawn admission: project this launch's demand floor and refuse *before*
  // spawn if it won't fit the sampled budget minus the bytes other in-flight
  // launches already reserved. This is the safety net `--fit` can't provide on
  // UMA (its free reading conflates GTT with system RAM). Keyed by the reserved
  // `port`; released when the child settles or on any failure below. Best-effort:
  // skipped when there is no host-metrics sample yet. A backend that streams from
  // disk (`bypasses_admission`) skips the hard OOM refusal — logged + surfaced.
  // The bypass note is suppressed when the daemon auto-resolved the streaming
  // knob (that path already warned, in memory terms).
  let bypass_note_suppressed = !auto_set_knobs.is_empty();
  let mut admitted = false;
  // A non-GGUF identity used to skip the gate entirely, so a
  // process-per-model backend serving a directory could OOM the host with no
  // pre-spawn refusal at all. It has no header to project from, but the
  // weight bytes are recoverable — from the catalog, or by measuring the
  // directory — and the backend has already resolved its own cache budget,
  // which is enough for the same arithmetic. Gating on a non-zero catalog
  // size here would put the zero-size case (a launch by absolute path from
  // outside the scan roots) back outside the gate, which is the one case
  // that most needs it.
  let gate_applies = identity.as_gguf().is_some()
    || crate::backend::Backends::all()
      .into_iter()
      .find(|b| crate::backend::Backend::id(b) == resolved_backend_id)
      .map(|b| crate::backend::Backend::lifecycle(&b))
      == Some(crate::backend::Lifecycle::ProcessPerModel);
  if gate_applies {
    if let Some(host_slot) = ctx.host_metrics.as_ref() {
      let snapshot = host_slot.read().await.clone();
      if crate::launch::admission::is_sampled(&snapshot) {
        // Project demand against the pinned ctx, else the backend's admission
        // floor (llama.cpp's `--fit-ctx` floor), else a neutral default.
        let effective_ctx = launch_params
          .ctx
          .or(admission_floor)
          .unwrap_or(crate::config::DEFAULT_FIT_CTX_FLOOR);
        let free = crate::launch::admission::effective_free_bytes(&snapshot);
        let gpu_backend = snapshot.gpu_backend.clone();
        let model_path = launch_params.model_path.clone();
        let knobs = launch_params.knobs.clone();
        let arch_owned = arch.clone();
        // Weights the launch actually has to hold. Tensors the loader streams
        // row-by-row from the mapping never become resident, and counting them
        // refuses launches that fit with room to spare (a 103.7 GiB
        // Qwen3.8-Flash-Next projected 133.5 GiB on a 121 GiB host but needs
        // ~77 GiB). Zero for every model without such a tensor, and for any
        // launch pinning `no-mmap`.
        let lazy_bytes =
          launch_lazy_bytes(ctx, &launch_params.model_path, &launch_params.knobs).await;
        let weights_total = total_weight_bytes.saturating_sub(lazy_bytes);
        let mtp_active = launch_params.mtp_directive.is_some();
        let demand = if identity.as_gguf().is_some() {
          let demand_backend_id = resolved_backend_id.clone();
          tokio::task::spawn_blocking(move || {
            let header = read_gguf_header(&model_path, HeaderReadOptions::default())
              .ok()?
              .header;
            Some(crate::launch::admission::project_demand(
              &header,
              arch_owned.as_deref(),
              &knobs,
              &demand_backend_id,
              effective_ctx,
              &gpu_backend,
              weights_total,
              mtp_active,
            ))
          })
          .await
          .ok()
          .flatten()
        } else {
          // No header, so no per-layer KV estimate. Weights come from
          // `launch_total_bytes`, which measures a directory when neither the
          // catalog nor `stat` can size it — the same figure the backend priced
          // its cache against, so the gate and the cap cannot disagree.
          if weights_total == 0 {
            log::warn!(
              "admission: no weight size for {} — gate cannot engage",
              model_path.display()
            );
          }
          // Only the backend can price the rest, since the figure lives in its
          // own knob vocabulary and may be a pool fraction rather than bytes.
          crate::backend::Backends::all()
            .into_iter()
            .find(|b| crate::backend::Backend::id(b) == resolved_backend_id)
            .and_then(|b| crate::backend::Backend::projected_cache_bytes(&b, &launch_params, free))
            .filter(|_| weights_total > 0)
            .map(|cache| {
              weights_total
                .saturating_add(cache)
                .saturating_add(crate::launch::headroom::overhead_band_bytes(&gpu_backend))
            })
        };
        if let Some(demand) = demand {
          if let Err(refusal) = ctx.admission.try_admit(u64::from(port), demand, free) {
            if bypasses_admission {
              if !bypass_note_suppressed {
                let msg = format!(
                  "this launch bypasses the memory admission gate (streaming from disk) — {}",
                  format_admission_refusal(&refusal)
                );
                log::warn!("{msg}");
                warnings.push(msg);
              }
            } else {
              ctx.supervisors.release_reserved_port(port).await;
              return Err(ErrorObject::with_data(
                ErrorCode::ResourceExhausted,
                format_admission_refusal(&refusal),
                serde_json::json!({ "cause": "launch_refused" }),
              ));
            }
          } else {
            admitted = true;
          }
        }
      }
    }
  }

  // The strict-fit ctx-clamp readiness gate is resolved by the backend
  // (`Backend::readiness_fit_gate`) and passed in — llama.cpp builds it from its
  // `fit_ctx_floor` / `strict_fit` config; every other backend passes `None`.
  let spawn_result = supervisor_spawn(ManagedSpawn {
    id: id.clone(),
    params: launch_params.clone(),
    port,
    mode,
    log_path: log_path.clone(),
    plan: launch_spec,
    origin,
    fit_gate,
    resolved_backend: resolved_backend_id.clone(),
  })
  .await;
  let model = match spawn_result {
    Ok(m) => m,
    Err(e) => {
      ctx.supervisors.release_reserved_port(port).await;
      if admitted {
        ctx.admission.release(u64::from(port));
      }
      return Err(ErrorObject::new(
        ErrorCode::InternalError,
        format!("supervisor spawn: {e}"),
      ));
    }
  };

  let launch_id = ctx.supervisors.next_id();
  ctx
    .supervisors
    .insert(launch_id.clone(), model.clone())
    .await;
  ctx.supervisors.release_reserved_port(port).await;

  // Persist running snapshot, retained by `(id, port)` so the same GGUF launched
  // twice on different ports persists both (the orphan sweep re-adopts either).
  // Stamp the live `L#` (same value keying the supervisor map above) so
  // `backend_for_launch` can resolve *this* launch's backend from the snapshot
  // and hand its stop to the right backend — a process backend that overrides
  // `stop` is then dispatched correctly, not silently routed to the default.
  let pid = model.pid().await.unwrap_or(0) as i32;
  let started_at = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  ctx
    .state
    .mutate(|s| {
      s.running.retain(|r| !(r.id == identity && r.port == port));
      s.running.push(RunningSnapshot {
        id: identity.clone(),
        pid,
        port,
        started_at,
        launch_id: Some(launch_id.clone()),
        params: launch_params.clone(),
        actuals: Default::default(),
        resolved_backend: resolved_backend_id.clone(),
      });
    })
    .await;

  // Persist the *user-supplied* knob deltas on the Loading → Ready transition
  // (source chips stay meaningful; resolver output isn't frozen).
  let mut persist_params = launch_params.clone();
  persist_params.knobs = user_knobs;
  persist_params.ctx = None;
  persist_params.reasoning = false;
  persist_params.knobs = knobs_for_persist(persist_params.knobs, &auto_set_knobs, &volatile_knobs);
  spawn_last_params_recorder(
    ctx.state.clone(),
    model.clone(),
    identity.clone(),
    persist_params,
    resolved_backend_id.clone(),
    scaled_probe.timeout,
    ctx.shutdown.clone(),
  );

  // Settle the admission reservation when the child leaves Loading.
  if admitted {
    spawn_admission_settle(
      ctx.admission.clone(),
      model.clone(),
      port,
      scaled_probe.timeout,
      ctx.shutdown.clone(),
    );
  }

  Ok(StartedLaunch {
    launch_id,
    model_id: id,
    port,
    model,
    log_path,
    warnings,
    layer_sources,
  })
}

/// Stop a **supervised child process** launch: SIGTERM (bounded by `grace_secs`),
/// deregister it, and drop its running snapshot. The backend-agnostic body of a
/// process backend's [`crate::backend::Backend::stop`] (the default) — the
/// counterpart to [`spawn_supervised`]. A managed multiplexer overrides `stop`
/// and calls this only to tear its own umbrella process down. Returns the
/// `{launch_id, state}` stop response, or `InvalidParams` for an unknown id.
pub(crate) async fn stop_supervised(
  ctx: &MethodContext,
  launch_id: &LaunchId,
  grace_secs: u64,
) -> Result<serde_json::Value, ErrorObject> {
  let model = ctx.supervisors.get(launch_id).await.ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("unknown launch_id: {}", launch_id.as_str()),
    )
  })?;
  let stopped_port = model.port();
  let final_state = model.stop(Duration::from_secs(grace_secs)).await;
  ctx.supervisors.remove(launch_id).await;
  drop_running_snapshots(ctx, &[(launch_id.clone(), stopped_port)]).await;
  Ok(serde_json::json!({
    "launch_id": launch_id,
    "state": crate::ipc::methods::flatten_state(&final_state),
  }))
}

/// Drop the persisted running snapshots for launches that have just stopped.
///
/// Keyed on the **launch id** — unique per launch, and the same key whatever
/// shape the model's identity is. This used to compare a `ModelIdentity` built
/// from the supervisor's `ModelId`, which is always the `Gguf` variant, so a
/// row persisted under a `Backend` identity never matched and its snapshot
/// outlived the process. `status` then matched that orphan by port and reported
/// its backend and its resolved ctx for whatever launched next on the reused
/// port — every later launch in the session, of any backend.
///
/// The `port` fallback covers a row adopted before launch ids were stamped.
/// Keyed per launch, so a second launch of the same model on another port keeps
/// its own row either way.
pub(crate) async fn drop_running_snapshots(ctx: &MethodContext, stopped: &[(LaunchId, u16)]) {
  if stopped.is_empty() {
    return;
  }
  let stopped = stopped.to_vec();
  ctx
    .state
    .mutate(move |s| {
      s.running.retain(|r| {
        !stopped.iter().any(|(launch_id, port)| match &r.launch_id {
          Some(id) => id == launch_id,
          None => r.port == *port,
        })
      });
    })
    .await;
}

/// The backend that owns a running launch: an umbrella's owner (via
/// [`crate::backend::umbrella_owner`]), else the resolved backend recorded on
/// the launch's running snapshot, else the default backend. Lets the stop path
/// hand a launch to its backend without the caller knowing whether it is a
/// supervised child or a delegated model — the registry resolves the owner, the
/// backend decides *how* to stop.
pub(crate) async fn backend_for_launch(
  ctx: &MethodContext,
  launch_id: &LaunchId,
) -> crate::backend::Backends {
  if let Some(owner) = crate::backend::umbrella_owner(launch_id) {
    return owner;
  }
  let backend_id = ctx
    .state
    .snapshot()
    .await
    .running
    .into_iter()
    .find(|r| r.launch_id.as_ref() == Some(launch_id))
    .map(|r| r.resolved_backend);
  match backend_id {
    Some(id) => crate::backend::Backends::all()
      .into_iter()
      .find(|b| b.id() == id)
      .unwrap_or_else(crate::backend::default_backend),
    None => crate::backend::default_backend(),
  }
}

/// The `backend_knobs` to persist into `last_params`: the resolved set, minus
/// the two kinds of knob that must not be replayed.
///
/// - `auto_set` — what the *daemon* resolved this launch. A one-time response
///   to live conditions, not a user opt-in; freezing it would make the next
///   no-selection relaunch inherit it as explicit after conditions change.
/// - `volatile` — what the *user* set, on a knob the backend declares as a
///   here-and-now judgement ([`KnobDef::volatile`](crate::launch::knobs::KnobDef)).
///   It applies to the launch that asked for it and to any launch that names
///   its preset again, but is not remembered on the user's behalf.
///
/// Every other user-set knob is preserved. Pure so the invariant is unit-testable.
fn knobs_for_persist(
  resolved: crate::launch::knobs::KnobSet,
  auto_set: &std::collections::BTreeSet<String>,
  volatile: &[&str],
) -> crate::launch::knobs::KnobSet {
  // Generic — both key sets come from the backend. A value llamastash
  // resolved on the user's behalf must not come back as if they had asked
  // for it, so it is stripped before the launch is remembered.
  let mut out = resolved;
  out.retain_ids(|id| !auto_set.contains(id.as_str()) && !volatile.contains(&id.as_str()));
  out
}

/// Launch identity carried over from the model's configured default preset or
/// its last successful launch. Empty unless the caller made no selection.
#[derive(Default)]
struct InheritedIdentity {
  backend: Option<crate::launch::params::BackendChoice>,
  server: Option<String>,
}

/// The backend / server a no-selection launch should reuse.
///
/// Precedence matches every other inherited field: the model's `default:`
/// preset outranks its last successful launch. Returns empties when neither
/// pins anything, which leaves the identity rule and the priority-default
/// build in charge exactly as before.
async fn inherited_launch_identity(
  ctx: &MethodContext,
  parsed: &StartParams,
  identity: &crate::backend::identity::ModelIdentity,
  arch: Option<&str>,
) -> InheritedIdentity {
  let from_preset = {
    let store = ctx.presets.snapshot().await;
    let rows = crate::ipc::methods::catalog_rows(ctx).await;
    let key = crate::util::paths::model_file_label(&parsed.model_path);
    let path_str = parsed.model_path.display().to_string();
    crate::launch::presets::effective_presets(&key, &path_str, arch, &store, &rows)
      .default_preset()
      .map(|np| (np.params.backend.clone(), np.params.server.clone()))
  };
  let from_last = {
    let snap = ctx.state.snapshot().await;
    snap
      .last_params
      .iter()
      .find(|e| &e.id == identity)
      .map(|e| (e.params.backend.clone(), e.params.server.clone()))
  };

  let pick = |f: fn(&(crate::launch::params::BackendChoice, Option<String>)) -> bool| {
    from_preset
      .as_ref()
      .filter(|v| f(v))
      .or(from_last.as_ref().filter(|v| f(v)))
  };
  InheritedIdentity {
    backend: pick(|(b, _)| b.explicit_id().is_some()).map(|(b, _)| b.clone()),
    server: pick(|(_, s)| s.is_some()).and_then(|(_, s)| s.clone()),
  }
}

/// Human-readable admission refusal: the effective free (post-headroom),
/// what other launches hold, this launch's projected demand, and the
/// remediation menu — so the number is self-explaining and actionable.
fn format_admission_refusal(refusal: &crate::launch::admission::Refusal) -> String {
  // One canonical GiB formatter (bytes ÷ 1024³, 1 decimal) shared with
  // every other memory surface — see `crate::init::detection::fmt_gib`.
  let gib = crate::init::detection::fmt_gib;
  format!(
    "launch refused: needs {} but only {} is free (effective {} after headroom, minus {} reserved by in-flight launches). \
     Stop a resident model, pin a smaller --ctx, lower fit_ctx_floor, or retry once a model frees memory.",
    gib(refusal.demand_bytes),
    gib(refusal.available_bytes()),
    gib(refusal.effective_free_bytes),
    gib(refusal.reserved_bytes),
  )
}

/// Poll the child until it leaves Loading (Ready / Error / Stopped) and
/// drop its admission reservation. Mirrors the recorder's poll shape;
/// bounded by the scaled probe budget and the shutdown token so a child
/// that never settles can't leak the reservation forever.
///
/// Best-effort hand-off: the reservation drops on Ready, but the 1 Hz
/// host-metrics sampler may not yet reflect the child's freshly-committed
/// allocation, so a concurrent launch in that sub-second window can see
/// stale free *and* no reservation. The window is bounded by one sample
/// tick and the in-process load check is the final OOM backstop, so this
/// is accepted rather than papered over with a longer hold.
fn spawn_admission_settle(
  ledger: Arc<crate::launch::admission::Ledger>,
  model: ManagedModel,
  port: u16,
  probe_budget: Duration,
  shutdown: ShutdownToken,
) {
  tokio::spawn(async move {
    let deadline = Instant::now() + probe_budget;
    loop {
      match model.state().await {
        ManagedState::Ready | ManagedState::Error { .. } | ManagedState::Stopped => {
          ledger.release(u64::from(port));
          return;
        }
        _ => {}
      }
      if Instant::now() > deadline {
        ledger.release(u64::from(port));
        return;
      }
      tokio::select! {
        _ = shutdown.wait_until_triggered() => {
          ledger.release(u64::from(port));
          return;
        }
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
      }
    }
  });
}

fn spawn_last_params_recorder(
  state: PersistedState,
  model: ManagedModel,
  id: ModelIdentity,
  params: LaunchParams,
  resolved_backend: String,
  probe_budget: Duration,
  shutdown: ShutdownToken,
) {
  tokio::spawn(async move {
    // Wait out the same size-scaled probe budget the supervisor uses
    // (base 120 s + up to 2 h for very large weights) so a slow load
    // still gets its params recorded on the Loading → Ready transition.
    // The poll also observes the daemon's shutdown token so SIGTERM
    // during a pending Loading state doesn't block clean process exit.
    let deadline = Instant::now() + probe_budget;
    loop {
      match model.state().await {
        ManagedState::Ready => {
          state
            .mutate(|s| s.upsert_last_params(id.clone(), params.clone(), resolved_backend.clone()))
            .await;
          // Post-launch actuals: stamp what the backend actually chose
          // on the running snapshot so `status` / the TUI Running view /
          // `show` can render the resolved context. The supervisor's
          // readiness gate already fetched actuals for fit-governed
          // launches (to run the strict-fit ctx-clamp check) and stashed
          // the result on the model, so reuse it instead of fetching
          // twice; only fall back to a fetch when the gate didn't run
          // (pinned ctx / no trained-window metadata). The fetch is the
          // resolved backend's — a backend with no actuals endpoint (ds4)
          // returns empty, so the row stays "unavailable" without a wasted
          // probe. Best-effort — an empty result leaves the row unavailable.
          if let Some(port) = params.port {
            let mut actuals = model.actuals().await;
            if actuals.is_empty() {
              let backend = crate::backend::Backends::all()
                .into_iter()
                .find(|b| b.id() == resolved_backend)
                .unwrap_or_else(crate::backend::default_backend);
              actuals = backend.fetch_actuals(port, Duration::from_secs(5)).await;
            }
            if !actuals.is_empty() {
              let id = id.clone();
              state
                .mutate(move |s| {
                  if let Some(snap) = s.running.iter_mut().find(|r| r.id == id && r.port == port) {
                    snap.actuals = actuals;
                  }
                })
                .await;
            }
          }
          return;
        }
        ManagedState::Error { .. } | ManagedState::Stopped => return,
        _ => {}
      }
      if Instant::now() > deadline {
        return;
      }
      tokio::select! {
        _ = shutdown.wait_until_triggered() => return,
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
      }
    }
  });
}

async fn collect_in_use_ports(ctx: &MethodContext) -> Vec<u16> {
  let mut ports: Vec<u16> = ctx
    .supervisors
    .snapshot()
    .await
    .into_iter()
    .map(|(_, m)| m.port())
    .collect();
  // Also avoid colliding with `llama-server` processes that this
  // daemon didn't spawn directly but were launched by *some*
  // llamastash instance — typically a previous run of this daemon
  // or a sibling UAT/test daemon whose state.json the orphan sweep
  // didn't see. The `LLAMASTASH_LAUNCHED=1` env marker (stamped by
  // `supervisor::spawn`) is what makes these recognisable; the port
  // gets parsed out of the orphan's argv in `orphans::sweep`.
  //
  // The bind probe in `ports::try_bind_probe` already rejects an
  // externally-held port at reservation time, so this list is a
  // hint to the allocator rather than a correctness gate — it just
  // lets us skip straight past known-busy slots instead of probing
  // them one by one on every launch.
  let externals = ctx.external.read().await;
  for ext in externals.iter() {
    if ext.launched_by_llamastash {
      if let Some(p) = ext.port {
        if !ports.contains(&p) {
          ports.push(p);
        }
      }
    }
  }
  ports
}

/// Does the caller's `extras` tail already manage the multimodal
/// projector? `--mmproj <path>` pins a projector and `--no-mmproj`
/// force-disables it; in either case the daemon must not auto-detect
/// one too. Matches the flag head in both space form (`--mmproj`) and
/// equals form (`--mmproj=/path`), case-insensitively. `--no-mmproj-offload`
/// (offload tuning, projector still on) is left to auto-detect, so the
/// match is exact rather than a prefix test.
fn extras_manage_mmproj(extras: &[OsString]) -> bool {
  extras.iter().any(|e| {
    let lossy = e.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy);
    head.eq_ignore_ascii_case("--mmproj") || head.eq_ignore_ascii_case("--no-mmproj")
  })
}

/// Bytes the engine will stream from the mapping rather than hold resident,
/// summed over every file of the model.
///
/// Mirrors [`launch_total_bytes`]'s catalog-first lookup so the gate subtracts
/// over exactly the file set it measured. Returns `0` — leaving the projection
/// untouched — when the launch pins `no-mmap` (the loader's lazy path needs
/// mmap), or when no shard carries a lazy-flagged tensor, which is every model
/// without a per-layer embedding table.
///
/// Reads sibling shards because the tensor lives in one of them: a split set's
/// first shard is often metadata-only, so the header the gate already has says
/// nothing about it.
async fn launch_lazy_bytes(
  ctx: &MethodContext,
  model_path: &std::path::Path,
  knobs: &crate::launch::knobs::KnobSet,
) -> u64 {
  if knobs.bool(crate::launch::knobs::kid("no-mmap")) == Some(true) {
    return 0;
  }
  let snap = ctx.catalog.snapshot().await;
  let paths: Vec<std::path::PathBuf> = match snap.iter().find(|m| m.path == model_path) {
    Some(row) => std::iter::once(row.path.clone())
      .chain(row.split_siblings.iter().cloned())
      .collect(),
    None => vec![model_path.to_path_buf()],
  };
  tokio::task::spawn_blocking(move || {
    paths
      .iter()
      .filter_map(|p| read_gguf_header(p, HeaderReadOptions::default()).ok())
      .map(|r| crate::gguf::memory::lazy_streamed_bytes(&r.header))
      .fold(0u64, u64::saturating_add)
  })
  .await
  .unwrap_or(0)
}

/// Total on-disk weight bytes for the model the launch handler is
/// about to spawn. Prefers the catalog row (which already includes
/// split-shard aggregation via `discovery::shard_sizes`); falls back
/// to `shard_sizes::on_disk_total` on the bare path for direct
/// launches that bypass scan. `0` when neither path is reachable —
/// the probe scaler treats that as "no signal, keep the default".
async fn launch_total_bytes(ctx: &MethodContext, model_path: &std::path::Path) -> u64 {
  let snap = ctx.catalog.snapshot().await;
  if let Some(row) = snap.iter().find(|m| m.path == model_path) {
    if let Some(b) = row.metadata.as_ref().and_then(|md| md.weights_bytes) {
      return b;
    }
    let total = crate::discovery::shard_sizes::on_disk_total(&row.path, &row.split_siblings);
    if total > 0 {
      return total;
    }
  }
  let total = crate::discovery::shard_sizes::on_disk_total(model_path, &[]);
  if total > 0 {
    return total;
  }
  // `stat` on a directory reports its inode size, not its contents, so a
  // directory-shaped model launched by absolute path from outside the scan
  // roots measured 0 here — and 0 is the figure that disarms the memory
  // guards. The admission gate used to patch this up privately, which left the
  // backend's own cache-cap arithmetic still working from 0: one launch, two
  // different weights. Measured once, here, so every consumer agrees.
  // Off the runtime — a stat per shard, following the HF cache's symlinks.
  let p = model_path.to_path_buf();
  tokio::task::spawn_blocking(move || crate::launch::admission::dir_weight_bytes(&p))
    .await
    .unwrap_or(0)
}

/// Live GPU-backend flavor — keys the built-in defaults table.
/// Reads the host-metrics sampler when available; falls back to
/// `Unsampled` (treated identically to `Unknown` by the table) when
/// the daemon has no sampler attached (catalog-only tests).
async fn current_backend_flavor(ctx: &MethodContext) -> crate::daemon::host_metrics::GpuFlavor {
  if let Some(slot) = &ctx.host_metrics {
    let snap = slot.read().await;
    return snap.flavor();
  }
  crate::daemon::host_metrics::GpuFlavor::Unsampled
}

fn build_log_path(log_dir: &std::path::Path, id: &ModelId) -> PathBuf {
  let stem = id
    .path
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model");
  let ts = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  let short = id.short_fingerprint();
  log_dir.join(format!("{stem}-{short}-{ts}.log"))
}

#[cfg(test)]
mod tests {
  use tokio::sync::RwLock;

  use super::*;
  use crate::config::LemonadeConfig;
  use crate::daemon::context::LaunchEnv;
  use crate::daemon::probe::ProbeOptions;
  use crate::daemon::registry::SupervisorRegistry;

  /// A named preset / TUI form launch (`Explicit`) is self-contained: it must
  /// not inherit a stale `last_params`, so it resolves as pure-fit and skips
  /// the `LastUsed` layer. `Auto` is pure-fit by construction; a no-selection
  /// launch is pure-fit only when its configured default is `auto`.
  #[test]
  fn explicit_selection_is_pure_fit_and_skips_last_used() {
    assert!(is_pure_fit(LaunchSelection::Explicit, false));
    assert!(is_pure_fit(LaunchSelection::Auto, false));
    assert!(is_pure_fit(LaunchSelection::Default, true));
    assert!(!is_pure_fit(LaunchSelection::Default, false));
  }
  /// A `--device` selector that resolves to a server must stamp that server's
  /// id (and, when the backend is still `Auto`, its owning backend) so status
  /// and Ctrl+P capture report the real build instead of the default.
  #[test]
  fn pick_launch_binary_stamps_device_derived_identity() {
    let mut params = LaunchParams::new(
      PathBuf::from("/m/a.gguf"),
      crate::launch::mode::LaunchMode::Chat,
    );
    let rocm = crate::backend::Server {
      id: "llamacpp-rocm".into(),
      backend_id: "llamacpp".into(),
      binary: PathBuf::from("/bin/llama-server-rocm"),
      name: "llamacpp-rocm".into(),
      devices: vec![crate::backend::Device {
        selector: "ROCm0".into(),
        gpu_backend: "ROCm".into(),
        name: "Radeon".into(),
        total_mib: None,
        free_mib: None,
      }],
    };
    let default = PathBuf::from("/bin/llama-server");
    let binary = pick_launch_binary(
      &mut params,
      None,
      Some("ROCm0"),
      std::slice::from_ref(&rocm),
      &default,
    );
    assert_eq!(binary, PathBuf::from("/bin/llama-server-rocm"));
    assert_eq!(params.server.as_deref(), Some("llamacpp-rocm"));
    assert_eq!(
      params.backend,
      crate::launch::params::BackendChoice::from_id("llamacpp")
    );
  }

  /// An explicit server pick wins outright and is not re-derived from the
  /// device selector.
  #[test]
  fn pick_launch_binary_prefers_explicit_server() {
    let mut params = LaunchParams::new(
      PathBuf::from("/m/a.gguf"),
      crate::launch::mode::LaunchMode::Chat,
    );
    let rocm = crate::backend::Server {
      id: "llamacpp-rocm".into(),
      backend_id: "llamacpp".into(),
      binary: PathBuf::from("/bin/llama-server-rocm"),
      name: "llamacpp-rocm".into(),
      devices: vec![],
    };
    let default = PathBuf::from("/bin/llama-server");
    let binary = pick_launch_binary(
      &mut params,
      Some(&rocm),
      Some("ROCm0"),
      std::slice::from_ref(&rocm),
      &default,
    );
    assert_eq!(binary, PathBuf::from("/bin/llama-server-rocm"));
  }

  /// A stale selector (no server owns it) drops the `device` knob and falls
  /// back to the default binary without inventing an identity.
  #[test]
  fn pick_launch_binary_drops_stale_selector() {
    let mut params = LaunchParams::new(
      PathBuf::from("/m/a.gguf"),
      crate::launch::mode::LaunchMode::Chat,
    );
    params.knobs.set_by_name("device", "ROCm0");
    let default = PathBuf::from("/bin/llama-server");
    let binary = pick_launch_binary(&mut params, None, Some("ROCm0"), &[], &default);
    assert_eq!(binary, default);
    assert!(params.server.is_none());
    assert!(params.knobs.text_by_name("device").is_none());
  }

  /// A volatile id has to be one `retain_ids` will actually see, which is the
  /// knob's registry id. The guard used to read a hand-kept list that still
  /// carried pre-registry spellings, so it matched nothing and silently stopped
  /// guarding — one `--preset` run then disabled an automatic memory cap for
  /// every launch after it.
  #[test]
  fn volatile_ids_are_registry_ids_that_persistence_can_match() {
    use crate::backend::Backend;
    for b in crate::backend::Backends::all() {
      let declared: Vec<&str> = Backend::knobs(&b).iter().map(|d| d.id).collect();
      for id in crate::launch::knobs::volatile_ids(Backend::id(&b)) {
        assert!(
          declared.contains(&id),
          "{} marks `{id}` volatile but never declares the knob itself",
          Backend::id(&b)
        );
      }
    }
  }

  #[tokio::test]
  async fn backend_for_launch_resolves_process_launch_from_its_snapshot() {
    use crate::backend::Backend;
    // A process launch stamps its `L#` + resolved backend on the running
    // snapshot, so `backend_for_launch` hands the stop to the launch's *real*
    // backend rather than defaulting — the guard for a process-per-model backend
    // that overrides `stop`. (llama.cpp and ds4 share the default stop today, so
    // this is latent-correctness, not observable yet.)
    let ctx = MethodContext::new(ShutdownToken::new());
    let push = |id_path: &'static str, lid: &'static str, backend: &'static str, port: u16| {
      let identity = ModelIdentity::Gguf(crate::gguf::identity::compute(id_path, b"hdr"));
      let params = LaunchParams::new(PathBuf::from(id_path), LaunchMode::Chat);
      RunningSnapshot {
        id: identity,
        pid: 1,
        port,
        started_at: 0,
        launch_id: Some(LaunchId(lid.to_string())),
        params,
        actuals: Default::default(),
        resolved_backend: backend.to_string(),
      }
    };
    ctx
      .state
      .mutate(|s| {
        s.running.push(push("/m/ds4.gguf", "L1", "ds4", 41100));
        s.running
          .push(push("/m/llama.gguf", "L2", "llamacpp", 41101));
      })
      .await;

    assert_eq!(
      backend_for_launch(&ctx, &LaunchId("L1".to_string()))
        .await
        .id(),
      "ds4",
      "a ds4-tagged process launch resolves to ds4, not the default backend"
    );
    assert_eq!(
      backend_for_launch(&ctx, &LaunchId("L2".to_string()))
        .await
        .id(),
      "llamacpp"
    );
    // An unknown id falls back to the default backend.
    assert_eq!(
      backend_for_launch(&ctx, &LaunchId("L9".to_string()))
        .await
        .id(),
      crate::backend::DEFAULT_BACKEND_ID
    );
  }

  /// A stopped launch must drop its snapshot **whatever shape its identity is**.
  /// Keyed on `ModelIdentity` equality, a row persisted under a `Backend`
  /// identity never matched the supervisor's always-`Gguf` id, so the snapshot
  /// outlived the process; `status` then matched the orphan by port and
  /// reported its backend and ctx for the next launch on that port.
  #[tokio::test]
  async fn stopping_a_backend_identity_launch_drops_its_running_snapshot() {
    let ctx = MethodContext::new(ShutdownToken::new());
    let backend_row = RunningSnapshot {
      id: ModelIdentity::Backend(crate::backend::identity::BackendModelId {
        backend: "some-backend".to_string(),
        name: "o/r".to_string(),
      }),
      pid: 1,
      port: 41100,
      started_at: 0,
      launch_id: Some(LaunchId("L1".to_string())),
      params: LaunchParams::new(PathBuf::from("/m/snapshots/rev"), LaunchMode::Chat),
      actuals: Default::default(),
      resolved_backend: "some-backend".to_string(),
    };
    let other = RunningSnapshot {
      launch_id: Some(LaunchId("L2".to_string())),
      port: 41101,
      ..backend_row.clone()
    };
    ctx
      .state
      .mutate(move |s| {
        s.running.push(backend_row);
        s.running.push(other);
      })
      .await;

    drop_running_snapshots(&ctx, &[(LaunchId("L1".to_string()), 41100)]).await;

    let left = ctx.state.snapshot().await.running;
    assert_eq!(left.len(), 1, "only the stopped launch's row is dropped");
    assert_eq!(left[0].launch_id, Some(LaunchId("L2".to_string())));
  }

  /// A row with no stamped launch id (adopted before the stamp existed) still
  /// has to be reapable, so the port fallback stays.
  #[tokio::test]
  async fn drop_running_snapshots_falls_back_to_port_for_an_unstamped_row() {
    let ctx = MethodContext::new(ShutdownToken::new());
    ctx
      .state
      .mutate(|s| {
        s.running.push(RunningSnapshot {
          id: ModelIdentity::Gguf(crate::gguf::identity::compute("/m/a.gguf", b"hdr")),
          pid: 1,
          port: 41100,
          started_at: 0,
          launch_id: None,
          params: LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat),
          actuals: Default::default(),
          resolved_backend: "llamacpp".to_string(),
        });
      })
      .await;

    drop_running_snapshots(&ctx, &[(LaunchId("L1".to_string()), 41100)]).await;
    assert!(ctx.state.snapshot().await.running.is_empty());
  }

  #[test]
  fn extras_manage_mmproj_detects_explicit_projector_flags() {
    let pin = vec![OsString::from("--mmproj"), OsString::from("/m/p.gguf")];
    assert!(extras_manage_mmproj(&pin), "space-form --mmproj");
    let pin_eq = vec![OsString::from("--MMPROJ=/m/p.gguf")];
    assert!(
      extras_manage_mmproj(&pin_eq),
      "equals-form, case-insensitive"
    );
    let disable = vec![OsString::from("--no-mmproj")];
    assert!(extras_manage_mmproj(&disable), "--no-mmproj force-disable");
    // Offload tuning leaves the projector on → auto-detect still runs.
    let offload = vec![OsString::from("--no-mmproj-offload")];
    assert!(
      !extras_manage_mmproj(&offload),
      "--no-mmproj-offload is not projector management"
    );
    let unrelated = vec![OsString::from("--threads"), OsString::from("8")];
    assert!(!extras_manage_mmproj(&unrelated));
  }

  #[tokio::test]
  async fn lemonade_start_without_binary_releases_reserved_port() {
    use crate::config::loader::PortRange;
    use crate::gguf::test_fixtures::build_minimal_gguf;
    use crate::launch::params::BackendChoice;

    // A real (minimal) GGUF on disk so `compose_and_spawn` clears header
    // resolution and reaches the backend-selection seam.
    let dir = tempfile::tempdir().expect("tempdir");
    let model_path = dir.path().join("tiny.gguf");
    std::fs::write(&model_path, build_minimal_gguf("llama")).expect("write gguf");

    // A single-port range on a probe-clear port. Find one the allocator
    // accepts (tolerates TIME_WAIT), then release it so the run under test
    // starts from an empty reservation set.
    let registry = SupervisorRegistry::new();
    let mut found = None;
    for _ in 0..16 {
      let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
      let p = l.local_addr().unwrap().port();
      drop(l);
      let range = PortRange { start: p, end: p };
      if registry.reserve_port(None, &[], &range).await.is_ok() {
        registry.release_reserved_port(p).await;
        found = Some(p);
        break;
      }
    }
    let port = found.expect("at least one of 16 attempts lands on a probe-clear port");
    let range = PortRange {
      start: port,
      end: port,
    };

    let env = LaunchEnv {
      // Never spawned on this path — the managed-multiplexer arm errors out
      // before any process launch.
      binary: PathBuf::from("/nonexistent/llama-server"),
      port_range: range,
      log_dir: dir.path().to_path_buf(),
      probe: ProbeOptions::default(),
      arch_defaults: Default::default(),
      servers: Arc::new(RwLock::new(Vec::new())),
      default_launch_mode: Default::default(),
    };

    // Lemonade enabled but pointed at a binary that does not exist. The
    // explicit-`binary` branch never falls back to PATH, so resolution is
    // deterministically `None` even on a host that has a real `lemond`
    // installed — the test can't be fooled by the dev machine's PATH.
    let ctx = MethodContext::new(ShutdownToken::new())
      .with_supervisors(registry)
      .with_launch_env(env)
      .with_backend(
        crate::backend::BackendConfig {
          lemonade: LemonadeConfig {
            enabled: Some(true),
            servers: vec![crate::backend::ServerConfig {
              binary: PathBuf::from("/nonexistent/lemond-xyz"),
              name: None,
            }],
            port: 13305,
          },
          ..Default::default()
        },
        std::collections::BTreeMap::new(),
      );

    let parsed = StartParams {
      model_path,
      // Force the managed-multiplexer seam: an explicit Lemonade override
      // outranks the GGUF identity rule.
      backend: Some(BackendChoice::Explicit("lemonade".into())),
      ..Default::default()
    };

    // `StartedLaunch` (the Ok variant) isn't `Debug`, so match rather than
    // `expect_err`.
    let err = match compose_and_spawn(
      &ctx,
      parsed,
      crate::daemon::supervisor::LaunchOrigin::Manual,
    )
    .await
    {
      Ok(_) => panic!("unresolvable lemond binary must error"),
      Err(e) => e,
    };
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("lemond"),
      "error should name the missing lemond binary, got: {}",
      err.message
    );

    // The reservation must have been released: the single-port range is
    // allocatable again only if `compose_and_spawn` dropped its hold on the
    // error path (otherwise the range is exhausted and this errors).
    let reclaimed = ctx
      .supervisors
      .reserve_port(None, &[], &range)
      .await
      .expect("reserved port must be released on the lemonade-unavailable error path");
    assert_eq!(reclaimed, port);
  }

  /// A `MethodContext` wired with a real (single-port) launch env and a
  /// minimal GGUF on disk, so `compose_and_spawn` clears identity
  /// resolution and reaches the post-header validation seams (port / ctx
  /// bounds) without ever spawning a process. Returns `(ctx, model_path,
  /// _tempdir-guard)`; keep the guard alive for the test's duration.
  async fn ctx_with_env_and_gguf() -> (MethodContext, PathBuf, tempfile::TempDir) {
    use crate::config::loader::PortRange;
    use crate::gguf::test_fixtures::build_minimal_gguf;

    let dir = tempfile::tempdir().expect("tempdir");
    let model_path = dir.path().join("tiny.gguf");
    std::fs::write(&model_path, build_minimal_gguf("llama")).expect("write gguf");

    let env = LaunchEnv {
      binary: PathBuf::from("/nonexistent/llama-server"),
      port_range: PortRange {
        start: 41000,
        end: 41000,
      },
      log_dir: dir.path().to_path_buf(),
      probe: ProbeOptions::default(),
      arch_defaults: Default::default(),
      servers: Arc::new(RwLock::new(Vec::new())),
      default_launch_mode: Default::default(),
    };
    let ctx = MethodContext::new(ShutdownToken::new())
      .with_supervisors(SupervisorRegistry::new())
      .with_launch_env(env);
    (ctx, model_path, dir)
  }

  async fn expect_invalid_params(ctx: &MethodContext, parsed: StartParams) -> String {
    match compose_and_spawn(ctx, parsed, crate::daemon::supervisor::LaunchOrigin::Manual).await {
      Ok(_) => panic!("expected an InvalidParams error, launch succeeded"),
      Err(e) => {
        assert_eq!(e.code, ErrorCode::InvalidParams.as_i32());
        e.message
      }
    }
  }

  #[tokio::test]
  async fn compose_rejects_both_port_and_prefer_port() {
    // The mutual-exclusion guard runs before the env lookup, so a bare
    // context (no launch env) is enough to exercise it.
    let ctx = MethodContext::new(ShutdownToken::new());
    let parsed = StartParams {
      model_path: PathBuf::from("/m/x.gguf"),
      port: Some(11500),
      prefer_port: Some(11501),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains("exactly one of"), "got: {msg}");
  }

  #[tokio::test]
  async fn compose_rejects_privileged_port() {
    let (ctx, model_path, _guard) = ctx_with_env_and_gguf().await;
    let parsed = StartParams {
      model_path,
      port: Some(80),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains(">= 1024"), "got: {msg}");
  }

  #[tokio::test]
  async fn compose_rejects_ctx_over_maximum() {
    let (ctx, model_path, _guard) = ctx_with_env_and_gguf().await;
    let parsed = StartParams {
      model_path,
      ctx: Some(crate::config::MAX_CTX_TOKENS + 1),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains("exceeds maximum"), "got: {msg}");
  }

  #[tokio::test]
  async fn compose_rejects_unknown_server_id() {
    let (ctx, model_path, _guard) = ctx_with_env_and_gguf().await;
    // Populate the server catalog so the id is validated (an empty catalog is
    // the "not known yet" startup race, which intentionally falls through).
    ctx
      .launch
      .as_ref()
      .unwrap()
      .servers
      .write()
      .await
      .push(crate::backend::Server {
        id: "llamacpp-rocm".into(),
        backend_id: "llamacpp".into(),
        binary: PathBuf::from("/nonexistent/llama-server"),
        name: "llamacpp-rocm".into(),
        devices: Vec::new(),
      });
    let parsed = StartParams {
      model_path,
      server: Some("nope".into()),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains("unknown server"), "got: {msg}");
    assert!(msg.contains("nope"), "names the bad id: {msg}");
    assert!(msg.contains("llamacpp-rocm"), "lists valid ids: {msg}");
  }

  #[test]
  fn format_admission_refusal_reports_every_number() {
    // The refusal string must surface demand, available (effective −
    // reserved), effective free, and reserved bytes so the operator can
    // see exactly why the launch was turned away.
    let refusal = crate::launch::admission::Refusal {
      demand_bytes: 8 * 1024 * 1024 * 1024,
      effective_free_bytes: 10 * 1024 * 1024 * 1024,
      reserved_bytes: 4 * 1024 * 1024 * 1024,
    };
    let msg = format_admission_refusal(&refusal);
    assert!(msg.contains("launch refused"));
    // demand 8 GiB, available 6 GiB (10 − 4), effective 10 GiB, reserved 4 GiB.
    assert!(msg.contains("8.0 GiB"), "demand: {msg}");
    assert!(msg.contains("6.0 GiB"), "available: {msg}");
    assert!(msg.contains("10.0 GiB"), "effective free: {msg}");
    assert!(msg.contains("4.0 GiB"), "reserved: {msg}");
    // Remediation menu is part of the contract — it tells the user what
    // to do next.
    assert!(msg.contains("Stop a resident model"));
  }

  #[test]
  fn build_log_path_uses_stem_fingerprint_and_timestamp() {
    let id = crate::gguf::identity::ModelId {
      path: PathBuf::from("/models/Qwen3-7B-Q4_K_M.gguf"),
      header_blake3: [0xabu8; 32],
    };
    let path = build_log_path(std::path::Path::new("/var/log/ls"), &id);
    let name = path.file_name().unwrap().to_string_lossy();
    // `<stem>-<short-fingerprint>-<unix-ts>.log`
    assert!(name.starts_with("Qwen3-7B-Q4_K_M-"), "stem prefix: {name}");
    assert!(name.ends_with(".log"), "log suffix: {name}");
    assert!(
      name.contains(&id.short_fingerprint()),
      "embeds the short fingerprint: {name}"
    );
    assert_eq!(path.parent().unwrap(), std::path::Path::new("/var/log/ls"));
  }

  #[test]
  fn build_log_path_falls_back_to_model_stem_for_pathless_id() {
    // An id whose path has no file stem (e.g. a bare directory) still
    // produces a usable log filename rather than panicking.
    let id = crate::gguf::identity::ModelId {
      path: PathBuf::from("/"),
      header_blake3: [0u8; 32],
    };
    let path = build_log_path(std::path::Path::new("/tmp"), &id);
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("model-"), "fallback stem: {name}");
  }

  #[test]
  fn launch_selection_defaults_to_default_and_round_trips() {
    // An absent `selection` on the wire is the no-selection default — what
    // the proxy's `StartParams::default()` auto-start path relies on.
    let parsed: StartParams =
      serde_json::from_value(serde_json::json!({"model_path": "/m/x.gguf"})).unwrap();
    assert_eq!(parsed.selection, LaunchSelection::Default);
    assert_eq!(StartParams::default().selection, LaunchSelection::Default);
    for (s, want) in [
      ("default", LaunchSelection::Default),
      ("explicit", LaunchSelection::Explicit),
      ("auto", LaunchSelection::Auto),
    ] {
      let p: StartParams =
        serde_json::from_value(serde_json::json!({"model_path": "/m/x.gguf", "selection": s}))
          .unwrap();
      assert_eq!(p.selection, want, "selection {s} round-trips");
    }
  }
}
