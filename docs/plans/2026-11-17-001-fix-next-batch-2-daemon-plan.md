---
title: "fix: Next batch 2 — daemon and IPC fixes"
type: fix
status: active
date: 2026-11-17
---

# fix: Next batch 2 — daemon and IPC fixes

## Goal

Fix the three `TODO.md` `## Next` → `### Batch 2` items:

1. One invalid UTF-8 byte in an engine's output stops its log capture for good, and lines have no length cap.
2. `delete` frees no disk for a model in a second Hugging Face hub root.
3. Nothing notices a CLI or TUI that talks to a daemon from an older build.

## Scope

In: the three fixes, one test each, docs (`CHANGELOG.md`, `TODO.md`, `docs/architecture.md`, `docs/troubleshooting.md`).
Out: a protocol version bump, a git-sha build id (the build stays `CARGO_PKG_VERSION`), any new config key or flag.

## Decisions

- **D1. Log pump reads bytes, not `String`.** Replace `read_line` with a bounded byte read: read up to `\n`, keep at most `MAX_LOG_LINE_BYTES = 8 * 1024` bytes of the line, and discard (consume without storing) the rest of an over-long line up to its `\n`. Then `String::from_utf8_lossy` on the kept bytes. Reason: `read_line` returns `InvalidData` on bad UTF-8 and the pump returned, which dropped the pipe. `read_until` alone has no memory cap, so do the cap while reading: loop on `fill_buf()` / `consume()` (`tokio::io::AsyncBufReadExt`), copy at most the remaining budget into a reused `Vec<u8>`, stop at the first `\n`. A truncated line gets a ` …[truncated]` suffix so the reader sees the cut. A read error other than EOF still ends the pump (that is a broken pipe, not a bad byte), as today. Keep the trailing `\r`/`\n` trim.
- **D2. Ring slots stay bounded.** With the 8 KiB cap at read time, `push_copy` never grows a slot past ~8 KiB. No change to `RingBuffer` beyond that.
- **D3. Delete plans against the hub root that holds the path.** Add `pub fn hf_hub_root_for(path: &Path) -> Option<PathBuf>` in `src/discovery/delete.rs`. It calls `crate::util::model_caches::huggingface_hub_dirs(crate::util::paths::home_dir().as_deref())` and passes the list to a private `fn hub_root_holding(path: &Path, roots: &[PathBuf]) -> Option<PathBuf>`, which returns the first root for which `hf_repo_dir_in_cache(path, Some(root))` is `Some`. Both callers use it in place of `crate::init::download::hf_cache_dir()`: `delete_model_handler` in `src/ipc/methods.rs` (`plan(row, &catalog, cache_root)`) and `plan_for_path` in `src/discovery/delete.rs`. `plan()` / `execute()` / `remove_file_and_blob` keep their `cache_root: Option<&Path>` signature, so the blob unlink and the shared-store sweep work unchanged against the right root.
- **D4. Build mismatch: restart an idle daemon, else warn.** In `src/cli/client.rs`, `connect_or_spawn`'s `Ok(client)` branch already calls `reconcile_binary_with_running_daemon`, which reads `status`, restarts the daemon when no managed launch runs (`status.models` empty), and keeps the connection when launches run. Extend this one reconcile step so it also compares `status.daemon.build` with `env!("CARGO_PKG_VERSION")`:
  - Same build and no `--llama-server` mismatch: return the client, as today.
  - Different build, no managed launches: restart the daemon with the same shutdown + `spawn_and_attach` sequence the `--llama-server` path uses (one shared code path, not a copy). Print one stderr line: `daemon: restarted to match this build (<old> → <new>)`. Also `log::info!`.
  - Different build, managed launches running: keep the client. Print one stderr warning: `daemon: running build <old>, this client is <new>; stop the running models and run \`llamastash daemon restart\` to update it`. Also `log::warn!`.
  - `status` fails or has no `daemon.build` field: pass through with no restart and no warning.
  - Call `status` once and use it for both checks. When `cli.llama_server` is unset, the build check still runs (today the function returns early in that case; move that early return so it only skips the binary comparison).
  - stderr, not stdout: `--json` output on stdout stays byte-stable.
- **D5. TUI shows the mismatch.** `DaemonInfo.build` is already parsed from `status` (`src/tui/app.rs`) but never drawn. In `daemon_row` in `src/tui/info_pane.rs`, when `app.daemon_connected` and `daemon_info.build` is `Some(b)` with `b != env!("CARGO_PKG_VERSION")`, append `{middot} build <b> vs <client>` in `palette.warning_style()`, with the separator from `crate::tui::glyphs::active().middot()` like the other spans in that row. Use the plain word `vs` so the ASCII glyph set needs no new glyph. No keybinding text involved.

## Implementation units

All units run in one lane, in order. They share `CHANGELOG.md` and `TODO.md`.

### U1. Log pump survives bad UTF-8 and long lines

- Files: `src/daemon/supervisor.rs`.
- Edit `pump_stream` per D1. Add `const MAX_LOG_LINE_BYTES: usize = 8 * 1024;` near the ring/log constants.
- Factor the bounded line read into a small `async fn read_capped_line<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>, cap: usize) -> io::Result<Option<bool>>` (returns `None` on EOF with nothing read, else `Some(truncated)`), so it is unit-testable without a child process. Keep it private.
- Tests (in the existing `#[cfg(test)] mod tests` in `supervisor.rs`), using `tokio::io::BufReader` over a `&[u8]`:
  - bad UTF-8 in line 1 (`b"ok\xff\xfe\nnext\n"`): two lines come back, the first lossy (`ok��`), the second `next`.
  - a line longer than the cap: kept bytes == cap, `truncated == true`, and the next line reads correctly.
  - a final line with no `\n` before EOF is returned, then EOF.
- Depends on: nothing.

### U2. Delete plans against the right hub root

- Files: `src/discovery/delete.rs`, `src/ipc/methods.rs`.
- Add `hf_hub_root_for` and `hub_root_holding` per D3. Switch both callers.
- Test in `src/discovery/delete.rs` tests: build two temp hub roots (`unique_temp_dir`-style helper the module already uses, or `tempfile`), put a `models--o--r/{blobs/<sha>, snapshots/<rev>/m.gguf -> ../../blobs/<sha>}` tree in the **second** root, assert `hub_root_holding(&snapshot_file, &[root_a, root_b]) == Some(root_b canonical or as given)`, then `plan(&row, &[row], Some(&root_b))` + `execute` removes the blob (the file under `blobs/` is gone). Follow the existing delete tests in that module for how they build a `DiscoveredModel` row and an HF tree; reuse their helpers.
- Depends on: nothing.

### U3. Build mismatch on connect

- Files: `src/cli/client.rs`, `src/tui/info_pane.rs`.
- Edit `reconcile_binary_with_running_daemon` per D4 (rename it to `reconcile_with_running_daemon` and update its one caller and its doc comment). Pull the "is there a mismatch, and what does the status say" decision into a pure function so it is testable without a daemon, e.g. `fn reconcile_action(status: &Value, client_build: &str, binary_matches: Option<bool>) -> ReconcileAction` with variants `Keep`, `Restart { reason }`, `Warn { message }`. The async function does the IO.
- Tests in `src/cli/client.rs` tests for `reconcile_action`: same build → `Keep`; different build + empty `models` → `Restart`; different build + non-empty `models` → `Warn`; missing `daemon.build` → `Keep`; binary mismatch + same build + idle → `Restart` (old behavior kept).
- TUI: edit `daemon_row` per D5. Add one test next to the existing `info_pane.rs` tests: a connected app with `daemon_info.build = Some("0.0.1")` renders a line containing `0.0.1`; same build as `CARGO_PKG_VERSION` renders no `build` text.
- Golden snapshots: if a TUI golden snapshot test breaks, it must not, since fixtures with `build` equal to the crate version add nothing. If a fixture uses an older build string (`"0.1.0"` in `app.rs` tests is a unit test, not a golden), check `tests/` goldens with `rg -n '"build"' tests/` and update only what this change truly alters.
- Depends on: nothing.

### U4. Docs

- Files: `CHANGELOG.md`, `TODO.md`, `docs/architecture.md`, `docs/troubleshooting.md`.
- `CHANGELOG.md` `[Unreleased]`: add a `### Fixed` section (match the format of earlier releases in the file) with three one-liners: engine log capture survives invalid UTF-8 and caps a line at 8 KiB; `delete` frees the blob of a model in any Hugging Face hub root; a CLI or TUI from a newer build restarts an idle daemon from an older build, or warns when models run.
- `TODO.md`: mark the three `### Batch 2` items `- [x]` with strike-through and ` — **done 2026-11-17**` (same style as the Batch 4 entry in the main checkout: `- [x] ~~text~~ — **done …**`). Keep them in place under `## Next`.
- `docs/architecture.md` near line 423 (the `--llama-server` reconcile paragraph): add one or two sentences that the same reconcile also compares the daemon's `build` with the client's and restarts an idle daemon or warns.
- `docs/troubleshooting.md`: add a short entry: symptom "after an upgrade the CLI warns `daemon: running build X, this client is Y`", fix "stop the running models, then `llamastash daemon restart`".
- Depends on: U1-U3.

## Setup

None beyond a normal cargo build (the worktree's `target/` is fresh; the first `cargo check` builds it).

## Verification

```bash
cargo check --features test-fixtures
cargo nextest run --features test-fixtures --lib supervisor
cargo nextest run --features test-fixtures --lib delete
cargo nextest run --features test-fixtures --lib client
cargo nextest run --features test-fixtures --lib info_pane
make lint
```

The pre-commit hook runs `cargo fmt` + `make lint` + `make doc`. The full suite runs in the final review (step 8), not in the lane.

E2E (final review only, isolated daemon, never the user's): build, start a daemon under a temp `LLAMASTASH_STATE_DIR` and non-default ports, confirm `status --json | jq .daemon.build`, confirm the TUI `--render` Daemon row.

## Definition of done

- All three Batch 2 items fixed, each with the test above.
- Verification commands pass.
- Docs updated per U4, all in the lane's commits.
- `- [x]` ticks in this plan's checklist below.

## Checklist

- [ ] U1 log pump
- [ ] U2 delete hub root
- [ ] U3 build mismatch
- [ ] U4 docs
