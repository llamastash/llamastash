# AGENTS.md

This file provides project-level guidance to coding agents (Claude Code, OpenCode, Codex, Copilot CLI) working in this repository. Treat it as authoritative alongside `CONTRIBUTING.md`; on conflict, prefer this file's specifics.

## Source of truth

The implementation plan is the canonical design document:

- `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md` — v1 architecture, security contract, the nine Implementation Units (1: scaffold, 2: daemon/IPC, 3: GGUF, 4: discovery, 5: launch/supervisor, 6: TUI shell, 7: right-pane tabs, 8: non-interactive CLI, 9: release scaffolding), and what is explicitly out of v1.
- `docs/brainstorms/llamatui-requirements.md` — origin requirements (R1–R46) that the plan traces to.
- `docs/architecture.md` — stable user-facing summary of what's actually in the binary.

Before any non-trivial change, identify which Implementation Unit it falls under. PR descriptions should cite the unit; commit subjects often use `feat(unit5):` / `fix(unit3):` style.

## Scope boundaries

The v1 contract — these are deliberate omissions, not gaps:

- **Loopback-only, same-UID.** The daemon binds a Unix domain socket (mode `0600`) with peercred auth. There is no network listener and no v1 path to one. `--host` / `--listen` / `--bind` / `--api-key` / `--ssl-*` are refused if passed via `advanced[]` to `start_model`, and `LLAMA_ARG_*` env vars are stripped before spawn.
- **No HTTP or MCP surfaces in v1.** Deferred to v2 (R34). The CLI `pull` subcommand is hidden and exits unimplemented (R46).
- **Single binary, three roles.** The TUI, CLI, and daemon are all `llamadash`. Daemon spawns on demand when TUI/CLI attach and find the socket missing.
- **Catppuccin Macchiato is the default theme.** Five themes ship total (Macchiato, Latte, Gruvbox Dark, Solarized Dark, Monochrome). Themes are hard-coded palettes; no dynamic loading.

## Build, test, lint

```bash
cargo build                                                # release: cargo build --release
cargo test --features test-fixtures                        # full suite — required for CI parity
cargo test --features test-fixtures --test <name>          # one integration binary
cargo test --features test-fixtures <substring>            # filter by test name
cargo fmt --all -- --check
cargo clippy --all-targets --features test-fixtures -- -D warnings
```

`--features test-fixtures` is required for the integration suite. It enables:

- the `fake_llama_server` binary (`tests/fixtures/fake_llama_server.rs`) that integration tests spawn instead of a real `llama-server` — answers `/health`, `/v1/models`, streaming `/v1/chat/completions`, `/v1/embeddings`, `/v1/rerank`, with deliberate failure-injection markers in request bodies.
- the `_test_sleep` IPC method used by drain-timeout tests (never exposed in release builds because the feature is opt-in and not in the default set).
- `src/gguf/test_fixtures` (`FixtureBuilder`, `build_minimal_gguf`).

Two-space indentation is enforced by `rustfmt.toml`. Clippy denies `shadow_unrelated` crate-wide; rename rather than reuse `let` bindings inside the same scope.

## Running the daemon locally

```bash
cargo run -- daemon start                # foreground; logs to terminal, Ctrl-C to stop
cargo run -- list                        # in another terminal
cargo run                                # opens the TUI against the same daemon
cargo run -- daemon stop
```

Socket paths: `$XDG_RUNTIME_DIR/llamadash/daemon.sock` (Linux), `$TMPDIR/llamadash-$USER/daemon.sock` (macOS). Override with `LLAMADASH_SOCKET=/path/daemon.sock` for side-by-side daemons. If wedged, deleting both `daemon.sock` and `daemon.pid` in the same dir is safe — next `daemon start` rebinds clean.

## Architecture in one breath

```
TUI / CLI ──attach──► Unix-socket JSON-RPC server (peercred)
                          │
                          ├── Discovery (scan + watch + caches)
                          ├── GGUF parser (metadata + identity)
                          ├── Process supervisor (spawn / probe / stop)
                          ├── Resource monitor (RAM/VRAM/CPU)
                          └── Persisted state (favorites / presets / running)
```

- **Wire format.** Length-prefixed JSON-RPC 2.0 envelopes. `src/ipc/framing.rs` is the framing; `src/ipc/methods.rs` is the dispatch table.
- **Model lifecycle.** `Launching → Loading → Ready → Stopping → Stopped`, plus `Error{cause}`. Transitions are guarded — once Stopping or Error, the model never moves out. The supervisor health-probes `/health` every 500 ms during Loading. See `src/daemon/supervisor.rs`.
- **Process survival.** `llama-server` children get their own session via `setsid`, so they outlive the daemon. On daemon restart, an orphan sweep re-adopts each entry in `state.running` only after three-factor confirmation: PID alive, recorded port answering, and `/v1/models` body mentioning the recorded model path.
- **Model identity.** `(canonical absolute path, BLAKE3 of header bytes)`. Renames survive; symlinks dedupe to target; split GGUFs collapse to shard 1.
- **Persistence.** `$XDG_STATE_HOME/llamadash/state.json`, written via `state.json.tmp.<pid>.<rand>` + rename so concurrent writes can't clobber and a same-UID symlink plant can't redirect. Parse failure → `state.json.broken-<ts>` quarantine, boot with defaults.

## CLI agent surface (Unit 8)

Every read-and-mutation command supports `--json` and emits a wrapped object: `{"models":[…]}`, `{"favorites":[…]}`, `{"presets":[…]}`, `{"last_params":[…]}`, `{"stopped":[…],"count":N}`, etc. Stable shapes for agent consumption. Exit codes follow `<sysexits.h>` numerically but with project-specific meanings — pin against the table in `src/cli/exit_codes.rs`, not the libc constants. `stop --all` in a non-TTY context refuses without `--yes`. The IPC `capabilities` method enumerates supported methods so clients can feature-detect.

## Conventions

- Conventional-commit prefixes: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`. Unit-scoped variants are common (`feat(unit8): …`).
- Inline `#[cfg(test)] mod tests` per file is the default; integration tests under `tests/` for daemon-spawning scenarios.
- Comments explain **why**, not **what**. No multi-paragraph doc blocks unless the constraint is genuinely non-obvious. Don't reference task IDs or PR numbers in comments — those rot.
- No `#[allow(...)]` without a one-line reason.

## Protected artifacts

Do not flag these for deletion or `.gitignore` during reviews — they are part of the engineering record:

- `docs/brainstorms/*` — origin requirements.
- `docs/plans/*.md` — implementation plans (living docs with progress checkboxes).
- `docs/solutions/*.md` — solution memos when present.
- `.context/compound-engineering/ce-review/*` — multi-agent review run artifacts.

## Common gotchas

- The CLI/TUI/daemon are one binary. `cargo run -- daemon start` and `cargo run` (TUI) talk to each other via the same socket — running two `cargo run` invocations in parallel without distinct `LLAMADASH_SOCKET` will both attach to the same daemon.
- Integration tests bind to a temp dir per test (`unique_temp_dir(label)`); never share `state_dir` between tests, or they'll race the lockfile.
- `cargo build` (without `--features test-fixtures`) intentionally omits `fake_llama_server` and `_test_sleep`. CI runs both with and without the feature to catch accidental dependencies on test-only surface.
- `cargo install` artifacts deliberately exclude `src/gguf/test_fixtures` and the `_test_sleep` IPC method via feature gating — don't move them out from behind `#[cfg(any(test, feature = "test-fixtures"))]`.
