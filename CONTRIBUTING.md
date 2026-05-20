# Contributing to llamastash

Thanks for the interest. llamastash is still pre-1.0 and the API surface is moving, so coordination matters more than usual right now.

## Before you start

- Read [`docs/brainstorms/llamatui-requirements.md`](docs/brainstorms/llamatui-requirements.md) for the scope of v1 and what's deferred.
- Skim the implementation plan in [`docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md). If your contribution lands inside an existing Implementation Unit, mention which one in the PR description.
- Open an issue (or comment on an existing one) before opening a PR for anything bigger than a typo or a one-file bug fix. Saves us both time when scope or approach needs to be discussed.

## Repo layout

- `src/` — production code. Module boundaries mirror the plan's Implementation Units: `daemon/`, `discovery/`, `gguf/`, `gpu/`, `ipc/`, `launch/`, `tui/`, `cli/`, `theme/`, `config/`, `util/`.
- `tests/` — integration tests. The fake `llama-server` lives in `tests/fixtures/fake_llama_server.rs` and ships only when built with the `test-fixtures` feature.
- `docs/` — user-facing docs (`usage.md`, `architecture.md`, `troubleshooting.md`) and design history (`brainstorms/`, `plans/`).
- `.github/workflows/` — CI and release workflows.

## Building

```bash
cargo build
cargo test --features test-fixtures        # includes the integration suite
cargo clippy --all-targets -- -D warnings  # CI pins this
cargo fmt --check                          # CI pins this
```

The integration tests need `--features test-fixtures` because they spawn the `fake_llama_server` binary the daemon launches in place of a real llama.cpp child.

### Running the daemon locally

Two terminals are typically the easiest setup when you're hacking on the daemon or IPC surface:

```bash
# Terminal 1: foreground daemon. Logs land in your terminal; Ctrl-C stops it.
cargo run -- daemon start

# Terminal 2: drive it with the CLI or TUI.
cargo run -- list
cargo run -- start <model>
cargo run                              # opens the TUI against the same daemon
cargo run -- daemon status             # pid / uptime / connections
cargo run -- daemon stop               # graceful shutdown
```

The daemon binds its socket under `$XDG_RUNTIME_DIR/llamastash/daemon.sock` on Linux and `$TMPDIR/llamastash-$USER/daemon.sock` on macOS. If you need two daemons side-by-side (e.g. testing migrations), point each at a distinct path:

```bash
LLAMASTASH_SOCKET=/tmp/llamastash-dev/daemon.sock cargo run -- daemon start
LLAMASTASH_SOCKET=/tmp/llamastash-dev/daemon.sock cargo run -- list
```

If something is wedged and the normal `daemon stop` won't go through, deleting the socket file and `daemon.pid` in the same directory is safe — the next `daemon start` re-binds clean.

## One-shot rename migration (existing local installs)

The project was renamed from LlamaDash to LlamaStash before the first public release. If you ran the old binary locally before the rename, your config / cache / share directories still live under `llamadash/`. Run this once after pulling the rename to move them in place; sockets are ephemeral and regenerate on next `daemon start`.

```sh
[ -d "${XDG_CONFIG_HOME:-$HOME/.config}/llamadash" ] && \
  mv "${XDG_CONFIG_HOME:-$HOME/.config}/llamadash" \
     "${XDG_CONFIG_HOME:-$HOME/.config}/llamastash"
[ -d "${XDG_CACHE_HOME:-$HOME/.cache}/llamadash" ] && \
  mv "${XDG_CACHE_HOME:-$HOME/.cache}/llamadash" \
     "${XDG_CACHE_HOME:-$HOME/.cache}/llamastash"
[ -d "$HOME/.local/share/llamadash" ] && \
  mv "$HOME/.local/share/llamadash" \
     "$HOME/.local/share/llamastash"
```

The binary intentionally does not check old paths — pre-publish was the time to delete legacy code cleanly.

## Code conventions

- Rust edition 2021. Minimum supported Rust version is pinned in `Cargo.toml` (`rust-version`).
- Two-space indentation, `rustfmt.toml` is authoritative. `cargo fmt` before pushing.
- `cargo clippy -- -D warnings` is enforced in CI. No `#[allow(...)]` without a one-line reason.
- Inline tests under `#[cfg(test)] mod tests` per file; integration tests in `tests/`. Either is fine; pick whichever keeps the test close to the behavior it exercises.
- Comments explain **why**, not **what**. Identifiers should already say what.

## Submitting a PR

1. Branch off `main`.
2. Keep commits logically split — one cohesive change per commit when reasonable. Conventional commit prefixes (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`) are encouraged.
3. Make sure `cargo test --features test-fixtures` and `cargo clippy -- -D warnings` both pass locally.
4. Open the PR against `main`. The description should at minimum say *what* the change does, *why*, and how it was tested.

## Security

If you find a security issue, please follow the disclosure process in [`SECURITY.md`](SECURITY.md) instead of opening a public issue.

## License

By contributing, you agree your contribution is licensed under the MIT license, the same license the rest of the project uses.
