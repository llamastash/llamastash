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

- **D1. Log pump reads bytes, not `String`, and splits long lines.** Replace `read_line` with `read_capped_line`: it reads up to `\n` with `fill_buf()` / `consume()` and returns at most `MAX_LOG_LINE_BYTES = 8 * 1024` bytes. A longer line comes back as several 8 KiB lines, so memory stays bounded and no byte is lost. A line of exactly 8 KiB keeps its `\n`. Then `String::from_utf8_lossy`. A read error other than EOF still ends the pump. (Updated 2026-11-19 after review: the first version dropped the rest of a long line, which lost errors printed late on a `\r`-rewritten progress line.)
- **D2. Ring slots stay bounded.** With the 8 KiB cap at read time, `push_copy` never grows a slot past ~8 KiB. No change to `RingBuffer` beyond that.
- **D3. Delete plans against the hub root that holds the path.** Add `pub fn hf_hub_root_for(path: &Path) -> Option<PathBuf>` in `src/discovery/delete.rs`. It calls `crate::util::model_caches::huggingface_hub_dirs(crate::util::paths::home_dir().as_deref())` and passes the list to a private `fn hub_root_holding(path: &Path, roots: &[PathBuf]) -> Option<PathBuf>`, which returns the deepest root for which `hf_repo_dir_in_cache(path, Some(root))` is `Some`, so the shared-store sweep finds the root the repo really sits in. (Updated 2026-11-19 after review: the first version took the first match in env order.) Both callers use it in place of `crate::init::download::hf_cache_dir()`: `delete_model_handler` in `src/ipc/methods.rs` (`plan(row, &catalog, cache_root)`) and `plan_for_path` in `src/discovery/delete.rs`. `plan()` / `execute()` / `remove_file_and_blob` keep their `cache_root: Option<&Path>` signature, so the blob unlink and the shared-store sweep work unchanged against the right root.
- **D4. Build mismatch: a TUI popup only (user decision 2026-11-19).** When `status.daemon.build` differs from the TUI's `CARGO_PKG_VERSION`, `App::ingest_status` opens a `ConfirmAction::DaemonBuildMismatch` notice once per session, and waits for a later status tick while another popup is open. Both buttons close it. Its text names the restart-daemon key from the active keymap. The CLI does not change, and nothing restarts the daemon. Reason: the first version restarted an idle daemon from the current command's flags. That dropped the flags the old daemon started with (`--proxy-host`, `--proxy-port`, `--model-path`, `--llama-server`), and it took a daemon away from a systemd or launchd service.

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

### U3. Build mismatch notice in the TUI

- Files: `src/tui/app.rs`, `src/tui/confirm_overlay.rs`, `src/tui/events.rs`.
- Per D4. Tests: an older build opens the notice once; a matching build opens nothing; an open popup delays it to a later tick; the notice text follows a remapped restart key.
- Goldens need no change: their fixture sets `daemon_info` directly, so the notice never opens there.
- Depends on: nothing.

### U4. Docs

- Files: `CHANGELOG.md`, `TODO.md`, `docs/architecture.md`, `docs/troubleshooting.md`, `docs/usage.md`.
- `CHANGELOG.md` `[Unreleased]` `### Fixed`: three one-liners (log capture survives bad UTF-8 and splits long lines; `delete` frees the blob in any hub root; the TUI popup on a build mismatch).
- `TODO.md`: strike the three `### Batch 2` items with a done note, in place under `## Next`.
- `docs/architecture.md` (`daemon.build`), `docs/usage.md` (`delete` uses every hub root), `docs/troubleshooting.md` (the popup and how to restart the daemon).
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

- [x] U1 log pump
- [x] U2 delete hub root
- [x] U3 build mismatch notice
- [x] U4 docs
