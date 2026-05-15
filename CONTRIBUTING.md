# Contributing to llamatui

Thanks for the interest. llamatui is still pre-1.0 and the API surface is moving, so coordination matters more than usual right now.

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
