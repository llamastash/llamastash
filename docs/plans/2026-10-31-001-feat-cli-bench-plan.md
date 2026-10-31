---
title: "feat(cli): llamastash bench, per-preset speed numbers"
type: feat
status: active
date: 2026-10-31
---

# feat(cli): `llamastash bench`, per-preset speed numbers

## Overview

Add `llamastash bench <model-ref> [--preset a,b]`. For each preset it starts the model, waits for Ready, sends a fixed prompt N times through the proxy, reads the speed numbers the proxy's request log already records for those requests, stops the launch it started, and prints one row per preset (table on a TTY, tab-separated when piped, `--json` for agents).

Source: `TODO.md` § R18 Batch 5. Lets a user compare knob changes without the maintainer scripts in `scripts/bench/`.

## Requirements Trace

- R1. `bench <model-ref>` with no `--preset` benches one cycle with the model's `default:` preset, the same baseline a plain `start` uses. `--preset a,b` (comma list) benches each named preset in order; `auto` (pure fit) is a valid entry.
- R2. Per preset: start, wait for Ready, send the prompt `--runs N` times (default 3), stop the launch it started.
- R3. Numbers per preset come from the same field lookup as live speed stats and `llamastash requests`: prompt tok/s (`prefill_tokens_per_second`), generation tok/s (`tokens_per_second`), first-token time (`prefill_end_ms`), plus load time (start to Ready, client clock).
- R4. A server that reports no speed fields still gets numbers: the proxy's estimates, marked estimated (`~` in the table, `*_estimated: true` in JSON), exactly as `llamastash requests` shows them.
- R5. Flags: `--runs N`, `--max-tokens N` (default 256), `--prompt TEXT` / `--prompt-file PATH` (mutually exclusive, replace the built-in prompt), `--json`, and every per-launch override `start` takes (`--ctx`, `--reasoning`, `--mode`, knob flags, `-- <extra>` tail, `--backend`, `--server`, `--mtp`, `--mtp-draft-n`, `--port`), layered over each preset exactly as `start` layers them.
- R6. The model already running (any live launch of that catalog row, state not `stopped`/`error`) refuses up front, before any launch, naming the running launch. Exit `USAGE`.
- R7. Ctrl-C mid-bench stops the launch bench started before exiting.
- R8. Docs: `docs/usage.md` section, CHANGELOG one-liner, `TODO.md` Batch 5 item struck.

## Scope Boundaries

- Chat models only. A row whose resolved mode is not chat (embedding, rerank, image, stt) is refused up front with `USAGE`.
- No new IPC method. Everything runs client-side over existing methods (`status`, `presets_show`, `start_model`, `stop_model`, `requests_tail`) plus one proxy request per run.
- No `--force` and no make-room. A CLI `start_model` never evicts other launches (make-room lives only on the proxy auto-start path, `src/proxy/launch.rs` → `src/proxy/eviction.rs`), so a preset that does not fit is refused by admission; bench records that preset as failed and moves on.
- No concurrency / parallel-stream benching, no warm-up request, no saving results to disk, no comparison against `scripts/bench/` output.
- The maintainer scripts in `scripts/bench/` are unchanged.

## Context & Research

### Relevant Code and Patterns

- `src/cli/start.rs` `handle`: preset baseline (`fetch_preset_params`, `PartialParams`), `resolve_mode`, the override layering (`--ctx` / `--port` / `--reasoning` / `--mtp` / `--mtp-draft-n` / knob tokens via `parse_cli_knobs` / `-- <extra>`), the `--ctx` > native warning, the foreign `--server` pin drop (`pinned_server_is_foreign`), `build_payload`, `map_start_error`. `poll_until_settled` (already `pub(crate)`) waits for Ready/Error/Stopped. `start_row_with_preset` is a smaller existing reuse of the same pieces (used by the session launcher).
- `src/cli/cli_args.rs` `StartArgs`: the override fields bench must share.
- `src/cli/stop.rs`: how `stop_model` is called (`call_with_timeout`, `STOP_FAILED`).
- `src/cli/session.rs`: already has what bench needs to reach the proxy. Reuse, do not copy: `published_id`, `join_named_reference` (builds `<id>@<name>`), the proxy-URL lookup with its error, the key / `KEYLESS_STUB` logic, and `not_chat`. Make them `pub(crate)` (or move them to a shared spot in `src/cli/`) as needed.
- `src/cli/resolve.rs`: `fetch_catalog`, `fetch_status`, `RunningRow` (`launch_id`, `model_path`, `name`, `state`, `port`), `resolve_model_with_candidates`, `model_miss`.
- `src/launch/resolve.rs` `published_ids`: the id the proxy publishes a catalog row under. A named launch is reachable as `<published id>@<name>`.
- `src/init/external/proxy_url.rs` `from_proxy_block(status["proxy"])` → `http://host:port/v1`; `config.proxy.effective_api_key()` (`src/config/loader.rs`), with `KEYLESS_STUB` from `src/cli/api_key.rs` when unset.
- `src/proxy/request_log.rs` `RequestRow` (fields above) and `RequestRow::cells` for the `~` formatting; `requests_tail` (`src/ipc/methods.rs`) takes `launch_id` + `limit`.
- `src/cli/requests.rs`: how the CLI fetches and renders request rows; reuse its fetch/deserialize helpers and number formatting.
- `src/cli/output.rs` / `src/cli/colors.rs`: TTY-gated padded tables vs `\t` piped output, `pretty_json`.
- `src/cli/logs.rs`: an existing `tokio::signal::ctrl_c()` select in a CLI command.
- `src/tui/oai_client.rs`: reqwest client settings for streaming (no total timeout, read timeout).
- `tests/fixtures/fake_llama_server.rs`: a chat body containing `__TEST_EMIT_TIMINGS__` streams chunks with a `timings` object (`prompt_per_second` 38.9, `predicted_per_second` 38.64). A plain body streams with neither `usage` nor `timings`, so the proxy has no token count and `tokens_per_second` stays `null` (asserted in `tests/proxy_routing.rs`); it is not the estimate path.
- `tests/session_cli.rs`: the template for the integration test (real binary daemon, fake engine, `embed.gguf` fixture, own proxy port and `port_range`). It has no presets: copy the `presets:` YAML from `tests/cli_integration_test.rs`, `tests/proxy_eviction.rs` or `tests/proxy_fallback.rs`.

## Key Technical Decisions

- **Requests go through the proxy, numbers come from the request log.** The proxy already parses `usage` / `timings` / `metrics`, estimates when the server reports nothing, and records `prefill_end_ms`. Reading the row back gives R3 and R4 with zero new parsing. Sending straight to the launch port would mean re-implementing the tap and the estimate rules.
- **Reading the row back.** The proxy logs the row as `in_flight` at the first byte and finishes it only when the response body is dropped (`Drop for LoggedResponse`, `src/proxy/forward.rs`), which can trail the client's read by a scheduler tick (`src/test_support.rs` polls for the same reason). And `requests_tail {launch_id}` only ever returns that launch's rows, so it cannot reveal a request the proxy served elsewhere. So: before each request, read the newest `seq` for the model (`requests_tail {model_path, limit 1}`); after draining the body, poll `requests_tail {model_path}` until a row with a larger `seq` and `state != "in_flight"` appears, with a 5 s deadline. Then require that row's `launch_id` to be bench's own launch and its status < 400. A row from another launch is a failure for that preset; if that row's `auto_start` is set, stop that launch too, since bench caused it.
- **Each request streams** (`stream: true`, `stream_options: {include_usage: true}`, `temperature: 0`, `max_tokens`). Only a streamed row gets `prefill_end_ms`, which is the first-token time. The CLI drains the body and does not parse it.
- **Request model string:** the row's published id, plus `@<name>` when the launch's status row carries a `name` (a `--preset` launch takes the preset's name). Since R6 refuses when the model already runs, bench's launch is the only one of that model.
- **Defeat the prompt cache with a run prefix.** Each run's user message starts with `Run <i>.` so the KV cache misses from the first user token and every run re-computes the prompt. Engine-neutral (no engine-specific body field).
- **Built-in prompt:** a fixed instruction plus a fixed paragraph repeated to roughly 1k tokens, built at runtime from a short constant. `--prompt` / `--prompt-file` replace it; the run prefix still applies.
- **Aggregate = median per metric over the successful runs.** Robust to a slow first run without adding a warm-up. A metric is marked estimated when any run's value was estimated. JSON also carries the per-run rows.
- **Fail fast before launching anything:** resolve the model, check R6, check chat mode, and fetch every named preset's baseline (`presets_show`) up front, so an unknown preset name is a usage error before the first launch.
- **Per-preset failure is a row, not an abort.** Admission refusal, load error, a request error, or a row served by a different launch marks that preset `error: "<message>"`, stops whatever bench started for it, and continues with the next. Exit `LAUNCH_FAILED` when any preset failed, `0` otherwise. The table and JSON are printed either way.
- **Share the override layering with `start` instead of copying it.** Move the override fields of `StartArgs` into one `#[command(flatten)]` struct both `StartArgs` and the new `BenchArgs` embed, and extract the "baseline params → layered overrides → payload" part of `start::handle` into one `pub(crate)` function both commands call. `start`'s behavior, flags, and tests stay the same.
- **Chat check.** Up front: refuse when `--mode` names a non-chat mode or the catalog row is not chat, reusing `not_chat` from `src/cli/session.rs` (do not rely on `resolve_mode`'s return value: it is `None` for a mode the header merely implies). After each launch reaches Ready: if the running row's `mode` is not chat (a `default:` or named preset can pin `mode:`), stop it and record an error for that preset.
- **`--preset auto`.** Handled like `start --preset auto`: no `presets_show`, no `preset` in the payload, `selection: "auto"`, and the launch stays unnamed. Rendered as `auto` in the table and JSON.
- **Load time.** Measured on the client from the `start_model` call to the first `status` poll that shows the launch `ready` (a small Ready-only poll, ~250 ms interval, same 900 s ceiling). Do not use `poll_until_settled` for this: it waits up to 5 s after Ready for `resolved_ctx`, which would inflate the number.
- **Ctrl-C:** the bench loop runs inside a `tokio::select!` against `ctrl_c()`; on interrupt, stop the current launch (if `start_model` was still in flight and no `launch_id` is known yet, find the launch for the model path in `status`, which R6 makes unique, and stop that), print `interrupted; stopped <launch>` to stderr, and exit `UNKNOWN` with no results output. (`logs` treats Ctrl-C as a clean detach and exits 0; bench did not finish, so it must not report success.)

## Open Questions

### Resolved During Planning

- No `--preset`: default only (user, 2026-10-31).
- Model already running: refuse up front (user, 2026-10-31).
- Flags: `--runs`, `--max-tokens`, `--prompt` / `--prompt-file`, plus all of `start`'s per-launch overrides, reusing `start`'s functions (user, 2026-10-31).
- Preset cycle cost through admission / make-room: no make-room on the CLI path; a refusal is that preset's error row.
- No-speed-field servers: proxy estimates, marked estimated.

### Deferred to Implementation

- Exact names and signatures of the shared override struct and the extracted payload function. Constraint: `start`'s existing tests pass unchanged except for field-path edits (`args.ctx` → `args.<group>.ctx`).
- Whether `--name` should be allowed on bench. Default: not exposed.
- Shortcut to comment in code: if bench's launch dies between the Ready check and a request, the proxy can auto-start a fresh one for that model id. Bench re-checks its launch is still `ready` in `status` before each request and treats a row whose `launch_id` is not its own as a failure; the narrow window left is accepted.

## High-Level Technical Design

> *Directional guidance for review, not implementation specification.*

```
bench(args):
  client, rows = connect, fetch_catalog
  row = resolve(args.model); refuse unless chat
  status = fetch_status; refuse if a live launch of row.path exists
  presets = args.preset or [None]
  baselines = [preset_baseline(p) for p in presets]     # fail fast on unknown names
  proxy = from_proxy_block(status.proxy); key = effective_api_key or stub
  results = []
  select! {
    ctrl_c => stop(current); exit interrupted
    loop for (p, base) in presets:
      t0 = now; resp = start_model(shared_payload(row, base, args.overrides, p))
      poll status until launch ready (Ready-only)  -> error row if error/stopped
      load_ms = now - t0; error row if running mode != chat
      for i in 1..=runs:
        check launch still ready
        prev = newest seq in requests_tail(model_path, 1)
        POST {proxy}/chat/completions stream, model=<pub id>[@name], "Run i. " + prompt
        drain body
        poll requests_tail(model_path) until seq > prev and state != in_flight (5 s)
        check row.launch_id == ours and status < 400
      stop_model(launch_id)
      results.push(median(runs))
  }
  render(results); exit LAUNCH_FAILED if any error
```

`--json` shape (wrapped object, stable):

```
{ "model": "<name>", "runs": 3, "max_tokens": 256,
  "results": [ { "preset": "coder" | null, "launch_id": "L4", "load_ms": 8123,
                 "prompt_tokens": 1012, "prefill_tokens_per_second": 812.4, "prefill_estimated": false,
                 "completion_tokens": 256, "tokens_per_second": 41.2, "tokens_per_second_estimated": false,
                 "ttft_ms": 1310, "error": null,
                 "samples": [ { per-run copies of the same fields } ] } ] }
```

Table columns: `PRESET  LOAD  PP tok/s  TG tok/s  TTFT  RUNS  NOTE` (NOTE holds the error). `preset: null` renders as `default`.

## Implementation Units

- [x] **Unit 1: share `start`'s per-launch overrides**

**Goal:** One flattened args struct for the per-launch overrides and one function that turns (catalog row, preset baseline, overrides, preset name) into a `start_model` payload, used by `start` now and by `bench` in Unit 2.

**Requirements:** R5

**Dependencies:** None

**Files:**
- Modify: `src/cli/cli_args.rs` (move `ctx`, `port`, `reasoning`, `mode`, `knobs`, `extra`, `backend`, `server`, `mtp`, `mtp_draft_n` out of `StartArgs` into a flattened struct; update the parse tests that read those fields)
- Modify: `src/cli/start.rs` (extract the baseline fetch and the override-layering + payload build out of `handle`; `handle`, `start_row_with_preset`, `select_start_row` / `direct_path_candidate` use the new paths)
- Modify: every `StartArgs { .. }` struct literal: `tests/cli_integration_test.rs` (several) and the `#[cfg(test)]` module in `src/cli/start.rs`
- Modify: any other reader of those `StartArgs` fields (`rg 'args\.(ctx|port|reasoning|mode|knobs|extra|backend|server|mtp)' src/cli`; `session.rs` only calls `start_row_with_preset`)

**Approach:**
- Pure refactor. Help text, flag names, `-- <extra>` as `last = true`, and value parsers stay byte-identical. Confirm `llamastash start --help` output is unchanged (diff it before and after). `knobs` is already a flattened `Args` (`src/cli/knob_flags.rs`), so nesting it inside another flattened struct is fine. In every struct that embeds the group, declare the `model` positional before the flattened field, or the `last = true` tail gets the lower positional index.
- The extracted function keeps the `--ctx` > native warning (gated on `!json`) and the foreign-server pin drop.
- The launch-file path in `start::handle` keeps its own baseline (`partial_params_from_launch`) and passes it into the shared function.

**Patterns to follow:** `start_row_with_preset` already reuses `fetch_preset_params` / `resolve_mode` / `build_payload`.

**Test scenarios:**
- Test expectation: existing `start` unit tests in `src/cli/start.rs`, `src/cli/cli_args.rs`, and `tests/start_model_ipc_test.rs` pass with only field-path edits. No new behavior to test.

**Verification:** `start` behaves exactly as before; `cargo nextest run --features test-fixtures --lib start` and `--test start_model_ipc_test` pass.

- [x] **Unit 2: `llamastash bench` command**

**Goal:** The new verb, end to end.

**Requirements:** R1-R7

**Dependencies:** Unit 1

**Files:**
- Create: `src/cli/bench.rs` (handler, prompt building, median aggregation, table / `\t` / JSON rendering, inline `#[cfg(test)] mod tests`)
- Modify: `src/cli/cli_args.rs` (`Command::Bench(BenchArgs)`: `model`, `--preset` as a comma list, `--runs` (≥1), `--max-tokens` (≥1), `--prompt` / `--prompt-file` (conflicting), `--json`, flattened overrides from Unit 1)
- Modify: `src/cli/mod.rs` (dispatch + `mod bench;`)
- Modify: `src/cli/start.rs` only if a helper needs `pub(crate)` (e.g. `resolve_mode`, `map_start_error`)
- Test: `tests/cli_bench.rs` (new integration file, daemon + `fake_llama_server`)
- Modify: `tests/fixtures/fake_llama_server.rs` (only if no existing marker does this: a marker such as `__TEST_EMIT_USAGE__` whose stream ends with a `usage` chunk carrying `prompt_tokens` / `completion_tokens` and no `timings`, so the proxy estimates the speed)
- Possibly modify: shell-completion / CLI golden tests that list every subcommand, if any fail on the new verb

**Approach:**
- Follow the High-Level Technical Design sketch and the Key Technical Decisions above.
- Reqwest client: connect timeout 10 s, read timeout generous (300 s), no total timeout, like `src/tui/oai_client.rs`. Bearer header with the proxy key.
- Stop with `stop_model` the way `src/cli/stop.rs` does; a stop failure is reported in that preset's NOTE but does not abort the remaining presets.
- Human output goes to stdout; progress lines (`starting coder…`, `run 2/3`) go to stderr and are suppressed under `--json` and `--quiet`.
- Medians over successful runs only; a preset with zero successful runs is an error row.
- Load time is measured by the CLI from the `start_model` call to the settled Ready row.

**Patterns to follow:** `src/cli/requests.rs` for fetching rows and formatting speeds (`~` prefix), `src/cli/output.rs` for TTY-gated tables, `src/cli/logs.rs` for the Ctrl-C select, `src/cli/stop.rs` for `stop_model`.

**Test scenarios:**
- Unit, happy path: median of `[30.0, 41.0, 39.0]` → 39.0; even count takes the mean of the middle two; one value → that value.
- Unit, edge: estimated flag is true when any sample was estimated; a metric missing from every sample is `null`.
- Unit: run prefix differs per run index and is prepended to a custom `--prompt`; built-in prompt is non-empty and identical across calls.
- Unit: JSON rendering of one ok row and one error row has the documented keys; `preset: null` renders as `default` in the table.
- Unit, args: `--preset a,b` parses to two entries; `--prompt` with `--prompt-file` is a usage error; `--runs 0` is rejected; bench accepts `--ctx 4096` and a `-- --foo` tail.
- Integration, happy path: fake server, `bench <model> --runs 2 --prompt __TEST_EMIT_TIMINGS__ --json` → exit 0, one result, `tokens_per_second` 38.64 (not estimated), `prefill_tokens_per_second` present, `ttft_ms` present, two samples; afterwards `status` shows no live launch of that model.
- Integration, estimate path: `--prompt __TEST_EMIT_USAGE__` (usage, no timings) → `tokens_per_second` present with `tokens_per_second_estimated: true`.
- Integration, no speed at all: a plain prompt (no marker) → exit 0, `tokens_per_second: null`, table cell shows the placeholder, not `0`.
- Integration, `--preset auto` → one result labelled `auto`, launch unnamed, stopped afterwards.
- Integration, two presets: two presets saved in the test's `config.yaml` → two results in order, each with its own `launch_id`, both stopped afterwards.
- Integration, error paths: unknown preset name → `USAGE` and no launch started; model already started with `start` → `USAGE` naming that launch, the running launch is left alone; an embedding-mode row → `USAGE`.

**Verification:** `cargo nextest run --features test-fixtures --lib bench` and `--test cli_bench` pass; manual E2E below shows real numbers.

- [x] **Unit 3: docs**

**Goal:** Docs ship with the code.

**Requirements:** R8

**Dependencies:** Unit 2

**Files:**
- Modify: `docs/usage.md` (new `### llamastash bench <model-ref>` section after `requests`: synopsis, flags, what each column means and where the number comes from, `~` estimates, the up-front refusals, admission refusals as error rows, no make-room, `--json` shape, exit codes)
- Modify: `CHANGELOG.md` (one line under `[Unreleased]`)
- Modify: `TODO.md` (strike the Batch 5 item: `- [x]`)
- Modify: `README.md` only if it lists every subcommand
- Modify: `docs/architecture.md` only if it lists CLI commands per module

**Test expectation:** none -- docs only.

**Verification:** every flag in `BenchArgs` appears in `docs/usage.md`; no doc still describes bench as open.

## System-Wide Impact

- **Interaction graph:** client-side only; uses `status`, `presets_show`, `start_model`, `stop_model`, `requests_tail`, and the proxy's `/v1/chat/completions`.
- **State lifecycle risks:** a launch left running after a crash or Ctrl-C. Mitigated by the Ctrl-C handler and stopping on every per-preset error path. `start` with a named preset writes last-params as any `start` does; that is accepted.
- **Unchanged invariants:** `start`'s flags, help text, and payloads; no IPC method added; `--json` of every other command.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Unit 1 refactor changes `start` behavior | Pure move; existing tests and `start --help` diff guard it |
| Proxy auto-starts a fresh launch if bench's launch dies mid-run | Ready check before each request, `launch_id` check after; remaining window commented as a shortcut |
| Prompt cache makes later runs' prefill near zero | Per-run prefix forces a miss from the first user token |
| Request log ring drops the row before bench reads it | Read the row right after each request, not at the end |
| `--port` with several presets: the next launch binds the port before the previous one freed it | Not checked during planning; verify `stop_model` waits for exit, else document that `--port` applies to every preset and may collide |

## Setup and Verification Commands

Setup (once, in the worktree): `cargo build --features test-fixtures` (warms `target/`).

Verification:
- `cargo nextest run --features test-fixtures --lib start`
- `cargo nextest run --features test-fixtures --lib bench`
- `cargo nextest run --features test-fixtures --test start_model_ipc_test`
- `cargo nextest run --features test-fixtures --test cli_bench`
- `cargo nextest run --features test-fixtures --test cli_completions` (if it lists subcommands)
- `make lint`

The pre-commit hook runs fmt + lint + doc; the pre-push hook runs the full suite.

Manual E2E (isolated daemon, never the user's): `LLAMASTASH_STATE_DIR` + `LLAMASTASH_CONFIG_DIR` + `LLAMASTASH_CACHE_DIR` pointed at a temp dir under `target/`, a non-default `--proxy-port`, then `target/debug/llamastash bench <a small cached GGUF> --runs 2` and `--json | jq .`, then `status` shows nothing left running.

## Definition of Done

- All three units checked; verification commands pass.
- `bench` prints real numbers against a real `llama-server` in the isolated daemon.
- `TODO.md` Batch 5 item struck, CHANGELOG line added, `docs/usage.md` section present.

## Sources & References

- `TODO.md` § R18 Batch 5
- Related code: `src/cli/start.rs`, `src/proxy/request_log.rs`, `src/proxy/usage_tap.rs`, `src/proxy/eviction.rs`
