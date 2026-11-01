---
title: "feat: llamastash delete and the delete_model IPC method"
type: feat
status: active
date: 2026-11-01
---

# feat: `llamastash delete` and the `delete_model` IPC method

## Overview

Deleting a model is TUI-only today: `src/tui/delete.rs` plans and removes the files on the TUI's own thread, then the TUI sends `forget_models` so the daemon drops the rows. An agent can pull a model but never remove one. This plan moves the delete into the daemon behind one new IPC method, `delete_model`, adds the CLI verb `llamastash delete <model>`, and switches the TUI to the same method, so there is one delete path for both clients. R18 Batch 6 in `TODO.md`.

## Problem Frame

- No `delete` subcommand and no IPC method: the one CRUD verb missing from the agent contract.
- `DeletePlan` already encodes what a plain `rm -rf` gets wrong (split shards, an mmproj another model still pairs with, a separate MTP head, unpaired companion precisions, a cache repo that still holds a second quant, HF blob and shared-store blob cleanup, directory-shaped vLLM rows). The CLI must reuse it, not reimplement it.
- The running-launch check lives in the TUI (`delete_refusal_reason_for` in `src/tui/events.rs`). The daemon owns the launch registry, so the check belongs there.

## Requirements Trace

- R1. `llamastash delete <model>` removes the model's files with the same plan the TUI uses.
- R2. A new `delete_model` IPC method does the refusal checks, the plan, the removal and the catalog update in the daemon.
- R3. The TUI's `Ctrl+D` confirm goes through `delete_model`. The TUI keeps its pre-popup toast refusals.
- R4. On a TTY without `--yes`, `delete` shows what goes and asks y/N. Without a TTY, or with `--json`, it refuses without `--yes` (exit 64), the same rule `stop --all` follows.
- R5. No `--dry-run` CLI flag.
- R6. The plan/execute code moves out of `src/tui/` into a non-TUI module.
- R7. `--json` emits a stable wrapped object; exit codes are documented.

## Scope Boundaries

- No `--dry-run` flag (user decision). The IPC method has a `plan_only` param, used internally by the CLI prompt.
- No change to what a delete removes. `DeletePlan`'s rules move as they are.
- No new refusal rules beyond what the TUI enforces today, plus "path is not a catalog row" (trust boundary, see decisions).
- No locking against a `start` that lands between the daemon's check and the unlink. Today's TUI path has the same window; a launch of a file being removed fails on its own.
- No change to the HF pull dialog's `Ctrl+D` (repo delete in `src/tui/hf_dialog*`), unless it calls `tui::delete` (then only the import path changes).

## Context & Research

### Relevant Code and Patterns

- `src/tui/delete.rs`: `DeletePlan`, `plan`, `plan_for_path`, `run`, `execute`, `download_in_flight`, blob helpers, ~1,100 lines of tests. Callers: `src/tui/events.rs` (stage at `apply_delete_model`, confirm at `apply_confirmed` → `spawn_blocking(run)`, `apply_delete_done`), `src/tui/app.rs` (`ConfirmAction::DeleteModel { plan }`), `src/tui/confirm_overlay.rs` (test uses `DeletePlan::single`).
- `src/tui/events.rs`: `delete_refusal_reason_for` (config-declared row, managed `Ready/Loading/Launching/Error`, external process), `WriterCmd::ForgetModels`, `spawn_writer` (connects a `Client` per command and sends feedback events), `apply_delete_done`, delete tests near the end of the file's test module.
- `src/ipc/methods.rs`: dispatch table, `PUBLIC_METHODS` (mirrors `docs/architecture.md`), `forget_models_handler`, `parse_params`, `respond`.
- `src/discovery/catalog.rs`: `ModelCatalog::snapshot`, `forget_missing` (drops rows whose files are gone, then asks for a rescan).
- `src/daemon/context.rs`: `MethodContext` has `catalog`, `supervisors` (`SupervisorRegistry::snapshot` → `(LaunchId, ManagedModel)`), `external` (`RwLock<Vec<…>>` with `model_path`).
- `src/discovery/mod.rs`: `ModelSource::has_local_file`.
- `src/ipc/client.rs`: `timeout_for` gives `start_model` / `stop_model` / `stop_all` the 120 s `ENGINE_CALL_TIMEOUT`.
- `src/cli/stop.rs`: `--all` + `--yes` + `confirm_or_refuse` / `confirm_from` (TTY guard, pure-stdin y/N for tests), JSON shapes `{"stopped": [], "cancelled": true}`.
- `src/cli/resolve.rs`: `fetch_catalog`, `resolve_model` (refuses zero or many matches with `MODEL_NOT_FOUND`).
- `src/cli/cli_args.rs` `Command` enum, `src/cli/mod.rs` dispatch, `src/cli/exit_codes.rs`.
- `tests/cli_bench.rs`, `tests/cli_integration_test.rs`: daemon-spawning CLI test patterns with `unique_temp_dir`.

### Institutional Learnings

- No `docs/solutions/` in this repo. Git history of `src/tui/delete.rs` shows review rounds about a confirm that removes more than the popup named (`18b4e3a`), deletes reaching past what was shown (`5bdb6c5`), and keeping refusals keyed by path. The `expect` check below keeps that guarantee now that the daemon re-plans.

## Key Technical Decisions

- **The daemon only deletes catalog rows.** `delete_model` takes a `path` and refuses one that is not a row in its catalog, or a row whose `source.has_local_file()` is false. Without this the control plane would delete any path a client names. The TUI's old "no row → single-file plan" fallback goes away.
- **The daemon re-plans, the client says what it was shown.** The CLI prompt and the TUI popup show a plan; the delete call carries `expect: {files, repo_dir}` from that plan. The daemon re-plans against its own catalog and HF cache root and refuses, removing nothing, when the new plan would unlink a file or remove a directory `expect` did not name. A smaller plan runs. `--yes` without a prompt sends no `expect`. The client never sends paths to delete, only paths it accepts.
- **Refusals in the daemon** (all are errors, nothing removed): not a catalog row; no local file (config / backend row); a managed launch of that path in any state except `Stopped` (covers `Launching`, `Loading`, `Ready`, `Error`, `Stopping`); an external process whose `model_path` is that path; `expect` exceeded. `execute`'s existing "download in flight" error stays.
- **`plan_only: true`** returns the plan without removing anything. The CLI uses it for the prompt. Not exposed as a CLI flag (R5).
- **Reply shape**, both modes: `{path, files: [...], repo_dir: <path|null>, description}`; a real delete adds `message` (the summary `execute` returns). `files` is `DeletePlan::files()`; `description` is `DeletePlan::describe(<display name>)` with the row's display name.
- **Forget in the handler.** After `execute` (success or failure), the handler calls `catalog.forget_missing(files + repo_dir)`, the same reason the TUI sends `forget_models` on both outcomes today: `remove_dir_all` can fail partway.
- **Remove `forget_models`.** Its only client was the TUI, which now uses `delete_model`. No backwards-compat paths before the first release (AGENTS.md).
- **Module home: `src/discovery/delete.rs`.** It depends only on `discovery::scanner`, `DiscoveredModel` and `util::model_caches`; no rendering. `run` and `DeleteOutcome` stay TUI-side (or are deleted if unused), since they are the TUI's worker shape.
- **CLI flow.** Resolve the reference with `resolve_model` over `fetch_catalog` (exit 66 on zero/many). Without `--yes`: refuse with exit 64 when stdin is not a TTY or `--json` is set; otherwise call `plan_only`, print the description and the file list, ask `[y/N]`. Declined: print `delete: cancelled`, exit 0 (no JSON case: `--json` without `--yes` already refused). Then call `delete_model` with `expect` from the plan (none under `--yes`).
- **New exit code `75` `DELETE_FAILED`**: the daemon refused or the removal failed. Distinct from `STOP_FAILED` so agents can branch on it.
- **`--json` success shape**: `{"deleted": true, "path", "files", "repo_dir", "message"}`. Same keys the IPC reply carries, plus `deleted`.
- **Timeout.** `delete_model` joins the 120 s `ENGINE_CALL_TIMEOUT` list in `src/ipc/client.rs`: a multi-GB `remove_dir_all` on a slow disk can pass 5 s, and the blocking removal runs to the end even if the client gives up.
- **TUI.** `WriterCmd::ForgetModels(paths)` becomes `WriterCmd::DeleteModel { path, expect }`. The writer runs the call in its own spawned task so a long delete does not hold up stop/start commands behind it, and always answers with `Event::Delete(DeleteOutcome { forget, result })`, including on connect failure and RPC error, so `delete_in_flight` and the `deleting…` marker always clear. `apply_delete_done` keeps dropping rows locally on success and stops sending anything to the daemon. `delete_refusal_reason_for` stays for the toast before the popup and at confirm.
- **Shared y/N helper.** Generalize `stop::confirm_from` to take the question text (or move it next to `picker.rs`'s TTY helpers) so `stop --all` and `delete` share one stdin reader. Keep `stop`'s output unchanged.

## Open Questions

### Resolved During Planning

- Daemon vs CLI-local delete: daemon (user).
- TUI switch: yes, to `delete_model` (user).
- Confirm rule: TTY prompt, refuse without `--yes` in non-TTY or `--json` (user).
- `--dry-run`: no (user).
- Should a `Stopping` launch block? Yes; the process still has the file open.

### Deferred to Implementation

- Exact display name the daemon passes to `describe` (catalog row label vs file stem): use whatever `list_models` shows as the row's name.
- Whether the HF pull dialog's repo delete imports `tui::delete`; if so, update the import only.
- Whether `run` / `DeleteOutcome` are still needed after the TUI switch; delete what has no caller.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

```mermaid
sequenceDiagram
  participant C as CLI / TUI
  participant D as daemon delete_model
  C->>D: delete_model {path, plan_only: true}   (CLI prompt only; TUI plans locally)
  D-->>C: {path, files, repo_dir, description}
  Note over C: user confirms
  C->>D: delete_model {path, expect: {files, repo_dir}}
  D->>D: row in catalog? local file? no live/external launch?
  D->>D: plan(row, catalog, cache_root); refuse if plan ⊄ expect
  D->>D: spawn_blocking(execute); catalog.forget_missing(files + repo_dir)
  D-->>C: {path, files, repo_dir, description, message} or error
```

## Implementation Units

- [x] **U1: Move the plan/execute module out of the TUI**

**Goal:** `src/tui/delete.rs` becomes `src/discovery/delete.rs`, with a containment check the daemon will use.

**Requirements:** R6

**Dependencies:** None

**Files:**
- Create (move): `src/discovery/delete.rs` (from `src/tui/delete.rs`, tests included)
- Modify: `src/discovery/mod.rs`, `src/tui/mod.rs`, `src/tui/app.rs`, `src/tui/events.rs`, `src/tui/confirm_overlay.rs`, any other `tui::delete` importer (`rg 'tui::delete'`)

**Approach:**
- `git mv` so history follows. Update the module doc (it no longer says "before the confirm prompt" only for the TUI).
- Add a method on `DeletePlan` that answers "does this plan remove anything outside `{files, repo_dir}`" (true when it has a `repo_dir` and `expect` has none or a different one, or any of `files()` is not in the expected set). `18b4e3a`'s `plan_removes_more_than` is the shape.
- Make `plan_for_path` return `None` (or drop it) for a path with no catalog row, if the TUI still needs it; the daemon calls `plan` directly.

**Test scenarios:**
- Existing `delete.rs` tests pass unchanged after the move.
- Happy path: a plan compared with an `expect` equal to itself removes nothing more.
- Edge case: `expect` without `repo_dir`, plan with `repo_dir` → removes more.
- Edge case: plan has a projector `expect` lacks → removes more.
- Edge case: plan is a strict subset of `expect` (a companion now paired by a neighbour) → does not remove more.

**Verification:** the crate builds; the moved tests pass under `--lib discovery::delete`.

- [x] **U2: `delete_model` IPC method; drop `forget_models`**

**Goal:** the daemon refuses, plans, deletes and forgets in one handler.

**Requirements:** R2, R7

**Dependencies:** U1

**Files:**
- Modify: `src/ipc/methods.rs` (handler, dispatch arm, `PUBLIC_METHODS`, remove `forget_models` handler and its tests), `src/ipc/client.rs` (`timeout_for`)
- Test: `src/ipc/methods.rs` (inline `mod tests`)

**Approach:**
- Params `{path, plan_only?: bool, expect?: {files: [path], repo_dir?: path}}`; unknown shape → `InvalidParams`.
- Order: catalog row lookup → `has_local_file` → registry snapshot (any state except `Stopped` blocks) → `external` → `plan(row, catalog, hf_cache_dir().ok())` → `plan_only` return → `expect` check → `spawn_blocking(execute)` → `forget_missing(files + repo_dir)` → reply.
- Refusal messages name the reason and the next step, matching the TUI toasts ("model is running — stop the launch first", etc.). Use `InvalidParams` for refusals and `InternalError` for an `execute` I/O failure, carrying the `io::Error` text.
- `forget_missing` runs on the error path too.

**Patterns to follow:** `forget_models_handler`, `stop_model_handler` (`parse_params`, `respond`), the existing `forget_models_drops_gone_rows_and_keeps_the_live_one` test setup (`unique_temp_dir`, catalog seeding).

**Test scenarios:**
- Happy path: a single-file GGUF row in a temp dir → file gone, row gone from `catalog.snapshot()`, reply lists the file and a `message`.
- Happy path: `plan_only: true` → reply lists files, nothing removed, row still listed.
- Happy path: split set → every shard removed.
- Error path: path not in the catalog (a real file on disk) → refused, file still there.
- Error path: a `Config`/`Backend` source row → refused.
- Error path: a registry entry for that path in `Ready` (and one in `Error`) → refused, file stays; one in `Stopped` → allowed. Use whatever test constructor the registry tests use; if none exists for a non-running `ManagedModel`, cover `Ready` via the closest existing helper and note it.
- Error path: an `external` row with that `model_path` → refused.
- Error path: `expect` names only the primary while the plan also takes an unpaired projector → refused, nothing removed.
- Edge case: `expect` larger than the plan → runs.
- `capabilities` lists `delete_model` and no longer lists `forget_models`.

**Verification:** `--lib ipc::methods` passes; `forget_models` has no remaining reference in `src/`.

- [x] **U3: `llamastash delete <model>`**

**Goal:** the CLI verb on top of `delete_model`.

**Requirements:** R1, R4, R5, R7

**Dependencies:** U2

**Files:**
- Create: `src/cli/delete.rs`
- Modify: `src/cli/cli_args.rs` (`Command::Delete(DeleteArgs { model, yes, json })`, help text, parse tests), `src/cli/mod.rs` (dispatch), `src/cli/exit_codes.rs` (`DELETE_FAILED = 75`), `src/cli/stop.rs` (shared y/N helper)
- Test: `src/cli/delete.rs`, `src/cli/cli_args.rs`, `tests/cli_delete.rs` (new)

**Approach:**
- Flow per Key Technical Decisions. `--json` and the TTY rendering follow `stop`'s patterns (`pretty_json`, `colors::success`, respect `cli.quiet`).
- Daemon errors map to `DELETE_FAILED` with the daemon's message; resolve misses stay `MODEL_NOT_FOUND`; the missing-`--yes` refusal is `USAGE` with "delete in a non-interactive context requires --yes".
- If the completions test or a help golden lists subcommands, update it.

**Patterns to follow:** `src/cli/stop.rs` (`--all`/`--yes`, `confirm_from`), `src/cli/bench.rs` for a newer verb's wiring.

**Test scenarios:**
- Unit: `delete qwen --yes --json` parses; `delete` with no model is a clap error.
- Unit: shared y/N helper: `y\n` → true, `n\n`/empty → false; `stop --all`'s prompt text unchanged.
- Integration (`tests/cli_delete.rs`, isolated daemon, temp model dir with a fixture GGUF): `delete <name> --yes --json` → exit 0, JSON has `deleted: true` and the file path, file gone, `list --json` no longer shows it.
- Integration: piped stdin, no `--yes` → exit 64, file still there.
- Integration: `--json` without `--yes` → exit 64.
- Integration: unknown reference → exit 66.
- Integration: a model the daemon refuses (simplest available: a reference that resolves but whose file the test removed and replaced with a directory, or a running fake launch if the file already uses `fake_llama_server`) → exit 75. Pick whichever the existing fixtures make cheap; one refusal case is enough here, U2 covers the rest.

**Verification:** `--test cli_delete` and `--lib cli::` pass.

- [x] **U4: TUI `Ctrl+D` through `delete_model`**

**Goal:** the TUI has no local delete path left.

**Requirements:** R3

**Dependencies:** U1, U2

**Files:**
- Modify: `src/tui/events.rs` (`WriterCmd`, `encode_writer_cmd`, `spawn_writer`, `apply_confirmed`, `apply_delete_done`, delete tests), `src/tui/app.rs` if `ConfirmAction::DeleteModel` changes
- Test: `src/tui/events.rs` (inline tests)

**Approach:**
- Staging stays: refusal toast, then `plan_for_path` over `app.models` for the popup text.
- Confirm: re-ask `delete_refusal_reason_for`, set `delete_in_flight`, send `WriterCmd::DeleteModel { path, expect }` built from the staged plan. Toast "deleting …" as now.
- Writer: for `DeleteModel`, spawn the call (connect + `delete_model`) in its own task and send `Event::Delete` with `forget = files + repo_dir` from the staged plan and `result` = the reply's `message` or the error text. Connect failure and RPC error both produce `Event::Delete(Err)`.
- `apply_delete_done`: no more `ForgetModels`; the rest unchanged.
- Without a writer (tests / no daemon): the existing "unavailable" toast path, and `delete_in_flight` is not set.

**Patterns to follow:** `dispatch_writer`, the `StartAccepted` feedback event in `spawn_writer`.

**Test scenarios:**
- Happy path: confirm sends exactly one `WriterCmd::DeleteModel` whose `expect` matches the staged plan's files; no file is touched by the TUI.
- Happy path: `Event::Delete(Ok)` drops the rows under `forget`, clears `delete_in_flight`, toasts the message, sends nothing to the writer.
- Error path: `Event::Delete(Err)` keeps the rows and clears `delete_in_flight`.
- Error path: confirm while a launch of that path became `Ready` after staging → toast, no command sent (existing test, kept).
- Replace the tests that called `delete::run` directly; drop ones that only covered `ForgetModels`.

**Verification:** `--lib tui::events` passes. E2E in step 8: TUI `Ctrl+D` on a temp model against an isolated daemon.

- [x] **U5: Docs, CHANGELOG, TODO**

**Goal:** docs match the new surface in the same branch.

**Requirements:** R7

**Dependencies:** U2, U3, U4

**Files:**
- Modify: `docs/usage.md` (new `llamastash delete` section near `stop`, refusals, JSON shape, exit-code table row 75, TUI "Deleting a model" paragraph: the TUI now asks the daemon, `forget_models` gone), `docs/architecture.md` (IPC table: `delete_model` row replaces `forget_models`; module list if it names `tui/delete.rs`), `CHANGELOG.md` (one line under `[Unreleased]`), `TODO.md` (tick R18 Batch 6), `FEATURES.md` / `README.md` only if they list CLI verbs or say delete is TUI-only, this plan (tick units)

**Approach:** one short section, plain facts. `rg -n 'forget_models|tui/delete|TUI-only' docs README.md FEATURES.md AGENTS.md` to find drift.

**Test expectation:** none -- docs only.

**Verification:** no doc mentions `forget_models` or `src/tui/delete.rs`.

## System-Wide Impact

- **Interaction graph:** CLI and TUI both call `delete_model`; the daemon's catalog update replaces the TUI-sent `forget_models`.
- **Error propagation:** daemon refusal/I/O error → JSON-RPC error → CLI exit 75 with the message / TUI error toast.
- **State lifecycle risks:** partial `remove_dir_all` failure: the handler still forgets what is gone. A `start` racing the check is accepted (see Scope).
- **API surface parity:** `capabilities` changes (`+delete_model`, `-forget_models`); `docs/architecture.md` must match `PUBLIC_METHODS`.
- **Unchanged invariants:** what `DeletePlan` removes; TUI refusal toasts and the `deleting…` marker; `stop --all` prompt and JSON.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| The daemon and the TUI resolve a different HF cache root (different env), so plans differ | `expect` check: a bigger daemon plan refuses instead of deleting more |
| Writer task blocked by a long delete | Delete call runs in its own spawned task |
| `delete_in_flight` never clears when the writer cannot connect | Every writer outcome for `DeleteModel` sends `Event::Delete` |
| Substring reference deletes the wrong unique match under `--yes` | `resolve_model` refuses ambiguity; the reply and output name the deleted path |

## Setup

- `CARGO_HUSKY_DONT_INSTALL_HOOKS=true` for cargo commands in this worktree.
- Test binaries are pre-built in this worktree's `target/` (`cargo nextest run --features test-fixtures --no-run`).

## Verification

- `cargo nextest run --features test-fixtures --lib discovery::delete`
- `cargo nextest run --features test-fixtures --lib ipc::methods`
- `cargo nextest run --features test-fixtures --lib cli::`
- `cargo nextest run --features test-fixtures --lib tui::events`
- `cargo nextest run --features test-fixtures --test cli_delete`
- `make lint`
- Full suite once at the end: `cargo nextest run --features test-fixtures`

## Definition of Done

- All units ticked, verification commands pass.
- `rg forget_models src docs` and `rg 'tui::delete' src` return nothing.
- E2E against an isolated daemon (`LLAMASTASH_STATE_DIR`, non-default `--proxy-port`): `delete` on a temp model via CLI (TTY prompt, `--yes`, `--json`, non-TTY refusal) and via TUI `Ctrl+D`.

## Sources & References

- `TODO.md` § R18 Batch 6
- `src/tui/delete.rs`, `src/tui/events.rs`, `src/ipc/methods.rs`, `src/cli/stop.rs`
- Commits `18b4e3a`, `5bdb6c5`, `6f80fb5` (delete review history)
