# Code Review — llamatui v1 (consolidated)

**Scope:** Full codebase, root commit → HEAD
**Intent:** Comprehensive review of llamatui v1 — greenfield Rust TUI + CLI launcher for llama.cpp
**Verdict:** **No-Go on v1 without addressing the P0 and the security/correctness P1 cluster** — fixable, but several issues calcify into the wire/CLI contract after the first tagged release.
**Status**: All fixed.

This document consolidates **two independent `ce-review` runs**:

| Run | Date | Run id | Reviewers | Findings |
|---|---|---|---|---|
| **R1** | 2026-05-15 | `20260515-235146-4db0842c` | 10 retrieved | 37 |
| **R2** | 2026-05-16 | `20260516-014905-b9cee91c` | 11 retrieved | 96 |

R2 dispatched 12 reviewers in parallel (R1's 10 + `cli_readiness`, plus a fresh pass). R2's `REPORT-PARTIAL.md` was written when only 3 reviewers had been retrieved; all 11 JSON artifacts subsequently landed on disk and are consolidated here. R1's `report.md` (older `docs/review.md`) is superseded by this document.

**Findings after dedup:** **~110** (1 P0 in R1+R2 jointly + new P0 issues, ~30 P1, ~38 P2, ~42 P3). Each row tags the source run(s) and reviewer(s); rows flagged by both runs are noted `R1+R2`.

---

## Reviewer dispatch outcome

| Reviewer | R1 | R2 |
|---|---|---|
| correctness | ✅ 2 | ✅ 10 |
| testing | ✅ 19 | ✅ 14 |
| maintainability | ✅ 3 | ✅ 11 |
| project_standards | ✅ 0 (clean) | ✅ 5 |
| agent_native | ✅ 5 | ✅ 8 |
| security | ✅ 2 | ✅ 4 |
| performance | ✅ 2 | ✅ 10 |
| reliability | ✅ 4 | ✅ 8 |
| adversarial | ✅ 8 | ✅ 9 |
| api_contract | ✅ 3 | ✅ 10 |
| cli_readiness | — | ✅ 7 |
| learnings | empty (greenfield) | prose only |

Severity normalization: reliability's `high|medium|low` mapped to `P1|P2|P3`. Confidences are shown as percentages where the reviewer emitted 0–100, and as decimals (0–1) where they emitted floats; both are reported as the reviewer wrote them.

---

## P0 — Critical (3)

| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P0-1 | `src/tui/events.rs:633` | **`current_launch.lock().unwrap()` in render hot loop.** A poisoned `std::sync::Mutex` causes an unrecoverable TUI panic at ~125 Hz with no recovery. Fix: `lock().unwrap_or_else(\|e\| e.into_inner())` or switch to `tokio::sync::Mutex` (no poisoning). | R1 reliability | 100 |
| P0-2 | `src/daemon/supervisor.rs:294` | **Probe task overwrites `Stopped` back to `Error` after user-initiated stop.** Probe runs to completion (up to 120 s) and unconditionally calls `transition(Error{cause})`, clobbering the user's stop. Fix: before transition, read current state and skip if already `Stopped`/`Stopping`/`Error`. Pair with P1-7 (exit-watcher overwrite). | R1 correctness, R2 correctness, R2 reliability | 90 / 0.65 |
| P0-3 | `src/launch/params.rs:79` | **Advanced-flag passthrough breaks loopback-only contract.** `params.advanced` is appended verbatim; llama-server honours last-occurrence, so a trailing `--host 0.0.0.0` (or `--listen`/`--bind`/`--api-key`/`--ssl-*`) wins. Any IPC caller — including the TUI advanced panel — can expose the model to the LAN. Fix: filter or refuse a denylist before extending argv, or fail `start_model` with `InvalidParams`. | R2 adversarial | 0.92 |

---

## P1 — High (≈30)

### Security / hardening
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P1-1 | `src/daemon/orphans.rs:214` | Permissive substring match in orphan adoption. After daemon crash + PID reuse, an unrelated local listener whose `/v1/models` body merely contains the basename passes the three-factor probe; `stop` then SIGTERMs a bystander. Tighten to strict JSON parse + path equality, plus BLAKE3 cross-check of advertised model. | R2 adversarial / correctness / security | 0.85 / 0.70 / 0.55 |
| P1-2 | `src/ipc/methods.rs:581` | `--port 0` cascade. Daemon records port 0 in `RunningSnapshot`; `collect_in_use_ports` treats 0 as in-use, `status`/`logs_tail` route by port and find none, and orphan sweep on restart drops the entry while the real llama-server keeps running. Reject `port == 0` (and `< 1024` unless root). | R2 adversarial | 0.88 |
| P1-3 | `src/ipc/methods.rs:446` | `stop_external` TOCTOU on PID reuse. Liveness check then SIGKILL can land on a bystander if the PID gets recycled between the two calls (more likely on macOS). Use `pidfd_send_signal` on Linux; capture+verify `start_time` on macOS. | R2 adversarial / correctness | 0.78 / 0.75 |
| P1-4 | `src/gguf/header.rs:290` | GGUF nested-array recursion → stack overflow. `read_value` recurses synchronously on `Array(Array(...))`; within the 1 MiB header cap a hostile `.gguf` can nest ~87 000 deep, crashing the `spawn_blocking` worker on every scan. Add `MAX_ARRAY_NEST_DEPTH = 4`. | R2 adversarial | 0.72 |
| P1-5 | `src/daemon/mod.rs:133` | Symlink hijack on macOS `/tmp/llamatui-$USER/`. `ensure_parent_dir` calls `create_dir_all` without ownership / `O_NOFOLLOW` checks; same-host attacker can plant symlinks at `daemon.sock` / `daemon.pid` and steer the daemon's writes. Refuse parents not owned by daemon UID, or open with `O_NOFOLLOW` + stat-after-open. | R2 adversarial | 0.70 |
| P1-6 | `src/daemon/lockfile.rs:126` | Lockfile `set_len(0)` follows symlinks. `OpenOptions{read,write,create,truncate(false)}` + `set_len(0)` will follow a planted symlink — daemon zeroes whatever it points to on every cold start. Add `OpenOptionsExt::custom_flags(libc::O_NOFOLLOW)` and stat-verify regular-file type. | R2 adversarial | 0.74 |

### Correctness / runtime
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P1-7 | `src/daemon/supervisor.rs:338` | Exit-watcher overwrites probe's detailed `Error{cause}` with generic "process exited before becoming ready"; the probe's HTTP status + stderr tail is lost within the 100 ms poll window. Watcher's match guards `Ready\|Stopping\|Stopped` but not `Error`, so it falls through and overwrites. Preserve existing `Error{cause}`; pair with P0-2. | R1 adversarial+correctness, R2 reliability+correctness | 95 / 0.65 |
| P1-8 | `src/daemon/supervisor.rs:397` | Log write I/O errors silently dropped in `pump_stream`. Disk-full causes silent durable-log loss with zero observability. Replace `let _ = file.write_all(...)` with logged error; transition model to `Error` after consecutive failures. | R1 reliability | 100 |
| P1-9 | `src/daemon/supervisor.rs:266` | Spawned supervisor tasks drop their `JoinHandle`s. Panic in probe / pump / watcher is silently swallowed; model stuck in `Loading` forever. Store handles, spawn a watchdog that `tokio::select!`s and logs any panic. | R1 reliability | 75 |
| P1-10 | `src/daemon/supervisor.rs:17`, `:229-234`, `:379-406` | Supervisor advertises log rotation (10 MiB / 5 files) but no rotation logic exists. A llama-server logging ~1 KB/s produces ~80 MB/day with no bound. Implement size-checked rotation, or correct docstring + plan to acknowledge deferral. (R1 rated P2; reliability reviewer in R2 rated high → P1.) | R1 adversarial, R2 reliability | 100 / 0.95 |
| P1-11 | `src/ipc/methods.rs:580` | Concurrent `start_model` race on port allocation. `collect_in_use_ports → allocate → supervisor_spawn → insert` is not atomic; two callers observing the same snapshot can bind-probe the same port, both succeed, both spawn — the second supervisor's child fails to bind. Hold a `Mutex<HashSet<u16>>` of reserved ports across choose-and-reserve, or serialize `start_model`. | R2 correctness / adversarial | 0.90 / 0.83 |
| P1-12 | `src/ipc/methods.rs:628` | `state.json::running` drops prior launch on same-GGUF relaunch. `s.running.retain(\|r\| r.id != id)` deletes every snapshot for the `ModelId` before pushing the new one; on daemon restart the orphan sweep can only re-adopt the surviving snapshot. Retain by `(id, port)` and persist `LaunchId` on `RunningSnapshot`. | R2 correctness | 0.90 |
| P1-13 | `src/ipc/client.rs:112` | `Client::call_with_timeout` desynchronises the wire on cancellation. `tokio::time::timeout` cancels a future that may be mid-`write_frame`; the next call writes a new prefix that the daemon mis-frames, corrupting every subsequent call on the connection. Poison the client on timeout (force reconnect), or run `interaction` on a detached task with a oneshot. | R2 correctness | 0.85 |

### Testing — plan-mandated scenarios with zero coverage
| # | Plan Unit / File | Scenario | Source | Confidence |
|---|---|---|---|---|
| P1-14 | 2 / `src/daemon/server.rs:35-107` | SIGINT mid-request drain (request completes within DRAIN_TIMEOUT=2s) and the drop branch when deadline exceeded — only `ShutdownToken` notify mechanics unit-tested. | R2 testing | 0.82 |
| P1-15 | 5 / `src/daemon/mod.rs:158-166,235-251` | `quarantine_broken_state` (`state.json` → `state.json.broken-<ts>` + default boot) — zero coverage. Plan-mandated mitigation for state-corruption brick. | R2 testing | 0.88 |
| P1-16 | 5 / `src/daemon/orphans.rs`, `mod.rs` | Orphan adoption end-to-end (state.json running entry + live child + matching probe → adopted on restart, surfaces in `status` IPC) — only `orphans::sweep` unit-tested in isolation. | R2 testing | 0.83 |
| P1-17 | 7 / `src/tui/oai_client.rs:67-74,105-108` | Chat tab HTTP 4xx → `ChatStreamMsg::Error` and malformed-SSE silent-skip — neither asserted. | R2 testing | 0.85 |

### CLI / API contract
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P1-18 | `src/ipc/methods.rs:218` | IPC method naming inconsistent. Mixes verb-noun (`list_models`) with noun-verb (`presets_list`), plural (`presets_*`) with singular (`favorite_*`), no namespace separator. v1 freezes the wire contract; pick one convention now (recommended dotted `model.list`, `preset.list`, etc.). | R2 api_contract | 0.92 |
| P1-19 | `src/ipc/methods.rs:220` | No protocol-version handshake. `version` returns binary version only; v1 client ↔ v2 daemon cannot feature-detect. Add `protocol_version: 1` (additive, non-breaking); optionally a `system.capabilities` method. | R2 api_contract | 0.85 |
| P1-20 | `src/cli/output.rs:41` / `src/cli/favorites.rs:22` / `src/cli/presets.rs:38` | `--json` schemas inconsistent across CLI subcommands. `list --json` is a bare array, `status --json` an object, `presets list --json` a bare array, `favorites list --json` a raw daemon-body passthrough. Pick "always object" (recommended) and document. | R1 api_contract (#18, #19), R2 api_contract / agent_native | 100 / 0.95 / 0.75 |
| P1-21 | `src/cli/favorites.rs:22` | `favorites list --json` leaks the raw daemon body. Future fields added to `favorite_list` become CLI contract by accident. Add a `favorites_json` projection. | R2 api_contract | 0.88 |
| P1-22 | `src/cli/output.rs:176` | `status --json` schema diverges from daemon `status` wire shape — docstring claims parity; in fact the CLI collapses `id` (full `ModelId`) to `model_path` string and drops `fingerprint`. Either preserve `id` or update the comment + document the projection as deliberate. | R1 api_contract (#19), R2 api_contract | 100 / 0.92 |
| P1-23 | `src/ipc/methods.rs:307` | `status --json` omits per-launch params. `LaunchParams` (ctx/port/reasoning/mode/advanced) is in `RunningSnapshot.params` but not surfaced — an agent inspecting a running model can't tell whether `--ctx 32768` or `8192` is in effect. | R2 agent_native | 0.85 |
| P1-24 | `src/ipc/methods.rs:910` | `last_params_list` IPC reachable only from TUI. Plan invariant: any new daemon method must be reachable from both TUI and CLI. Add a thin subcommand or fold the data into `status` / `presets show`. | R1 agent_native (#6), R2 agent_native | 100 / 0.90 |
| P1-25 | `src/cli/cli_args.rs:167` / `src/cli/logs.rs` | `logs` subcommand has no `--json` flag — violates the documented "every read command supports --json" contract. Daemon's `logs_tail` returns structured JSON; CLI handler discards it. Add `--json` (JSON Lines for `--follow`). | R1 agent_native | 100 |
| P1-26 | `src/cli/stop.rs:100` | `stop --all` `confirm()` blocks on stdin with no TTY guard. Agent without `--yes` either silently no-ops or hangs. Refuse with `USAGE` exit when `!is_terminal(stdin)` && `!yes`. | R2 cli_readiness | 0.92 |
| P1-27 | `src/cli/start.rs:211` | `start` has no `--json`. Mutation-critical command; agents must regex-parse `started X → launch_id=L1 port=41150 pid=12345`. Add `--json` emitting `{name, launch_id, port, pid, preset, path}`. | R2 cli_readiness | 0.95 |

### Project standards / release readiness
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P1-28 | `README.md:86`, `docs/usage.md:13` | Default scan paths per OS not enumerated in user docs (plan §"Documentation" requires it). Only `src/discovery/known_caches.rs` lists HF / Ollama / LM Studio paths. | R2 project_standards | 0.88 |
| P1-29 | `.github/workflows/release.yml:47` | macOS release artefacts not codesigned/notarised AND the gap is undocumented. Plan accepts either signing or a documented `xattr -d com.apple.quarantine` workaround; both are absent. | R2 project_standards | 0.92 |

---

## P2 — Moderate (≈38)

### Reliability / runtime
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P2-1 | `src/tui/events.rs:704,710-747` | `spawn_logs_poller` reconnects every 500 ms with no backoff (sibling `spawn_refresher` does). Daemon outage produces a 2 Hz connect-attempt rate forever. | R2 reliability / performance | 0.90 / 0.71 |
| P2-2 | `src/daemon/mod.rs:331-358` | `start_detached_with_exe` uses `std::thread::sleep(50ms) × ~60` inside an async call path. On a single-threaded tokio runtime (CLI default) this blocks every other task during daemon auto-spawn. | R2 reliability | 0.85 |
| P2-3 | `src/discovery/watcher.rs:132-151` | Debouncer callback uses `blocking_send` into a 64-slot bounded channel; overflow blocks the debouncer thread or silently drops events (only `log::debug` on failure). Switch to `try_send` + warn + larger default. | R2 reliability | 0.80 |
| P2-4 | `src/daemon/shutdown.rs:82` | Signal-handler installation failure returns early, leaving the daemon SIGINT/SIGTERM-immune silently. On failure, trigger the shutdown token instead. | R1 reliability | 75 |
| P2-5 | `src/ipc/methods.rs:464-481` | `stop_all_handler` runs per-launch stops sequentially: N × 5 s grace period serializes. Default IPC client timeout is 5 s → 2+ stuck launches → client `Timeout` while daemon succeeds in the background. Also blocks indefinitely on D-state children. Use `futures::future::join_all` / `FuturesUnordered`. (R1 rated P2; R2 reliability rated low → P3 — promoted to P2.) | R1 adversarial, R2 reliability | 75 / 0.70 |
| P2-6 | `src/daemon/supervisor.rs:168` (stop), `:273-304` (probe), `:309-347` (watcher) | Recycled-PID hazard: three concurrent paths poll the same `Child`. `try_wait`/`wait` can reap the zombie and the kernel may recycle the PID before `kill(pid, SIGKILL)` is delivered. Guard signal delivery under the child mutex + re-check, or use `pidfd_send_signal`. | R2 correctness | 0.75 |
| P2-7 | `src/daemon/server.rs:91` | Drain phase aborts in-flight write tasks at DRAIN_TIMEOUT, leaving partially-written response frames on connections that survive. Issue an explicit cancel to per-connection tasks, or document drain as best-effort. | R2 correctness | 0.70 |
| P2-8 | `src/daemon/orphans.rs:222` | Orphan sweep accepts adoption when `/v1/models` body contains the model *basename* — same-named GGUF in different directory passes (PID-reuse false positive, distinct from P1-1's substring attack). Re-parse advertised model file and compare BLAKE3 to stored `header_blake3`. | R2 correctness | 0.70 |
| P2-9 | `src/daemon/supervisor.rs:319` | Watcher task can transition `Error → Stopped`; the documented state machine has no such edge. `transition` should reject moves out of terminal states. (See also P0-2 / P1-7.) | R2 correctness | 0.65 |
| P2-10 | `src/daemon/mod.rs:138` | Socket TOCTOU: `bind` happens before `chmod 0600`, leaving a window where the socket file is world-accessible on Linux. Set process umask `0o077` around `UnixListener::bind`, or socketpair+chmod-before-listen. | R2 security | 0.70 |
| P2-11 | `src/daemon/state_store.rs:183` | `state.json.tmp` write uses a predictable path without `O_NOFOLLOW` / symlink check. Same family as P1-5 / P1-6 but for state store specifically. Open with `O_NOFOLLOW`; refuse if path is a symlink. | R1 security | 50 |
| P2-12 | `src/ipc/methods.rs:581` | Concurrent `start_model` with the same pinned `port` both pass dispatch; OS arbitrates bind, the loser lands in `Error` and leaks a `RunningSnapshot` until explicit stop. Validate `requested` against `collect_in_use_ports + try_bind`. | R2 adversarial | 0.83 |
| P2-13 | `src/tui/events.rs:407` | Favorite toggle not reverted on writer's RPC failure — optimistic UI flips, but on RPC failure the TUI doesn't roll back, so the state drifts from daemon truth after reconnect. Add response channel from writer task; revert on confirmed failure. | R1 adversarial | 75 |
| P2-14 | `src/ipc/methods.rs:593` | No upper bound on context length — `u32::MAX` is passed directly to `llama-server -c`, bypassing the TUI picker's 131 072 cap. Validate `ctx <= 1_048_576` in `start_model_handler`. | R1 adversarial | 75 |

### API / agent surface
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P2-15 | `src/cli/output.rs:41` | `list --json` schema diverges from daemon `list_models` (bare array vs wrapped `{"models":[...]}`; drops `weights_bytes, tokenizer_kind, reasoning_hint, has_chat_template, total_parameters, split_siblings`). Decide curated-view vs faithful-dump and document. | R1 api_contract (#18), R2 api_contract | 100 / 0.88 |
| P2-16 | `src/ipc/methods.rs:321` | `status.models[*].state` is double-nested `{state: {state: "..."}}` due to default serde repr. Hard for non-Rust consumers to discover. Serialize `ManagedState` as a plain lowercase string. | R2 api_contract | 0.78 |
| P2-17 | `src/discovery/catalog.rs:99` | `reasoning_hint` field is a *boolean* despite the name implying a hint label; sibling row uses `has_chat_template`. Rename to `has_reasoning_hint` or emit the actual label. | R2 api_contract | 0.82 |
| P2-18 | `src/cli/exit_codes.rs:16` | Exit codes reuse sysexits.h numbers but deviate from sysexits semantics (65=DAEMON_UNREACHABLE vs EX_DATAERR, etc.). Agents importing sysexits constants will mis-branch. README must document the deviation prominently. | R2 api_contract | 0.72 |
| P2-19 | `src/cli/output.rs:41` (agent-native view) | `list --json` omits the canonical short-fingerprint `model_id`; agents must round-trip via brittle path strings. | R2 agent_native | 0.78 |
| P2-20 | `src/cli/cli_args.rs:144` | Daemon's `stop_model`/`stop_external` accept `grace_secs` (default 5); `StopArgs` lacks the flag. | R2 agent_native | 0.82 |
| P2-21 | `src/tui/events.rs:587` | `encode_writer_cmd` builds the `start_model` payload without `mode`; daemon defaults to Chat regardless of GGUF mode hint. CLI is stricter. An embedding GGUF launched from the TUI silently starts in chat mode. | R2 agent_native | 0.82 |
| P2-22 | `src/cli/stop.rs:41`, `src/cli/favorites.rs:52`, `src/cli/presets.rs` | `stop`, `favorites add/remove`, `presets save/delete` have no `--json`. Idempotency info (`added`, `removed`, `replaced`, SIGTERM vs SIGKILL) is folded into prose; agents can't safely retry. | R2 cli_readiness | 0.90 / 0.88 |

### Maintainability — structural
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P2-23 | `src/lib.rs:11` | Crate-wide `#![allow(dead_code)]` (added during Unit 2) still in place after Unit 9 shipped — globally hides dead code from the compiler. Remove the global allow; narrow per-item if needed. | R2 maintainability | 0.80 |
| P2-24 | `src/launch/mode.rs:16` | Three parallel `LaunchMode` enums (`cli_args::LaunchMode`, `launch::mode::LaunchMode`, private `LaunchModeWire` inside `ipc/methods.rs:538`). The wire enum is pure duplication. | R2 maintainability | 0.82 |
| P2-25 | `src/tui/events.rs:1` | 1052 LOC, 32 functions, mixing input dispatch, async writers, refresh tasks, drainers, and the run loop. Split into `events/{input,runtime,drain}.rs`. | R2 maintainability | 0.78 |
| P2-26 | `src/tui/tabs/embed.rs:23`, `src/tui/tabs/rerank.rs:35` | Tab modules import `crate::tui::events::TabEvent`, while `events.rs` imports back into `tabs::*` — circular-by-path. Move `TabEvent` into `tui/tabs/mod.rs` or a neutral file. | R2 maintainability | 0.74 |
| P2-27 | `src/ipc/methods.rs:1` | 1151 LOC, 14 handlers, 8 param structs, 4 helpers in one module. Split into `ipc/methods/{mod.rs (dispatch), context.rs, status.rs, supervisor.rs, presets.rs, favorites.rs, params.rs}`. | R2 maintainability | 0.72 |
| P2-28 | `src/tui/events.rs:297` (+ 6 sibling call sites) | `model_name` extraction pattern duplicated 7× across 3 modules. Extract `model_display_name(path: &Path) -> String` into `src/util/`. | R1 maintainability | 75 |
| P2-29 | `src/tui/events.rs:825` | `drain_embed_pending` / `drain_rerank_pending` are near-identical copy-paste. Unify with a `TabPending` trait providing `record_ok`/`record_err`/`set_busy`/`take_pending`. | R1 maintainability | 75 |

### Performance
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P2-30 | `src/tui/app.rs:237,277` | `rendered_rows()` rebuilds the row vec on every call; called 2–3× per frame and 10× per PageUp/Down (each `move_*` rebuilds). Cache with a dirty flag; or compute once and pass result into `move_*`. | R1 performance (#8), R2 performance | 75 / 0.82 |
| P2-31 | `src/discovery/scanner.rs:92` | `walk_root` awaits `build_discovered_model` per entry strictly serially. On a cold HF cache with hundreds of GGUFs this dominates first-list latency. Use `buffer_unordered(num_cpus)` for parsing. (Same pattern in `discovery/ollama.rs:69`.) | R2 performance | 0.78 |
| P2-32 | `src/tui/oai_client.rs:48` | `reqwest::Client` constructed per request; connection pool + TLS config rebuilt unnecessarily. Create a single `OnceLock<Client>` and reuse across `spawn_chat_stream`, `embed`, `rerank`. | R1 performance | 75 |

### Test coverage
| # | Plan Unit / File | Scenario | Source | Confidence |
|---|---|---|---|---|
| P2-33 | 2 / `src/daemon/server.rs:66` | Peercred-rejection branch (`drop(stream)` after authz failure) and parse-error response path have no tests — security + protocol boundary. | R1 testing, R2 testing | 75 / 0.75 |
| P2-34 | 1 / `src/util/paths.rs:139-160` | XDG/macOS path resolution tests assert only `is_some()`; never override `XDG_*_HOME` and never check resolved strings. Refactor helpers to take an explicit `home_or_env` parameter and assert path content. | R2 testing | 0.78 |
| P2-35 | 6 / `src/tui/events.rs:519-555` | TUI connect→disconnect→reconnect cycle (backoff doubling, reset on success, `daemon: connecting` pill transitions) — unverified. | R2 testing | 0.72 |
| P2-36 | 5 / supervisor + fake-llama | Stderr-burst capture (few KB / 50 ms, no dropped lines, 4096-line ring cap) — existing test only captures the single `listening on …` line. | R2 testing | 0.74 |
| P2-37 | 8 / `tests/cli_integration_test.rs:526` | `presets list --json` test asserts only exit code, not JSON shape; no unit test in `cli::output::tests` either — agent contract has zero shape-level coverage. | R2 testing | 0.86 |
| P2-38 | 8 / `src/cli/logs.rs:121-132` | `logs --follow \| head` SIGPIPE → exit 0 handler exists (`safe_println`) but no test reaches the BrokenPipe arm. | R2 testing | 0.82 |
| P2-39 | `src/ipc/methods.rs` (5 handlers) | No unit tests for: `stop_all`, `logs_tail` error path, `last_params_list`, `presets_show` (unknown), `favorite_remove` (unknown). | R1 testing | 75 |
| P2-40 | `src/cli/output.rs:225` (12 formatters) | 12 CLI output formatters have zero unit tests — agent-facing JSON contract unprotected against regressions. (Subsumes P2-37 if extended.) | R1 testing | 75 |
| P2-41 | `src/config/loader.rs:20` | Config loader oversized-file rejection and invalid-YAML parsing are untested. | R1 testing | 75 |
| P2-42 | `src/daemon/probe.rs:101` | `poll_until_ready` polling loop timeout/retry logic only exercised via slow integration tests; no unit test. | R1 testing | 75 |
| P2-43 | `src/daemon/supervisor.rs:443` | `ManagedModel` state machine transitions have no inline unit tests — only tested via integration against fake binary. Pair with P0-2 / P1-7 / P2-9 fixes. | R1 testing | 75 |
| P2-44 | `.github/workflows/ci.yml:40` | CI matrix runs clippy/fmt only on linux x86_64; `cfg(target_os = "macos")` peercred branch is never lint-checked pre-merge. | R2 project_standards | 0.70 |
| P2-45 | `CONTRIBUTING.md:18` | Does not document how to run the daemon locally (`cargo run -- daemon start`, socket path, teardown). First-contributor onboarding cost. | R2 project_standards | 0.78 |

---

## P3 — Low (≈42)

### Hardening / runtime
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P3-1 | `src/gguf/header.rs:234` | Asymmetric pre-alloc: `HashMap::with_capacity(kv_count)` vs `Vec::with_capacity(tensor_count.min(4096))`. `MAX_KV_COUNT = 10_000` bounds OOM but the inconsistency reads as accidental. Symmetrise with `.min(1024)`. | R2 correctness | 0.85 |
| P3-2 | `src/ipc/methods.rs:428` | `parsed.pid as i32` cast: a malicious `u32 > i32::MAX` flips negative; `libc::kill(negative_pid, sig)` signals a process group. Not currently exploitable (sysinfo bounds) but add a guard. | R2 correctness | 0.60 |
| P3-3 | `src/ipc/methods.rs:664` | `spawn_last_params_recorder` 180 s budget vs probe 120 s — safe today but silently drops valid Ready states if probe timeout ever raised past 180 s. Tie deadline to `env.probe.timeout + buffer`. | R2 correctness | 0.60 |
| P3-4 | `src/daemon/probe.rs:38-45` | Probe timeout hard-coded at 120 s; plan says configurable. Large 70B+ models on slow storage routinely exceed it. Thread through Config + `--probe-timeout`. | R2 reliability | 0.75 |
| P3-5 | `src/daemon/mod.rs:168-189` | Adopted `RunningSnapshot`s have no `ManagedModel`; `stop_model` returns `InvalidParams` and adopted entries leak forever (re-adopted on every restart). | R2 reliability | 0.85 |
| P3-6 | `src/daemon/mod.rs:406` | macOS fallback `$TMPDIR/llamatui-$USER` created with default umask (~0755); other local users can enumerate. Create with `DirBuilder::mode(0o700)`. | R2 security | 0.65 |
| P3-7 | `src/daemon/orphans.rs:107` | External-process snapshot exposes other-user llama-server cmdlines via IPC `status`. `/proc/<pid>/cmdline` is already world-readable on Linux; filter by daemon UID as cheap hardening. | R2 security | 0.62 |
| P3-8 | `src/discovery/scanner.rs:118` | Scanner follows symlinks outside scan roots — opens arbitrary files (contents not exposed). | R1 security | 50 |
| P3-9 | `src/daemon/supervisor.rs:160` | `stop()` takes full 5 s grace period if watcher races and reaps child exit first — latency regression, not correctness bug. | R1 adversarial | 75 |
| P3-10 | `src/tui/events.rs:568` | Unbounded mpsc channel in writer task — memory exhaustion possible under scripted rapid input. | R1 adversarial | 50 |
| P3-11 | `src/daemon/mod.rs:180` | Orphan sweep calls `state_store::save()` outside the `PersistedState` mutex — fragile if a periodic re-sweep is added later. | R1 adversarial | 50 |
| P3-12 | `src/cli/logs.rs:25` | `logs --follow` dedupe `SEEN_WINDOW = 1024` but `args.lines` can exceed it; seeding drops lines past index 1024 and reprints them next tick. Collapses repeated heartbeats silently. | R2 adversarial | 0.80 |

### Performance
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P3-13 | `src/discovery/metadata_cache.rs:90` | `get` takes a tokio write lock unconditionally (LRU bookkeeping); defeats RwLock concurrency. Switch to `AtomicU64` counter or the `lru` crate. | R2 performance | 0.70 |
| P3-14 | `src/discovery/metadata_cache.rs:132` | LRU eviction is O(n) scan on every put past capacity (default 2048). Use linked-list-keyed LRU. | R2 performance | 0.72 |
| P3-15 | `src/daemon/supervisor.rs:310` | Child-exit watcher polls `try_wait` every 100 ms forever, holding the inner `child.lock().await` each iteration. Switch to `wait().await` with a oneshot coordinating with `stop()`. | R2 performance | 0.74 |
| P3-16 | `src/tui/events.rs:187` | `Action::PageUp/PageDown` loops `move_up/move_down` 10× and each call rebuilds `rendered_rows`. Add a `move_by(delta)` that builds rows once. (Pairs with P2-30.) | R2 performance | 0.80 |
| P3-17 | `src/tui/tabs/logs.rs:89` | `set_tail` replaces the full `lines` vec on every 500 ms poll, reallocating up to 4096 strings even when the daemon's tail only appended a handful. Diff and append. | R2 performance | 0.62 |
| P3-18 | `src/daemon/supervisor.rs:393` | `pump_stream` does trim → to_string → `format!("[{source}] {trimmed}")` → clone per log line. Three allocations + a heap clone; bad under token-trace volume. Reuse a single owned buffer. | R2 performance | 0.61 |
| P3-19 | `src/tui/oai_client.rs:87` | OAI chat stream uses `String::from_utf8_lossy(&bytes)` + `push_str` per network chunk, then `buffer[..idx].to_string()` per SSE frame — ~3 allocations per chunk. Below-bar for v1. | R2 performance | 0.60 |

### Maintainability
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P3-20 | `src/daemon/supervisor.rs:76` | `ManagedModel` adds unnecessary `Arc` indirection layer — 8 trivial pass-through accessors on a module-private inner type. | R1 maintainability | 50 |
| P3-21 | `src/daemon/supervisor.rs:76` (and friends) | Naming drift: `ManagedModel`, `ManagedState`, `ManagedSpawn`, `ManagedRow`, `RunningSnapshot`, `RunningRow` describe overlapping concepts. Pick one axis per layer. | R2 maintainability | 0.64 |
| P3-22 | `src/cli/client.rs:62` | Unused `connect_or_fail` helper; docstring claims callers in `daemon stop` / `daemon status` that don't exist. Delete, or refactor sites to use it. | R2 maintainability | 0.92 |
| P3-23 | `src/ipc/methods.rs:936` | Tautological `#[allow(dead_code)] const _: fn() = ...` guards on `BTreeMap` and `ManagedState` imports that are already live. Delete. | R2 maintainability | 0.86 |
| P3-24 | `src/discovery/catalog.rs:120` | Three `#[allow(dead_code)]` "schema anchors" with no callers and no enforced invariants. Delete. | R2 maintainability | 0.78 |
| P3-25 | `src/cli/mod.rs:28` | Re-exports of `LaunchMode, FavoritesAction, PresetsAction, PullAction, ReasoningFlag` have zero external consumers; `#[allow(unused_imports)]` is the smoke. | R2 maintainability | 0.75 |
| P3-26 | `src/ipc/methods.rs:517,738` | `StartParams` and `PresetsSaveParams` duplicate 6 fields with identical serde defaults + identical hand-build logic. Factor a `LaunchParamsWire` flatten-able struct. | R2 maintainability | 0.66 |

### CLI / API surface (low)
| # | File:Line | Issue | Source | Confidence |
|---|---|---|---|---|
| P3-27 | `src/ipc/methods.rs:897-900` | `favorite_list` lacks `model_path` convenience field present in `last_params_list` — inconsistent API shape. Also wraps `id` as `{id: <ModelId>}` so CLI must reach two levels for `path`. Emit `path` and `name` at row root. | R1 api_contract, R2 api_contract | 50 / 0.66 |
| P3-28 | `src/cli/presets.rs:38` (agent-native view) | `presets list --json` shape diverges from `favorites list --json` (bare array vs wrapped). Same family as P1-20. | R2 agent_native | 0.75 |
| P3-29 | `src/cli/cli_args.rs:16` | No `--schema` / `--help-agent` / capability-introspection. Agents can read clap `--help` but no single command enumerates stable JSON shapes + exit codes. | R2 agent_native | 0.62 |
| P3-30 | `src/cli/output.rs:192` | External rows in `status --json` lack `launch_id` (TUI synthesises `ext-<pid>`). Add `"launch_id": "ext-<pid>"` for symmetry. | R2 agent_native | 0.70 |
| P3-31 | `src/cli/daemon.rs:162` | `daemon status` emits JSON unconditionally and dumps the raw `version` RPC body — inconsistent with sibling `status` (`--json` opt-in). Either add `--json` flag or document JSON-only. | R2 cli_readiness | 0.78 |
| P3-32 | `src/cli/cli_args.rs:47` | No global `--quiet` to suppress mutation prose for agents. | R2 cli_readiness | 0.70 |
| P3-33 | `src/cli/output.rs:17` | No auto-detect of non-TTY stdout for machine-readable defaults. TSV-by-default is defensible; document in `--help`. | R2 cli_readiness | 0.62 |
| P3-34 | `README.md:19` | "Why" bullet hints at Chat/Embed/Rerank but Quickstart never demonstrates them; no screenshots/asciinema (plan §628 calls for screenshots). | R2 project_standards | 0.62 |

### Test coverage
| # | File / Scenario | Source | Confidence |
|---|---|---|---|
| P3-35 | `src/gguf/memory.rs:200` — memory estimator unknown-architecture fallback has no test. | R1 testing | 75 |
| P3-36 | `src/cli/exit_codes.rs:78` — `from_client_error` wildcard arm map-to-UNKNOWN untested. | R1 testing | 75 |
| P3-37 | `src/discovery/watcher.rs:215` — `watcher::start` with empty roots list has no test. | R1 testing | 75 |
| P3-38 | `tests/tui_chat_smoke_test.rs:144` — `rerank_returns_sorted_scores` weak assertion (doesn't verify scores are sorted). | R1 testing | 75 |
| P3-39 | `tests/split_gguf_test.rs:10` — `end_to_end_grouping` doesn't verify the `Single` entry. | R1 testing | 75 |
| P3-40 | `tests/tui_smoke_test.rs:374` — `narrow_terminal_does_not_crash_render` only asserts the frame contains `"llamatui"`; doesn't check `…` truncation glyph at 60 cols or compact help bar. | R1 testing, R2 testing | 75 / 0.78 |
| P3-41 | `src/tui/launch_picker.rs` — custom ctx-length non-numeric rejection has no test (plan Unit 6 edge case). | R2 testing | 0.68 |
| P3-42 | `src/cli/stop.rs:22-43,100-111` — `stop --all` interactive `confirm` reads stdin with no test seam; neither cancel nor `y` path tested. | R2 testing | 0.70 |
| P3-43 | `src/util/clipboard.rs:110-114,207-210` — backend argv table only exercised against `cat`; per-backend flags (`xclip -selection clipboard`, `xsel --input --clipboard`, `wl-copy`, `pbcopy`) have no argv snapshot. | R2 testing | 0.72 |

---

## Residual risks (consolidated)

**Authentication & isolation**
- Trust boundary is the daemon's own UID. Any same-user process can drive the full IPC surface (start_model with arbitrary path, advanced flag passthrough, stop). Documented design.
- `LLAMATUI_SOCKET` env var is not validated against the per-user runtime tree — same-UID only, but worth noting.
- `supervisor::spawn` inherits the daemon's full environment; secrets like `HF_TOKEN` are passed to children verbatim.
- Peercred compares to `getuid()` (real uid), not effective uid. Not currently an issue (no setuid intent).

**Process / signal hazards**
- macOS lacks pidfd-equivalent; `supervisor.stop()`'s SIGKILL-after-grace path is theoretically vulnerable to PID reuse during the polling window.
- Port allocator binds-then-drops as probe; small TOCTOU window between `try_bind` returning `true` and `llama-server`'s real bind.
- No continuous health check after `Ready`; a llama-server that becomes unhealthy after Ready stays `Ready` until exit or explicit stop. Agents may rely on `Ready` being a live signal.

**Shutdown / persistence**
- 2 s daemon shutdown drain can drop in-flight `start_model` calls whose `ManagedModel` is registered but whose persisted state hasn't flushed; no partial-state cleanup.
- `state_store::save` uses a shared `state.json.tmp` filename — safe today (writer under `Arc<Mutex>`), but a future move to `spawn_blocking` makes the shared tmp path a bug.
- `start_detached`'s 3 s connect deadline may be tight on slow CI hosts; retry usually works.

**Watcher / discovery**
- Linux `max_user_watches` (default 8192) is unbounded from llamatui's view; exhaustion silently degrades to the 5-minute periodic backstop with no diagnostic surfaced.
- `MetadataCache` default capacity 2048; HF hub blobs alone can exceed this on power-user systems. Once full, every put triggers an O(n) eviction scan.
- First-scan latency bounded by serial per-file parsing — flag if catalogs grow past ~500 files.

**Wire shape stability**
- No serialized JSON Schema / OpenAPI for IPC or CLI `--json` shapes; consumer drift can happen unnoticed between v1 and v2.
- `status.state` nested `{state: {state: "..."}}` depends on default serde enum repr — silent shape change risk on enum mutation.
- Exit codes diverge from sysexits.h meanings; libraries importing standard constants will mis-branch.

**Misc**
- `build_log_path` uses caller-supplied path's `file_stem()` — Linux filenames may contain newlines/control chars, ending up in log filenames.
- Chat SSE `from_utf8_lossy` corrupts multibyte chars split across chunk boundaries (`U+FFFD` at seams).
- `ipc/methods.rs` accepts caller-supplied `model_path` without validation; same-UID caller can have the daemon attempt to read arbitrary paths (information disclosure via parse-error strings).
- TUI render loop has no explicit frame-rate cap; bounded by the 8 ms event::poll which yields when nothing is happening.
- CHANGELOG `[Unreleased]` heading is not bumped at tag time; first `v*` tag push will produce a release with no matching CHANGELOG section unless manually bumped. Intentional per CHANGELOG L25-27 but a release-day footgun.

---

## Testing gaps (consolidated)

Mapped to plan units where applicable. Severities are the highest assigned across reviewers.

| Unit | Scenario | Severity |
|---|---|---|
| 1 | `XDG_STATE_HOME` override resolves `state_dir` under `llamatui/`; macOS path test absent (helpers only asserted `is_some()`) | P2 |
| 2 | SIGINT mid-request drain (request completes within 2 s) and DRAIN_TIMEOUT drop branch | P1 |
| 2 | Peer with foreign UID rejected at `server.rs` accept loop (only predicate unit-tested) | P2 |
| 5 | Log file growth across documented 10 MiB rotation threshold | P1 |
| 5 | Stderr burst capture (few KB in 50 ms, no dropped lines) | P2 |
| 5 | Orphan adoption end-to-end (state.json + live child + restart → adopted, surfaces in `status`) | P1 |
| 5 | State.json corruption recovery (`state.json.broken-<ts>` quarantine + default boot) | P1 |
| 5 | Probe vs exit-watcher race on final state | P0/P1 (see P0-2, P1-7) |
| 5 | `stop_all` parallelism / timeout with N stuck launches | P2 |
| 6 | Daemon-disconnect → reconnect-backoff cycle (`spawn_refresher` + `spawn_logs_poller`) | P2 |
| 6 | Custom ctx-length non-numeric rejection in launch picker | P3 |
| 6 | Terminal width 60 cols → ellipsis truncation in list pane | P3 |
| 6 | Clipboard backend argv table (xclip / xsel / wl-copy / pbcopy) | P3 |
| 7 | Chat request 4xx → error toast; malformed SSE chunk → ignored | P1 |
| 7 | Embed / Rerank HTTP error paths (4xx, missing fields) | P2 |
| 8 | SIGPIPE during `logs --follow \| head` → exit 0 | P2 |
| 8 | `presets list --json` shape (agent contract) | P2 |
| 8 | `stop --all` interactive confirmation prompt — neither branch tested | P3 |
| n/a | Concurrent `start_model` race on port allocation | P1 |
| n/a | Same GGUF launched twice persists two snapshots across daemon restart | P1 |
| n/a | `Client::call_with_timeout` mid-write cancellation desyncs frames | P1 |
| n/a | Watcher / stop() / probe-timeout three-way race | P2 |
| n/a | Orphan sweep false-positive: PID reuse + matching basename + collision | P2 |
| n/a | Monotone supervisor state transitions (`Error → Stopped` must be rejected) | P2 |
| n/a | Fuzz: GGUF parser nested-array depth + non-regular-file `model_path` | P2 |
| n/a | Lockfile symlink-swap, state.json `.tmp` racing across two writers | P2 |
| n/a | 5 IPC handlers lacking unit tests (`stop_all`, `logs_tail` error, `last_params_list`, `presets_show` unknown, `favorite_remove` unknown) | P2 |
| n/a | 12 CLI output formatters lacking unit tests | P2 |
| n/a | Config loader oversized-file rejection / invalid-YAML | P2 |
| n/a | `poll_until_ready` polling loop timeout/retry | P2 |
| n/a | `ManagedModel` state-machine inline unit tests | P2 |
| n/a | Memory estimator unknown-architecture fallback | P3 |
| n/a | `from_client_error` wildcard arm | P3 |
| n/a | Watcher start with empty roots | P3 |
| n/a | `rerank_returns_sorted_scores` weak assertion | P3 |
| n/a | `end_to_end_grouping` weak assertion (Single entry) | P3 |
| n/a | Discovery scanner: `.gguf` symlink to `/etc/passwd` / `/dev/zero` | P3 |
| n/a | Warm-attach <200 ms (Unit 6 R29 verification) — no benchmark | P3 |
| n/a | CLI integration tests for `start --json` / `stop --json` / `favorites add --json` / `presets save --json` shapes (once landed) | P2 |
| n/a | `daemon status` JSON shape stability | P3 |
| n/a | README exit-code table vs `cli::exit_codes` parity | P3 |

---

## Learnings & past solutions

Greenfield project — `docs/solutions/` is empty (expected per plan §"Institutional Learnings").

**Patterns inherited from sibling repo `kdash` (same maintainer):**
- clap-derive `Cli` + banner + panic hook + raw-mode setup → `src/main.rs`, `src/banner.rs`
- YAML config + theme pattern, generalised to a named-theme enum → `src/config/loader.rs:78` (cites kdash in a comment)
- mpsc-stream refresher → `src/tui/events.rs`
- Long-running task + child-process plumbing → `src/launch/`, `src/daemon/`
- Help-bar layout + keybinding init + Key enum/event plumbing → `src/tui/{help_bar,keybindings}.rs`
- Tabs-with-state pattern → `src/tui/right_pane.rs` + `src/tui/tabs/*`
- Subcommand-as-Rust-handler → `src/cli/*` (Unit 8)

**Plan §"Open Questions → Deferred to Implementation" is stale (advisory, non-blocking).** Four of five are answered in code:

| Question | Resolution | Action |
|---|---|---|
| JSON-RPC error code wire format | Standard set (`-32700..-32603`) + `UnauthorizedPeer -32001` in `src/ipc/protocol.rs:97-124` | Move to "Resolved" |
| nucleo-matcher vs fuzzy-matcher | Hand-rolled subsequence ranker, documented in `src/tui/filter.rs:1-15`; Unit 6 P2 follow-up `[x]` | Move to "Resolved" |
| arboard fallback to wl-copy/xclip/xsel/pbcopy | Three-tier ladder in `src/util/clipboard.rs`; commit `ddc7f2d`; `docs/troubleshooting.md:64-77` | Move to "Resolved" |
| KV-cache estimation precision | Parsed/computed in `src/gguf/memory.rs:46-77`; snapshots in `tests/` per commit `92d1576`. Open: does the TUI surface which cache-type was assumed? | Partially resolved |
| Intel macOS GPU detection scope | `src/gpu/metal.rs:9-40,112` gates Metal on `cfg(target_arch = "aarch64")`; `docs/troubleshooting.md:20` confirms intent | Move to "Resolved" |

**Known scaffolding (NOT gaps):**
- R46 HuggingFace pull: `src/cli/cli_args.rs:72` has `TODO(v2-R46)`; `pull` subcommand is `hide = true` and dispatcher exits `unimplemented!`.
- R34 HTTP/MCP: deliberately absent from v1 per plan lines 13, 48, 567-569. CLI is the v1 agent surface.

**Memory hints for future review/work:**
- User profile: senior Rust TUI dev, kdash + jwt-ui author. Stack preference (`ratatui 0.30` + `crossterm` + `tokio` + `clap` derive + `anyhow` + `duct` + `serde_yaml` + `simplelog`) matches what landed — do not flag these choices.
- Project identity: "launcher + smoke-test side panel + agent-driveable CLI." HTTP/MCP explicitly deferred — do not propose reintroducing them. Catppuccin Macchiato is the canonical default theme.
- Stable anchor: `docs/brainstorms/llamatui-requirements.md` is the source of truth for product scope with stable IDs R1-R47.
- In-repo learning artifacts: `docs/troubleshooting.md` (operator-facing failure modes) and `docs/architecture.md` (system shape); no `critical-patterns.md` yet.

---

## Requirements completeness

| Plan item | Status |
|---|---|
| Units 1-7 (Phases A-D) | Met |
| Unit 8 (CLI + JSON outputs) | **Partially** — `logs` lacks `--json` (P1-25); `last_params_list` has no CLI (P1-24); `start`/`stop`/`favorites`/`presets save` lack `--json` (P1-27, P2-22); CLI JSON shapes diverge from daemon wire (P1-20, P1-22, P2-15) |
| Unit 9 (Distribution, docs) | Mostly met — macOS codesign/notarise gap (P1-29); default scan paths not documented (P1-28); CONTRIBUTING daemon-local-run gap (P2-45); CODE_OF_CONDUCT.md missing per plan line 635 (minor) |
| Plan review follow-ups | Resolved |

**Agent-native score:** roughly 10 / 15 high-priority capabilities cleanly agent-accessible — downgraded by CLI contract divergence (P1-20/22, P2-15/19), missing `--json` flags (P1-25/27, P2-22), and absent CLI surface for `last_params_list` (P1-24).

---

## Coverage

- R1 reviewers retrieved: 10 of 11 (learnings empty as expected for greenfield)
- R2 reviewers retrieved: 11 of 12 (correctness, testing, maintainability, project_standards, agent_native, security, performance, reliability, adversarial, api_contract, cli_readiness)
- Confidence-gated suppressions: applied per-reviewer in their JSON; not re-applied at synthesis time
- Pre-existing findings: N/A (greenfield project)
- Cross-reviewer / cross-run agreement (informational): rendered_rows rebuild (R1 #8 / R2 P2-30), log rotation (R1 #11 / R2 P1-10), `status --json` shape divergence (R1 #19 / R2 P1-22), `last_params_list` CLI gap (R1 #6 / R2 P1-24), narrow-terminal weak assertion (R1 #37 / R2 P3-40), peercred-rejection test gap (R1 #21 / R2 P2-33), favorite_list inconsistency (R1 #31 / R2 P3-27), probe vs exit-watcher race (R1 #2/#7 / R2 P0-2 / P1-7).

The probe/exit-watcher race is the cluster where both runs converged most strongly — three reviewers across two runs all flagged it. Treat as the top correctness fix after the P0 mutex crash.

---

## Recommended fix order

1. **P0-1 Mutex crash** (`events.rs:633`). One-line fix; trivial.
2. **P0-2 + P1-7 + P2-9** Probe/watcher state machine. Add `transition(from, to)` compare-and-set; reject moves out of terminal states; preserve probe `Error{cause}` in watcher. Pair with P2-43 (state-machine unit tests).
3. **P0-3 Advanced-flag denylist.** Filter `--host`/`--listen`/`--bind`/`--api-key`/`--ssl-*` before extending argv. Add a test that `start_model { advanced: ["--host", "0.0.0.0"] }` is refused or stripped.
4. **Security P1 cluster** (P1-1 through P1-6): symlink hazards (`O_NOFOLLOW` + ownership checks), `--port 0` reject, `stop_external` pidfd / start_time check, GGUF `MAX_ARRAY_NEST_DEPTH`. Plus state.json TOCTOU (P2-11) and socket TOCTOU (P2-10) in the same patch family.
5. **Correctness P1 runtime** (P1-8, P1-9): log-write error logging, supervisor JoinHandles + watchdog. Plus log rotation (P1-10): either implement and test, or scope down and update the docstring.
6. **Wire/CLI contract freeze before v0.1.0 tag** (P1-18 through P1-27). The renames and `--json` shape decisions calcify after the first tag.
   - Pick IPC method-naming convention (P1-18); add `protocol_version` (P1-19).
   - Standardise `--json` shapes (P1-20, P2-15, P2-19, P2-22, P3-28).
   - Project `favorites list --json` (P1-21); preserve `id` in `status --json` (P1-22); surface launch params (P1-23).
   - Add CLI for `last_params_list` (P1-24); `--json` on `logs`/`start`/`stop`/mutating commands (P1-25, P1-27, P2-22); TTY guard on `stop --all` (P1-26).
   - Document sysexits deviation (P2-18); flatten `status.state` (P2-16); rename `reasoning_hint` boolean (P2-17).
7. **Plan-mandated test gaps that map to P1 findings** (P1-14 through P1-17): drain test, quarantine test, orphan-adoption e2e, chat error-path tests. Plus the framing-desync test for P1-13 (`call_with_timeout`), port-allocation race for P1-11, multi-launch persistence for P1-12.
8. **Reliability P2 cleanup**: `std::thread::sleep` in async (P2-2), watcher `blocking_send` (P2-3), `spawn_logs_poller` backoff (P2-1), signal-handler install failure (P2-4), parallel `stop_all` (P2-5), context-length cap (P2-14), favorite-toggle rollback (P2-13).
9. **Ship hygiene** (P1-28, P1-29, P2-44, P2-45, P3-34): codesign or document `xattr` workaround, list default scan paths, daemon-local-run section in CONTRIBUTING, clippy on macOS in CI, screenshots/asciinema.
10. **Performance** (P2-30 through P2-32, P3-13 through P3-19): row caching, reqwest client reuse, scanner parallelism, LRU swap, supervisor `wait().await`.
11. **Maintainability** (P2-23 through P2-29, P3-20 through P3-26): drop crate-wide `dead_code` allow, collapse `LaunchModeWire`, split `tui/events.rs` and `ipc/methods.rs` before they grow another KLOC, fix the `tabs ↔ events` circular import, extract `model_display_name`, unify drainers.
12. **Test coverage fillers** (P2-33 through P2-43 and P3-35 through P3-43).
13. **Plan doc hygiene**: move four resolved "Open Questions" to "Resolved During Planning".

---

## Source artifacts

**R2 (this run):** `.context/compound-engineering/ce-review/20260516-014905-b9cee91c/`
- `correctness.json` · `testing.json` · `maintainability.json` · `project_standards.json` · `agent_native.json` · `security.json` · `performance.json` · `reliability.json` · `adversarial.json` · `api_contract.json` · `cli_readiness.json`
- `REPORT-PARTIAL.md` — earlier 3-reviewer report (superseded)
- `agent_native.json` includes a `capability_map` table (TUI ↔ CLI parity matrix)

**R1 (prior run):** `/tmp/compound-engineering/ce-code-review/20260515-235146-4db0842c/`
- `adversarial.json` · `correctness` (inline in `report.md`) · `maintainability.json` · `performance.json` · `reliability.json` · `security.json` · `testing.json` · `agent-native-reviewer.txt` · `learnings-researcher.txt` · `report.md` · `metadata.json`

The prior `docs/review.md` (R1 only, 37 findings) is superseded by this consolidated document.
