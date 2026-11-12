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

use crate::daemon::registry::LaunchId;
use crate::daemon::supervisor::ManagedState;
use crate::discovery::DiscoveredModel;
use crate::gguf::identity::ModelId;
use crate::launch::resolve::{
  parse_named_reference, resolve_model_with_candidates, CatalogRow, ResolveError,
};

use super::alias::TargetVerdict;
use super::launch::{self, LaunchOutcome};
use super::mru::{pick_fallback, FallbackCandidate};
use super::request_log::RequestRecord;
use super::router::ProxyResponse;
use super::state::ProxyState;
use crate::launch::mode::LaunchMode;

/// Default inbound body size cap. 16 MiB lets vision payloads through
/// (a base64 phone photo is ~4–11 MiB; base64 inflates the source file
/// ~33%) while still bounding worst-case memory and refusing accidental
/// uploads. Anything larger surfaces as HTTP 413 (a `BodyError::TooLarge`
/// from the `Limited` adapter). `proxy.max_body_size` overrides per daemon.
pub const DEFAULT_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// The `x-llamastash-fallback-reason` of a request that reached a model by way
/// of the endpoint's mode rather than by name.
pub(crate) const MODE_MATCH: &str = "mode_match";

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
    /// Catalog path of the model that answers. The request log files the
    /// row under it.
    model_path: String,
    /// A `proxy.aliases` entry named this model.
    alias: bool,
    fallback: bool,
    fallback_reason: Option<String>,
  },
  /// The catalog has the model but no Ready supervisor is serving it.
  /// Dispatched into `handle_not_running` which runs the auto-start +
  /// single-flight + family-MRU fallback flow. The variant carries the
  /// resolved row so the launch path doesn't repeat the lookup.
  NotRunning {
    requested_model: String,
    /// Resolved catalog entry consumed by the launch path to build
    /// `StartParams` for `compose_and_spawn` without re-running the
    /// resolver.
    // dead_code: consumed via destructuring in router::forward_request;
    // the field itself is moved out, not read by name.
    #[allow(dead_code)]
    resolved_row: Box<CatalogRow>,
    /// User-chosen launch name parsed from the `@name` suffix. `None`
    /// for unnamed launches. Threaded through to `auto_start` so a
    /// second named launch of the same model gets its own flight.
    // dead_code: consumed via destructuring in router::forward_request.
    #[allow(dead_code)]
    name: Option<String>,
    /// A `proxy.aliases` entry named this model.
    alias: bool,
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
///
/// `content_type` decides which shape to read: a `multipart/form-data` body
/// names its model in a form field, and parsing it as JSON would refuse a
/// legitimate upload with "not valid JSON".
pub(crate) async fn buffer_and_extract(
  body: Incoming,
  cap: usize,
  content_type: Option<&str>,
) -> Result<ParsedBody, BodyError> {
  let bytes = buffer_body(body, cap).await?;

  // An empty body is allowed in principle — `model` extraction
  // then returns None and the caller emits `model_required`.
  let model = match content_type {
    Some(ct) if crate::util::multipart_body::is_multipart(ct) => multipart_model(&bytes, ct),
    _ if bytes.is_empty() => None,
    _ => match serde_json::from_slice::<JustModel>(&bytes) {
      Ok(parsed) => parsed
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      Err(err) => {
        return Err(BodyError::Malformed {
          message: format!("request body is not valid JSON: {err}"),
        });
      }
    },
  };

  Ok(ParsedBody { bytes, model })
}

/// [`buffer_and_extract`] for a path a row declared as its own API. The body is
/// forwarded as it arrived, so a shape the proxy cannot read names no model
/// rather than refusing the request: an engine that owns the path decides what a
/// body means. Only the size cap can fail here.
pub(crate) async fn buffer_and_extract_tolerant(
  body: Incoming,
  cap: usize,
  content_type: Option<&str>,
) -> Result<ParsedBody, BodyError> {
  let bytes = buffer_body(body, cap).await?;
  let model = match content_type {
    Some(ct) if crate::util::multipart_body::is_multipart(ct) => multipart_model(&bytes, ct),
    _ => serde_json::from_slice::<JustModel>(&bytes)
      .ok()
      .and_then(|parsed| parsed.model)
      .map(|s| s.trim().to_string())
      .filter(|s| !s.is_empty()),
  };
  Ok(ParsedBody { bytes, model })
}

/// The `model` form field of a `multipart/form-data` body, trimmed, `None` when
/// the body names none. An upload's file part is never read.
fn multipart_model(bytes: &[u8], content_type: &str) -> Option<String> {
  crate::util::multipart_body::field_value(bytes, content_type, "model")
    .and_then(|raw| std::str::from_utf8(raw).ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// The `model` a buffered body names, or `None` for a body that is empty, is
/// not JSON, or names nothing. Unlike [`buffer_and_extract`] this never fails,
/// so a caller can label a row without deciding the body's validity — the
/// `/ui` forwarder forwards the bytes either way and lets the engine judge.
pub(crate) fn model_in_body(bytes: &[u8]) -> Option<String> {
  serde_json::from_slice::<JustModel>(bytes)
    .ok()?
    .model
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
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

  // The catalog's shared pre-built row view (the resolver speaks
  // `&[CatalogRow]`). Shared rather than rebuilt per request — a request costs one
  // refcount bump — and never fetched over IPC, which we explicitly want to avoid
  // on the hot path.
  let rows = state.ctx.catalog.shared_rows().await;

  // Reference → catalog row, in the one order that applies everywhere a client
  // names a model (see [`resolve_client_reference`]).
  let (mut name, found) = match resolve_client_reference(state, &rows, &requested) {
    Ok(v) => v,
    Err(ClientRefMiss::NotFound) => {
      return RouteDecision::NotFound {
        requested_model: requested,
      }
    }
    // The ids `/v1/models` publishes, not the bare names: two same-named
    // GGUFs in different roots listed the identical string twice, leaving
    // the client nothing to refine with. Every entry here routes.
    Err(ClientRefMiss::Ambiguous(candidates)) => {
      return RouteDecision::Ambiguous {
        requested_model: requested,
        candidates,
      }
    }
  };
  let resolved = found.row;
  let alias = found.alias;

  // Walk the supervisor snapshot for Ready entries serving the resolved row's
  // path. When a name is present we must map a launch back to its name to
  // confirm the match; read the state snapshot once (not per-iteration).
  let sup_snap = state.ctx.supervisors.snapshot().await;
  let mut state_snap = if name.is_some() {
    Some(state.ctx.state.snapshot().await)
  } else {
    None
  };
  let same_model: Vec<_> = sup_snap
    .into_iter()
    .filter(|(_, model)| same_path(&model.id().path, &resolved.path))
    .collect();
  let mut ready = Vec::new();
  for (launch_id, model) in same_model.iter().cloned() {
    // When a name is present, only a launch with that name is a match.
    if let (Some(n), Some(st)) = (&name, &state_snap) {
      if !st
        .running
        .iter()
        .any(|r| r.carries_name(Some(&launch_id), model.port(), n))
      {
        continue;
      }
    }
    if matches!(model.state().await, ManagedState::Ready) {
      ready.push((launch_id, model));
    }
  }
  if ready.len() > 1 && state_snap.is_none() {
    state_snap = Some(state.ctx.state.snapshot().await);
  }
  let running = state_snap.as_ref().map_or(&[][..], |s| &s.running[..]);
  let keys: Vec<(LaunchId, u16)> = ready.iter().map(|(id, m)| (id.clone(), m.port())).collect();
  let mut target = pick_ready_launch(&keys, running).map(|i| ready[i].1.clone());

  // A `<model>@<name>` with no launch of that name goes to the model's only
  // launch when that one is unnamed, instead of loading a second copy with the
  // same settings. A name that is one of the model's presets still auto-starts,
  // because it asks for different settings. A launch still loading is reached
  // by dropping the name: the unnamed auto-start attaches to it and waits. If
  // that load then fails, the auto-start brings up an unnamed copy, not `@<name>`.
  if target.is_none() {
    if let (Some(n), Some(st), [(launch_id, model)]) = (&name, &state_snap, &same_model[..]) {
      let model_state = model.state().await;
      if !is_named(&st.running, launch_id, model.port())
        && matches!(
          model_state,
          ManagedState::Launching | ManagedState::Loading | ManagedState::Ready
        )
        && crate::daemon::launch_service::model_presets(
          &state.ctx,
          std::path::Path::new(&resolved.path),
          resolved.arch.as_deref(),
        )
        .await
        .named(n)
        .is_none()
      {
        if model_state == ManagedState::Ready {
          target = Some(model.clone());
        } else {
          name = None;
        }
      }
    }
  }

  if let Some(model) = target {
    return RouteDecision::ReadyAt {
      port: model.port(),
      served_model_id: served_name_for_row(&resolved),
      served_model_key: model.id().clone(),
      model_path: resolved.path.clone(),
      alias,
      fallback: false,
      fallback_reason: None,
    };
  }

  // Catalog matched but no supervisor is in Ready state — dispatch
  // into the auto-start + single-flight + family-MRU-fallback flow
  // implemented by `route::handle_not_running`.
  RouteDecision::NotRunning {
    requested_model: requested,
    resolved_row: Box::new(resolved),
    name,
    alias,
  }
}

/// The row a media endpoint falls back to when the client's name settled
/// nothing: the only catalog row whose entry declares the endpoint's mode, or,
/// when several declare it, the only one of those that is Ready. A miss says what
/// was found, so the caller's refusal can name it.
///
/// This exists because clients hardcode names (`whisper-1`, `tts-1`) that no
/// model of ours answers to, and one declared entry per mode leaves no real
/// ambiguity. A row that declares no mode is not a candidate: it would answer
/// anything, so picking it is a guess at what the client meant.
pub(crate) async fn decide_by_mode(state: &Arc<ProxyState>, mode: LaunchMode) -> ModePick {
  use crate::backend::Backend as _;
  let rows = state.ctx.catalog.shared_rows().await;
  let mut declared: Vec<CatalogRow> = Vec::new();
  for row in rows.iter() {
    let Some(backend) = would_route_backend(state, row).await else {
      continue;
    };
    if backend.declares_mode(std::path::Path::new(&row.path), None, mode) {
      declared.push(row.clone());
    }
  }
  if declared.is_empty() {
    return ModePick::Miss(ModeMiss::NoDeclarer);
  }
  let sup_snap = state.ctx.supervisors.snapshot().await;
  let mut ready: Vec<usize> = Vec::new();
  for (i, row) in declared.iter().enumerate() {
    if ready_launch(&sup_snap, &row.path).await.is_some() {
      ready.push(i);
    }
  }
  let pick = match (declared.len(), ready.len()) {
    (1, _) => 0,
    (_, 1) => ready[0],
    (n, 0) => return ModePick::Miss(ModeMiss::NoneRunning(n)),
    (n, r) => {
      return ModePick::Miss(ModeMiss::SeveralReady {
        declarers: n,
        ready: r,
      })
    }
  };
  let row = declared.remove(pick);
  let served_model_id = served_name_for_row(&row);
  ModePick::Hit(match ready_launch(&sup_snap, &row.path).await {
    Some((port, served_model_key)) => RouteDecision::ReadyAt {
      port,
      served_model_id,
      served_model_key,
      model_path: row.path.clone(),
      alias: false,
      fallback: true,
      fallback_reason: Some(MODE_MATCH.to_string()),
    },
    None => RouteDecision::NotRunning {
      requested_model: served_model_id,
      resolved_row: Box::new(row),
      name: None,
      alias: false,
    },
  })
}

/// What the mode fallback settled on, or why it could not.
pub(crate) enum ModePick {
  Hit(RouteDecision),
  Miss(ModeMiss),
}

/// Why the mode fallback left the proxy's own refusal standing, so the refusal
/// can say what was found instead of guessing.
pub(crate) enum ModeMiss {
  /// Nothing claims the route's mode at all.
  NoDeclarer,
  /// `declarers` entries claim it and every one is stopped.
  NoneRunning(usize),
  /// `ready` of `declarers` entries claim it are running, so the name has to
  /// choose and the client named none that matches.
  SeveralReady { declarers: usize, ready: usize },
}

/// The prefix a catalog row declares for `request_path`, or `None` when no row
/// publishes it. The proxy logs this instead of the client's path, so what lands
/// in the request log is a name the config chose. The proxy asks it before
/// answering 404, so an unlisted path costs one row scan and nothing more.
pub(crate) async fn declared_prefix(state: &Arc<ProxyState>, request_path: &str) -> Option<String> {
  declaring_rows(state, request_path)
    .await
    .into_iter()
    .map(|(prefix, _)| prefix)
    .max_by_key(|p| p.len())
}

/// The rows that declare `request_path`, each with the prefix of its own that
/// covers it. Config load refuses two entries claiming one path, so everything
/// here belongs to one entry.
async fn declaring_rows(state: &Arc<ProxyState>, request_path: &str) -> Vec<(String, CatalogRow)> {
  use crate::backend::Backend as _;
  // Only a backend that can declare paths is asked, and no row is resolved to a
  // backend first: a request for a path nobody declared costs a config flag and a
  // row scan, not an identity probe per row. Unlisted paths are what scanners and
  // mistyped URLs send.
  let declarers = crate::backend::Backends::path_declarers();
  if declarers.is_empty() {
    return Vec::new();
  }
  let rows = state.ctx.catalog.shared_rows().await;
  let mut hits = Vec::new();
  for row in rows.iter() {
    let path = std::path::Path::new(&row.path);
    if let Some(prefix) = declarers
      .iter()
      .find_map(|b| b.declared_path_prefix(path, None, request_path))
    {
      hits.push((prefix, row.clone()));
    }
  }
  hits
}

/// A request on a path a row declares for itself. This is not a model route: the
/// body's name settles nothing on its own, so the answer is the entry that
/// published the path — a Ready launch of it when one is up, its one row when it
/// has exactly one, and its row list as `ambiguous_model` when it has several and
/// none is up. `None` is the proxy's own 404: nothing publishes the path. The
/// body is forwarded as it arrived — an engine that owns the path owns the name
/// inside it too.
pub(crate) async fn decide_declared(
  state: &Arc<ProxyState>,
  request_path: &str,
) -> Option<RouteDecision> {
  let mut hits = declaring_rows(state, request_path).await;
  if hits.is_empty() {
    return None;
  }
  let sup_snap = state.ctx.supervisors.snapshot().await;
  for (_, row) in hits.iter() {
    if let Some((port, served_model_key)) = ready_launch(&sup_snap, &row.path).await {
      return Some(RouteDecision::ReadyAt {
        port,
        served_model_id: served_name_for_row(row),
        served_model_key,
        model_path: row.path.clone(),
        alias: false,
        fallback: false,
        fallback_reason: None,
      });
    }
  }
  if hits.len() == 1 {
    let (_, row) = hits.remove(0);
    let served_model_id = served_name_for_row(&row);
    return Some(RouteDecision::NotRunning {
      requested_model: served_model_id,
      resolved_row: Box::new(row),
      name: None,
      alias: false,
    });
  }
  Some(RouteDecision::Ambiguous {
    requested_model: request_path.to_string(),
    candidates: hits.iter().map(|(_, r)| served_name_for_row(r)).collect(),
  })
}

/// The Ready launch that serves the catalog row at `path`, newest first. Named
/// launches of the row are not singled out: a media client sends no `@<name>`.
async fn ready_launch(
  snap: &[(LaunchId, crate::daemon::supervisor::ManagedModel)],
  path: &str,
) -> Option<(u16, ModelId)> {
  let mut candidates: Vec<(LaunchId, u16, ModelId)> = Vec::new();
  for (launch_id, model) in snap {
    if same_path(&model.id().path, path) && matches!(model.state().await, ManagedState::Ready) {
      candidates.push((launch_id.clone(), model.port(), model.id().clone()));
    }
  }
  candidates
    .into_iter()
    .max_by_key(|(launch_id, _, _)| launch_id.counter().unwrap_or(0))
    .map(|(_, port, key)| (port, key))
}

/// What [`resolve_client_reference`] could not settle, left in a shape the
/// callers render their own error bodies from.
pub(crate) enum ClientRefMiss {
  /// Nothing matched, neither as a reference nor as an alias.
  NotFound,
  /// Several real models matched. The entries are published ids, each of which
  /// routes.
  Ambiguous(Vec<String>),
}

/// Where a client's reference landed: the catalog row, and whether an alias is
/// what put the request on it.
pub(crate) struct ResolvedRef {
  pub row: CatalogRow,
  /// A `proxy.aliases` entry named the row, so `requested_model` and `model`
  /// differ by the table rather than by a fallback. Carried as a flag because
  /// the requested string is user input a note must not repeat.
  pub alias: bool,
}

impl ResolvedRef {
  fn plain(row: CatalogRow) -> Self {
    Self { row, alias: false }
  }

  fn aliased(row: CatalogRow) -> Self {
    Self { row, alias: true }
  }
}

/// Turn a client's `model` string into `(launch name, catalog row)`.
///
/// The whole string is resolved first, as one reference. Only when it names no
/// model is it taken apart at `@`, so a file named `foo@bar.gguf` still resolves
/// whole. Both halves of that address go through the same rule, which is what
/// makes `<alias>@<launch>` reach the model behind the alias with the launch half
/// honoured.
pub(crate) fn resolve_client_reference(
  state: &ProxyState,
  rows: &[CatalogRow],
  requested: &str,
) -> Result<(Option<String>, ResolvedRef), ClientRefMiss> {
  match resolve_reference(state, rows, requested) {
    Ok(row) => return Ok((None, row)),
    Err(ClientRefMiss::Ambiguous(candidates)) => return Err(ClientRefMiss::Ambiguous(candidates)),
    Err(ClientRefMiss::NotFound) => {}
  }

  if let Some((model, launch)) = parse_named_reference(requested) {
    return match resolve_reference(state, rows, model) {
      Ok(row) => Ok((Some(launch.to_string()), row)),
      Err(miss) => Err(miss),
    };
  }

  Err(ClientRefMiss::NotFound)
}

/// One reference to one model, the same way every surface that takes a model
/// string resolves it: a name that owns a model outright, then a `proxy.aliases`
/// entry, then a partial match. An alias beats a partial match: `gpt-4o-mini`
/// standing for one model must not be swallowed by a `gpt-4o-mini-Q4_K_M.gguf`
/// that appears later, nor by two models that both contain the string.
fn resolve_reference(
  state: &ProxyState,
  rows: &[CatalogRow],
  reference: &str,
) -> Result<ResolvedRef, ClientRefMiss> {
  match crate::launch::resolve::resolve_exact_reference(rows, reference) {
    Ok(row) => {
      state.ctx.aliases.warn_shadowed(reference);
      return Ok(ResolvedRef::plain(row));
    }
    // No outright model under this name, so an alias of it is next. A name two
    // models share counts as no outright model: that is what the alias settles.
    Err(ResolveError::None) | Err(ResolveError::Many(_)) => {}
    Err(ResolveError::Empty) => return Err(ClientRefMiss::NotFound),
  }

  if let Some(target) = state.ctx.aliases.target(reference) {
    return resolve_aliased_reference(state, rows, reference, target);
  }

  resolve_model_with_candidates(rows, reference)
    .map(ResolvedRef::plain)
    .map_err(|err| client_miss(rows, err))
}

/// The alias half of [`resolve_reference`]. The target has to name one model
/// outright: a path, a file name, a published id or a repo-qualified id. An alias
/// is written once and the client sending its name cannot refine it, so answering
/// a loose target with a substring match would auto-start whichever row happened
/// to contain it, and nothing downstream could tell that was not the model named.
///
/// The verdict comes from the table's own ladder, which is also what `list --json`
/// reports, so the reason in the operator's audit and the reason in this response
/// are decided by one walk over the same rungs.
fn resolve_aliased_reference(
  state: &ProxyState,
  rows: &[CatalogRow],
  reference: &str,
  target: &str,
) -> Result<ResolvedRef, ClientRefMiss> {
  match state.ctx.aliases.verdict(rows, target) {
    TargetVerdict::Routes(row) => {
      note_moved_partial(state, rows, reference, target, &row);
      Ok(ResolvedRef::aliased(*row))
    }
    // The value is another alias name, so the chain reaches nothing; or nothing
    // owns it, not even loosely. Each gets its own line rather than the generic
    // miss, because each is fixed a different way in `config.yaml`.
    TargetVerdict::Chain => {
      state.ctx.aliases.warn_points_at_alias(reference, target);
      Err(ClientRefMiss::NotFound)
    }
    TargetVerdict::Dead => {
      state.ctx.aliases.warn_dead_target(reference, target);
      Err(ClientRefMiss::NotFound)
    }
    // Two or more models answer to the value, which is a mistake only the
    // operator can fix, and the client is handed the same candidate list any
    // ambiguous reference gets.
    TargetVerdict::Ambiguous(candidates) => {
      state.ctx.aliases.warn_ambiguous_target(reference, target);
      Err(ClientRefMiss::Ambiguous(
        crate::launch::resolve::published_ids_for(rows, &candidates),
      ))
    }
  }
}

/// What a failed resolve means to the client: a candidate list when several models
/// answered, a plain miss when none did. Both tiers are consulted for a client's
/// own name and for an alias's value, so this is decoded in one place.
fn client_miss(rows: &[CatalogRow], error: ResolveError) -> ClientRefMiss {
  match error {
    ResolveError::Many(candidates) => {
      ClientRefMiss::Ambiguous(crate::launch::resolve::published_ids_for(rows, &candidates))
    }
    _ => ClientRefMiss::NotFound,
  }
}

/// The alias won, but the same string also reached a model on its own - a longer
/// file name that contains it - so clients that were already being served move to
/// another model. Asked only while that line is still unspent, because answering it
/// costs the second pass over the catalog.
fn note_moved_partial(
  state: &ProxyState,
  rows: &[CatalogRow],
  reference: &str,
  target: &str,
  row: &CatalogRow,
) {
  if !state
    .ctx
    .aliases
    .report_pending(reference, super::alias::REPORT_PARTIAL)
  {
    return;
  }
  if let Ok(other) = resolve_model_with_candidates(rows, reference) {
    if other.path != row.path {
      state
        .ctx
        .aliases
        .warn_moved_partial(reference, target, &other.name());
    }
  }
}

/// Which of several Ready launches of one model a request goes to: an unnamed
/// launch over a named one (a named launch has its own address), then the
/// newest (highest `L#`). Returns an index into `ready`, `None` when it is empty.
fn pick_ready_launch(
  ready: &[(LaunchId, u16)],
  running: &[crate::daemon::state_store::RunningSnapshot],
) -> Option<usize> {
  ready
    .iter()
    .enumerate()
    .max_by_key(|(_, (id, port))| (!is_named(running, id, *port), id.counter().unwrap_or(0)))
    .map(|(i, _)| i)
}

fn is_named(
  running: &[crate::daemon::state_store::RunningSnapshot],
  id: &LaunchId,
  port: u16,
) -> bool {
  running.iter().any(|r| {
    r.name.is_some()
      && match &r.launch_id {
        Some(mine) => mine == id,
        None => r.port == port,
      }
  })
}

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
    return Backends::from_id(&tag);
  }
  let cat = state.ctx.catalog.shared_view().await;
  let rb = cat
    .iter()
    .find(|m| m.path == id.path)
    .and_then(|m| m.default_backend().map(str::to_string))?;
  Backends::from_id(&rb).filter(|b| b.available(&state.ctx))
}

/// The backend an `Auto` launch of `row` would land on — resolved through the
/// same [`crate::backend::resolve_backend_for_launch`] seam the daemon's
/// `start_model` uses, with the catalog mode hint as the launch mode. A row a
/// backend mints its own identity for (a declared server entry) keeps that
/// identity: forcing every row through the GGUF shape routed a `stt`-only entry
/// to the default engine, which then answered for a mode it never declared. Used
/// by the not-running arm of the mode guard: a compatible model launched for a
/// `/v1/embeddings` request would auto-start in chat mode on its routed backend,
/// then that backend's own error surfaces after the load — the guard refuses
/// first (see [`Backend::serves_mode`]). Names no backend.
pub(crate) async fn would_route_backend(
  state: &Arc<ProxyState>,
  row: &CatalogRow,
) -> Option<crate::backend::Backends> {
  let cat = state.ctx.catalog.shared_view().await;
  let m = cat.iter().find(|m| same_path(&m.path, &row.path))?;
  let launch_mode = match row.mode_hint.as_deref() {
    Some("embedding") => LaunchMode::Embedding,
    Some("rerank") => LaunchMode::Rerank,
    _ => LaunchMode::Chat,
  };
  // A GGUF identity is the fallback, whose default is the default backend and
  // whose priority-ordered supported list drives the auto pick — the daemon's rule.
  let identity = crate::backend::synthetic_identity_for_path(&m.path)
    .map(|(id, _)| id)
    .unwrap_or_else(|| {
      crate::backend::identity::ModelIdentity::Gguf(crate::gguf::identity::ModelId {
        path: m.path.clone(),
        header_blake3: [0u8; 32],
      })
    });
  Some(crate::backend::resolve_backend_for_launch(
    &identity,
    &m.path,
    None,
    crate::launch::params::BackendChoice::Auto,
    &m.supported_backends,
    launch_mode,
    &state.ctx,
  ))
}

/// Compare a `ModelId::path` (PathBuf) with a `CatalogRow::path`
/// (String). The catalog row is built from the discovered path's
/// `to_string_lossy()` view, and `ModelId::path` is canonical too —
/// equality is exact in production.
fn same_path(model_id_path: &std::path::Path, row_path: &str) -> bool {
  model_id_path.to_string_lossy() == row_path
}

/// Whether a Ready launch serves the endpoint's `mode`. The media modes
/// (image / tts / stt) reach a launch only through an entry that declares
/// them, so those need [`Backend::declares_mode`]; chat, embedding and
/// rerank go through the looser [`Backend::serves_mode`].
pub(crate) fn launch_serves_mode(
  backend: &crate::backend::Backends,
  path: &std::path::Path,
  server: Option<&str>,
  mode: LaunchMode,
) -> bool {
  use crate::backend::Backend;
  if mode.is_declared_only() {
    backend.declares_mode(path, server, mode)
  } else {
    backend.serves_mode(path, server, mode)
  }
}

/// The [`RouteDecision::NotRunning`] payload, as the auto-start path needs it.
pub(crate) struct NotRunningPlan {
  pub requested_model: String,
  /// Resolved catalog entry consumed by the launch path to build `StartParams`
  /// for `compose_and_spawn` without re-running the resolver.
  pub resolved_row: CatalogRow,
  /// The `@<name>` half of the client's reference, `None` for unnamed launches.
  pub name: Option<String>,
  /// The endpoint's mode picked this row rather than the client's name, so the
  /// forward carries the `mode_match` header.
  pub mode_fallback: bool,
  /// `false` when the request arrived on a path the engine declared for itself,
  /// where nothing in the body is rewritten.
  pub pin_request_model: bool,
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
///      when the pick's arch differs or either side lacks arch metadata
///      (clients that care should branch on the header). Embedding and
///      rerank requests never fall back. If no Ready candidate exists
///      → 503 `launch_failed` with the running list.
///
/// `endpoint_mode` carries the mode the requested endpoint implies so
/// the auto-start composes an argv that can answer it (see
/// [`launch::auto_start`]).
pub(crate) async fn handle_not_running(
  state: &Arc<ProxyState>,
  inbound: super::forward::InboundRequest,
  endpoint_mode: Option<crate::launch::mode::LaunchMode>,
  plan: NotRunningPlan,
  mut record: RequestRecord,
) -> ProxyResponse {
  let NotRunningPlan {
    requested_model,
    resolved_row,
    name,
    mode_fallback,
    pin_request_model,
  } = plan;
  let outcome = launch::auto_start(state, &resolved_row, endpoint_mode, name, &mut record).await;
  match outcome {
    LaunchOutcome::Ready { port, model_id } => {
      // Touch the MRU using the supervisor we just confirmed Ready.
      // The display name for the response header on the happy path
      // is the requested model name — no fallback, no surprise.
      state.mru.touch(&model_id).await;
      let served = served_name_for_row(&resolved_row);
      if mode_fallback {
        record.set_fallback(MODE_MATCH);
      }
      super::forward::forward_to_upstream(
        state,
        inbound,
        super::forward::Target {
          port,
          served_model_id: &served,
          served_model_key: &model_id,
          fallback: mode_fallback,
          fallback_reason: mode_fallback.then_some(MODE_MATCH),
          pin_request_model,
        },
        Some(record),
      )
      .await
    }
    LaunchOutcome::Failed { cause } => {
      // Another model's vectors or scores never match what the client indexed.
      if matches!(
        endpoint_mode,
        Some(LaunchMode::Embedding) | Some(LaunchMode::Rerank)
      ) {
        return super::router::answered(record, launch_failed_response(&cause, &requested_model));
      }
      // Operator can disable the family-MRU fallback entirely (see
      // `ProxyConfig::fallback_enabled` and the `--no-proxy-fallback`
      // CLI / `LLAMASTASH_NO_PROXY_FALLBACK` env overrides). When
      // off, a launch failure flows straight to the 503
      // `launch_failed` response below — clients never silently get a
      // payload from a different model.
      if !state.fallback_enabled {
        return super::router::answered(record, launch_failed_response(&cause, &requested_model));
      }
      // Family-MRU fallback. Walk the supervisor snapshot, filter
      // to Ready, attach each entry's catalog arch + MRU
      // timestamp, then defer to `pick_fallback` for the policy,
      // which pivots on the requested row's arch (`None` when
      // discovery couldn't parse the GGUF header).
      let requested_arch = resolved_row.arch.as_deref();
      let candidates = collect_fallback_candidates(state, endpoint_mode).await;
      if let Some(pick) = pick_fallback(candidates, requested_arch) {
        state.mru.touch(&pick.model_id).await;
        let reason = fallback_reason_for(requested_arch, pick.arch.as_deref());
        // Another model answers, so the row moves to it. `requested_model`
        // and `fallback` on the row keep what was asked for and why.
        record.set_model(&pick.served_model_id, &pick.model_id.path.to_string_lossy());
        record.set_fallback(reason);
        return super::forward::forward_to_upstream(
          state,
          inbound,
          super::forward::Target {
            port: pick.port,
            served_model_id: &pick.served_model_id,
            served_model_key: &pick.model_id,
            fallback: true,
            fallback_reason: Some(reason),
            pin_request_model,
          },
          Some(record),
        )
        .await;
      }
      // No Ready model to fall back to. R155 mandates a 503 with
      // the `running: []` list inline (empty by construction here:
      // `pick_fallback` only returns None when zero Ready candidates
      // exist). Drop the requested model name into the message so
      // logs surface what was being attempted.
      super::router::answered(record, launch_failed_response(&cause, &requested_model))
    }
  }
}

/// Pick the `x-llamastash-fallback-reason` value based on whether the
/// picked candidate is in the same arch family as the failed request.
///
/// - `launch_failed`: in-family substitution. Both sides name the same
///   arch — the upstream output shape is what the client asked for.
/// - `family_mismatch`: the arches differ, or either side has none, so
///   nothing proves the pick answers like the requested model.
fn fallback_reason_for(requested: Option<&str>, picked: Option<&str>) -> &'static str {
  match (requested, picked) {
    (Some(a), Some(b)) if a == b => "launch_failed",
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
  let message =
    format!("auto-start of `{requested_model}` failed and no running model is available: {cause}");
  let error =
    super::openai::ErrorObject::new("launch_failed", message).with_running(Vec::<String>::new());
  Ok(super::router::error_json(
    hyper::StatusCode::SERVICE_UNAVAILABLE,
    error,
  ))
}

/// Resolve a display name for a CatalogRow that mirrors what
/// `/v1/models` returns. `display_label` (Ollama's `<name>:<tag>`)
/// wins when present; otherwise fall back to the file stem via
/// [`crate::util::paths::model_display_name`] so the value of
/// `x-llamastash-served-by` is byte-equal to the corresponding
/// `/v1/models` `id` for the same model (closes R-11).
pub(crate) fn served_name_for_row(row: &CatalogRow) -> String {
  if let Some(label) = &row.display_label {
    return label.clone();
  }
  crate::util::paths::model_display_name(std::path::Path::new(&row.path))
}

/// The same rule for a catalog row looked up by path, for callers that hold a
/// supervisor's path rather than a catalog row: the row's `display_label` when
/// the catalog has one, else the file stem of `fallback` (the launched path,
/// which is what `/v1/models` would print for that model).
pub(crate) fn served_name_for_model(
  row: Option<&DiscoveredModel>,
  fallback: &std::path::Path,
) -> String {
  row
    .and_then(|m| m.display_label.clone())
    .unwrap_or_else(|| crate::util::paths::model_display_name(fallback))
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
/// Only launches that serve `endpoint_mode` join. A media entry launches
/// in chat mode, so a chat request also skips a row that declares a media mode.
async fn collect_fallback_candidates(
  state: &Arc<ProxyState>,
  endpoint_mode: Option<LaunchMode>,
) -> Vec<FallbackCandidate> {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  // Index the catalog by canonical path so each supervisor entry can
  // attach arch + display label without re-walking the catalog.
  let cat_snap = state.ctx.catalog.shared_view().await;
  let by_path = index_catalog_by_path(&cat_snap);

  let mut out: Vec<FallbackCandidate> = Vec::with_capacity(sup_snap.len());
  for (_launch_id, model) in sup_snap.into_iter() {
    if !matches!(model.state().await, ManagedState::Ready) {
      continue;
    }
    let id = model.id().clone();
    let path_key = id.path.to_string_lossy().into_owned();
    let catalog_entry = by_path.get(&path_key);
    let params = model.params();
    let serves = match endpoint_mode {
      Some(mode) => launch_serves_mode(model.backend(), &id.path, params.server.as_deref(), mode),
      None => {
        params.mode == LaunchMode::Chat
          && launch_serves_mode(
            model.backend(),
            &id.path,
            params.server.as_deref(),
            LaunchMode::Chat,
          )
          && crate::ipc::status::declared_media_mode(&cat_snap, &id.path).is_none()
      }
    };
    if !serves {
      continue;
    }
    let arch = catalog_entry
      .and_then(|m| m.metadata.as_ref())
      .and_then(|md| md.arch.clone());
    let served_model_id = served_name_for_model(catalog_entry.copied(), &id.path);
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

  #[test]
  fn pick_ready_launch_prefers_unnamed_then_newest() {
    use super::{pick_ready_launch, LaunchId};
    let l = |n: u64| (LaunchId::from_counter(n), 41000 + n as u16);
    let named = |n: u64, name: &str| crate::daemon::state_store::RunningSnapshot {
      launch_id: Some(LaunchId::from_counter(n)),
      port: 41000 + n as u16,
      name: Some(name.to_string()),
      ..crate::test_support::running_row("/m.gguf").build()
    };
    // Numeric, not lexicographic: L10 is newer than L9.
    assert_eq!(pick_ready_launch(&[l(9), l(10)], &[]), Some(1));
    // An unnamed launch beats a newer named one.
    assert_eq!(
      pick_ready_launch(&[l(2), l(5)], &[named(5, "coder")]),
      Some(0)
    );
    // All named: the newest.
    assert_eq!(
      pick_ready_launch(&[l(3), l(4)], &[named(3, "a"), named(4, "b")]),
      Some(1)
    );
    assert_eq!(pick_ready_launch(&[], &[]), None);
  }

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
  fn no_arch_on_either_side_is_family_mismatch() {
    assert_eq!(fallback_reason_for(None, None), "family_mismatch");
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
  use crate::discovery::catalog::catalog_row;
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
        lazy_tensor_bytes: Vec::new(),
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
    let row = catalog_row(&m);
    assert_eq!(
      row.mode_hint.as_deref(),
      Some("embedding"),
      "proxy auto-start needs embedding hint to add --embeddings"
    );
  }

  #[test]
  fn catalog_row_propagates_rerank_mode_hint_to_proxy() {
    let m = discovered_with_mode(ModeHint::Rerank);
    let row = catalog_row(&m);
    assert_eq!(row.mode_hint.as_deref(), Some("rerank"));
  }

  #[test]
  fn catalog_row_propagates_chat_mode_hint_to_proxy() {
    let m = discovered_with_mode(ModeHint::Chat);
    let row = catalog_row(&m);
    assert_eq!(row.mode_hint.as_deref(), Some("chat"));
  }

  #[test]
  fn catalog_row_labels_unknown_mode_hint() {
    // `launch_mode_from_hint` reads `unknown` as no hint, so auto-start
    // keeps the chat default when the GGUF carried no signal.
    let m = discovered_with_mode(ModeHint::Unknown);
    let row = catalog_row(&m);
    assert_eq!(row.mode_hint.as_deref(), Some("unknown"));
  }

  // ─── served-by name resolution ──────────────────────────────────
  use super::{index_catalog_by_path, served_name_for_row};

  #[test]
  fn served_name_prefers_display_label_then_falls_back_to_stem() {
    // Ollama rows carry a human `<name>:<tag>` display_label — that wins
    // so the `x-llamastash-served-by` header is readable.
    let mut m = discovered_with_mode(ModeHint::Chat);
    m.display_label = Some("gemma3:4b".to_string());
    let labelled = catalog_row(&m);
    assert_eq!(served_name_for_row(&labelled), "gemma3:4b");

    // Without a display_label the served name derives from the file
    // stem, byte-equal to the `/v1/models` id for the same model.
    let mut plain = discovered_with_mode(ModeHint::Chat);
    plain.display_label = None;
    plain.path = std::path::PathBuf::from("/models/Qwen3-7B-Q4_K_M.gguf");
    let stem_row = catalog_row(&plain);
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
