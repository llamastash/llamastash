---
title: "test: fix the two open flaky tests (Next batch 3)"
type: test
status: active
date: 2026-11-17
---

# Next batch 3: flaky tests

## Goal

Fix the two flaky tests listed in `TODO.md` under `## Next` → `### Batch 3: flaky tests`.

## Decisions

1. **`supervisor_ipc_test::status_lists_active_supervised_model`**: widen the sampler poll from 4 s to `DAEMON_STARTUP_TIMEOUT` (20 s, already imported in that file). Reason: the test exists to prove the sampler runs, so keep the wait, but under a loaded full-suite run the 4 s budget is too short. The other daemon-bound tests use `DAEMON_STARTUP_TIMEOUT` for the same kind of wait.
2. **`proxy_fallback::case_variant_of_a_live_name_reuses_that_launch`** (and every other caller of the shared helper): change `test_support::allocate_port_range` to draw its range from ports outside the OS ephemeral range, instead of from just-released ephemeral ports. Reason: a just-released ephemeral port is the next one a concurrent `bind(0)` in any other test can get, between `ports::allocate`'s probe and the fake server's bind.
   - Pool: `20_000..32_000`. Linux's default ephemeral range starts at 32768; macOS and Windows start theirs at 49152. So no `bind(0)` on any of the three hosts hands out a pool port.
   - The range is `width = n.max(1)` contiguous ports at a random start in the pool, from `rand::rngs::OsRng` (`rand` 0.9 is already a dependency with `os_rng`; use `TryRngCore::try_next_u32`, the same trait `src/daemon/auth.rs` imports).
   - Accept a window only when every port in it binds on `127.0.0.1` right now (bind and drop, like `ports::try_bind`). Retry with a new random start up to 64 times, then panic with a clear message. Some callers use `.start` alone as a listener port (`proxy_ui`, `proxy_metrics`, `proxy_ollama_compat`, `proxy_routing`), so `.start` must be free when the helper returns.
   - Keep the signature `allocate_port_range(n: usize) -> PortRange`, so no caller changes. Rename the parameter from `probes` to `width` and rewrite the doc comment: the range is `width` contiguous ports in the pool, with the reason in one or two lines.
   - Do not touch the other bespoke ephemeral-port helpers in `tests/` (`allocate_port`, `allocate_port_range_pair`, and similar). They are out of scope.

## Units

### U1: both fixes (one lane, no parallel units)

Files:

- `src/test_support.rs`: new `allocate_port_range` as in decision 2, plus an inline `#[cfg(test)] mod tests` with one test: 50 calls with width 8 each return `end - start + 1 == 8`, `start >= 20_000`, `end < 32_000`. The file is compiled only with `test-fixtures`, which the verification commands pass.
- `tests/supervisor_ipc_test.rs`: in `status_lists_active_supervised_model`, replace `Duration::from_secs(4)` in the sampler poll with `DAEMON_STARTUP_TIMEOUT`. Update the comment above the poll only if it states the 4 s budget.
- `TODO.md`: in `## Next` → `### Batch 3`, mark both items done in the file's existing style: `- [x] ~~<text>~~ — **done 2026-11-17**: <one short clause>`. See the Batch 4 item in the same section for the format. Leave all other TODO entries alone.
- `.claude/skills/ls-release/references/flakes.md`: in the `case_variant_of_a_live_name_reuses_that_launch` section, replace "Tracked in `TODO.md` under `## Next`." with one sentence that says `test_support::allocate_port_range` now draws ranges from ports 20000-31999, outside the OS ephemeral range, since 2026-11-17. Keep the recovery line.

No CHANGELOG entry: test-only change, not user-visible.

Depends on: nothing.

## Setup

None. The worktree builds with the normal cargo commands.

## Verification

```bash
cargo nextest run --features test-fixtures --lib test_support
cargo nextest run --features test-fixtures --test supervisor_ipc_test
cargo nextest run --features test-fixtures --test proxy_fallback
cargo nextest run --features test-fixtures --test proxy_ui --test proxy_metrics --test proxy_ollama_compat --test proxy_routing
make lint
```

## Definition of done

- Both decisions implemented, the new unit test passes, all verification commands pass.
- `TODO.md` and `flakes.md` updated in the same commit as the code.
