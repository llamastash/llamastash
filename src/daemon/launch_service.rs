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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backend::identity::ModelIdentity;
use crate::backend::Backend;
use crate::config::MAX_CTX_TOKENS;
use crate::config::{KnobValue, KnobValueOpt};
use crate::daemon::context::{MethodContext, PersistedState};
use crate::daemon::registry::LaunchId;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::RunningSnapshot;
use crate::daemon::supervisor::{
  spawn as supervisor_spawn, ManagedModel, ManagedSpawn, ManagedState,
};
use crate::gguf::header::{read_path as read_gguf_header, HeaderReadOptions};
use crate::gguf::identity::ModelId;
use crate::ipc::methods::resolve_model_id_and_arch;
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
  pub(crate) knobs: crate::config::TypedKnobs,
  /// Free-form argv tail for `llama-server` flags the typed editor
  /// doesn't model. Appended after the resolved knobs.
  #[serde(default)]
  pub(crate) extras: Vec<String>,
  /// Per-backend native-knob values, keyed by descriptor id (see
  /// [`crate::launch::native_knobs`]). Passed through to the backend's
  /// `prepare_launch`; not layered by the typed-knob resolver. Empty for
  /// llama.cpp / Lemonade (`#[serde(default)]` keeps existing clients'
  /// payloads byte-stable). ds4 is the first consumer.
  #[serde(default)]
  pub(crate) backend_knobs: std::collections::BTreeMap<String, crate::config::KnobValue<String>>,
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
/// default layer, let `last_params` fill knob gaps, extras verbatim);
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
  pub(crate) user_knobs: crate::config::TypedKnobs,
  /// Native-knob keys the backend auto-resolved this launch — stripped from the
  /// persisted `last_params` so they re-resolve next launch (see
  /// [`crate::backend::Backend::resolve_native_knobs`]).
  pub(crate) auto_set_knobs: std::collections::BTreeSet<String>,
  /// Whether this launch bypasses the memory admission gate (streams from disk).
  pub(crate) bypasses_admission: bool,
  /// Advisories accumulated during composition, extended by the execution.
  pub(crate) warnings: Vec<String>,
  /// The backend id this launch resolved to, stamped on the persisted rows.
  pub(crate) resolved_backend_id: String,
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

  // Resolve identity + (for GGUF) the architecture. A file-less backend-registry
  // path (e.g. a managed-multiplexer's synthetic `<scheme>://<name>`) has no local
  // file, so a backend mints the identity from the path instead of reading a
  // header — that is what makes the backend dispatch below select the registry
  // backend rather than crashing on the missing GGUF. Every other path is a local
  // GGUF: one header read yields both the canonical id and the arch. Names no
  // backend — the synthetic-path recognition is registry-driven.
  let (id, arch, native_ctx, supported_backends, mtp_embedded, identity): (
    ModelId,
    Option<String>,
    Option<u32>,
    Vec<String>,
    Option<u32>,
    ModelIdentity,
  ) = match crate::backend::synthetic_identity_for_path_available(&parsed.model_path, ctx) {
    Some((identity, _backend_id)) => {
      // A synthetic ModelId keeps the file-keyed plumbing (log path, running
      // snapshot retention) working; the sentinel header hash marks it as
      // not-a-GGUF. Arch + native_ctx are `None` — the backend owns the recipe,
      // not us, so the strict-fit ctx gate never applies to such a row.
      let synthetic = ModelId {
        path: parsed.model_path.clone(),
        header_blake3: [0u8; 32],
      };
      (synthetic, None, None, Vec::new(), None, identity)
    }
    None => {
      let (id, arch, native_ctx, supported_backends, mtp_embedded) =
        resolve_model_id_and_arch(&parsed.model_path)?;
      let identity: ModelIdentity = id.clone().into();
      (
        id,
        arch,
        native_ctx,
        supported_backends,
        mtp_embedded,
        identity,
      )
    }
  };

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

  // Mode resolution: explicit override > catalog hint > default to chat.
  // The CLI surface refuses to default silently when discovery says
  // "Unknown" (cli_args.rs::StartArgs::mode comment), but the daemon
  // is one layer down; a missing override here means the caller has
  // already accepted the default.
  let mode = parsed
    .mode
    .map(LaunchMode::from)
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
  // Per-model backend override: `None` keeps the default `Auto`
  // (identity rule); an explicit choice from `start --backend` / the TUI
  // picker overrides it.
  launch_params.backend = parsed.backend.unwrap_or_default();

  // Chosen server (a build/binary of a backend). Resolve it from the catalog
  // once — it drives the launch binary below and, when the backend is still
  // `Auto`, the backend itself (the server's owning backend), so a `--server`
  // pick subsumes backend selection. An unknown id was already rejected before
  // the port reservation; the only `None` that reaches here is the empty-catalog
  // startup race, which falls back to the default binary.
  launch_params.server = parsed.server.clone();
  let picked_server: Option<crate::backend::Server> = match &launch_params.server {
    Some(server_id) => {
      let servers = env.servers.read().await;
      let found = servers.iter().find(|s| &s.id == server_id).cloned();
      if found.is_none() {
        log::warn!("server {server_id:?} not in catalog yet; using default binary");
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

  // The model's configured `default:` preset (config-only), resolved
  // server-side so it applies uniformly on CLI plain `start`, the TUI, and
  // proxy auto-start. Only a no-selection launch consults it, so explicit /
  // auto launches skip the preset-store snapshot + catalog projection. Read
  // via the same `effective_presets` the IPC handlers use.
  let is_default_sel = matches!(parsed.selection, LaunchSelection::Default);
  let effective_default = if is_default_sel {
    let store = ctx.presets.snapshot().await;
    let rows = crate::ipc::methods::catalog_rows(ctx).await;
    let key = crate::util::paths::path_basename(&parsed.model_path);
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
  let pure_fit = matches!(parsed.selection, LaunchSelection::Auto) || default_is_auto;
  let no_selection = is_default_sel && !pure_fit;

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
  launch_params.backend_knobs = if !parsed.backend_knobs.is_empty() {
    parsed.backend_knobs.clone()
  } else if no_selection {
    // Same precedence as `extras` above: the default preset's native knobs
    // outrank last_params, so a `default:` preset that pins `--mtp` /
    // `--ssd-streaming` survives a plain `start`.
    effective_default
      .as_ref()
      .and_then(|e| e.default_preset())
      .map(|np| np.params.backend_knobs.clone())
      .filter(|k| !k.is_empty())
      .or_else(|| last_params.as_ref().map(|p| p.backend_knobs.clone()))
      .unwrap_or_default()
  } else {
    std::collections::BTreeMap::new()
  };
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
  if user_knobs.ctx.is_none() {
    user_knobs.ctx = parsed.ctx.map(KnobValue::Set);
  }
  if user_knobs.reasoning.is_none() {
    user_knobs.reasoning = parsed.reasoning.map(KnobValue::Set);
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
  let empty_yaml = crate::config::TypedKnobs::default();
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
  let mut layers: Vec<(LayerLabel, &crate::config::TypedKnobs)> =
    vec![(LayerLabel::User, &user_knobs)];
  if let Some(k) = default_preset_knobs.as_ref() {
    layers.push((LayerLabel::PresetDefault, k));
  }
  if !pure_fit {
    layers.push((LayerLabel::LastUsed, &last_params_knobs));
  }
  layers.push((LayerLabel::ArchDefault, yaml_knobs));
  layers.push((LayerLabel::ArchDefault, &builtin_knobs));
  let mut resolved = crate::launch::params::resolve_layered(&layers);
  // Seed knobs no layer filled per the default launch mode: under
  // `Auto` a layer-less knob delegates to `--fit` (an Auto knob emits
  // nothing, exactly like the unset slot it replaces). The mode is
  // `Config.default_launch_mode` (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`),
  // threaded through `LaunchEnv`.
  crate::launch::params::seed_layerless(&mut resolved, env.default_launch_mode);
  // Project resolved ctx/reasoning back onto the top-level
  // `LaunchParams` fields — `compose` emits them inline (ctx as
  // `-c <N>`, reasoning as the `--jinja --reasoning-format deepseek`
  // bundle).
  // An `Auto` ctx/reasoning collapses to "no inline flag" here
  // (`set_value()` → `None`): `compose` emits nothing and `--fit`
  // governs ctx, the chat template governs reasoning.
  launch_params.ctx = resolved.knobs.ctx.set_value().copied();
  launch_params.reasoning = resolved
    .knobs
    .reasoning
    .set_value()
    .copied()
    .unwrap_or(false);
  launch_params.knobs = resolved.knobs;
  // Close the `knobs.ctx` bypass of `MAX_CTX_TOKENS` (the early check
  // only saw the top-level `parsed.ctx`): validate the *resolved* ctx,
  // which folds in both `parsed.ctx` and a typed `knobs.ctx` set via the
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
    .device
    .set_value()
    .map(String::as_str)
    .filter(|s| !s.is_empty())
    .map(str::to_string);
  let launch_binary = if let Some(server) = &picked_server {
    server.binary.clone()
  } else {
    match selector {
      Some(sel) => {
        let owning_binary = {
          let servers = env.servers.read().await;
          servers
            .iter()
            .find(|s| s.devices.iter().any(|d| d.selector == sel))
            .map(|s| s.binary.clone())
        };
        owning_binary.unwrap_or_else(|| {
        // Stale persisted selector or the catalog probe failed. Drop
        // the selector so the backend's argv emitter
        // (`src/backend/llama_cpp/compose.rs`) doesn't emit an invalid
        // `--device` the default binary would reject, and spawn the
        // default binary with auto-select.
        log::warn!(
          "device selector {sel:?} not in server catalog; dropping it and spawning default binary {}",
          env.binary.display()
        );
        launch_params.knobs.device = None;
        env.binary.clone()
      })
      }
      None => env.binary.clone(),
    }
  };

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
  // Dropped-knob surfacing (R6): typed knobs the user set that the resolved
  // backend can't honor are silently dropped from argv — tell the user which.
  // ds4 honors only `Ctx`, so a `--flash-attn` on a ds4-routed model warns.
  {
    let caps = crate::backend::Backend::capabilities(&inference_backend);
    let dropped: Vec<&str> = crate::launch::flag_aliases::knob_specs()
      .iter()
      .filter(|spec| !caps.supports(spec.field) && field_is_set(&launch_params.knobs, spec.field))
      .map(|spec| spec.canonical)
      .collect();
    if !dropped.is_empty() {
      let msg = format!(
        "{} does not support these knobs — dropped from the launch: {}",
        crate::backend::Backend::id(&inference_backend),
        dropped.join(", ")
      );
      log::warn!("{msg}");
      warnings.push(msg);
    }
  }

  // Native-knob auto-resolution: a backend resolves its own **Auto** native
  // knobs from live host context (e.g. enabling disk streaming when residency
  // won't fit), mutating `backend_knobs` in place — the uniform knob
  // auto-behavior, not a special case. A user on/off is left untouched.
  // Registry-driven, so this path names no backend or knob.
  let native_resolution = inference_backend
    .resolve_native_knobs(ctx, &mut launch_params, total_weight_bytes)
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
    auto_set_knobs,
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
    auto_set_knobs,
    bypasses_admission,
    resolved_backend_id,
    default_binary: _,
  } = exec;
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
  // catalog knows the weight bytes and the backend has already resolved its
  // own cache budget, which is enough for the same arithmetic.
  let gate_applies = identity.as_gguf().is_some()
    || (crate::backend::Backends::all()
      .into_iter()
      .find(|b| crate::backend::Backend::id(b) == resolved_backend_id)
      .map(|b| crate::backend::Backend::lifecycle(&b))
      == Some(crate::backend::Lifecycle::ProcessPerModel)
      && total_weight_bytes > 0);
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
        let weights_total = total_weight_bytes;
        let mtp_active = launch_params.mtp_directive.is_some();
        let demand = if identity.as_gguf().is_some() {
          tokio::task::spawn_blocking(move || {
            let header = read_gguf_header(&model_path, HeaderReadOptions::default())
              .ok()?
              .header;
            Some(crate::launch::admission::project_demand(
              &header,
              arch_owned.as_deref(),
              &knobs,
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
          // No header, so no per-layer KV estimate. Weights are known from the
          // catalog, and the cache is whatever budget the backend resolved for
          // itself (its `resolve_native_knobs` ran above) — an unbounded one
          // means we cannot project, so stay out of the way rather than guess.
          crate::launch::admission::resolved_cache_bytes(&launch_params).map(|cache| {
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
  persist_params.backend_knobs =
    backend_knobs_for_persist(&launch_params.backend_knobs, &auto_set_knobs);
  spawn_last_params_recorder(
    ctx.state.clone(),
    model.clone(),
    identity.clone(),
    persist_params,
    resolved_backend_id,
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
  // Drop the running snapshot keyed by `(id, port)` so a second launch of the
  // same GGUF on a different port keeps its row.
  let stopped_id: ModelIdentity = model.id().clone().into();
  ctx
    .state
    .mutate(move |s| {
      s.running
        .retain(|r| !(r.id == stopped_id && r.port == stopped_port));
    })
    .await;
  Ok(serde_json::json!({
    "launch_id": launch_id,
    "state": crate::ipc::methods::flatten_state(&final_state),
  }))
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

/// Whether `field` holds a concrete (`Set`) value in `knobs` — the view the
/// dropped-knob warning needs (an `Auto` / unset knob emits nothing, so it is
/// not "dropped" in any user-visible sense).
fn field_is_set(
  knobs: &crate::config::TypedKnobs,
  field: crate::launch::flag_aliases::KnobField,
) -> bool {
  knobs.slot(field).as_u32().is_some()
    || knobs.slot(field).as_f32().is_some()
    || knobs.slot(field).as_bool().is_some()
    || knobs.slot(field).as_str().is_some()
}

/// The `backend_knobs` to persist into `last_params`: the resolved set, minus
/// any knob the backend auto-resolved this launch (`auto_set`). An auto-resolved
/// knob is a one-time response to live conditions, not a user opt-in — freezing
/// it would make the next no-selection relaunch inherit it as explicit even after
/// conditions change. A user-set / inherited knob is preserved. Pure so the
/// invariant is unit-testable.
fn backend_knobs_for_persist(
  resolved: &std::collections::BTreeMap<String, KnobValue<String>>,
  auto_set: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeMap<String, KnobValue<String>> {
  // Drop any knob the daemon auto-resolved this launch (`auto_set`): those
  // re-resolve from live conditions next launch, so persisting them would freeze
  // a value the user never chose. Generic — the key set comes from the backend.
  let mut out = resolved.clone();
  out.retain(|k, _| !auto_set.contains(k));
  out
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
    return crate::discovery::shard_sizes::on_disk_total(&row.path, &row.split_siblings);
  }
  crate::discovery::shard_sizes::on_disk_total(model_path, &[])
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

  #[test]
  fn auto_resolved_knobs_are_stripped_from_persisted_last_params() {
    use crate::config::KnobValue;
    let mut resolved = std::collections::BTreeMap::new();
    resolved.insert("user_knob".to_string(), KnobValue::Set("80".to_string()));
    resolved.insert("auto_knob".to_string(), KnobValue::Set("true".to_string()));

    // A knob the backend auto-resolved this launch is stripped from what we
    // persist, so a later no-selection relaunch re-evaluates from live conditions
    // instead of inheriting a value the user never chose; unrelated (user-set /
    // inherited) knobs survive verbatim.
    let auto_set: std::collections::BTreeSet<String> =
      ["auto_knob".to_string()].into_iter().collect();
    let persisted = backend_knobs_for_persist(&resolved, &auto_set);
    assert!(
      !persisted.contains_key("auto_knob"),
      "an auto-resolved knob must not be frozen into last_params"
    );
    assert_eq!(
      persisted.get("user_knob"),
      Some(&KnobValue::Set("80".to_string()))
    );

    // Nothing auto-resolved → every knob persists verbatim.
    let none_auto = backend_knobs_for_persist(&resolved, &std::collections::BTreeSet::new());
    assert_eq!(
      none_auto.get("auto_knob"),
      Some(&KnobValue::Set("true".to_string())),
      "with no auto-set keys, all knobs persist"
    );
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
