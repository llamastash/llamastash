# Plan: Preset Isolation + Layer Provenance in `--json`

Tracking TODO.md line 299 (R9 checklist).

## Problem

Two gaps in the `User > PresetDefault > LastUsed > ArchDefault` layer cascade:

1. **Named preset not isolated.** `start --preset <name>` (CLI) and TUI form launches both send `LaunchSelection::Explicit`, which still applies the `LastUsed` layer. A named preset inherits every knob from the previous launch that it does not itself declare — e.g. `pi-coding` picks up `--threads 16 --flash-attn on` left behind by a prior `pi-cache` run. `--preset auto` already suppresses this (`LaunchSelection::Auto` → `pure_fit`), but a named preset does not. The A/B benchmark harness works around it by wiping `state.json` between launches (`scripts/bench/local/preset-ab.sh`).

2. **No layer provenance in `--json`.** The resolver computes `Resolved.sources: BTreeMap<KnobId, LayerLabel>` (which layer each knob value came from), but it is never propagated past `compose_and_spawn`. `start --json` cannot tell a caller where a value originated — critical for A/B comparison where the same value can come from different layers.

## Scope

- `src/daemon/launch_service.rs` — `compose_and_spawn` layer assembly, `LaunchExec`, `StartedLaunch`, `spawn_supervised` destructuring, `LaunchSelection` doc comment.
- `src/backend/lemonade/backend.rs` — the second `StartedLaunch` construction site (the lemonade multiplexer builds its own `StartedLaunch` and never reaches `spawn_supervised`).
- `src/ipc/methods.rs` — `start_model_handler` JSON response.
- `src/cli/start.rs` — `emit_response` and `wait_and_emit` `--json` output.
- `src/launch/params.rs` — add `Serialize`/`Deserialize` to `LayerLabel` (snake_case).
- `src/launch/knobs/def.rs` — add `Serialize`/`Deserialize` to `KnobId`.
- `docs/architecture.md` — the launch layering / preset-selection rules (no separate `launch-composer-rules.md` exists).
- `CHANGELOG.md` — one bullet for the fix, one for the JSON addition.
- `scripts/bench/local/preset-ab.sh` — drop the `state.json` wipe workaround once E2E confirms isolation (if it has no other purpose).

Not in scope: TUI running view provenance (already reads from declarations), preset save/load (already correct per R9).

## Changes

### Part 1: Named preset isolation (CLI + TUI)

In `compose_and_spawn`, extend the `LastUsed` skip to `LaunchSelection::Explicit` (covers both CLI `start --preset <name>` and TUI form launches, which both send `Explicit`):

```rust
// Current:
let pure_fit = matches!(parsed.selection, LaunchSelection::Auto) || default_is_auto;
// Fix:
let pure_fit = matches!(parsed.selection, LaunchSelection::Auto | LaunchSelection::Explicit)
    || default_is_auto;
```

`Explicit` fires for the CLI `start --preset <name>` path and the TUI form launch path (`src/tui/events.rs:1727`). The TUI scope is broader than the preset picker: it sends `explicit` for **any** non-`auto` form launch, including manual knob edits with no preset selected — in that case the form is already seeded from last_params client-side, so skipping the `LastUsed` layer is a no-op rather than a behavior change. The inline-flag-only CLI path (no `--preset`, just `--threads 8` etc.) uses `Default`, not `Explicit`. `no_selection` stays false for `Explicit` (it requires `is_default_sel`, which is `false` for `Explicit`), so extras and MTP inheritance are unaffected — they already fall to "no inherit" for `Explicit`.

Also update the `LaunchSelection::Explicit` doc comment (currently says "let last_params fill knob gaps") and the `compose_and_spawn` section comment that describes `Explicit` as carrying flattened knobs/extras.

### Part 2: Layer provenance in `--json`

1. **`LayerLabel`** (`src/launch/params.rs`): derive `Serialize`, `Deserialize` with `#[serde(rename_all = "snake_case")]` so it serializes as `"user"`, `"preset_default"`, `"last_used"`, `"arch_default"`, `"model_default"`, `"server_default"`.

2. **`KnobId`** (`src/launch/knobs/def.rs`): derive `Serialize`, `Deserialize` (serializes as its `&'static str` inner via `#[serde(transparent)]`).

3. **`LaunchExec`** (`src/daemon/launch_service.rs`): add `layer_sources: BTreeMap<KnobId, LayerLabel>`.

4. **`StartedLaunch`** (`src/daemon/launch_service.rs`): add `layer_sources: BTreeMap<KnobId, LayerLabel>`.

5. **`compose_and_spawn`**: set `layer_sources: resolved.sources` on the `LaunchExec` (move — `resolved` is not used after the `LaunchExec` is built).

6. **`spawn_supervised`**: destructure `layer_sources` from `exec`, pass to `StartedLaunch`.

7. **Lemonade `StartedLaunch` site** (`src/backend/lemonade/backend.rs`): add `layer_sources: exec.layer_sources` to the second `StartedLaunch` construction (the multiplexer never reaches `spawn_supervised`, so omitting this breaks the build once `StartedLaunch` has the new field).

8. **`start_model_handler`** (`src/ipc/methods.rs`): insert `layer_sources` into the `resp` object **conditionally** (mirroring the existing `warnings` pattern: `if !started.layer_sources.is_empty() { resp["layer_sources"] = json!(...) }`). `StartedLaunch` is not serde-serialized in this path — the handler projects fields onto an ad-hoc `json!` — so a `skip_serializing_if` attribute has nothing to attach to.

9. **CLI `start --json`** (`src/cli/start.rs`): `emit_response` and `wait_and_emit` each build their **own** `--json` body via an explicit `json!` macro (they do not forward the daemon `resp`). Add `layer_sources` to both bodies, read from `resp.get("layer_sources")`, with the same omit-when-absent behavior.

The JSON output adds:
```json
"layer_sources": {"n-gpu-layers": "user", "flash-attn": "preset_default", "threads": "arch_default"}
```

Present only when non-empty (conditional insertion in `start_model_handler`, mirroring the `warnings` pattern — not a serde attribute).

## Verification

- `cargo test --features test-fixtures` — existing resolver tests still pass.
- Add a unit test in `launch_service.rs` asserting `LaunchSelection::Explicit` skips `LastUsed`: two layers where `User` sets one knob and `LastUsed` sets another; only the `User` value resolves.
- E2E: `start --preset <name> --json` (CLI) and TUI form launch both show `layer_sources` with only the preset's/user's knobs as `user`; no knobs inherited from a stale `last_params`. Verify the TUI path by selecting a preset in the picker and checking the daemon-side layer sources via the IPC response.
- E2E: TUI **manual** form launch (no preset selected, knobs edited by hand) shows no behavior change versus before the fix (the form is seeded from last_params client-side, so the `LastUsed` skip is a no-op on this path).
- `start --preset auto --json` and `start --json` (no preset) both still behave as before.
- Update the layering / preset-selection rules in `docs/architecture.md` so named presets and inline flags are documented separately (no separate `launch-composer-rules.md` exists).
- Once E2E confirms isolation, drop the `state.json` wipe workaround in `scripts/bench/local/preset-ab.sh` for the preset isolation case (check if it serves other purposes first).
