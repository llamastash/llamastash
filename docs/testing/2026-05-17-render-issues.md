---
title: "Render-test issues — 2026-05-17"
type: test-issues
status: resolved
date: 2026-05-17
resolved_date: 2026-05-17
test_plan: docs/testing/2026-05-17-render-test-plan.md
target_plans:
  - docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md
  - docs/plans/2026-05-16-001-feat-kdash-style-dashboard-ui-plan.md
hardware:
  cpu: "AMD Ryzen AI Max+ 395 (16C/32T)"
  gpu: "AMD Radeon 8060S iGPU (gfx1151, ROCm)"
  ram_gb: 128
---

## Resolution summary (2026-05-17)

| Issue | Status | Fix |
|-------|--------|-----|
| I1 — GGUF header cap | **Fixed** | `DEFAULT_HEADER_CAP_BYTES` 1 MiB → 16 MiB, `MAX_HEADER_CAP_BYTES` 4 MiB → 64 MiB; new regression test `default_cap_handles_realistic_tokenizer_payload`. |
| I2 — `start_detached` flag propagation | **Fixed** | New `DaemonOptions::propagated_cli_args`; `build_options` populates from cli; `start_detached_with_exe` re-emits before the subcommand. Two new tests in `cli/daemon.rs`. |
| I3 — Modern GGUFs `· unknown` | **Resolved by I1** | All chat / embed models now parse correctly; arch / quant / ctx / mode all surface. |
| I4 — `host` missing from `status --json` | **Fixed** | `StatusSnapshot` gains a `host: Value` field; `fetch_status` preserves it; `status_json` emits it; new test `status_json_preserves_host_block_verbatim`. |
| I5 — GPU detection cpu_only on ROCm | **Already fixed in working tree** | `Stdio::piped()` + `VRAM Total Used Memory (B)` key alternative — verified after rebuild. |
| I6 — LM Studio `downloadsFolder` | **Fixed** | Added the key to `LmStudioSettings`; new test `downloads_folder_key_resolves_when_others_absent`. |
| I7 — mmproj projector companions visible | **Fixed** | New `is_projector_companion` filter in `scanner.rs`; new test `collect_gguf_paths_drops_mmproj_projector_companions`. |
| I8 — `socket` row missing path | **Fixed** | New `MethodContext::socket_path`, threaded into `status.daemon.socket_path`. TUI `DaemonInfo.socket_path` populated by `ingest_status`. `socket_row` lays out `…/daemon.sock  pid N` with min-budget fallback. New test `socket_row_renders_path_alongside_pid_when_available`. |
| I9 — `server  — (rocm)` for unresolved binary | **Fixed** | `server_row` now collapses to `—` when no binary is resolved; new test `server_row_omits_flavor_when_binary_unresolved`. |
| I10 — render-time first-tick wait | **Deferred** (cosmetic, ~500 ms) | No action; documented. |
| I11 — full absolute paths in group headers | **Deferred** | Filed; needs Models-pane layout audit. |
| I12 — tiny-terminal title truncation | **Deferred** | Cosmetic. |
| **I13 (new) — `gemma3` / `gemma4` chat models labeled `embedding`** | **Open / out-of-scope** | Found during retest; the mode-hint pipeline in `src/gguf/metadata.rs` misclassifies Gemma instruct models. Filed for follow-up — not regressed by these fixes (it was hidden before because the header cap prevented metadata parsing at all). |

### Final geometry matrix (post-fix)

| Geometry | Discovery | GPU | Layout |
|----------|-----------|-----|--------|
| T1 (160×50) | 8 launchable (mmproj hidden) | amd · 1 GPU · 2.8G/64G · 54 °C | clean |
| T2 (120×40) | 8 launchable | amd · 1 GPU | clean |
| T3 (100×30) | 8 launchable | amd · 1 GPU | clean |
| T4 (80×25)  | 8 launchable; socket falls back to pid-only (path budget too tight); `server —` clean | amd · 1 GPU | clean |
| T5 (50×12)  | 8 launchable | info row collapsed | clean |
| T6 (120×40, default discovery) | 9 launchable (1 legacy + 8 via `downloadsFolder`) | amd · 1 GPU | clean |

### Test counts

- `cargo test --lib --features test-fixtures`: 468 passed (was 463 — 5 new tests landed).
- `cargo clippy --all-targets --features test-fixtures -- -D warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- One pre-existing flake remains in `tests/ipc_handshake_test.rs::version_reports_pid_uptime_and_connections` (connections=2 vs 1). Reproduces on `HEAD` before any of these fixes — out of scope.

---

## Original findings



# Render-test issues — 2026-05-17

Findings from running `--render` across T1..T6 from
`2026-05-17-render-test-plan.md`. All renders use the binary at
`target/release/llamastash`.

Severity legend: **P0** — breaks a user-visible promise of the plan;
**P1** — degraded UX or stale metadata; **P2** — polish.

---

## I1 (P0) — GGUF header cap too small for modern models

**Symptom:** Most launchable models render as `name · unknown` with no
arch / quant / context. Discovery + `list --json` show
`parse_error: "gguf string length out of range: N"` or
`"gguf header truncated: needed 1048579 bytes, got 1048576"`.

```text
Qwen3.6-27B-Q4_K_M.gguf       arch=- quant=- ctx=- mode=- err=gguf string length out of range: 9
Qwen3.6-27B-Q6_K.gguf         arch=- quant=- ctx=- mode=- err=gguf string length out of range: 9
Qwen3.6-27B-Q8_0.gguf         arch=- quant=- ctx=- mode=- err=gguf string length out of range: 9
Qwen3.6-35B-A3B-Q8_0.gguf     arch=- quant=- ctx=- mode=- err=gguf string length out of range: 5
gemma-4-31B-it-Q4_K_M.gguf    arch=- quant=- ctx=- mode=- err=gguf header truncated: needed 1048579 bytes, got 1048576
gemma-4-31B-it-Q8_0.gguf      arch=- quant=- ctx=- mode=- err=gguf header truncated: needed 1048579 bytes, got 1048576
nomic-embed-code-Q4_K_M.gguf  arch=- quant=- ctx=- mode=- err=gguf string length out of range: 20
```

**Root cause:** `src/gguf/header.rs::DEFAULT_HEADER_CAP_BYTES = 1 << 20`
(1 MiB) and `MAX_HEADER_CAP_BYTES = 4 << 20` (4 MiB). Modern
tokenizers (Gemma 3+, Qwen 2.5+) embed a tokens.list array of 256k+
strings directly in the metadata KV section, which routinely pushes
the structural header past 1 MiB and sometimes past 4 MiB. The cap
clips the buffer mid-string, so the next `read_gguf_string` either
reads a junk length (`9`, `20`) or hits an EOF mid-payload.

**Fix:** Raise the default cap to 16 MiB and the hard cap to 64 MiB.
Update fixture limits to match. Add a fixture-based regression test
that builds a GGUF with a 2 MiB metadata payload (e.g. a long
tokens.list) and asserts it parses cleanly. Failing this cap budget
on a real model is a clear regression signal.

---

## I2 (P0) — `start_detached` drops `-p` / `--no-scan` / `--llama-server` / `--config` on re-exec

**Symptom:** `llamastash --render -p /mnt/work/lmstudio-models --no-scan`
ignores both flags. The auto-spawned detached daemon walks the default
LM Studio path (`~/.lmstudio/models`, 2 GGUFs) instead of the requested
root (`/mnt/work/lmstudio-models`, 11 GGUFs).

Repro:

```text
$ llamastash daemon stop
$ llamastash --render -p /mnt/work/lmstudio-models --no-scan
# Models block title shows "[2]" with paths under ~/.lmstudio/models
```

Foreground (`daemon start` without `--detach`) honours the flags
correctly — proving `build_options` is fine. The bug is isolated to
the re-exec in `start_detached_with_exe`.

**Root cause:** `src/daemon/mod.rs::start_detached_with_exe` only
appends `--state-dir` and `--socket-path` to the child's argv. The
child rebuilds `DaemonOptions` from its own empty `Cli` and falls
through to `default_set` with no user paths and `no_scan = false`.

**Fix:** Before spawning the child, serialize every flag that affects
discovery (`--model-path` repeated, `--no-scan`, `--llama-server`,
`--config`) and append them to the child argv. Add an integration test
that spawns a detached daemon with `-p /tmp/some-dir --no-scan` and
asserts the daemon's `list_models` echoes the right root.

---

## I3 (P0) — Modern GGUFs render as `· unknown` with no mode hint

**Symptom:** Even the metadata that *does* parse (mxbai-embed-large,
mmproj-*) shows `mmproj-Qwen3.6-27B-BF16 clip · BF16 · 888M · unknown`
and `mxbai-embed-large-v1-f16 bert · F16 · 512 · 638M · embedding`.
The Qwen / Gemma chat models can't be classified because their header
never parses (I1). After I1 is fixed, this should resolve — but the
mode-hint pipeline relies on `metadata.arch` + tokenizer hints, so
this issue can only be confirmed after the header cap fix.

**Root cause:** Cascading effect of I1. Once metadata is None, the
mode hint defaults to `unknown`.

**Fix:** Re-test after I1.

---

## I4 (P0) — `status --json` is missing the `host` field

**Symptom:** `llamastash status --json` returns no top-level `host`
block. AGENTS.md guarantees `host` is "always an object (no `null`)".

```json
{
  "daemon": { … },
  "external": [],
  "gpu": { "backend": "amd", "devices": [ … ] },
  "models": []
}
```

The IPC layer (`src/ipc/methods.rs::status_response`) emits the field
correctly — verified by reading the raw daemon JSON. The CLI layer
(`src/cli/output.rs::status_json` and `src/cli/resolve.rs::fetch_status`)
strips it on the round-trip.

**Root cause:**
1. `fetch_status` parses `models`, `external`, `gpu`, `daemon` but
   does not preserve `host` from the wire response.
2. `StatusSnapshot` has no `host` field.
3. `status_json` constructs the output object from the snapshot and
   never re-attaches `host`.

**Fix:** Preserve `host` as a `Value` on `StatusSnapshot` and emit it
unmodified in `status_json`. Add a unit test in `cli/output.rs` that
round-trips a wire response containing `host` and asserts the field
survives both formats (`--json` and `human`).

---

## I5 (P0) — GPU detection silently degrades to `cpu_only` on ROCm 7.x

**Symptom (resolved in working tree, called out for traceability):**
A binary built from `main` (commit `1da06ab`) reports
`gpu.backend = "cpu_only"` on this AMD Strix Halo machine despite
`rocm-smi --json` working fine. The render frame shows
`backend  cpu only`.

The working tree has two uncommitted fixes addressing this:
- `src/gpu/mod.rs` — `run_with_timeout` now sets
  `stdout/stderr = Stdio::piped()`. Without this, child stdout writes
  to the parent's inherited handle and `wait_with_output()` returns
  empty buffers, so every probe variant silently falls through to
  `CpuOnly`.
- `src/gpu/amd.rs` — `pick_u64` now accepts
  `"VRAM Total Used Memory (B)"` as a synonym for
  `"VRAM Used Memory (B)"`. The on-host `rocm-smi 7.8.0` emits the
  newer key, so without this the `used_memory_bytes` field would read 0.

**Status:** Both fixes already applied locally. After rebuild, the
status response correctly reports:

```json
"gpu": {
  "backend": "amd",
  "devices": [{
    "name": "card0",
    "total_memory_bytes": 68719476736,
    "used_memory_bytes": 3159384064,
    "temperature_c": 54.0,
    "utilization_pct": 5.0
  }]
}
```

**Follow-up:** Commit both files and add an integration test that
covers the piped-stdout behaviour (e.g., via a stub binary on
`$PATH`).

---

## I6 (P1) — LM Studio `downloadsFolder` setting is not honoured

**Symptom:** `~/.lmstudio/settings.json` says
`downloadsFolder: /mnt/work/lmstudio-models` (where 11 GGUFs live).
But discovery doesn't pick that up — `resolve_models_dirs` walks only
`~/.lmstudio/models` (the legacy default directory, 2 GGUFs).

**Root cause:** `src/discovery/lm_studio.rs::read_settings_models_dir`
looks for `paths.models` and `modelsDirectory` only. LM Studio's
current schema uses `downloadsFolder`.

**Fix:** Add `downloadsFolder` to the keys checked in
`LmStudioSettings`. Extend the test fixture to cover a settings.json
that only sets `downloadsFolder`.

---

## I7 (P1) — `mmproj-*.gguf` projector companions appear as launchable rows

**Symptom:** mmproj files show up in the Models list with the same
visual weight as launchable chat / embed models, e.g.:

```text
> Qwen3.6-27B-Q4_K_M · unknown
  Qwen3.6-27B-Q6_K · unknown
  Qwen3.6-27B-Q8_0 · unknown
  mmproj-Qwen3.6-27B-BF16  clip · BF16 · 888M · unknown
```

Selecting one and pressing Enter would launch `llama-server` on a
projector file, which fails downstream. Per the user policy, mmproj
companions should be **hidden** unless they are independently
launchable (they aren't — they only have value paired with a parent
model).

**Root cause:** `src/discovery/scanner.rs::collect_gguf_paths` emits
every `.gguf` file. There is no filter for the `mmproj-` prefix or
the GGUF arch `clip`.

**Fix:** In the discovery pipeline, drop entries whose canonical
filename starts with `mmproj-` (or whose parsed metadata's `arch ==
"clip"`). The display name is the same canonical name on disk —
filtering by filename is sufficient and avoids a header re-read. Add
a test fixture covering a directory with `model.gguf` +
`mmproj-model.gguf` that surfaces only `model.gguf`.

---

## I8 (P1) — Daemon panel `socket` row shows only PID, no socket path

**Symptom:** Plan wireframe specifies
`socket  …/daemon.sock  pid 1234`. Render shows
`socket  pid 3865322` — the socket path is missing entirely.

**Root cause:** Probably one of:
- `App` doesn't have `daemon_socket_path` populated from the IPC
  response, so the rendering falls back to PID-only.
- The `status` response doesn't include the socket path (the daemon
  has it in `DaemonOptions` but never serialises it).

**Fix:** Add `socket_path: String` to `daemon: { … }` in the IPC
`status` response, parse it into `App`, render it before PID with the
`…/` left-truncate budget already implemented for the server path.

---

## I9 (P1) — `server` row shows `— (rocm)` when no `llama-server` is resolved

**Symptom:** When `daemon.server_path` is `null`, the Daemon panel
renders `server  — (rocm)`. The `(rocm)` flavor is misleading — there
is no server binary, so there is no flavor either.

**Root cause:** `src/tui/info_pane.rs` formats the server row by
concatenating the path (`—` when None) with the GPU-derived flavor
unconditionally.

**Fix:** When the path is None, render `server  not configured` (or
similar) and drop the flavor tag. Only attach the flavor when the
path is Some.

---

## I10 (P2) — Render-time poll never sees a primed `host_metrics` snapshot in foreground daemon

**Symptom:** First `--render` after `daemon start --detach` waits the
full 1.5 s deadline because the daemon's first host-metrics tick lands
~1 s after socket bind. Acceptable in practice; flagged for future
optimisation (e.g., the daemon could push the first reading inside
the same tick that binds the socket).

**Fix:** No immediate action. Re-evaluate if startup latency is a
user complaint.

---

## I11 (P2) — Models list group headers show full absolute paths

**Symptom:** The Models list groups rows by parent directory, but the
group header is the absolute parent path (e.g.,
`/mnt/work/lmstudio-models/lmstudio-community/Qwen3.6-27B-GGUF`).
The wireframe shows `/home/d/models` — a shortened form.

**Fix (deferred):** Tilde-collapse `$HOME`, and elide intermediate
segments with `…/` once the path exceeds the available width. Already
implemented for the Daemon panel's `socket` / `server` rows — the
same util is in `util/paths.rs`. Not done in this pass because the
Models-pane truncation interacts with the count chip; needs a small
layout audit. Filed for a follow-up.

---

## I12 (P2) — Tiny-terminal title row truncates mid-version

**Symptom:** At 50×12 the title row reads
`LlamaStash v0.1.?:help  t:theme  /:filter  q:quit` — the `v0.1.0-dev`
is cut mid-string by the global hint set.

**Fix (deferred):** Drop the version suffix first when total width is
below a small threshold (e.g., < 60 cols). Pure cosmetic.

---

## Coverage matrix

| Geometry | Discovery | GPU | Layout | Issues hit |
|----------|-----------|-----|--------|-----------|
| T1 (160×50) | 11 found (after I2 workaround) | amd · 1 GPU | Wireframe match | I1, I3, I4, I7, I8, I9 |
| T2 (120×40) | 11 found | amd · 1 GPU | Wireframe match | I1, I3, I4, I7, I8, I9 |
| T3 (100×30) | 11 found | amd · 1 GPU | Wireframe match | I1, I3, I4, I7, I8, I9 |
| T4 (80×25)  | 11 found | amd · 1 GPU | Logo present; hint chip dropped (`y:yank`) | I1, I3, I4, I7, I8, I9 |
| T5 (50×12)  | 11 found | n/a (info row collapsed correctly) | Body-only fallback ✓ | I12 |
| T6 (120×40, auto-spawn defaults) | 2 found (I2 + I6) | amd · 1 GPU | Wireframe match | I1, I2, I3, I6, I7 |

## Fix order

1. **I2** — without this, every other test has to manually start the daemon foreground.
2. **I1** — unblocks I3 and a clean visual for the chat models.
3. **I4** — IPC contract symmetry; small fix.
4. **I6** — picks up the user's actual LM Studio install.
5. **I7** — discovery filter for mmproj.
6. **I8, I9** — info pane polish.
7. **I5** — commit existing GPU fixes with regression test.
8. **I11, I12** — defer.
