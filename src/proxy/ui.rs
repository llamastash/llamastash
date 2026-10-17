//! `/ui` browser surface: serve a chat UI for the running model through the
//! proxy on one port-stable origin (`http://127.0.0.1:11435/ui/`) so users
//! stop hunting the ephemeral backend port in `status`.
//!
//! A launch gets its UI from one of three sources, decided by [`ui_source`]:
//! the backend's own UI, the llama.cpp UI bundled in the binary
//! ([`super::ui_bundle`]) for engines that ship none, or nothing at all.
//!
//! The stock UI is base-path aware — its index uses relative assets
//! (`./bundle.js`) and SvelteKit computes `base` from the URL — so
//! served under `/ui/` every asset and base-relative API request stays
//! under `/ui/`. We strip the `/ui` prefix and hand the rest to
//! [`super::forward::forward_to_upstream`], the same reverse-proxy path
//! the OpenAI surface uses. No rewriting, no vhost, no custom shell.
//!
//! Backend selection for any `/ui/...` request, in order:
//!   1. Cookie `ls_ui_target=<launch_id>` present and still running →
//!      that backend.
//!   2. Else exactly one running model → that backend.
//!   3. Else zero running → a "no model running" page.
//!   4. Else (>1, no valid cookie) → the chooser page.
//!
//! A chooser link is `/ui/?target=<launch_id>`: it sets the pin cookie
//! and 302s to `/ui/`. The cookie keeps asset/API requests pinned to
//! the model whose UI is loaded; chat history persists across switches
//! because it's browser-origin keyed and the origin never changes.
//!
//! Plan: docs/plans/2026-06-15-001-feat-proxy-ui-surface-plan.md.

use std::sync::Arc;

use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use hyper::{HeaderMap, Request, Response, StatusCode};

use super::router::{BodyError, ClientAddr, ProxyResponse, GENERATION_ROUTES};
use super::state::ProxyState;
use super::{forward, metrics, route, ui_bundle};
use crate::daemon::registry::LaunchId;
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use serde_json::Value;

/// Cookie that pins a browser session to one running backend so asset
/// and base-relative API requests under `/ui/` all reach the model
/// whose UI is loaded. Value is a `LaunchId` string (`L<n>`).
const TARGET_COOKIE: &str = "ls_ui_target";

/// One running, Ready model the UI can target.
struct RunningEntry {
  launch_id: String,
  model_id: ModelId,
  port: u16,
  name: String,
  /// Where this launch's browser UI comes from. A launch with no UI at all
  /// is never auto-pinned and renders non-selectable in the chooser.
  ui: UiSource,
}

/// Where a launch's browser UI comes from. Decided from the owning
/// backend's `serves_web_ui` verdict and the launch mode, so no backend is
/// named here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiSource {
  /// The backend serves its own UI; `/ui` reverse-proxies all of it.
  Engine,
  /// The backend serves none, so `/ui` serves the bundled llama.cpp UI and
  /// forwards only the engine's `/v1/...` surface.
  Bundled,
  /// Nothing to open: an embedding or rerank launch on a backend that
  /// serves no UI.
  None,
}

impl UiSource {
  /// Whether a chooser row for this launch opens a UI.
  fn is_selectable(self) -> bool {
    matches!(self, Self::Engine | Self::Bundled)
  }
}

/// The one rule behind the three sources: a backend with a UI always uses
/// its own, a UI-less backend gets the bundled chat UI, and a UI-less
/// embedding or rerank launch has nothing worth opening.
fn ui_source(serves_web_ui: bool, mode: LaunchMode) -> UiSource {
  if serves_web_ui {
    UiSource::Engine
  } else if mode == LaunchMode::Chat {
    UiSource::Bundled
  } else {
    UiSource::None
  }
}

/// Where a `/ui/...` request should be served from.
enum UiTarget {
  /// Forward to this Ready backend.
  Forward(RunningEntry),
  /// Two or more running and no valid cookie pin → the chooser page.
  Chooser(Vec<RunningEntry>),
  /// Nothing running → the "start a model" page.
  None,
}

/// `GET /ui` → 302 to `/ui/`. The trailing slash matters: the stock UI
/// computes its base from `new URL('.', location)`, so without it
/// `./bundle.js` would resolve against `/` instead of `/ui/`.
pub(crate) fn redirect_to_ui_slash() -> ProxyResponse {
  Ok(
    Response::builder()
      .status(StatusCode::FOUND)
      .header(LOCATION, "/ui/")
      .body(empty_body())
      .expect("static redirect response must build"),
  )
}

/// Entry point for every `/ui/...` request. Resolves a target backend
/// (cookie → single → none/chooser) and either forwards to it or serves
/// one of the two static pages.
pub(crate) async fn serve(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  // A chooser-link click lands as `/ui/?target=<launch_id>`: pin the
  // cookie and bounce back to `/ui/` so the reload resolves the backend
  // from the cookie. Single origin throughout ⇒ chat history persists.
  if let Some(target) = target_query_param(req.uri()) {
    return set_target_cookie_redirect(&target);
  }

  let running = collect_running(&state).await;

  // `/ui/switch` always shows the chooser, ignoring any cookie pin, so a
  // user already pinned to one model can switch to another. The stock UI
  // is single-model with no in-page switcher and we don't inject one, so
  // this reserved URL is the switch affordance. Zero running still shows
  // the no-model page.
  if is_switch_path(req.uri().path()) {
    let active = active_pin(req.headers(), &running);
    let body = if running.is_empty() {
      no_model_html()
    } else {
      chooser_html(&running, active.as_deref())
    };
    return Ok(html_response(StatusCode::OK, body));
  }

  match resolve_target(req.headers(), running) {
    UiTarget::Forward(entry) => match entry.ui {
      UiSource::Engine => forward_to_launch(&state, req, &entry, None).await,
      UiSource::Bundled => serve_bundled(&state, req, &entry).await,
      // Unreachable through `resolve_target`, which only forwards
      // selectable entries; the chooser is what a UI-less launch gets.
      UiSource::None => Ok(html_response(StatusCode::OK, chooser_html(&[entry], None))),
    },
    // No valid pin here by construction, so nothing is marked active.
    UiTarget::Chooser(entries) => Ok(html_response(StatusCode::OK, chooser_html(&entries, None))),
    UiTarget::None => Ok(html_response(StatusCode::OK, no_model_html())),
  }
}

/// `/ui/switch` (with or without a trailing slash) — the forced model
/// switcher. Reserved by us; the stock UI lives at `/ui/` and never
/// serves a `/switch` route.
fn is_switch_path(path: &str) -> bool {
  path == "/ui/switch" || path == "/ui/switch/"
}

/// The current cookie pin, but only when it maps to a model that is
/// actually running — so the switcher marks the live target, never a
/// stale one.
fn active_pin(headers: &HeaderMap, running: &[RunningEntry]) -> Option<String> {
  let pin = read_target_cookie(headers)?;
  running.iter().any(|e| e.launch_id == pin).then_some(pin)
}

/// Apply the selection rule over the running list, honoring the UI-less
/// exclusion (D-ui): a backend that serves no web UI is never
/// auto-pinned and never satisfies a cookie pin — it can only appear in the
/// chooser as a non-selectable row. The "no model running" page stays
/// reserved for *zero* running models, so a running UI-less model never reads as
/// "nothing running".
fn resolve_target(headers: &HeaderMap, mut running: Vec<RunningEntry>) -> UiTarget {
  if running.is_empty() {
    return UiTarget::None;
  }
  // 1. Cookie pin → that backend, but only if it has a UI to serve.
  if let Some(pin) = read_target_cookie(headers) {
    if let Some(idx) = running
      .iter()
      .position(|e| e.launch_id == pin && e.ui.is_selectable())
    {
      return UiTarget::Forward(running.swap_remove(idx));
    }
  }
  // 2. Auto-forward only to a *lone* model with a UI; anything else (a lone
  //    UI-less model, or several models) shows the chooser — which renders
  //    UI-less rows non-selectable.
  if running.len() == 1 && running[0].ui.is_selectable() {
    return UiTarget::Forward(running.pop().expect("len == 1"));
  }
  UiTarget::Chooser(running)
}

/// Every Ready, servable launch as a [`RunningEntry`], in launch order. The
/// rows come from [`metrics::live_launches`], the registry walk shared with
/// `/metrics`, so the walk happens in one place. Names mirror `/v1/models`
/// (display label wins, else file stem) so the chooser shows the same
/// identifier the API surfaces. The managed-multiplexer umbrella is skipped
/// there — it's a multiplexer process, not a web UI.
async fn collect_running(state: &Arc<ProxyState>) -> Vec<RunningEntry> {
  let mut out = Vec::new();
  for launch in metrics::live_launches(state).await {
    if !launch.ready {
      continue;
    }
    // Whether this launch's backend serves a web UI, from the supervisor that
    // owns *this launch* rather than the backend the model would route to: one
    // GGUF can run on two servers at once, and only the launch on a UI-less
    // engine needs the bundle. Every row listed here came from the registry, so
    // a missing one means it stopped mid-flight; the default backend's verdict
    // is what the resolver would have answered a moment earlier. Names no
    // backend.
    let serves_web_ui = {
      use crate::backend::Backend;
      match state
        .ctx
        .supervisors
        .get(&LaunchId(launch.id.clone()))
        .await
      {
        Some(model) => model.backend().serves_web_ui(),
        None => crate::backend::default_backend().serves_web_ui(),
      }
    };
    out.push(RunningEntry {
      launch_id: launch.id,
      model_id: launch.model_id,
      port: launch.port,
      name: launch.model,
      ui: ui_source(serves_web_ui, launch.mode),
    });
  }
  out
}

/// Which logged route this stripped `/ui` path is, or `None` when it is UI
/// plumbing (an asset, `/props`, the stream lookup). Matched on the path alone,
/// so a query string cannot turn a logged route into an unlogged one, and on the
/// exact fixed strings the router forwards — nothing user-controlled is stored.
fn logged_ui_route(stripped: &str) -> Option<&'static str> {
  let path = stripped.split('?').next().unwrap_or("/");
  GENERATION_ROUTES
    .iter()
    .copied()
    .find(|route| *route == path)
}

/// Strip the `/ui` prefix and forward the request to `entry`'s backend.
/// The UI's relative assets (`/ui/bundle.js`) and base-relative API
/// calls (`/ui/v1/chat/completions`, `/ui/props`) all land here and
/// route to the same target. Shared by the engine-UI path (every request)
/// and the bundled path (its `/v1/...` arm).
///
/// `model_when_absent` names the model to put in a JSON body that carries
/// none. Only the bundled path passes it: that page sends no `model` in
/// single-model mode and engines differ on whether they need one. The engine's
/// own page always names its model, so that path passes `None`.
async fn forward_to_launch(
  state: &Arc<ProxyState>,
  req: Request<Incoming>,
  entry: &RunningEntry,
  model_when_absent: Option<&str>,
) -> ProxyResponse {
  let client = req.extensions().get::<ClientAddr>().map(|c| c.0);
  let stripped = strip_ui_prefix(req.uri());
  let logged = logged_ui_route(&stripped);
  let (method, _uri, headers, body) = forward::deconstruct(req);
  // Buffer the body under the daemon's cap via the same helper the
  // data plane uses. UI asset GETs are bodyless; the base-relative API
  // POSTs carry a JSON payload.
  let cap = state.max_body_size;
  let body_bytes = match route::buffer_body(body, cap).await {
    Ok(b) => b,
    Err(e) => return route::body_error_response(e, cap),
  };
  // Opened after the body is in hand so the row can carry what the page
  // actually asked for, and before `model_when_absent` fills the body in, so
  // `requested_model` stays what the browser sent.
  let record = logged.map(|route| {
    let mut record = state.ctx.requests.begin(route, client);
    record.set_requested_model(route::model_in_body(&body_bytes).as_deref());
    record.set_model(&entry.name, &entry.model_id.path.to_string_lossy());
    record.set_ui();
    record.publish();
    record
  });
  let body_bytes = match model_when_absent {
    Some(name) => forward::with_model_added(body_bytes, name),
    None => body_bytes,
  };

  let upstream_uri: hyper::Uri = stripped
    .parse()
    .unwrap_or_else(|_| hyper::Uri::from_static("/"));
  forward::forward_to_upstream(
    state,
    forward::InboundRequest {
      method,
      uri: upstream_uri,
      headers,
      body_bytes,
    },
    forward::Target {
      port: entry.port,
      served_model_id: &entry.name,
      served_model_key: &entry.model_id,
      upstream_path_prefix: None,
      fallback: false,
      fallback_reason: None,
    },
    // Web-UI traffic that reaches a model is logged (`ui` marks the row);
    // everything else under `/ui` is not.
    record,
  )
  .await
}

/// What a bundled-UI request maps to once `/ui` is stripped.
enum BundledRoute<'a> {
  /// The proxy's own answer for the `/props` route, which no engine other
  /// than the bundle's origin serves.
  Props,
  /// The engine's OpenAI surface, forwarded as the client sent it, except
  /// that a JSON body naming no model gets this launch's served name.
  Forward,
  /// A route only the bundle's origin engine serves, answered here with a
  /// fixed status so the page degrades the way it does against an engine that
  /// started without that surface.
  Refused {
    status: StatusCode,
    why: &'static str,
  },
  /// The page's "is any stream still running?" query. Nothing resumes through
  /// us, so it is the empty list the origin engine answers for ids it has no
  /// stream for, which the page reads without logging a problem.
  NoStreams,
  /// An embedded bundle file, or a path the bundle does not carry.
  File(&'a str),
}

fn classify_bundled(stripped: &str) -> BundledRoute<'_> {
  let path = stripped.split('?').next().unwrap_or("/");
  if client_path(path).as_deref() != Some(path) {
    // A path the forward client resolves to something else (a dot segment in
    // any of its spellings) is answered from the bundle, so it can only be a
    // 404. Checking before the prefix tests is what keeps `/v1/x/\..\..\x`
    // from reaching the engine as `/x`.
    return BundledRoute::File(path);
  }
  if path == "/props" {
    BundledRoute::Props
  } else if path == "/v1/streams/lookup" {
    BundledRoute::NoStreams
  } else if path == "/tools" {
    // 403 is what the bundle's origin answers when started without server
    // tools, and the page reads it as "tools are disabled" instead of a
    // broken endpoint.
    BundledRoute::Refused {
      status: StatusCode::FORBIDDEN,
      why: "server tools are not enabled",
    }
  } else if path == "/v1/stream" || path.starts_with("/v1/streams/") {
    BundledRoute::Refused {
      status: StatusCode::NOT_FOUND,
      why: "stream resume is not supported",
    }
  } else if path.starts_with("/v1/") {
    BundledRoute::Forward
  } else {
    BundledRoute::File(path)
  }
}

/// The path the forward client would put on the wire for this request path.
/// It runs the path through the same URL parser reqwest parses its request
/// URL with, so dot segments come out resolved in every spelling that parser
/// accepts: literal `..`, `%2e%2e` (with any casing) and backslash separators.
/// `None` when the path will not parse at all, which is refused the same way.
fn client_path(path: &str) -> Option<String> {
  reqwest::Url::parse("http://localhost")
    .ok()?
    .join(path)
    .ok()
    .map(|url| url.path().to_string())
}

/// Serve a bundled-UI request: `/props` is ours, `/v1/...` goes to the engine,
/// the origin-only routes get our own answer, and everything else is a file
/// from the embedded bundle.
async fn serve_bundled(
  state: &Arc<ProxyState>,
  req: Request<Incoming>,
  entry: &RunningEntry,
) -> ProxyResponse {
  match classify_bundled(&strip_ui_prefix(req.uri())) {
    BundledRoute::Props => {
      let ctx = launch_ctx(state, &entry.launch_id).await;
      let vision = launch_vision(state, &entry.model_id.path).await;
      Ok(json_response(
        StatusCode::OK,
        ui_bundle::props(&entry.name, ctx, vision),
      ))
    }
    BundledRoute::Forward => forward_to_launch(state, req, entry, Some(&entry.name)).await,
    BundledRoute::Refused { status, why } => Ok(plain(status, why)),
    BundledRoute::NoStreams => Ok(json_response(StatusCode::OK, serde_json::json!([]))),
    BundledRoute::File(path) => match ui_bundle::get(path) {
      Some(bytes) => {
        let mut response = static_response(
          StatusCode::OK,
          ui_bundle::content_type(path),
          Bytes::from_static(bytes),
        );
        response.headers_mut().insert(
          CACHE_CONTROL,
          hyper::header::HeaderValue::from_static(ui_bundle::cache_control(path)),
        );
        Ok(response)
      }
      None => Ok(plain(StatusCode::NOT_FOUND, "no such UI file")),
    },
  }
}

/// The context window to report for a bundled launch: what the backend
/// resolved when it reports actuals, else what the launch asked for, else
/// zero. A gauge at zero beats a guessed number.
async fn launch_ctx(state: &Arc<ProxyState>, launch_id: &str) -> u32 {
  let Some(model) = state
    .ctx
    .supervisors
    .get(&LaunchId(launch_id.to_string()))
    .await
  else {
    return 0;
  };
  let resolved = model.actuals().await.resolved_ctx;
  ctx_to_report(resolved, model.params().ctx)
}

fn ctx_to_report(resolved: Option<u32>, requested: Option<u32>) -> u32 {
  resolved.or(requested).unwrap_or(0)
}

/// Whether the launch's model takes images, from its catalog row's
/// projector flags. No row and no projector both read as text-only.
async fn launch_vision(state: &Arc<ProxyState>, path: &std::path::Path) -> bool {
  let catalog = state.ctx.catalog.shared_view().await;
  catalog
    .iter()
    .find(|row| row.path == path)
    .and_then(|row| row.multimodal)
    .is_some_and(|m| m.vision)
}

/// Strip the leading `/ui` from a `/ui/...` request URI, preserving the
/// query string. `/ui/` → `/`, `/ui/props?x=1` → `/props?x=1`. The
/// router only delegates here when the path starts with `/ui/`, so the
/// prefix is always present; the fallback covers nothing in practice.
fn strip_ui_prefix(uri: &hyper::Uri) -> String {
  let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/ui/");
  match pq.strip_prefix("/ui") {
    Some(rest) if rest.starts_with('/') => rest.to_string(),
    _ => "/".to_string(),
  }
}

// ─── cookie + query helpers ───────────────────────────────────────────

/// Read the `ls_ui_target` pin out of the `Cookie` header, if present.
fn read_target_cookie(headers: &HeaderMap) -> Option<String> {
  let raw = headers.get(COOKIE)?.to_str().ok()?;
  raw.split(';').find_map(|pair| {
    pair
      .trim()
      .strip_prefix(TARGET_COOKIE)
      .and_then(|r| r.strip_prefix('='))
      .filter(|v| !v.is_empty())
      .map(str::to_string)
  })
}

/// Read a plausible `target=<launch_id>` query parameter.
fn target_query_param(uri: &hyper::Uri) -> Option<String> {
  let q = uri.query()?;
  q.split('&').find_map(|pair| {
    pair
      .strip_prefix("target=")
      .filter(|v| is_plausible_launch_id(v))
      .map(str::to_string)
  })
}

/// Launch ids are `L<digits>`; accept the conservative `[A-Za-z0-9_-]+`
/// so a crafted `?target=` can never smuggle bytes into the
/// `Set-Cookie` header (an invalid `HeaderValue` would otherwise panic
/// the response builder).
fn is_plausible_launch_id(s: &str) -> bool {
  !s.is_empty()
    && s.len() <= 64
    && s
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Set the pin cookie and 302 back to `/ui/`. `Path=/ui` scopes the pin
/// to the UI surface; `SameSite=Lax` matches the browser default for
/// top-level navigation. No `Secure` flag — the proxy is plain HTTP.
fn set_target_cookie_redirect(launch_id: &str) -> ProxyResponse {
  let cookie = format!("{TARGET_COOKIE}={launch_id}; Path=/ui; SameSite=Lax");
  Ok(
    Response::builder()
      .status(StatusCode::FOUND)
      .header(LOCATION, "/ui/")
      .header(SET_COOKIE, cookie)
      .body(empty_body())
      .expect("static redirect response must build"),
  )
}

// ─── static pages ─────────────────────────────────────────────────────

/// Minimal model-chooser page. Used both for the auto-chooser (>1
/// running, no valid cookie) and the `/ui/switch` switcher. Each link
/// pins the cookie via `/ui/?target=<launch_id>` and reloads. `active`
/// is the currently-pinned, still-running launch id (the switcher passes
/// it so the live model is marked); `None` for the first-time chooser.
fn chooser_html(running: &[RunningEntry], active: Option<&str>) -> String {
  let mut items = String::new();
  for e in running {
    let current = if active == Some(e.launch_id.as_str()) {
      " <span class=\"current\">current</span>"
    } else {
      ""
    };
    if e.ui.is_selectable() {
      items.push_str(&format!(
        "<li><a class=\"has-ui\" href=\"/ui/?target={id}\"><span class=\"name\">{name}</span><span class=\"port\">:{port}{current}</span></a></li>",
        id = escape_html(&e.launch_id),
        name = escape_html(&e.name),
        port = e.port,
      ));
    } else {
      // UI-less backend: shown so the user knows it is running, but not
      // a link — it serves no web UI to open.
      items.push_str(&format!(
        "<li class=\"no-ui\"><span class=\"name\">{name}</span>\
         <span class=\"port\">:{port}{current}</span> \
         <span class=\"reason\">no web UI</span></li>",
        name = escape_html(&e.name),
        port = e.port,
      ));
    }
  }
  page(
    "Choose a model",
    &format!(
      "<h1>Choose a model</h1>\
       <p>Pick the model whose web UI you want to open. Green rows open one; \
       <span class=\"no-ui-text\">red rows</span> are running but serve no \
       browser interface.</p>\
       <ul class=\"models\">{items}</ul>\
       <p class=\"tip\">Already on a model? Open \
       <a href=\"/ui/switch\"><code>/ui/switch</code></a> to come back here and pick \
       another.</p>"
    ),
  )
}

/// Minimal "nothing is running" page (zero running). Points the user at
/// the TUI / CLI rather than returning a bare 500.
fn no_model_html() -> String {
  page(
    "No model running",
    "<h1>No model running</h1>\
     <p>Start a model first, then reload this page:</p>\
     <ul class=\"hints\">\
       <li>Run <code>llamastash</code> and press <kbd>Enter</kbd> on a model, or</li>\
       <li>From a shell: <code>llamastash start &lt;model&gt;</code></li>\
     </ul>",
  )
}

/// Wrap body markup in the shared minimal HTML shell + inline CSS.
fn page(title: &str, body: &str) -> String {
  format!(
    "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
     <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
     <title>{title} · llamastash</title><style>{CSS}</style></head>\
     <body><main>{body}</main></body></html>",
    title = escape_html(title),
  )
}

/// Inline stylesheet for the two static pages — small, dark, dependency
/// free. Kept terse on purpose; this is a fallback surface, not a UI.
const CSS: &str = "\
:root{color-scheme:dark}\
body{margin:0;background:#24273a;color:#cad3f5;\
font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}\
main{max-width:42rem;margin:4rem auto;padding:0 1.5rem}\
h1{font-size:1.5rem;margin:0 0 1rem}\
p{color:#a5adcb}\
a{color:#8aadf4;text-decoration:none}\
ul.models{list-style:none;padding:0;margin:1.5rem 0}\
ul.models li{margin:.5rem 0}\
ul.models a{display:flex;justify-content:space-between;align-items:center;\
padding:.75rem 1rem;background:#363a4f;border-radius:.5rem}\
ul.models a:hover{background:#494d64}\
ul.models a.has-ui .name{color:#a6da95}\
li.no-ui{display:flex;justify-content:space-between;align-items:center;\
padding:.75rem 1rem;background:#363a4f;border-radius:.5rem;opacity:.85}\
li.no-ui .name,span.no-ui-text{color:#ed8796}\
li.no-ui .reason{color:#ed8796;font-size:.85em;text-transform:uppercase;\
letter-spacing:.05em}\
.port{color:#939ab7;font-variant-numeric:tabular-nums}\
.current{color:#8aadf4;font-variant-numeric:normal;margin-left:.5rem;\
text-transform:uppercase;font-size:.72em;letter-spacing:.05em}\
p.tip{color:#939ab7;font-size:.9em;margin-top:1.5rem}\
code,kbd{background:#363a4f;border-radius:.25rem;padding:.1rem .35rem;\
font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}";

fn escape_html(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for c in s.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&#39;"),
      _ => out.push(c),
    }
  }
  out
}

// ─── body builders ────────────────────────────────────────────────────

fn empty_body() -> BoxBody<Bytes, BodyError> {
  Full::new(Bytes::new())
    .map_err(|never| match never {})
    .boxed()
}

fn html_response(status: StatusCode, body: String) -> Response<BoxBody<Bytes, BodyError>> {
  static_response(status, "text/html; charset=utf-8", Bytes::from(body))
}

/// A response whose body we built here — a static page, an embedded file,
/// a stand-in JSON body — as opposed to one forwarded from an engine.
fn static_response(
  status: StatusCode,
  content_type: &str,
  body: Bytes,
) -> Response<BoxBody<Bytes, BodyError>> {
  Response::builder()
    .status(status)
    .header(CONTENT_TYPE, content_type)
    .body(Full::new(body).map_err(|never| match never {}).boxed())
    .expect("static response must build")
}

fn json_response(status: StatusCode, body: Value) -> Response<BoxBody<Bytes, BodyError>> {
  static_response(status, "application/json", Bytes::from(body.to_string()))
}

/// A fixed short-text answer: the bundle's 404, and the status a route the
/// proxy owns the answer to asks for. A body a browser can show, not a blank
/// page.
fn plain(status: StatusCode, why: &'static str) -> Response<BoxBody<Bytes, BodyError>> {
  static_response(
    status,
    "text/plain; charset=utf-8",
    Bytes::from_static(why.as_bytes()),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::daemon::supervisor::ManagedState;
  use crate::discovery::{DiscoveredModel, ModelSource, Multimodal};
  use crate::proxy::test_support::state_with;
  use std::path::{Path, PathBuf};

  /// The log matches a `/ui` request on its path alone, against the fixed
  /// routes the router forwards: a query string cannot hide a generation call,
  /// and nothing else the page asks for becomes a row.
  #[test]
  fn only_generation_routes_on_the_ui_surface_are_logged() {
    assert_eq!(
      logged_ui_route("/v1/chat/completions"),
      Some("/v1/chat/completions"),
      "the chat call is logged under its own path"
    );
    assert_eq!(
      logged_ui_route("/v1/responses?stream=1"),
      Some("/v1/responses"),
      "a query keeps the route logged, and stays out of the row"
    );
    for path in [
      "/",
      "/bundle.js",
      "/props",
      "/v1/models",
      "/v1/streams/lookup",
      "/v1/chat/completions/../props",
      "/v1/chat/completionsx",
    ] {
      assert_eq!(
        logged_ui_route(path),
        None,
        "{path} is page traffic, not a generation call"
      );
    }
  }

  /// The running list is Ready rows only, in launch order (`L2` before `L10`),
  /// each carrying its port and served name, and the full model id the forward
  /// path keys its in-flight guard on.
  #[tokio::test]
  async fn collect_running_lists_ready_launches_in_launch_order() {
    let state = state_with(&[
      ("L10", ManagedState::Ready, "/models/zulu.gguf", 41110),
      ("L2", ManagedState::Ready, "/models/alpha.gguf", 41102),
      ("L3", ManagedState::Loading, "/models/booting.gguf", 41103),
    ])
    .await;
    let running = collect_running(&state).await;
    assert_eq!(
      running
        .iter()
        .map(|e| (e.launch_id.as_str(), e.name.as_str(), e.port))
        .collect::<Vec<_>>(),
      vec![("L2", "alpha", 41102), ("L10", "zulu", 41110)],
      "Ready rows only, in launch order, each with its port and name"
    );
    assert_eq!(
      running[0].model_id.path,
      std::path::PathBuf::from("/models/alpha.gguf"),
      "the row carries the launched model's identity, not just a path string"
    );
    assert!(
      running.iter().all(|e| e.ui == UiSource::Engine),
      "the default backend's row still serves its own web UI, so it stays selectable"
    );
  }

  fn headers_with_cookie(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(COOKIE, hyper::header::HeaderValue::from_str(value).unwrap());
    h
  }

  fn entry(launch_id: &str, port: u16, name: &str) -> RunningEntry {
    entry_src(launch_id, port, name, UiSource::Engine)
  }

  /// A row that either has a UI of its own or has none at all (an embedding
  /// or rerank launch), for the rules that only care about that split.
  fn entry_ui(launch_id: &str, port: u16, name: &str, has_ui: bool) -> RunningEntry {
    entry_src(
      launch_id,
      port,
      name,
      if has_ui {
        UiSource::Engine
      } else {
        UiSource::None
      },
    )
  }

  fn entry_src(launch_id: &str, port: u16, name: &str, ui: UiSource) -> RunningEntry {
    RunningEntry {
      launch_id: launch_id.to_string(),
      model_id: ModelId {
        path: std::path::PathBuf::from(format!("/m/{name}.gguf")),
        header_blake3: [0u8; 32],
      },
      port,
      name: name.to_string(),
      ui,
    }
  }

  /// The three sources: a backend with a UI always uses its own, whatever it
  /// was launched for; a UI-less backend gets the bundle only for chat.
  #[test]
  fn ui_source_prefers_the_engine_then_the_bundle_then_nothing() {
    for mode in [LaunchMode::Chat, LaunchMode::Embedding, LaunchMode::Rerank] {
      assert_eq!(ui_source(true, mode), UiSource::Engine, "{mode:?}");
    }
    assert_eq!(ui_source(false, LaunchMode::Chat), UiSource::Bundled);
    assert_eq!(ui_source(false, LaunchMode::Embedding), UiSource::None);
    assert_eq!(ui_source(false, LaunchMode::Rerank), UiSource::None);
    // Only a launch with no UI at all is excluded from opening one.
    assert!(UiSource::Engine.is_selectable());
    assert!(UiSource::Bundled.is_selectable());
    assert!(!UiSource::None.is_selectable());
  }

  /// A bundled launch is a real target: a lone one opens directly and a
  /// cookie pin forwards to it, exactly like the backend's own UI.
  #[test]
  fn resolve_target_forwards_a_bundled_launch() {
    let lone = || vec![entry_src("L1", 41100, "noui", UiSource::Bundled)];
    match resolve_target(&HeaderMap::new(), lone()) {
      UiTarget::Forward(e) => assert_eq!(e.launch_id, "L1"),
      _ => panic!("a lone bundled launch opens directly"),
    }
    match resolve_target(&headers_with_cookie("ls_ui_target=L1"), lone()) {
      UiTarget::Forward(e) => assert_eq!(e.launch_id, "L1"),
      _ => panic!("a cookie pinned to a bundled launch forwards to it"),
    }
  }

  /// A bundled row is a green link; only a row with no UI at all stays red.
  #[test]
  fn chooser_links_a_bundled_row_and_keeps_a_ui_less_row_red() {
    let html = chooser_html(
      &[
        entry_src("L1", 41100, "bundled", UiSource::Bundled),
        entry_src("L2", 41101, "embed", UiSource::None),
      ],
      None,
    );
    assert!(
      html.contains(
        "<a class=\"has-ui\" href=\"/ui/?target=L1\"><span class=\"name\">bundled</span>"
      ),
      "a bundled row is a link: {html}"
    );
    let row = html
      .split("<li class=\"no-ui\">")
      .nth(1)
      .unwrap_or_default();
    let row = row.split("</li>").next().unwrap_or(row);
    assert!(
      !row.contains("<a "),
      "a UI-less row must not be a link: {row}"
    );
  }

  /// How a bundled launch's requests split after `/ui` is stripped: the
  /// proxy answers `/props`, the engine answers its `/v1/...` surface, and
  /// everything else is looked up in the embedded bundle.
  #[test]
  fn bundled_paths_split_props_forward_and_files() {
    assert!(matches!(classify_bundled("/props"), BundledRoute::Props));
    assert!(matches!(
      classify_bundled("/props?x=1"),
      BundledRoute::Props
    ));
    assert!(matches!(
      classify_bundled("/v1/models"),
      BundledRoute::Forward
    ));
    assert!(matches!(
      classify_bundled("/v1/chat/completions"),
      BundledRoute::Forward
    ));
    for path in ["/", "/slots", "/nope.js"] {
      assert!(
        matches!(classify_bundled(path), BundledRoute::File(p) if p == path),
        "{path} is a bundle lookup"
      );
    }
  }

  /// The routes only the bundle's origin engine serves get the proxy's own
  /// answer: a 403 on `/tools` is what the page reads as "server tools are
  /// disabled", and stream resume answers 404 for every engine.
  #[test]
  fn bundled_origin_only_routes_are_answered_locally() {
    assert!(matches!(
      classify_bundled("/tools"),
      BundledRoute::Refused {
        status: StatusCode::FORBIDDEN,
        ..
      }
    ));
    for path in [
      "/v1/stream",
      "/v1/stream?conv_id=c&from=4",
      "/v1/streams/other",
    ] {
      assert!(
        matches!(
          classify_bundled(path),
          BundledRoute::Refused {
            status: StatusCode::NOT_FOUND,
            ..
          }
        ),
        "{path} reports stream resume off"
      );
    }
    // The page asks this on every load, so it gets the origin engine's "no
    // stream for these ids" answer rather than an error to log.
    assert!(matches!(
      classify_bundled("/v1/streams/lookup"),
      BundledRoute::NoStreams
    ));
  }

  /// A dot segment must not smuggle a path past the `/v1` forward prefix:
  /// the forward client would collapse it into an engine route outside `/v1`.
  #[test]
  fn bundled_dot_segments_stay_inside_the_bundle() {
    for path in [
      "/v1/../../secret",
      "/../etc/passwd",
      "/a/../b",
      // Every spelling the URL parser the forward client uses resolves:
      // percent-encoded dots in any casing, and backslash separators.
      "/v1/%2e%2e/%2e%2e/metrics",
      "/v1/%2E%2e/%2E%2e/health",
      "/v1/.%2e/%2e./health",
      "/v1/x/\\..\\..\\health",
    ] {
      assert!(
        matches!(classify_bundled(path), BundledRoute::File(_)),
        "{path} is a bundle lookup, not a forward"
      );
    }
  }

  #[test]
  fn ctx_to_report_prefers_resolved_then_requested_then_zero() {
    assert_eq!(ctx_to_report(Some(8192), Some(4096)), 8192);
    assert_eq!(ctx_to_report(None, Some(4096)), 4096);
    assert_eq!(ctx_to_report(None, None), 0);
  }

  /// The paths a real page asks for are untouched by the guard: the client
  /// resolves each of them to itself.
  #[test]
  fn plain_bundled_paths_survive_the_normalization_guard() {
    for path in [
      "/",
      "/props",
      "/v1/models",
      "/v1/chat/completions",
      "/v1/streams/lookup",
      "/_app/immutable/bundle.Bkusd0vb.js",
    ] {
      assert_eq!(
        client_path(path).as_deref(),
        Some(path),
        "{path} must classify on its own spelling"
      );
    }
  }

  #[tokio::test]
  async fn launch_vision_follows_the_catalog_record() {
    let state = state_with(&[("L1", ManagedState::Ready, "/models/foo.gguf", 4001)]).await;
    // With no catalog row the launch reads as text-only.
    assert!(!launch_vision(&state, Path::new("/models/foo.gguf")).await);
    state
      .ctx
      .catalog
      .upsert(catalog_row(
        "/models/foo.gguf",
        Some(Multimodal {
          vision: true,
          audio: false,
        }),
      ))
      .await;
    assert!(launch_vision(&state, Path::new("/models/foo.gguf")).await);
  }

  fn catalog_row(path: &str, multimodal: Option<Multimodal>) -> DiscoveredModel {
    let path = PathBuf::from(path);
    DiscoveredModel {
      parent: path.parent().unwrap().to_path_buf(),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal,
      supported_backends: Vec::new(),
      mtp_head: None,
      path,
    }
  }

  /// The chooser's colour contract: a model whose backend serves a web UI is
  /// a green link, a model without one is red and is not a link.
  #[test]
  fn chooser_rows_are_green_with_a_ui_and_red_without() {
    let html = chooser_html(
      &[
        entry_ui("L1", 41001, "with-ui", true),
        entry_ui("L2", 41002, "no-ui", false),
      ],
      Some("L1"),
    );
    assert!(
      html.contains(
        "<a class=\"has-ui\" href=\"/ui/?target=L1\"><span class=\"name\">with-ui</span>"
      ),
      "a UI-serving row is a green-classed link: {html}"
    );

    let no_ui = html
      .split("<li class=\"no-ui\">")
      .nth(1)
      .expect("the UI-less row is listed");
    let no_ui = no_ui.split("</li>").next().unwrap_or(no_ui);
    assert!(
      !no_ui.contains("<a "),
      "a row with no web UI must not be a link: {no_ui}"
    );
    assert!(no_ui.contains("<span class=\"name\">no-ui</span>"));

    // The two classes must actually carry two different colours.
    let green = CSS
      .split("ul.models a.has-ui .name{color:")
      .nth(1)
      .and_then(|r| r.split('}').next())
      .expect("green rule");
    let red = CSS
      .split("li.no-ui .name,span.no-ui-text{color:")
      .nth(1)
      .and_then(|r| r.split('}').next())
      .expect("red rule");
    assert!(
      !green.is_empty() && !red.is_empty() && green != red,
      "{green} vs {red}"
    );
  }

  #[test]
  fn strip_ui_prefix_maps_root_and_assets() {
    let uri = |s: &str| s.parse::<hyper::Uri>().unwrap();
    assert_eq!(strip_ui_prefix(&uri("/ui/")), "/");
    assert_eq!(strip_ui_prefix(&uri("/ui/props")), "/props");
    assert_eq!(strip_ui_prefix(&uri("/ui/bundle.js?v=2")), "/bundle.js?v=2");
    assert_eq!(
      strip_ui_prefix(&uri("/ui/v1/chat/completions")),
      "/v1/chat/completions"
    );
  }

  #[test]
  fn read_target_cookie_extracts_pin() {
    assert_eq!(
      read_target_cookie(&headers_with_cookie("ls_ui_target=L3")),
      Some("L3".to_string())
    );
    // Alongside other cookies, any order.
    assert_eq!(
      read_target_cookie(&headers_with_cookie("foo=bar; ls_ui_target=L7; baz=1")),
      Some("L7".to_string())
    );
    // A different cookie sharing the prefix must not match.
    assert_eq!(
      read_target_cookie(&headers_with_cookie("ls_ui_target_other=L7")),
      None
    );
    // Empty value reads as no pin.
    assert_eq!(
      read_target_cookie(&headers_with_cookie("ls_ui_target=")),
      None
    );
  }

  #[test]
  fn target_query_param_validates_launch_id() {
    let uri = |s: &str| s.parse::<hyper::Uri>().unwrap();
    assert_eq!(
      target_query_param(&uri("/ui/?target=L4")),
      Some("L4".to_string())
    );
    // No target param.
    assert_eq!(target_query_param(&uri("/ui/?foo=bar")), None);
    // A value with header-illegal bytes is rejected (can't reach the
    // Set-Cookie builder). `%0d` stays literal since we don't decode,
    // but the `%` makes it implausible and it's dropped anyway.
    assert_eq!(target_query_param(&uri("/ui/?target=a%0db")), None);
  }

  #[test]
  fn resolve_target_prefers_valid_cookie() {
    let running = vec![entry("L1", 41100, "a"), entry("L2", 41101, "b")];
    match resolve_target(&headers_with_cookie("ls_ui_target=L2"), running) {
      UiTarget::Forward(e) => assert_eq!(e.launch_id, "L2"),
      _ => panic!("expected the cookie-pinned backend"),
    }
  }

  #[test]
  fn resolve_target_single_running_ignores_missing_cookie() {
    let running = vec![entry("L9", 41100, "solo")];
    match resolve_target(&HeaderMap::new(), running) {
      UiTarget::Forward(e) => assert_eq!(e.launch_id, "L9"),
      _ => panic!("expected the single running backend"),
    }
  }

  #[test]
  fn resolve_target_stale_cookie_falls_through_to_chooser() {
    // Cookie points at a launch that's no longer running, and two
    // models run → the chooser, not a dead forward.
    let running = vec![entry("L1", 41100, "a"), entry("L2", 41101, "b")];
    match resolve_target(&headers_with_cookie("ls_ui_target=L99"), running) {
      UiTarget::Chooser(e) => assert_eq!(e.len(), 2),
      _ => panic!("expected the chooser for a stale cookie + 2 running"),
    }
  }

  #[test]
  fn resolve_target_zero_running_is_none() {
    assert!(matches!(
      resolve_target(&HeaderMap::new(), Vec::new()),
      UiTarget::None
    ));
  }

  #[test]
  fn resolve_target_lone_ui_less_model_shows_chooser_not_none() {
    // A single running UI-less model must NOT auto-pin and must NOT
    // read as "nothing running" — it renders the chooser (D-ui).
    let running = vec![entry_ui("L1", 41100, "deepseek-v4-flash", false)];
    assert!(matches!(
      resolve_target(&HeaderMap::new(), running),
      UiTarget::Chooser(_)
    ));
  }

  #[test]
  fn resolve_target_ui_less_is_excluded_from_autopin_and_cookie() {
    // With one UI-less + one llama (UI) running and no cookie, the
    // chooser lists both — UI-less non-selectable, llama selectable.
    let running = vec![
      entry_ui("L1", 41100, "deepseek-v4-flash", false),
      entry_ui("L2", 41101, "qwen3", true),
    ];
    match resolve_target(&HeaderMap::new(), running) {
      UiTarget::Chooser(entries) => {
        let html = chooser_html(&entries, None);
        assert!(
          html.contains("no web UI"),
          "UI-less row marked no-ui: {html}"
        );
        assert!(html.contains("/ui/?target=L2"), "llama row selectable");
        assert!(!html.contains("/ui/?target=L1"), "UI-less row not a link");
      }
      _ => panic!("expected chooser"),
    }
    // A cookie pinned to the UI-less model is ignored (it can't serve UI).
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::COOKIE, "ls_ui_target=L1".parse().unwrap());
    let pinned = vec![entry_ui("L1", 41100, "deepseek-v4-flash", false)];
    assert!(
      matches!(resolve_target(&headers, pinned), UiTarget::Chooser(_)),
      "cookie pin to a UI-less model must not forward"
    );
  }

  #[test]
  fn chooser_html_links_each_model_and_escapes() {
    let running = vec![entry("L1", 41100, "qwen3"), entry("L2", 41101, "gem<ma")];
    let html = chooser_html(&running, None);
    assert!(html.contains("/ui/?target=L1"));
    assert!(html.contains("/ui/?target=L2"));
    assert!(html.contains(":41100"));
    // The angle bracket in the model name is escaped, not raw.
    assert!(html.contains("gem&lt;ma"));
    assert!(!html.contains("gem<ma"));
    // The switcher hint surfaces /ui/switch for discoverability.
    assert!(html.contains("/ui/switch"));
    // No active pin passed → no rendered `current` badge (the CSS rule
    // `.current` is always present, so match the badge text, not the class).
    assert!(!html.contains(">current<"));
  }

  #[test]
  fn chooser_html_marks_the_active_model() {
    let running = vec![entry("L1", 41100, "a"), entry("L2", 41101, "b")];
    let html = chooser_html(&running, Some("L2"));
    // The active entry carries the `current` badge; the other does not.
    assert!(html.contains("class=\"current\">current"));
    assert_eq!(html.matches(">current<").count(), 1);
  }

  #[test]
  fn is_switch_path_matches_with_and_without_trailing_slash() {
    assert!(is_switch_path("/ui/switch"));
    assert!(is_switch_path("/ui/switch/"));
    assert!(!is_switch_path("/ui/"));
    assert!(!is_switch_path("/ui/switchboard"));
  }

  #[test]
  fn active_pin_only_when_cookie_maps_to_a_running_model() {
    let running = vec![entry("L1", 41100, "a"), entry("L2", 41101, "b")];
    assert_eq!(
      active_pin(&headers_with_cookie("ls_ui_target=L2"), &running),
      Some("L2".to_string())
    );
    // Stale pin (not running) → no active mark.
    assert_eq!(
      active_pin(&headers_with_cookie("ls_ui_target=L9"), &running),
      None
    );
    // No cookie at all.
    assert_eq!(active_pin(&HeaderMap::new(), &running), None);
  }

  #[test]
  fn no_model_html_points_at_tui_and_cli() {
    let html = no_model_html();
    assert!(html.contains("No model running"));
    assert!(html.contains("llamastash start"));
  }
}
