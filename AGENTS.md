# AGENTS.md

Project-level guidance for coding agents (Claude Code, OpenCode, Codex, Copilot CLI). Authoritative alongside `CONTRIBUTING.md`; on conflict, prefer this file's specifics.

> **Keep this file small — hard ceiling 200 lines.** It is loaded into every session before any work starts, so every line costs context on every task, forever. The bar for a line here is: *an agent behaves differently because it read this at session start*. Everything else is **reference** — feature descriptions, API field lists, wire shapes, config keys, "what shipped in vX" — and belongs in `docs/` or `CHANGELOG.md`, with at most a one-line pointer from here. Before adding: does this change what an agent does, or just tell it something it could look up? If a change would push past 200 lines, cut or relocate in the same commit; don't let it ride. This file was 358 lines and ~16k tokens once, mostly because feature specs accumulated here instead of in `docs/`.

## Where things are documented

Read the relevant doc before non-trivial work in that area; don't re-derive from code.

| Area | Doc |
|---|---|
| What's actually in the binary — modules, lifecycle, IPC, `status` shape, backend internals, servers, MTP, ds4 admission | `docs/architecture.md` |
| CLI subcommands, flags, JSON shapes, config keys, exit codes, keybindings | `docs/usage.md` |
| Failure modes an end user hits | `docs/troubleshooting.md` |
| Design intent + tradeoffs, per feature | `docs/plans/*.md` (dated, one per feature) |
| Origin requirements (R1–R46 v1, R48–R80 v2) | `docs/brainstorms/*requirements*.md` |
| Pre-implementation findings | `docs/spikes/*.md` |
| Everything still open | `TODO.md` |
| Real-hardware UAT | `docs/testing/hardware-uat.md` |
| Built-in `(arch, gpu_backend)` defaults table | `src/launch/AGENTS.md` (loads when working under `src/launch/`) |

v1's nine Implementation Units (1 scaffold, 2 daemon/IPC, 3 GGUF, 4 discovery, 5 launch/supervisor, 6 TUI shell, 7 right-pane tabs, 8 CLI, 9 release) are defined in `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`. Identify the unit before a non-trivial change; commit subjects use `feat(unit5):` / `fix(unit3):`.

## Rules

**Docs ship with code.** Any change to user-visible behavior, the CLI/IPC surface, config shape, install paths, exit codes, dependencies, scope, or architecture updates the affected docs in the **same commit**. Check for drift in: `README.md`, this file, `INSTALL.md`, `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, `config.example.yaml`, `Cargo.toml`, `TODO.md`, and the `docs/` files in the table above. Tick the matching `- [ ]` → `- [x]` in the feature's plan. If a change makes a doc statement wrong, fix or delete it — don't leave the contradiction. New user-facing concept: add a section to the closest existing doc, don't spawn a file.

**CHANGELOG entries are short one-liners** under `[Unreleased]`. Not every change earns a bullet; no implementation detail. Bundle related changes and link the PR/commit (`(#123)`).

**`TODO.md` is the single index of open work.** Adding a `TODO(...)`/`FIXME` in code, an unchecked `- [ ]` in a doc, or a deferred review follow-up means adding a one-line `TODO.md` entry linking back to it. Completing one strikes both in the same change.

**No backwards-compat / legacy paths** until the first release.

**Comments explain why, not what.** Add one only when it carries something the code can't show. No prose that paraphrases the next line, no multi-paragraph doc blocks unless the constraint is genuinely non-obvious, no task IDs or PR numbers (they rot). No `#[allow(...)]` without a one-line reason.

**Keybinding labels are never hardcoded in UI.** Help bars, footers, hints, and popup affordances all derive their key text from the active `KeyMap` (`src/tui/keybindings.rs` — `Binding::label` / `description`), never from inline literals, so user config overrides show up everywhere. New action: add it to the `Action` enum and the right `*_BINDINGS` slice with a label/description, then look it up at render time (`KeyMap::bindings_for(Focus)`).

**TUI glyphs are single-cell text-presentation BMP symbols.** Emoji-presentation codepoints (`⚡` U+26A1, anything in an emoji block or carrying a default emoji variation selector) render double-width and colored, which breaks column alignment. Pick from the geometric / arrow / symbol text ranges already in `src/tui/glyphs.rs` and eyeball it with `--render` before committing.

**Style:** plain facts and numbers over jargon. Conventional-commit prefixes (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`), unit-scoped where it fits.

## Build, test, lint

```bash
make build                                                 # release: cargo build --release
make test                                                  # cargo test --features test-fixtures — required for CI parity
cargo test --features test-fixtures --test <name>          # one integration binary
make lint                                                  # fmt --check + clippy -D warnings
make audit                                                 # maintainer bundle → target/audit; make audit-summary for the headline
```

Prefer `make` targets — they carry the standard flags (forgetting `--features test-fixtures` on tests is the classic mistake). See the `Makefile` for the rest, including `make uat-*`.

`--features test-fixtures` gates `fake_llama_server` (`tests/fixtures/`), the `_test_sleep` IPC method, and `src/gguf/test_fixtures`. `--features uat` gates the maintainer-only `llamastash uat` subcommand, never shipped in release binaries. Two-space indent is enforced by `rustfmt.toml`; clippy denies `shadow_unrelated`, so rename rather than reuse a `let` binding in the same scope.

Inline `#[cfg(test)] mod tests` per file is the default; `tests/` for daemon-spawning scenarios. Integration tests bind a temp dir per test (`unique_temp_dir(label)`) — never share `state_dir` between tests or they race the lockfile.

**Never hand out a bare `llamastash <args>` for a dev task** — it resolves to whatever is on `PATH`, not the working tree, so it won't reflect the change under test. Use `cargo run -- <args>`, a `make` target, or `cargo build` + `./target/debug/llamastash <args>`, and isolate side-by-side daemons with `LLAMASTASH_STATE_DIR` + a non-default `--proxy-port` so you never touch the user's real daemon. Bare `llamastash` is only for genuine model-management work the user is doing with the tool.

## End-to-end CLI validation (required for user-visible changes)

A passing `cargo test` is necessary but **not sufficient**. Stale daemons, missed env vars, deferred restarts, and client/server schema drift all hide behind green CI. After any change a user would notice, run the binary and look at it:

```bash
cargo build --bin llamastash
target/debug/llamastash daemon stop            # a running daemon uses the OLD (deleted) binary
target/debug/llamastash daemon start
target/debug/llamastash status --json | jq .   # confirm new IPC fields
target/debug/llamastash list                   # the change you just made
target/debug/llamastash                        # TUI: pan through every visible panel
```

For TUI changes, **look at the panel you touched** — golden snapshots catch byte-exact regressions, not "the field is empty in real life because the daemon doesn't surface it yet." Agents without a terminal drive the TUI in a pty via `scripts/tui/` (`tui_drive.py` to look, `harness.py` to gate; contract in `scripts/tui/README.md`). One-frame renders are cheaper via `llamastash --render --render-size 160x45` / `make render`.

When E2E catches a regression the suite missed, add the regression test before fixing.

## Running the daemon locally

```bash
cargo run -- daemon start   # foreground, Ctrl-C to stop
cargo run -- list           # another terminal
cargo run                   # TUI against the same daemon
```

Clients read `$XDG_STATE_HOME/llamastash/runtime.json` (mode `0600`) for the control-plane URL + bearer token, under the keys `ipc_url` / `ipc_token`. Under a `LLAMASTASH_STATE_DIR` override the file sits directly in that dir, no `llamastash/` subdir. `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` (both required together) skip reading it. Wedged? Deleting `runtime.json` + `daemon.pid` is safe — the next `daemon start` rebinds clean. For full isolation pair `LLAMASTASH_STATE_DIR` with `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR`, and `HF_HOME` (see `docs/usage.md §Environment variables`). The CLI, TUI, and daemon are **one binary**: two `cargo run` invocations without distinct `LLAMASTASH_STATE_DIR` attach to the same daemon.

## Architecture in one breath

JSON-RPC 2.0 over `POST /rpc`; `src/ipc/methods.rs` is the dispatch table, `src/daemon/control_plane.rs` the hyper service in front of it. Lifecycle, model identity, persistence shape, backend internals, and the full `status` response shape live in `docs/architecture.md`.

## Adding a backend (keep it leak-free)

A new backend is **one new module plus the minimum central wiring**; removing one is deleting the module plus that wiring. **No backend id-string or name may appear — in code *or comments* — outside these three places:**

1. `src/backend/<id>/` — **all** its logic, behind trait methods: argv/launch translation, `resolve_launch_binary`, identity, capabilities, native knobs, `seed_launch_knobs`, `auto_routes`, `serves_mode`, `serves_web_ui`, `refuses`, `kv_bytes`, `process_marker`, availability (`available` / `installed` / `status_*`), and — for a managed multiplexer — the `start` / `stop` overrides, `umbrella_launch_id`, and `supervise_at_boot`. Delegation specifics (umbrella-unload, "what's resident") stay private to the module. A process-per-model backend leaves `start` / `stop` on their defaults and never touches lifecycle plumbing.
2. `src/backend/mod.rs` — `pub mod <id>;`, the `use <id>::{…}`, a `Backends` variant, one `for_each_backend!` arm, one line in `Backends::all()`, and a `BackendConfig` field only if it has a `backend.<id>:` block.
3. Its typed config struct, owned by its own module, re-exported from `crate::config` for path stability.

`BackendChoice` is `Auto | Explicit(String)` — an id is just data, so no `BackendChoice` or CLI edit is needed.

**Rule of thumb:** about to write `== "<backend>"`, `resolve_<backend>_binary`, `<Backend>::new()`, or name a backend outside those three? Call a trait method or registry helper instead. User-facing *strings* naming the resolved backend are fine — derive them from `backend.id()`.

The hook-by-hook table of how the generic tree stays agnostic is in `docs/architecture.md` § Backend neutrality contract.

## Scope boundaries

Deliberate omissions, not gaps. Don't "fix" these without a decision.

- **Loopback-only, same-UID.** Control plane on `:11436` (bearer-authed), proxy on `:11435` (`:11434` in Ollama-compat mode). `--host` / `--listen` / `--bind` / `--api-key` / `--ssl-*` are refused via `advanced[]`, and `LLAMA_ARG_*` env vars are stripped before spawn. ds4 extends the denylist with `--cors` / `--dist-`. LAN bind + bearer key are opt-in; the loopback default has no auth, no TLS, no peercred.
- **Proxy scope.** OpenAI `/v1/*` plus the Anthropic `/v1/messages` surface are forwarded (no body translation); `/ui` reverse-proxies the running model's stock llama.cpp web UI on one port-stable origin. Still deferred from R34: MCP, fallback tuning, TLS for a LAN-exposed proxy. → `docs/architecture.md`, plans `2026-05-21-001` / `2026-06-15-001`.
- **Presets live in `config.yaml`, not `state.json`** — that is the writable source of truth, written comment-safe through `config::yaml_edit`. A typed knob delegated to `--fit` is the bare token `auto` (**not** `{auto:true}`); a literal `"auto"` value needs the `{value: auto}` escape. Same encoding in `config.yaml`, `--json`, and `state.json`. No `export`, no `presets_set_default`, no TUI list/delete. → `docs/architecture.md` § Named presets, plans `2026-06-22-001` / `2026-06-30-001`.
- **Three backends; llama.cpp is the stable default.** Lemonade and ds4 are experimental and default-on only when their binary resolves. A ds4-compatible GGUF that can't use ds4 **falls back to llama.cpp — never a refusal**. R13 ("a disk GGUF binds llama.cpp") has exactly this one exception. → `docs/architecture.md` § Backends.
- **`--json` is the agent contract**, not the TTY rendering. Every non-interactive command supports it and emits a wrapped object with a stable shape. Colors and padded tables are TTY-gated (TTY + no `NO_COLOR` + no `--no-colors`); piped output stays `\t`-separated so `awk -F\t` pipelines keep working, and `--json` is byte-stable regardless. `llamastash config` is interactive-only (no JSON). `stop --all` refuses without `--yes` in a non-TTY. → `docs/usage.md`.
- **Exit codes** follow `<sysexits.h>` numerically with project-specific meanings — pin against `src/cli/exit_codes.rs` (table in `docs/usage.md § Exit codes`), not the libc constants. `doctor` always exits `0`.
- **Single binary, three roles.** TUI, CLI, and daemon are all `llamastash`; the daemon spawns on demand when a client finds no socket.
- **Five hard-coded themes**, Catppuccin Macchiato by default. No dynamic loading.

## Protected artifacts

Never flag for deletion or `.gitignore` — these are the engineering record: `docs/brainstorms/*`, `docs/plans/*.md`, `docs/solutions/*.md` (when present), `docs/benchmarks/*` (the raw per-host JSONs under `runs/` and `overhead/` are the published evidence behind README claims; rewriting prior dated pages breaks the reproducibility contract in `docs/benchmarks/methodology.md`), `.context/compound-engineering/ce-review/*`.

## Gotchas

- `cargo build` without `--features test-fixtures` intentionally omits `fake_llama_server` and `_test_sleep`; CI runs both ways to catch accidental dependence on test-only surface. `cargo install` excludes them by the same gating — don't move them out from behind `#[cfg(any(test, feature = "test-fixtures"))]`.
- `LLAMASTASH_BENCH_DISABLE_DEFAULTS=1` collapses `resolve_layered` to User-labeled layers only (no preset / last-used / arch defaults). `scripts/bench/` sets it so `start` produces byte-identical argv to raw `llama-server`. Never set it in production — it disables the auto-tuning the launcher exists for.
- Release publishes `publish-homebrew`, `publish-site`, and `publish-cargo` in parallel; one failing job leaves channels diverged. Recover with `gh run rerun --failed <run-id>`. Pre-release tags (`vX.Y.Z-<suffix>`) skip all three by design.

## Release

`git tag vX.Y.Z && git push --tags` triggers `.github/workflows/release.yml`: `release-gate` (tests + cold CPU-only UAT on Linux and macOS) → 4 target tarballs → release assets → Homebrew formula + `install.sh` mirror + crates.io. Roughly 10-15 minutes on cold caches. Pre-tag guards live in ci.yml's `release-readiness` (`cargo publish --dry-run --locked`, crates.io name availability, CHANGELOG `[Unreleased]` header, Cargo.toml ↔ CHANGELOG version alignment). Trust-critical actions in release.yml are SHA-pinned; first-party `actions/*` are tag-pinned, updated via Dependabot. First-time org / token / Pages setup: `docs/runbooks/release-0.0.1-bootstrap.md`.
