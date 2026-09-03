//! Pre-flight: turn an inbound HTTP request into a forwarding plan.
//!
//! This module walks every incoming `/v1/...` request before reaching
//! for the upstream `llama-server`. The output is a
//! [`RouteDecision`] that captures everything [`super::forward`]
//! needs to do the pass-through, plus enough context for the error
//! arms to render an OpenAI-shaped body.
//!
//! Hot path:
//!   1. Buffer the body under the size cap ([`http-body-util::Limited`]).
//!   2. Extract `body.model` with a tolerant `JustModel` parse that
//!      ignores every other field. Empty / missing → 400.
//!   3. Build a `Vec<CatalogRow>` from the catalog snapshot and run
//!      the existing fuzzy resolver ([`crate::cli::resolve::resolve_model`]).
//!   4. Walk the supervisor snapshot for a Ready entry whose
//!      [`ModelId`] path matches the resolved catalog row.
//!
//! The [`RouteDecision::NotRunning`] arm drives the launch +
//! single-flight + fallback machinery; the variant carries the
//! resolved row + arch so that path doesn't have to repeat the
//! lookup.

use std::sync::Arc;

use http_body_util::{BodyExt, Limited};
use hyper::body::{Bytes, Incoming};

use crate::daemon::supervisor::ManagedState;
use crate::discovery::DiscoveredModel;
use crate::gguf::identity::ModelId;
use crate::launch::resolve::{resolve_model_with_candidates, CatalogRow, ResolveError};

use super::launch::{self, LaunchOutcome};
use super::mru::{pick_fallback, FallbackCandidate};
use super::router::ProxyResponse;
use super::state::ProxyState;

/// Default inbound body size cap. 16 MiB lets vision payloads through
/// (a base64 phone photo is ~4–11 MiB; base64 inflates the source file
/// ~33%) while still bounding worst-case memory and refusing accidental
/// uploads. Anything larger surfaces as HTTP 413 (a `BodyError::TooLarge`
/// from the `Limited` adapter). `proxy.max_body_size` overrides per daemon.
pub const DEFAULT_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Forwarding plan produced by [`decide`]. Keep this `pub(crate)` —
/// the router pattern-matches on variants but no external module
/// constructs them.
#[derive(Debug)]
pub(crate) enum RouteDecision {
  /// Forward to a Ready supervisor on `port`. `served_model_id` is
  /// the display name of the model actually serving the request;
  /// equal to `requested_model` on the happy path and diverges on
  /// fallback. `fallback` gates the `x-llamastash-*`
  /// response headers in [`super::forward`].
  ReadyAt {
    port: u16,
    served_model_id: String,
    /// Canonical `(path, header_blake3)` of the supervisor we picked.
    /// Threaded through so the forward path can re-verify the port
    /// still binds the same model just before sending the request
    /// upstream — defends against the Ready→Stopping→port-reuse race
    /// where a different model could be answering on the same port
    /// by the time we connect.
    served_model_key: ModelId,
    /// Upstream OpenAI path prefix (`None` → the direct backend's `/v1/...`,
    /// `Some("/api")` → a managed-multiplexer umbrella's `/api/v1/...`).
    upstream_path_prefix: Option<String>,
    fallback: bool,
    fallback_reason: Option<String>,
  },
  /// The catalog has the model but no Ready supervisor is serving it.
  /// Dispatched into `handle_not_running` which runs the auto-start +
  /// single-flight + family-MRU fallback flow. The variant carries the
  /// resolved row + arch so the launch path doesn't repeat the lookup.
  NotRunning {
    requested_model: String,
    /// Resolved catalog entry consumed by the launch path to build
    /// `StartParams` for `compose_and_spawn` without re-running the
    /// resolver.
    // dead_code: consumed via destructuring in router::forward_request;
    // the field itself is moved out, not read by name.
    #[allow(dead_code)]
    resolved_row: Box<CatalogRow>,
    /// Catalog arch metadata (e.g. `"llama"`, `"qwen3"`). `None`
    /// when discovery couldn't parse the GGUF header. The family-MRU
    /// fallback pivots on this field.
    // dead_code: consumed via destructuring in router::forward_request.
    #[allow(dead_code)]
    arch: Option<String>,
  },
  /// `resolve_model` returned zero matches. Emits 404
  /// `model_not_found` with `matches: []`.
  NotFound { requested_model: String },
  /// `resolve_model` returned > 1 matches. Emits 400
  /// `ambiguous_model` with the candidates' repo-qualified ids.
  Ambiguous {
    requested_model: String,
    candidates: Vec<String>,
  },
  /// `body.model` is absent or empty. Emits 400
  /// `invalid_request` with `code: "model_required"`.
  ModelRequired,
  /// The resolved model is served by a managed-multiplexer backend whose
  /// umbrella isn't running. Emitted for such a row when no umbrella is
  /// registered/Ready — a clean 503 instead of routing a GGUF-shaped
  /// auto-start that would fail on the missing local file.
  BackendUnavailable {
    backend: String,
    requested_model: String,
  },
}

/// Errors raised before [`decide`] returns — these escape the
/// forwarding-plan layer and propagate to the per-request HTTP
/// status mapping in [`super::router`].
#[derive(Debug)]
pub(crate) enum BodyError {
  /// Body exceeded the configured cap. HTTP 413.
  TooLarge,
  /// Body wasn't valid JSON or `body.model` wasn't a string.
  /// HTTP 400 `invalid_request`.
  Malformed { message: String },
  /// hyper choked reading the request body off the wire. Surface as
  /// HTTP 400; client-side framing is broken either way.
  Read { message: String },
}

/// Minimal-shape body parse: serde ignores all fields it doesn't
/// know about, so anything beyond `model` is preserved in the
/// buffered bytes we forward upstream unchanged.
#[derive(serde::Deserialize)]
struct JustModel {
  #[serde(default)]
  model: Option<String>,
}

/// Outcome of buffering + extracting. `bytes` is the full body
/// (capped at the daemon's limit, [`DEFAULT_BODY_LIMIT_BYTES`] by
/// default); we forward these verbatim, so no re-encoding ever
/// happens after this point.
pub(crate) struct ParsedBody {
  pub bytes: Bytes,
  pub model: Option<String>,
}

/// Drain the inbound body under `cap`, then peek the `model` field
/// with a single tolerant parse. The body bytes are kept as-is for
/// verbatim forwarding.
pub(crate) async fn buffer_and_extract(
  body: Incoming,
  cap: usize,
) -> Result<ParsedBody, BodyError> {
  let bytes = buffer_body(body, cap).await?;

  // An empty body is allowed in principle — `model` extraction
  // then returns None and the caller emits `model_required`.
  let model = if bytes.is_empty() {
    None
  } else {
    match serde_json::from_slice::<JustModel>(&bytes) {
      Ok(parsed) => parsed
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      Err(err) => {
        return Err(BodyError::Malformed {
          message: format!("request body is not valid JSON: {err}"),
        });
      }
    }
  };

  Ok(ParsedBody { bytes, model })
}

/// Drain an inbound body under `cap`, distinguishing the cap-overflow
/// (413) case from a read failure (400). `http-body-util::Limited`
/// wraps the inner error in a `Box<dyn Error>`; the overflow case is
/// exposed as `LengthLimitError`.
pub(crate) async fn buffer_body(body: Incoming, cap: usize) -> Result<Bytes, BodyError> {
  // A cap of 0 disables the check: `max_body_size: 0` means no cap
  // (like `idle_ttl_secs: 0` and nginx's `client_max_body_size 0`),
  // so the body is collected unwrapped rather than under `Limited`.
  if cap == 0 {
    return body
      .collect()
      .await
      .map(|c| c.to_bytes())
      .map_err(|err| BodyError::Read {
        message: format!("failed to read request body: {err}"),
      });
  }
  match Limited::new(body, cap).collect().await {
    Ok(c) => Ok(c.to_bytes()),
    Err(err) => {
      if err
        .downcast_ref::<http_body_util::LengthLimitError>()
        .is_some()
      {
        Err(BodyError::TooLarge)
      } else {
        Err(BodyError::Read {
          message: format!("failed to read request body: {err}"),
        })
      }
    }
  }
}

/// Map a body-buffering [`BodyError`] to its proxy error response, so
/// every surface that drains a request body (data plane + `/ui`) emits
/// the same status + message for an oversized / unreadable / malformed
/// payload. `cap` is the in-force limit; the 413 message names it twice
/// — a rounded human unit (`fmt_bytes` renders 2 MiB − 1 as `2.0 MiB`)
/// plus the exact byte count. The human half still rounds at unit
/// boundaries, so the raw count is the half that keeps a cap just under
/// a boundary honest; it is not a redundant echo of it.
pub(crate) fn body_error_response(err: BodyError, cap: usize) -> ProxyResponse {
  use hyper::StatusCode;
  match err {
    BodyError::TooLarge => super::router::error_response(
      StatusCode::PAYLOAD_TOO_LARGE,
      "payload_too_large",
      &format!(
        "request body exceeds the {} ({} bytes) limit",
        crate::init::detection::fmt_bytes(cap as u64),
        cap
      ),
    ),
    BodyError::Malformed { message } | BodyError::Read { message } => {
      super::router::error_response(StatusCode::BAD_REQUEST, "invalid_request", &message)
    }
  }
}

/// Build a [`RouteDecision`] from the parsed body. Does no I/O
/// beyond reading shared snapshots (catalog, supervisors). The
/// forwarding decision is pure — the side-effecting forward call
/// lives in [`super::forward`].
pub(crate) async fn decide(state: &Arc<ProxyState>, body_model: Option<String>) -> RouteDecision {
  let requested = match body_model {
    Some(m) if !m.is_empty() => m,
    _ => return RouteDecision::ModelRequired,
  };

  // Catalog snapshot → CatalogRow vec (the resolver speaks
  // `&[CatalogRow]`). Built in-process here because the existing
  // `cli::resolve::fetch_catalog` round-trips through IPC, which
  // we explicitly want to avoid on the hot path.
  let snap = state.ctx.catalog.snapshot().await;
  let rows: Vec<CatalogRow> = snap.iter().map(catalog_row_from_discovered).collect();
  let resolved = match resolve_model_with_candidates(&rows, &requested) {
    Ok(r) => r,
    Err(ResolveError::Empty) | Err(ResolveError::None) => {
      return RouteDecision::NotFound {
        requested_model: requested,
      };
    }
    Err(ResolveError::Many(candidates)) => {
      // The ids `/v1/models` publishes, not the bare names: two same-named
      // GGUFs in different roots listed the identical string twice, leaving
      // the client nothing to refine with. Every entry here routes.
      return RouteDecision::Ambiguous {
        requested_model: requested,
        candidates: crate::launch::resolve::published_ids_for(&rows, &candidates),
      };
    }
  };

  // Managed-multiplexer models are served by a shared umbrella process, not a
  // per-model supervisor: route them to the umbrella's port. Handled before
  // the GGUF supervisor walk because such a row has no local file for the
  // path-match (or the GGUF auto-start) to key on.
  if crate::discovery::ModelSource::from_label(&resolved.source)
    .is_some_and(|s| crate::backend::is_managed_multiplexer(s.backend_id()))
  {
    return decide_umbrella_route(state, requested, &resolved).await;
  }

  // Walk the supervisor snapshot for a Ready entry serving the
  // resolved row's path. Two HashMap lookups + one state read each
  // — well within the hot-path budget the plan asks for.
  let sup_snap = state.ctx.supervisors.snapshot().await;
  for (_launch_id, model) in sup_snap.into_iter() {
    if !same_path(&model.id().path, &resolved.path) {
      continue;
    }
    if matches!(model.state().await, ManagedState::Ready) {
      return RouteDecision::ReadyAt {
        port: model.port(),
        served_model_id: served_name_for_row(&resolved),
        served_model_key: model.id().clone(),
        upstream_path_prefix: None,
        fallback: false,
        fallback_reason: None,
      };
    }
  }

  // Catalog matched but no supervisor is in Ready state — dispatch
  // into the auto-start + single-flight + family-MRU-fallback flow
  // implemented by `route::handle_not_running`.
  let arch = resolved.arch.clone();
  RouteDecision::NotRunning {
    requested_model: requested,
    resolved_row: Box::new(resolved),
    arch,
  }
}

/// Routing decision for a managed-multiplexer-backed model. A managed
/// multiplexer serves every one of its models from one shared umbrella
/// process, so this forwards to the umbrella's port when it is registered +
/// Ready, and returns [`RouteDecision::BackendUnavailable`] otherwise. The
/// umbrella's chat endpoint autoloads, so no per-model launch is needed on the
/// hot path. Resolves the owning backend from the source and reads its
/// [`Backend::umbrella_launch_id`] — names no backend.
async fn decide_umbrella_route(
  state: &Arc<ProxyState>,
  requested: String,
  resolved: &CatalogRow,
) -> RouteDecision {
  use crate::backend::{Backend, Backends};
  let backend_id =
    crate::discovery::ModelSource::from_label(&resolved.source).map(|s| s.backend_id());
  let backend = backend_id.and_then(|id| Backends::all().into_iter().find(|b| b.id() == id));
  let umbrella_id = backend.as_ref().and_then(|b| b.umbrella_launch_id());
  let umbrella = match umbrella_id {
    Some(id) => state.ctx.supervisors.get(&id).await,
    None => None,
  };
  match umbrella {
    Some(umbrella) if matches!(umbrella.state().await, ManagedState::Ready) => {
      RouteDecision::ReadyAt {
        port: umbrella.port(),
        served_model_id: served_name_for_row(resolved),
        // The umbrella's own ModelId — the forward path re-verifies and
        // takes an inflight guard against this supervisor entry.
        served_model_key: umbrella.id().clone(),
        // The OpenAI path prefix the owning backend's umbrella serves under
        // (its `umbrella_openai_prefix`), so this arm names no backend.
        upstream_path_prefix: backend
          .as_ref()
          .and_then(|b| b.umbrella_openai_prefix())
          .map(str::to_string),
        fallback: false,
        fallback_reason: None,
      }
    }
    _ => RouteDecision::BackendUnavailable {
      backend: backend_id
        .unwrap_or(crate::backend::DEFAULT_BACKEND_ID)
        .to_string(),
      requested_model: requested,
    },
  }
}

/// Project a discovered-model entry onto the `CatalogRow` shape the
/// resolver expects. In-process equivalent of
/// `cli::resolve::parse_catalog_row` (which goes through the JSON
/// wire); kept here so the proxy doesn't pay a serialize/deserialize
/// round-trip on the hot path.
/// The backend a **running** model (keyed by its [`ModelId`]) is actually
/// served by, or `None` for the default (llama.cpp) backend. Prefers the
/// `resolved_backend` tag stamped on `last_params` — the launch's real backend,
/// honoring an explicit `--backend` override — and falls back to the model's
/// catalog routing verdict (when that backend is available) for a model with no
/// tag yet (an adopted row, or the brief window before the recorder stamps).
/// Used by `/ui` ([`Backend::serves_web_ui`]) and the mode guard
/// ([`Backend::serves_mode`]) so both act on the true backend, not a
/// prediction. Names no backend.
pub(crate) async fn running_model_backend(
  state: &Arc<ProxyState>,
  id: &ModelId,
) -> Option<crate::backend::Backends> {
  use crate::backend::{Backend, Backends};
  let ds_snap = state.ctx.state.snapshot().await;
  if let Some(tag) = ds_snap
    .last_params
    .iter()
    .find(|e| e.id.as_gguf().map(|g| g.path == id.path).unwrap_or(false))
    .map(|e| e.resolved_backend.clone())
  {
    return Backends::all().into_iter().find(|b| b.id() == tag);
  }
  let cat = state.ctx.catalog.snapshot().await;
  let rb = cat
    .iter()
    .find(|m| m.path == id.path)
    .and_then(|m| m.default_backend().map(str::to_string))?;
  Backends::all()
    .into_iter()
    .find(|b| b.id() == rb && b.available(&state.ctx))
}

/// The backend an `Auto` launch of `row` would land on — resolved through the
/// same [`crate::backend::resolve_backend_for_launch`] seam the daemon's
/// `start_model` uses, with the catalog mode hint as the launch mode. Used by
/// the not-running arm of the mode guard: a compatible model launched for a
/// `/v1/embeddings` request would auto-start in chat mode on its routed backend,
/// then that backend's own error surfaces after the load — the guard refuses
/// first (see [`Backend::serves_mode`]). Names no backend.
pub(crate) async fn would_route_backend(
  state: &Arc<ProxyState>,
  row: &CatalogRow,
) -> Option<crate::backend::Backends> {
  let cat = state.ctx.catalog.snapshot().await;
  let m = cat.iter().find(|m| same_path(&m.path, &row.path))?;
  let launch_mode = match row.mode_hint.as_deref() {
    Some("embedding") => crate::launch::mode::LaunchMode::Embedding,
    Some("rerank") => crate::launch::mode::LaunchMode::Rerank,
    _ => crate::launch::mode::LaunchMode::Chat,
  };
  // A GGUF identity: the fallback is the default backend, and the priority-
  // ordered supported list drives the auto pick — exactly the daemon's rule.
  let identity = crate::backend::identity::ModelIdentity::Gguf(crate::gguf::identity::ModelId {
    path: m.path.clone(),
    header_blake3: [0u8; 32],
  });
  Some(crate::backend::resolve_backend_for_launch(
    &identity,
    crate::launch::params::BackendChoice::Auto,
    &m.supported_backends,
    launch_mode,
    &state.ctx,
  ))
}

pub(crate) fn catalog_row_from_discovered(m: &DiscoveredModel) -> CatalogRow {
  let path = m.path.to_string_lossy().into_owned();
  let parent = m.parent.to_string_lossy().into_owned();
  let arch = m.metadata.as_ref().and_then(|md| md.arch.clone());
  let quant = m.metadata.as_ref().map(|md| md.quant_display());
  let native_ctx = m.metadata.as_ref().and_then(|md| md.native_ctx);
  let parameter_label = m
    .metadata
    .as_ref()
    .and_then(|md| md.parameter_label.clone());
  let weights_bytes = m.metadata.as_ref().and_then(|md| md.weights_bytes);
  let has_chat_template = m
    .metadata
    .as_ref()
    .map(|md| md.chat_template.is_some())
    .unwrap_or(false);
  let has_reasoning_hint = m
    .metadata
    .as_ref()
    .map(|md| md.reasoning_hint)
    .unwrap_or(false);
  let tokenizer_kind = m.metadata.as_ref().and_then(|md| md.tokenizer_kind.clone());
  let total_parameters = m.metadata.as_ref().and_then(|md| md.total_parameters);
  // Surface the GGUF-derived mode hint to the proxy so auto-start
  // composes the right `llama-server` argv (embedding / rerank
  // builds need the `--embeddings` / `--rerank` flag set up front;
  // chat builds need it absent). Without this the proxy defaulted
  // every auto-start to chat mode and any `POST /v1/embeddings`
  // call against an embedding-only model returned a 501 from
  // `llama-server` ("This server does not support embeddings").
  let mode_hint = m
    .metadata
    .as_ref()
    .and_then(|md| md.mode_hint.as_label())
    .map(str::to_string);
  CatalogRow {
    path,
    model_id: None,
    parent,
    source: m.source.label().to_string(),
    arch,
    quant,
    native_ctx,
    mode_hint,
    parameter_label,
    weights_bytes,
    display_label: m.display_label.clone(),
    parse_error: m.parse_error.clone(),
    split_siblings: m
      .split_siblings
      .iter()
      .map(|p| p.to_string_lossy().into_owned())
      .collect(),
    has_chat_template,
    has_reasoning_hint,
    tokenizer_kind,
    total_parameters,
    backend: None,
    supported_backends: m.supported_backends.clone(),
    multimodal: m.multimodal,
    mtp: m
      .mtp_capable()
      .then(|| crate::launch::resolve::MtpCapability {
        embedded_layers: m.metadata.as_ref().and_then(|md| md.mtp),
        separate_head: m.mtp_head.is_some(),
      }),
  }
}

/// Compare a `ModelId::path` (PathBuf) with a `CatalogRow::path`
/// (String). The catalog row is built from the discovered path's
/// `to_string_lossy()` view, and `ModelId::path` is canonical too —
/// equality is exact in production.
fn same_path(model_id_path: &std::path::Path, row_path: &str) -> bool {
  model_id_path.to_string_lossy() == row_path
}

/// Auto-start entry point — invoked from `router.rs` when a request
/// hits a catalog row whose model isn't currently Ready.
///
/// Drives:
///   1. Auto-start via [`launch::auto_start`] (single-flight
///      coalesced; waits for Ready or terminal Error).
///   2. On Ready → MRU touch + forward (no fallback headers).
///   3. On Error → pick a family-MRU fallback and forward with
///      `x-llamastash-served-by` + `x-llamastash-fallback-reason`.
///      Reason is `launch_failed` when the pick shares the requested
///      arch (graceful in-family substitution) and `family_mismatch`
///      when the pick is from a different arch (e.g. an embedding
///      request fell through to a chat model — the upstream output
///      will not match request semantics, and clients that care
///      should branch on the header). If no Ready candidate exists
///      → 503 `launch_failed` with the running list.
///
/// `endpoint_mode` carries the mode the requested endpoint implies so
/// the auto-start composes an argv that can answer it (see
/// [`launch::auto_start`]).
pub(crate) async fn handle_not_running(
  state: &Arc<ProxyState>,
  inbound: super::forward::InboundRequest,
  requested_model: String,
  resolved_row: CatalogRow,
  requested_arch: Option<String>,
  endpoint_mode: Option<crate::launch::mode::LaunchMode>,
) -> ProxyResponse {
  let outcome = launch::auto_start(state, &resolved_row, endpoint_mode).await;
  match outcome {
    LaunchOutcome::Ready { port, model_id } => {
      // Touch the MRU using the supervisor we just confirmed Ready.
      // The display name for the response header on the happy path
      // is the requested model name — no fallback, no surprise.
      state.mru.touch(&model_id).await;
      let served = served_name_for_row(&resolved_row);
      super::forward::forward_to_upstream(
        state,
        inbound,
        super::forward::Target {
          port,
          served_model_id: &served,
          served_model_key: &model_id,
          upstream_path_prefix: None,
          fallback: false,
          fallback_reason: None,
        },
      )
      .await
    }
    LaunchOutcome::Failed { cause } => {
      // Operator can disable the family-MRU fallback entirely (see
      // `ProxyConfig::fallback_enabled` and the `--no-proxy-fallback`
      // CLI / `LLAMASTASH_NO_PROXY_FALLBACK` env overrides). When
      // off, a launch failure flows straight to the 503
      // `launch_failed` response below — clients never silently get a
      // payload from a different model.
      if !state.fallback_enabled {
        return launch_failed_response(&cause, &requested_model);
      }
      // Family-MRU fallback. Walk the supervisor snapshot, filter
      // to Ready, attach each entry's catalog arch + MRU
      // timestamp, then defer to `pick_fallback` for the policy.
      let candidates = collect_fallback_candidates(state).await;
      if let Some(pick) = pick_fallback(candidates, requested_arch.as_deref()) {
        state.mru.touch(&pick.model_id).await;
        let reason = fallback_reason_for(requested_arch.as_deref(), pick.arch.as_deref());
        return super::forward::forward_to_upstream(
          state,
          inbound,
          super::forward::Target {
            port: pick.port,
            served_model_id: &pick.served_model_id,
            served_model_key: &pick.model_id,
            upstream_path_prefix: None,
            fallback: true,
            fallback_reason: Some(reason),
          },
        )
        .await;
      }
      // No Ready model to fall back to. R155 mandates a 503 with
      // the `running: []` list inline (empty by construction here:
      // `pick_fallback` only returns None when zero Ready candidates
      // exist). Drop the requested model name into the message so
      // logs surface what was being attempted.
      launch_failed_response(&cause, &requested_model)
    }
  }
}

/// Pick the `x-llamastash-fallback-reason` value based on whether the
/// picked candidate is in the same arch family as the failed request.
///
/// - `launch_failed`: in-family substitution. Same arch (or both
///   sides have no arch metadata) — the upstream output shape is
///   what the client asked for.
/// - `family_mismatch`: cross-arch fallback. The picked supervisor's
///   arch differs from the requested arch, or one side is missing
///   metadata. The upstream output shape is *not* what the client
///   asked for (e.g. an embedding request answered by a chat model).
///   Clients that don't read response headers will see surprising
///   payloads; this is the header that lets them branch.
fn fallback_reason_for(requested: Option<&str>, picked: Option<&str>) -> &'static str {
  match (requested, picked) {
    (Some(a), Some(b)) if a == b => "launch_failed",
    (None, None) => "launch_failed",
    _ => "family_mismatch",
  }
}

/// Build the 503 `launch_failed` envelope used by the fallback path
/// when no Ready model is available. The `running: []` field is
/// always present and always empty here by construction —
/// this helper is only reached when `pick_fallback` returned None,
/// which means zero Ready supervisors existed. The message surfaces
/// the supervisor's `cause` so clients see *why* the launch failed.
pub(crate) fn launch_failed_response(cause: &str, requested_model: &str) -> ProxyResponse {
  use super::openai::{ErrorObject, ErrorResponse};

  let message =
    format!("auto-start of `{requested_model}` failed and no running model is available: {cause}");
  let error = ErrorObject::new("launch_failed", message).with_running(Vec::<String>::new());
  let bytes = serde_json::to_vec(&ErrorResponse { error }).expect("json encoding of fixed shape");
  Ok(super::router::json_response(
    hyper::StatusCode::SERVICE_UNAVAILABLE,
    bytes,
  ))
}

/// Resolve a display name for a CatalogRow that mirrors what
/// `/v1/models` returns. `display_label` (Ollama's `<name>:<tag>`)
/// wins when present; otherwise fall back to the file stem via
/// [`crate::util::paths::model_display_name`] so the value of
/// `x-llamastash-served-by` is byte-equal to the corresponding
/// `/v1/models` `id` for the same model (closes R-11).
fn served_name_for_row(row: &CatalogRow) -> String {
  if let Some(label) = &row.display_label {
    return label.clone();
  }
  crate::util::paths::model_display_name(std::path::Path::new(&row.path))
}

/// Index a catalog snapshot by canonical path string for O(1) metadata
/// lookup keyed off a supervisor's `ModelId::path`. The catalog is small
/// (tens to hundreds of rows), so the build is cheap.
pub(crate) fn index_catalog_by_path(
  catalog: &[DiscoveredModel],
) -> std::collections::HashMap<String, &DiscoveredModel> {
  let mut by_path = std::collections::HashMap::with_capacity(catalog.len());
  for m in catalog.iter() {
    by_path.insert(m.path.to_string_lossy().into_owned(), m);
  }
  by_path
}

/// Build the candidate list the fallback selector picks from. Reads
/// the supervisor snapshot (filtering to Ready), joins each row
/// against the catalog snapshot to find the matching `arch`, and
/// stamps the latest MRU timestamp.
async fn collect_fallback_candidates(state: &Arc<ProxyState>) -> Vec<FallbackCandidate> {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  // Index the catalog by canonical path so each supervisor entry can
  // attach arch + display label without re-walking the catalog.
  let cat_snap = state.ctx.catalog.snapshot().await;
  let by_path = index_catalog_by_path(&cat_snap);

  let mut out: Vec<FallbackCandidate> = Vec::with_capacity(sup_snap.len());
  for (launch_id, model) in sup_snap.into_iter() {
    // An infrastructure launch (a managed-multiplexer umbrella) is a supervised
    // process, not a servable model — never offer it as a family-MRU fallback
    // (a bare `/v1` forward to a multiplexer's own API 404s anyway).
    if crate::backend::is_infra_launch(&launch_id) {
      continue;
    }
    if !matches!(model.state().await, ManagedState::Ready) {
      continue;
    }
    let id = model.id().clone();
    let path_key = id.path.to_string_lossy().into_owned();
    let catalog_entry = by_path.get(&path_key);
    let arch = catalog_entry
      .and_then(|m| m.metadata.as_ref())
      .and_then(|md| md.arch.clone());
    let served_model_id = catalog_entry
      .and_then(|m| m.display_label.clone())
      .unwrap_or_else(|| crate::util::paths::model_display_name(&id.path));
    let last_request_at = state.mru.last_request_at(&id).await;
    out.push(FallbackCandidate {
      model_id: id,
      arch,
      last_request_at,
      port: model.port(),
      served_model_id,
    });
  }
  out
}

// Matcher behaviour is covered by `cli::resolve` unit tests, which
// exercise `resolve_model_with_candidates` directly. End-to-end
// ambiguity / not_found handling (proxy → 400 / 404) is covered by
// `tests/proxy_routing.rs`.

#[cfg(test)]
mod tests {
  use super::fallback_reason_for;

  // ─── 413 envelope ──────────────────────────────────────────────
  //
  // The 413 message names the in-force cap twice: rounded human unit
  // plus exact byte count. The human half still rounds at unit
  // boundaries (`fmt_bytes`), so the raw count is what keeps any
  // configured value — including a cap just under a boundary — honest.

  #[tokio::test]
  async fn too_large_message_names_the_configured_cap() {
    use http_body_util::BodyExt;
    let resp = super::body_error_response(super::BodyError::TooLarge, 512 * 1024)
      .expect("error response must build");
    assert_eq!(resp.status(), hyper::StatusCode::PAYLOAD_TOO_LARGE);
    let collected = resp.into_body().collect().await.expect("body").to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&collected).expect("json envelope");
    assert_eq!(v["error"]["type"], "payload_too_large");
    let message = v["error"]["message"].as_str().expect("message");
    assert!(
      message.contains("512 KiB"),
      "message must name the configured cap, got: {message}"
    );
    assert!(
      message.contains("524288 bytes"),
      "message must carry the exact byte count, got: {message}"
    );
  }

  #[tokio::test]
  async fn too_large_message_keeps_a_sub_boundary_cap_honest() {
    use http_body_util::BodyExt;
    // 2 MiB − 1: `fmt_bytes` renders the human half as `2.0 MiB`, so
    // without the exact byte count a client sending exactly 2 MiB is
    // told it "exceeds the 2.0 MiB limit" with no way to tell where the
    // real boundary is. Assert the whole message: swap `fmt_bytes` for
    // a formatter that renders `2 MiB` (or drop the raw count) and
    // this fails, which is the regression it exists to catch.
    let cap = 2 * 1024 * 1024 - 1;
    let resp = super::body_error_response(super::BodyError::TooLarge, cap)
      .expect("error response must build");
    let collected = resp.into_body().collect().await.expect("body").to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&collected).expect("json envelope");
    let message = v["error"]["message"].as_str().expect("message");
    assert_eq!(
      message, "request body exceeds the 2.0 MiB (2097151 bytes) limit",
      "the whole message pins both halves — rounded human unit and exact byte count"
    );
  }

  // The four arms of `fallback_reason_for` decide whether the
  // x-llamastash-fallback-reason header reads "launch_failed" (the
  // picked model is in the same family as what the client asked for
  // — output shape parity holds) or "family_mismatch" (cross-arch
  // pick — output shape parity does not hold). Embedding/rerank
  // clients branch on this header, so the four cases need explicit
  // coverage rather than relying on the one integration test that
  // happens to hit the `(None, Some)` branch.

  #[test]
  fn same_arch_on_both_sides_is_launch_failed() {
    assert_eq!(
      fallback_reason_for(Some("llama"), Some("llama")),
      "launch_failed"
    );
  }

  #[test]
  fn no_arch_on_either_side_is_launch_failed() {
    assert_eq!(fallback_reason_for(None, None), "launch_failed");
  }

  #[test]
  fn requested_arch_missing_is_family_mismatch() {
    assert_eq!(fallback_reason_for(None, Some("llama")), "family_mismatch");
  }

  #[test]
  fn picked_arch_missing_is_family_mismatch() {
    assert_eq!(fallback_reason_for(Some("llama"), None), "family_mismatch");
  }

  #[test]
  fn different_arches_is_family_mismatch() {
    assert_eq!(
      fallback_reason_for(Some("bert"), Some("llama")),
      "family_mismatch"
    );
  }

  // ─── Mode-hint propagation into the proxy CatalogRow ────────────
  //
  // Regression cover for the bug where a `POST /v1/embeddings`
  // against an embedding-only model that wasn't already running
  // returned a 501 from `llama-server` — the proxy auto-start dropped
  // the GGUF-derived mode hint and the supervisor defaulted to chat
  // mode (no `--embeddings` flag in the composed argv).
  #[allow(unused_imports)]
  use super::catalog_row_from_discovered;
  use crate::discovery::DiscoveredModel;
  use crate::gguf::metadata::{ModeHint, ModelMetadata};

  fn discovered_with_mode(mode: ModeHint) -> DiscoveredModel {
    DiscoveredModel {
      path: std::path::PathBuf::from("/tmp/fake.gguf"),
      parent: std::path::PathBuf::from("/tmp"),
      source: crate::discovery::ModelSource::HuggingFace,
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
      parse_error: None,
      split_siblings: vec![],
      metadata: Some(ModelMetadata {
        arch: Some("nomic-bert".into()),
        quant: crate::gguf::metadata::Quant::Q2_K,
        quant_label: None,
        native_ctx: Some(2048),
        parameter_label: Some("0.5B".into()),
        weights_bytes: Some(100_000_000),
        mtp: None,
        chat_template: None,
        tokenizer_kind: Some("bert".into()),
        total_parameters: Some(500_000_000),
        reasoning_hint: false,
        mode_hint: mode,
      }),
    }
  }

  #[test]
  fn catalog_row_propagates_embedding_mode_hint_to_proxy() {
    let m = discovered_with_mode(ModeHint::Embedding);
    let row = catalog_row_from_discovered(&m);
    assert_eq!(
      row.mode_hint.as_deref(),
      Some("embedding"),
      "proxy auto-start needs embedding hint to add --embeddings"
    );
  }

  #[tokio::test]
  async fn mode_guard_refuses_embeddings_to_chat_only_backend() {
    use crate::backend::Backend;
    use crate::daemon::context::MethodContext;
    use crate::daemon::shutdown::ShutdownToken;
    use crate::discovery::ModelCatalog;
    use crate::launch::mode::LaunchMode;

    // A chat-only backend available on this host (ds4 is the real one; any
    // existing file satisfies its binary resolver). The guard is generic —
    // asserted via `serves_mode`, not by naming the backend.
    let exe = std::env::current_exe().unwrap();
    let ds4_cfg = crate::config::Ds4Config {
      enabled: Some(true),
      servers: vec![crate::backend::ServerConfig {
        binary: exe,
        name: None,
      }],
    };

    // A chat model routed to that backend → auto-start lands on a backend that
    // doesn't serve embeddings, so an embeddings/rerank request must be refused.
    let mut chat = discovered_with_mode(ModeHint::Chat);
    chat.path = std::path::PathBuf::from("/m/deepseek.gguf");
    chat.supported_backends = vec![
      crate::backend::ds4::DS4_BACKEND_ID.to_string(),
      crate::backend::DEFAULT_BACKEND_ID.to_string(),
    ];
    let chat_row = catalog_row_from_discovered(&chat);

    // A compatible-but-embedding-hint model routes to the default backend
    // (which serves embeddings), so it must NOT be guarded.
    let mut embed = discovered_with_mode(ModeHint::Embedding);
    embed.path = std::path::PathBuf::from("/m/embed.gguf");
    embed.supported_backends = vec![
      crate::backend::ds4::DS4_BACKEND_ID.to_string(),
      crate::backend::DEFAULT_BACKEND_ID.to_string(),
    ];
    let embed_row = catalog_row_from_discovered(&embed);

    let catalog = ModelCatalog::new();
    catalog.upsert(chat).await;
    catalog.upsert(embed).await;
    let ctx = MethodContext::with_catalog(ShutdownToken::new(), catalog).with_backend(
      crate::backend::BackendConfig {
        ds4: ds4_cfg,
        ..Default::default()
      },
      std::collections::BTreeMap::new(),
    );
    let state = crate::proxy::state::ProxyState::from_context(
      &ctx,
      false,
      true,
      super::DEFAULT_BODY_LIMIT_BYTES,
    );

    let chat_backend = super::would_route_backend(&state, &chat_row)
      .await
      .expect("chat row resolves a backend");
    assert!(
      !chat_backend.serves_mode(LaunchMode::Embedding),
      "compatible chat model auto-starts on a chat-only backend → embeddings guard fires"
    );
    let embed_backend = super::would_route_backend(&state, &embed_row)
      .await
      .expect("embed row resolves a backend");
    assert!(
      embed_backend.serves_mode(LaunchMode::Embedding),
      "embedding-hint model routes to an embedding-capable backend → not guarded"
    );
  }

  #[test]
  fn catalog_row_propagates_rerank_mode_hint_to_proxy() {
    let m = discovered_with_mode(ModeHint::Rerank);
    let row = catalog_row_from_discovered(&m);
    assert_eq!(row.mode_hint.as_deref(), Some("rerank"));
  }

  #[test]
  fn catalog_row_propagates_chat_mode_hint_to_proxy() {
    let m = discovered_with_mode(ModeHint::Chat);
    let row = catalog_row_from_discovered(&m);
    assert_eq!(row.mode_hint.as_deref(), Some("chat"));
  }

  #[test]
  fn catalog_row_leaves_unknown_mode_hint_as_none() {
    // Unknown stays None so the compose_and_spawn default (chat) is
    // what kicks in — same posture as before the propagation patch
    // when the GGUF carried no signal.
    let m = discovered_with_mode(ModeHint::Unknown);
    let row = catalog_row_from_discovered(&m);
    assert_eq!(row.mode_hint, None);
  }

  // ─── served-by name resolution ──────────────────────────────────
  use super::{index_catalog_by_path, served_name_for_row};

  #[test]
  fn served_name_prefers_display_label_then_falls_back_to_stem() {
    // Ollama rows carry a human `<name>:<tag>` display_label — that wins
    // so the `x-llamastash-served-by` header is readable.
    let mut m = discovered_with_mode(ModeHint::Chat);
    m.display_label = Some("gemma3:4b".to_string());
    let labelled = catalog_row_from_discovered(&m);
    assert_eq!(served_name_for_row(&labelled), "gemma3:4b");

    // Without a display_label the served name derives from the file
    // stem, byte-equal to the `/v1/models` id for the same model.
    let mut plain = discovered_with_mode(ModeHint::Chat);
    plain.display_label = None;
    plain.path = std::path::PathBuf::from("/models/Qwen3-7B-Q4_K_M.gguf");
    let stem_row = catalog_row_from_discovered(&plain);
    assert_eq!(served_name_for_row(&stem_row), "Qwen3-7B-Q4_K_M");
  }

  #[test]
  fn index_catalog_by_path_keys_on_canonical_path_string() {
    let a = {
      let mut m = discovered_with_mode(ModeHint::Chat);
      m.path = std::path::PathBuf::from("/m/a.gguf");
      m
    };
    let b = {
      let mut m = discovered_with_mode(ModeHint::Embedding);
      m.path = std::path::PathBuf::from("/m/b.gguf");
      m
    };
    let catalog = vec![a, b];
    let index = index_catalog_by_path(&catalog);
    assert_eq!(index.len(), 2);
    assert!(index.contains_key("/m/a.gguf"));
    let hit = index.get("/m/b.gguf").expect("b indexed");
    assert_eq!(
      hit.metadata.as_ref().unwrap().mode_hint,
      ModeHint::Embedding
    );
    assert!(!index.contains_key("/m/missing.gguf"));
  }
}
