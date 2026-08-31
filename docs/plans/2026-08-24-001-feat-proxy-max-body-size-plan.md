# Proxy `max_body_size` config key + default cap 2 MiB → 16 MiB

**Status:** ✅ done (2026-08-24). Note: stages were verified with separate `make test` + `make lint` checkpoints but shipped as a single commit (docs ride with the behavior, per the docs-ship-with-code rule).

Closes [#65](https://github.com/llamastash/llamastash/issues/65) (vision payloads > 2 MiB hit the hard-coded proxy body cap → 413). Maintainer decision on the issue thread: add the config key **and** raise the default — settled on **16 MiB** (covers the reporter's 10 MiB case with zero config; one base64 phone photo + long history; 2 concurrent max-size requests = 32 MiB RAM on a box already running a 27B vision model).

## Context — where the cap lives today

- `src/proxy/route.rs:42` — `pub const BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;` applied via `http-body-util::Limited` in `buffer_body` (overflow → `BodyError::TooLarge` → HTTP 413).
- Three call sites, all capped at the constant: `route::buffer_and_extract` → `router.rs:173` (`forward_request`, all `/v1/*` + `/v1/messages`) and `router.rs:474` (`ollama_show`, `POST /api/show`); `route::buffer_body(body, BODY_LIMIT_BYTES)` → `ui.rs:224` (`forward_ui`).
- The 413 message hard-codes the unit: `body_error_response` renders `"request body exceeds the {} MiB limit"` with `BODY_LIMIT_BYTES / (1024*1024)` — wrong for any non-MiB cap.
- Config: `ProxyConfig` (`src/config/loader.rs:247`, `#[serde(deny_unknown_fields)]`), resolved once per daemon; per-daemon knobs flow into `ProxyState` at startup (`daemon/mod.rs:583`, `from_context_with_auth`) — the `fallback_enabled` pattern this key follows.
- Test to keep green (or fix): `tests/proxy_routing.rs:560` `body_exceeding_two_mib_returns_413` (2 MiB+16 body, asserts 413 + `payload_too_large` envelope).
- Origin: plan `2026-05-21-001` Key Decision line 88 — "Not exposed in `[proxy]` config at v1." That decision predated vision models; this plan supersedes it. The dated plan is a protected historical record and is **not** edited.

## Design

| Decision | Value |
| --- | --- |
| Key | `proxy.max_body_size` — **bytes**, `usize` (matches the issue's example; no human-size strings, no human-`status` surface) |
| Default | **16 MiB** (`16 * 1024 * 1024`) — `BODY_LIMIT_BYTES` renamed `DEFAULT_BODY_LIMIT_BYTES`, becomes the serde default |
| 0 | Legal, documented: rejects every non-empty body (a `Limited` cap of 0 trips on the first byte). No new error type, no validation pass. **Superseded, see Decisions** — `0` now disables the check (no cap) |
| Scope | One global cap for every body the proxy buffers — `/v1/*`, `/v1/messages*`, `/api/show`, `/ui` forwards |
| Plumbing | `ProxyConfig.max_body_size` → `ProxyState.max_body_size` (set at daemon startup, like `fallback_enabled`) → threaded into `buffer_and_extract(body, cap)` and `body_error_response(err, cap)` |
| 413 message | `request body exceeds the {cap} limit` with a small private `format_bytes` — largest unit the value fits in, whole when exact (`16 MiB`, `1 GiB`), two decimals otherwise with trailing zeros trimmed (`976.57 KiB`, never `2.00 MiB`); sub-KiB stays raw bytes (`512 B`). **Superseded, see Decisions** — the private `format_bytes` was deleted and the message now carries the exact byte count via the shared `fmt_bytes` |
| CLI / env | **None** — config only (`--proxy-max-body-size` deferred, see below) |
| `status --json` | Unchanged — the `proxy` block is runtime state (listen/auth/status), not config |

## Stages

Each stage compiles + passes `make test`; commit per stage, docs ride with stage 3.

**Stage 1 — config key.**
`ProxyConfig.max_body_size: usize` + `#[serde(default = "ProxyConfig::default_max_body_size")]` (returns `crate::proxy::route::DEFAULT_BODY_LIMIT_BYTES`), into the `Default` impl (`src/config/loader.rs:409`). Loader unit tests: YAML `max_body_size: 10485760` parses; omitted → 16 MiB default; the existing `deny_unknown_fields` typo test (`loader.rs:2305`) still rejects misspells. Exhaustive `ProxyConfig` literals in `src/cli/daemon.rs:1139,1348,1422,1485` either gain the field or switch to `..ProxyConfig::default()`.

**Stage 2 — plumbing + default bump.**
- `route.rs`: `BODY_LIMIT_BYTES` → `pub const DEFAULT_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024`; `buffer_and_extract(body, cap)`; `body_error_response(err, cap)` + private `format_bytes` with unit tests (KiB / MiB / odd byte counts).
- `ProxyState` gains `max_body_size: usize`; `from_context` / `from_context_with_auth` gain the parameter (production call site `daemon/mod.rs:583` passes `opts.proxy.max_body_size`; test call sites `server.rs:474,500,733,767` + `route.rs` tests pass `DEFAULT_BODY_LIMIT_BYTES`).
- `ui.rs:224` passes `state.max_body_size`; `router.rs:173,474` pass `state.max_body_size`.
- Refresh the now-stale "2 MiB" comments (`route.rs:10,41,117,137,147,204`, `router.rs:168`, `ui.rs:221`, `forward.rs:86`) so none rot.

**Stage 3 — integration tests + docs (one commit).**
- `tests/proxy_routing.rs`: `proxy_state_with` helper gains a body-cap variant. Retarget `body_exceeding_two_mib_returns_413` → cap 1 KiB, 2 KiB body, asserts 413 + envelope + message containing `1 KiB`. New: cap 4 MiB, 3 MiB body, no running model → **503 `launch_failed`** (proves the body was accepted past the old 2 MiB wall). New: default-state 3 MiB body passes the (now 16 MiB) cap. Check `tests/proxy_ui.rs` for a body-bearing POST to `/ui/*` and add a cap case there if one exists.
- Docs (same commit as stage 3): `docs/usage.md` — §Endpoints body-cap paragraph + error-table 413 row + Configuration yaml sample gain `max_body_size` (default 16 MiB, bytes, 0 = reject-all); `config.example.yaml` `[proxy]` block gains the key with the standard `Sources — CLI: (none) · Env: (none)` comment; `docs/architecture.md` if its proxy section enumerates config keys; `CHANGELOG.md` `[Unreleased]` one-liner under `### Added` + `### Changed` (default 2 → 16 MiB, `(#65)`); `TODO.md` one-liner linking the issue, struck in this same commit.

## Explicitly deferred (not gaps)

- `--proxy-max-body-size` / `LLAMASTASH_PROXY_MAX_BODY_SIZE` overrides — config-only for now.
- `status.proxy.max_body_size` — config is not runtime state.
- Human-size strings in config (`"16MiB"`) — raw bytes, per the issue.
- Per-route caps (e.g. `/ui` separate from `/v1/*`).
- Upstream (llama-server / ds4-server / lemond) body limits — verified against, never changed.

## Decisions (pinned, not gaps)

Raised at plan review, shipped as-is, and now locked in as decisions rather than open questions (PR #71 review):

- **`0` means no check, not reject-all.** (Flipped at PR #71 review, 2026-08-29.) `max_body_size: 0` disables the body check — one request may buffer arbitrary RAM — matching how the sibling keys read (`idle_ttl_secs: 0` disables eviction) and nginx's `client_max_body_size 0` ("Setting size to 0 disables checking of client request body size"). The earlier reject-all reading had no use case `proxy.enabled: false` doesn't cover better, and effectively-unlimited was already reachable via a large literal (`99999999999` parses and is honoured), so refusing the clean spelling was inconsistent, not safer. Caveat documented, not implied: nginx's `0` is safe because nginx spools to disk; we buffer the whole body in RAM with no concurrency limit, so `0` is worded as "no cap, one request can buffer arbitrary RAM," not nginx-equivalent. Locked in by `zero_cap_disables_the_body_check` and documented in `config.example.yaml` + `usage.md` + the `ProxyConfig` doc. Pre-1.0 is the free moment; after release it's a breaking change.
- **No upper bound on the value.** `max_body_size: 99999999999` parses and is honoured — one request may buffer that much RAM. The cap is per request body, not a global pool, so N concurrent max-size bodies buffer up to N × the cap. On loopback that is a runaway-agent problem, not an attack; on a LAN-exposed proxy (`proxy.host`) a buggy authenticated client can OOM the daemon. Deliberate: no validation pass, no ceiling. `usage.md` states the per-request (not global) semantics.
- **Key name is `max_body_size`, not `max_body_bytes`.** Every other unit-carrying key in `ProxyConfig` is suffixed (`header_read_timeout_secs`, `idle_ttl_secs`), so `max_body_bytes` would match. The issue proposed `max_body_size` and it is already in `config.example.yaml` + two docs, so the name stays; the inconsistency with its neighbours is accepted, not a gap.
- **413 message carries the exact byte count** (refined post-plan, PR #71 review): `request body exceeds the {human} ({raw} bytes) limit`, where `{human}` is the canonical `init::detection::fmt_bytes`. The plan's private `format_bytes` (round-to-nearest, two decimals) rendered a cap just under a unit boundary as the boundary itself (2 MiB − 1 → "2 MiB"), so a client sending exactly 2 MiB was told it "exceeds the 2 MiB limit" — a contradiction. Carrying the raw count keeps it honest and reads better for agents parsing the error; it also drops the fourth byte formatter in the tree in favour of the shared `fmt_bytes` (whose doc comment exists precisely because earlier copies drifted on units and decimals).

## Verification

1. `make test` (`--features test-fixtures`) + `make lint` after every stage.
2. E2E on a real daemon + real vision model (no fakes — the cap is wire behaviour; real servers per project memory: ds4-server `:41100`, lemond `:13305`, or a daemon-managed `llama-server`):
   - Isolated daemon: `LLAMASTASH_STATE_DIR=$(mktemp -d)` + non-default `--proxy-port`, `proxy.max_body_size: 31457280` in that dir's config. `cargo build --bin llamastash`, `target/debug/llamastash daemon stop && start`, `status --json | jq .proxy`.
   - `POST /v1/chat/completions` with a base64 image, body **between 2 MiB and the cap** (e.g. ~10 MiB) → 200 streamed completion (was 413 before; also proves the real upstream accepts > 2 MiB bodies end-to-end).
   - Same daemon, body **over** the configured cap → 413 `payload_too_large` with the `30 MiB` message.
   - Default daemon (no key set): ~10 MiB body now passes (the headline default-bump behaviour change).
3. When E2E catches something the suite missed, add the regression test first (per AGENTS.md).
