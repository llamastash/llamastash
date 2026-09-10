# Plan: one `load-mode` knob, translated per build

**Status:** implemented (2026-09-10). Units 5 (launch/supervisor), 2 (daemon). Commit
subjects: `feat(unit5):`.

## Requirement

`llamastash start <model> --no-mmap` and any preset carrying `no-mmap: true` or
`mlock: true` must launch on **both** a current llama.cpp and an older fork
build, with one knob in `config.yaml`.

## Problem

llama.cpp removed `--mmap` / `--no-mmap` / `--mlock` / `-dio` in
[`14a9d09f7`](https://github.com/ggml-org/llama.cpp/commit/14a9d09f7) (PR #28334,
2026-09-09), replacing them with `-lm, --load-mode MODE`
([`e6dd0e29a`](https://github.com/ggml-org/llama.cpp/commit/e6dd0e29a), PR #20834).
Our `no-mmap` and `mlock` knobs still emit the deleted spellings, so on b10892+
**any** launch that sets either dies before ready:

```
error: invalid argument: --no-mmap
```

Found live: `Qwen3.8-27B-UD-Q6_K@coding-medium` 503'd on auto-start, because the
wildcard `Qwen3.8-27B-*` preset sets `no-mmap: true` and pins no `server:`, so it
fell to the default build (10892). The same preset name under
`Qwen3.8-27B-ROCmFP4-FAST.gguf` pins `server: llamacpp-ROCmFP4`, an older fork
that still accepts the flag — same config, opposite outcome, decided purely by
which binary answered.

Multiple installed builds is the normal state here (five servers configured:
stock ROCm/Vulkan, a `q38rocm` shim, an unsloth fork, a DFlash2 fork), so
"upgrade everything" is not a fix.

## Key decisions

### D1 — detect the capability, do not gate on a version

The obvious gate is `build >= 10892`. It is wrong: forks report their own
version strings (`v1.5.2`, `b10715-mix`), and a shim reports whatever it wraps.
Probe `<binary> [serve] --help` once at boot and look for the literal
`--load-mode`. This answers correctly for a fork, needs no table of build
numbers, and stays right when upstream changes again.

Same conclusion the ds4 work reached: when a flag set is build-dependent, ask
the build.

### D2 — one knob, not two booleans

`no-mmap` and `mlock` both map into one engine enum, so keeping both means
inventing a precedence rule for `no-mmap: true` + `mlock: true` that the user
cannot see. That is the shape of the MTP defect fixed on 2026-09-10 — one
setting riding two channels, resolved somewhere invisible.

So: `load-mode` becomes the single declared knob, spelled with the engine's own
values (`auto` / `none` / `mmap` / `mlock` / `mmap+mlock` / `dio`). This also
reaches `dio` and `mmap+mlock`, which the two booleans could not express.

### D3 — the translation table comes from the deleted upstream code

Not inferred. The removal commit's own argument handlers state the mapping, and
`llama-model-loader.cpp:559` (`use_mmap = MMAP || MMAP_MLOCK || AUTO`) confirms
`mlock` alone does **not** mmap:

| `load-mode` | new build | old build |
|---|---|---|
| `auto` | *(nothing)* | *(nothing)* |
| `none` | `--load-mode none` | `--no-mmap` |
| `mmap` | `--load-mode mmap` | `--mmap` |
| `mlock` | `--load-mode mlock` | `--mlock` |
| `mmap+mlock` | `--load-mode mmap+mlock` | `--mmap --mlock` |
| `dio` | `--load-mode dio` | `-dio` |

`dio` on an old build that lacks it is the one lossy cell; the engine rejects it
loudly, which is the right outcome for a flag the build genuinely lacks.

### D4 — migrate the old keys, don't keep them as aliases

Pre-1.0, so no compat shim. The D10 config migrator already rewrites knob shapes
in place on first daemon start with a `.pre-knobs.bak` beside the file; this adds
one arm: `no-mmap: true` → `load-mode: none`, `mlock: true` → `load-mode: mlock`,
both → `mlock` (mlock is the stronger request and implies no mmap). A `false`
value for either is dropped — it asserted the engine default.

Aliases were considered and rejected: `aliases` in the registry are alternate
*names* for one knob, and this is a value translation across kinds
(`Bool` → `Enum`), which they cannot express.

## Scope

- `src/backend/llama_cpp/caps.rs` — **new**: the `--help` probe. Modelled on
  `list_devices::probe` (same `serve_prefix`, same
  `run_with_drain_and_timeout`, same degrade-on-failure posture).
- `src/backend/server.rs` — `ServerCaps` on `Server`; `build_server_catalog`
  probes it next to `probe_devices`.
- `src/backend/mod.rs` — `Backend::probe_caps` hook, defaulting to "no special
  capabilities" so no other backend changes.
- `src/backend/llama_cpp/mod.rs` — implement `probe_caps`; `seed_launch_knobs`
  stamps the resolved server's dialect into `launch_config`.
- `src/backend/llama_cpp/knobs.rs` — drop `mlock` / `no-mmap`, add `load-mode`.
- `src/backend/llama_cpp/compose.rs` — the D3 table.
- `src/config/knob_migration.rs` — the D4 arm.
- `docs/usage.md`, `config.example.yaml`, `CHANGELOG.md`, `TODO.md`.

## What gets reused

| Need | Existing thing |
|---|---|
| Run a binary at boot, per server, and degrade on failure | `list_devices::probe` (`src/backend/llama_cpp/list_devices.rs:170`) |
| Per-server boot pass | `build_server_catalog` (`src/backend/server.rs:312`) |
| Daemon-resolved fact → `compose` | `LaunchParams.launch_config` (`src/launch/params.rs:428`), read at `compose.rs:66,104` |
| Backend owns its own argv for a knob | `Emit::Custom` (`ctx-size`, `mtp-draft-n` already do this) |
| Config knob rewrite with a backup | `config::knob_migration` (D10) |

Genuinely new: one probe module, one hook, one translation table.

## Risks

- **The probe adds one subprocess per server at boot** (five here), on top of
  `--list-devices`. Bounded by the same timeout; a binary that hangs or refuses
  `--help` degrades to the legacy dialect rather than blocking boot.
- **Do not persist the probe result.** A rebuild in place would leave a stale
  cache claiming the old dialect — the failure mode that already bit this box
  with stale build artifacts. Probe every boot.
- **The default-server fallback**: a launch that pins no `server:` must resolve
  the same binary `pick_launch_binary` would spawn, or it stamps the wrong
  dialect. Reuse that function rather than re-deriving the default.

## Verification

`make test` is necessary, not sufficient — this is an argv-shape bug that a unit
test can assert either way. Required, per the E2E rule:

- On the current stock build (b10892+): launch with `load-mode: none` and confirm
  `--load-mode none` in the real argv, and that the model reaches ready.
- On `llamacpp-ROCmFP4` (an old fork): same preset, confirm `--no-mmap` in argv.
- `Qwen3.8-27B-UD-Q6_K@coding-medium` — the launch that 503'd — reaches ready.

## Checklist

- [x] `caps.rs` probe + unit tests over captured `--help` text, both dialects
- [x] `Server.caps` + `probe_caps` hook + boot wiring (plus `seed_binary_caps`,
      which the plan missed: `seed_launch_knobs` runs *before* the binary is
      picked, so the dialect had to be stamped after `pick_launch_binary`)
- [x] `load-mode` knob replaces `mlock` / `no-mmap`
- [x] `compose` translation table + tests for all six values × both dialects
- [x] D10 migrator arm + test (including `no-mmap` + `mlock` both true).
      Needed a second trigger — these entries are already in the current shape,
      so `is_legacy_entry`'s flat-key test said nothing to do; and
      `rewrite_entry` skipped `KNOBS_KEY` in its reserved-copy loop, so a
      current entry came out empty until it learned to carry them through.
- [x] Docs: `usage.md`, `config.example.yaml`, `CHANGELOG.md`, `TODO.md`
- [x] E2E on both a stock build and a fork build — see below

## Verified (2026-09-10)

Against a copy of the real `config.yaml`, on an isolated daemon:

- The migration rewrote all 7 `no-mmap: true` entries to `load-mode: none` and
  wrote the `.pre-knobs.bak`.
- The probe split the host's five llama.cpp builds correctly: `llamacpp-rocm`,
  `llamacpp-vulkan`, `llamacpp-UnslothMTP`, `llamacpp-DFlash2` → `enum`;
  `llamacpp-ROCmFP4` (the `q38rocm` shim, an older fork) → `flags`. ds4 and
  vllm declare none.
- `Qwen3.8-27B-UD-Q6_K@coding-medium` — the request that 503'd — reached ready
  with `--load-mode none -c 131072` in its real argv.
- The same knob on the fork build emitted `--no-mmap` (read from
  `/proc/<pid>/cmdline`), and that launch reached ready too.
