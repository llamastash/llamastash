# Next Batch 1: proxy request checks, fallback and media listing

Date: 2026-11-13. Source: `TODO.md` § Next, Batch 1. All three items were found by reading the code, not reproduced. Each unit starts with a failing test that shows the bug.

## Goal

1. A web page cannot drive the loopback proxy (CSRF and DNS rebinding).
2. A fallback never answers with a model that does not serve the request's mode, and embeddings / rerank never fall back.
3. `/v1/models` lists a running media launch under its declared mode, the same as `status`.

## Scope and decisions

### U1: Host and Origin checks

- New pure helpers in `src/proxy/auth.rs` (it already decides who may call), with unit tests:
  - `host_is_loopback(host: &str) -> bool`: strip an optional `:port` (and the `[...]` of an IPv6 literal). Accept `localhost`, any `*.localhost` name (RFC 6761, browsers resolve it to loopback, so a rebinding page cannot own it), and any IP literal where `IpAddr::is_loopback()` is true (`127.0.0.0/8`, `::1`). Case-insensitive. Refuse everything else.
  - `origin_is_allowed(origin: &str, host: Option<&str>) -> bool`: allow when `origin` is `http://<host>` or `https://<host>` (case-insensitive; `https` is a TLS reverse proxy in front, which `SECURITY.md` recommends for LAN mode). Allow an origin whose scheme is not `http` or `https` (`chrome-extension://`, `moz-extension://`, `safari-web-extension://`, `vscode-webview://`, `tauri://`): a web page cannot send those, only an installed extension or app can, and Ollama allows them by default. Refuse `null` (a sandboxed iframe or a `file://` page sends it, so a page can forge it) and every other `http`/`https` origin.
- `ProxyState` gains `pub(crate) loopback_bind: bool`. `from_context_with_auth` takes it as a new last argument; `from_context` passes `true`. Callers: `src/daemon/mod.rs` (pass the configured proxy host's `is_loopback()`; the `host` variable is already there), `src/proxy/server.rs:505`, and any test caller of `from_context_with_auth`.
- In `router::route`, before the auth check and before the route table, for every path (including `GET /`, `/health`, `/ui*`):
  - When `loopback_bind && !state.auth.enforced()` and a `Host` header is present and `!host_is_loopback(host)`: `403` JSON error, code `forbidden_host`, message names the host and says the loopback proxy only answers `localhost` / `127.0.0.1` / `[::1]`. A missing `Host` passes: a browser always sends one.
  - In every mode (loopback or LAN, key or no key): when an `Origin` header is present and `!origin_is_allowed(origin, host)`: `403`, code `forbidden_origin`. Reason for LAN mode too: a browser caches the `/ui` Basic credentials and sends them on a cross-site form POST.
  - Use the existing `error_response` helper. These refusals are not request-log rows (they happen before `forward_request`), same as auth refusals.
- No CORS change. `OPTIONS` already falls through to 404, and a refused cross-site request never reaches an engine, so engine CORS headers (vLLM, SGLang `Access-Control-Allow-Origin: *`) no longer let a page read anything through the proxy port. This answers the TODO's open question.
- No Content-Type check (user decision 2026-11-13): it would break `curl -d` without `-H 'Content-Type: application/json'`, and the Origin check already blocks a browser, since every browser sends `Origin` on a cross-site POST.
- No config knob for extra origins. Not asked for.

### U2: mode-safe fallback

User decision 2026-11-13: fallback stays on by default; candidates must serve the request's mode; embeddings and rerank never fall back.

- `handle_not_running` in `src/proxy/route.rs`, `LaunchOutcome::Failed` arm: when `endpoint_mode` is `Some(Embedding)` or `Some(Rerank)`, return `launch_failed_response` with no fallback. Reason, in one comment line: another model's vectors or scores never match what the client indexed.
- `collect_fallback_candidates` takes the `endpoint_mode` and keeps a Ready launch only when it serves that mode:
  - Media mode (`mode.is_declared_only()`): the launch's backend `declares_mode(path, server, mode)`.
  - `None` (chat, completions, messages, responses, systemone): the launch's own mode (`RunningSnapshot.params.mode`, found by port in `state.ctx.state.snapshot().running`, as the `serves_mode` gate in `router.rs` does) is `LaunchMode::Chat`, the backend `serves_mode(path, server, LaunchMode::Chat)`, and the catalog row's mode hint is not a declared-only media mode (an `stt` entry launches with mode `chat`).
  - Backend lookup: `route::running_model_backend`. A launch whose backend cannot be resolved is not a candidate.
  - Keep one helper for the "does this running launch serve mode X" question if the `router.rs` Ready-arm gate can call it too without growing; do not force it.
- `fallback_reason_for`: `(None, None)` becomes `family_mismatch`. Update the doc comment (it names the embedding-to-chat case, which can no longer happen) and the unit test at `route.rs:1218`.
- `pick_fallback` in `src/proxy/mru.rs` is unchanged.

### U3: `/v1/models` media mode

User decision 2026-11-13: print the declared mode.

- First add a test in `tests/proxy_models.rs` (or the closest existing file) with a generic entry on the fake llama server with `modes: [stt]`, started unnamed and started with `--name`. Assert `/v1/models` prints `stt` for both rows. Find which row prints `chat` today (expected: the named-launch row built in `list_models` from `r.params.mode.label()`).
- Fix: for a running launch row, print the catalog row's `mode_hint` when it names a declared-only media mode (`image`/`tts`/`stt`), else the launch's mode. Same rule as the `status` MODE cell from `docs/plans/2026-11-01-001-fix-r18-batch-3-plan.md`. Reuse the existing helper if one exists (`cli/session.rs` has `declared_media_mode`; check `src/ipc/status.rs` for the status rule).

### Docs (same commits as the code)

- `SECURITY.md` § Threat model: only the control-plane token is protected by file-system permissions; the proxy is not. Add the Host / Origin checks to the proxy bullet. Add to in-scope issues: a web page that can drive or read the loopback proxy.
- `docs/usage.md` § Proxy: one short paragraph on the Host / Origin checks, the `403 forbidden_host` / `forbidden_origin` codes, and which origins pass. Line ~1026 (`backend.vllm.cors`): the proxy port now refuses a foreign page, so the cross-origin read is only on the engine's own port. Line ~1049 (SGLang): same if it says the proxy relays. Line ~1148 (fallback): mode-matched candidates, embeddings / rerank never fall back, missing arch is `family_mismatch`. Line ~1152: a running media launch prints its declared mode.
- `docs/architecture.md` line ~79 (proxy) and ~254 (fallback): same facts, one clause each.
- `docs/troubleshooting.md`: one entry for a browser app or extension that gets `403 forbidden_origin` / `forbidden_host`.
- `CHANGELOG.md` `[Unreleased]`: one-liners under the right heading (Security / Fixed).
- `TODO.md` § Next Batch 1: tick the three items `- [x]` with strike-through and `— **done 2026-11-13**` plus a link to this plan, matching the Batch 4 entry's format.

## Units

All three units touch `src/proxy/router.rs` and `docs/usage.md`, so they run in one lane, in order U1, U2, U3. One commit per unit.

| Unit | Files | Depends on |
|---|---|---|
| U1 | `src/proxy/auth.rs`, `src/proxy/router.rs`, `src/proxy/state.rs`, `src/proxy/server.rs`, `src/daemon/mod.rs`, test callers of `from_context_with_auth`, `tests/proxy_routing.rs` (or a new `tests/proxy_request_checks.rs`), `SECURITY.md`, `docs/usage.md`, `docs/architecture.md`, `docs/troubleshooting.md`, `CHANGELOG.md` | — |
| U2 | `src/proxy/route.rs`, maybe `src/proxy/router.rs`, `tests/proxy_fallback.rs`, `docs/usage.md`, `docs/architecture.md`, `CHANGELOG.md` | U1 (shared files) |
| U3 | `src/proxy/router.rs`, `tests/proxy_models.rs`, `docs/usage.md`, `CHANGELOG.md`, `TODO.md` | U2 (shared files) |

### Tests to add

- U1 unit: `host_is_loopback` accepts `127.0.0.1:11435`, `localhost`, `LOCALHOST:1`, `foo.localhost:11435`, `[::1]:11435`, `127.1.2.3`; refuses `evil.example`, `localhost.evil.example`, `192.168.1.5:11435`, `[::ffff:8.8.8.8]`, empty. `origin_is_allowed` accepts `http://127.0.0.1:11435` with host `127.0.0.1:11435`, `chrome-extension://abc`; refuses `null`, `http://evil.example`, `https://127.0.0.1:11435` (scheme differs), `http://localhost:11435` with host `127.0.0.1:11435`.
- U1 integration (real proxy on a temp state dir, fake llama server): a `text/plain` POST to `/v1/chat/completions` with `Origin: http://evil.example` gets `403 forbidden_origin` and starts no launch; `Host: evil.example` gets `403 forbidden_host`; the same request with the proxy's own `Origin` passes; a `/ui/` same-origin request still works (extend `tests/proxy_ui.rs` if a cheap test fits).
- U2 integration in `tests/proxy_fallback.rs`: an `/v1/embeddings` request whose launch fails, with a chat model Ready, gets `503 launch_failed`; a chat request whose launch fails, with only an embedding-mode launch Ready, gets `503`; a chat request with a chat launch Ready still falls back. Use the existing fallback test setup.
- U3: the test described in U3.

## Setup

None beyond what the lane's environment already sets. Lanes and the orchestrator use:

```bash
export CARGO_TARGET_DIR=/mnt/work/Workspace/oss-libs/llamastash-rs/llamastash/target CARGO_HUSKY_DONT_INSTALL_HOOKS=true
```

## Verification

Run in the worktree, with the exports above:

```bash
cargo check --all-targets --features test-fixtures
cargo nextest run --features test-fixtures --lib proxy
cargo nextest run --features test-fixtures --test proxy_routing --test proxy_fallback --test proxy_models --test proxy_ui --test proxy_ollama_compat --test proxy_listener_test
```

Plus any new integration file. The pre-commit hook runs `cargo fmt`, `make lint` and `make doc`; do not run them by hand. The orchestrator runs the E2E check (isolated daemon, `curl` with a foreign `Origin` and `Host`) and the full suite.

## Definition of done

- The three TODO items behave as decided above, each with a test that failed before the fix.
- Docs listed above match the code. `TODO.md` items ticked.
- Verification commands pass.
