---
title: "feat: MTP (multi-token prediction) speculative decoding — llama.cpp + ds4"
type: feat
status: implemented
date: 2026-07-14
origin: https://github.com/llamastash/llamastash/issues/51 (+ TODO.md ds4 MTP auto-pairing)
---

# feat: MTP (multi-token prediction) speculative decoding — llama.cpp + ds4

## Overview

Add first-class **MTP (multi-token prediction) speculative decoding** to the
launcher across both self-speculative backends:

1. **llama.cpp** (issue #51) — detect MTP-capable GGUFs from the header, emit
   the `--spec-type draft-mtp` speculative path (and `--model-draft` for the
   separate-head family), and surface it. MTP embeds a lightweight draft head
   in the model so the target verifies several tokens per forward pass —
   roughly a 2x decode speedup at high draft-acceptance with a small memory
   cost, and **output-equivalent** to non-speculative decoding (the target
   model still verifies every token).
2. **ds4** (TODO.md `DS4 › MTP auto-pairing`) — `ds4-server` already accepts
   `--mtp <path>` / `--mtp-draft N` / `--mtp-margin F`; expose them as native
   knobs and **auto-pair** antirez's published MTP sidecar GGUF from the same
   HF repo.

Both need a **companion-GGUF download** path from HuggingFace that does not
exist today. We build it once and use it for MTP sidecars **and** to close a
pre-existing gap: `pull` never fetches `mmproj` projector siblings, so
multimodal models pulled by llamastash arrive without their vision/audio
head. The same primitive fetches the needed companion (mmproj projector, MTP
head) alongside the selected base file — one per kind by default, opt-in to all
variants (KD5).

The unifying constraint, proven by a live test (see Problem Frame): emitting
the MTP flag on a model that has no MTP head is a **hard launch failure**, not
a graceful no-op. Detection is therefore a mandatory gate, not an
optimization.

## Problem Frame

Users who want the MTP speedup today must hand-write `--spec-type draft-mtp`
(llama.cpp) or `--mtp <path>` (ds4) into `extras`, which means knowing the
exact flag spelling, getting no capability detection, no companion-file
handling, and no VRAM-fit awareness. The issue asks for auto-detection,
auto-enable with knobs, separate-drafter support, fit adjustment, and TUI
indicators.

### Verified facts this plan builds on

Read from the local llama.cpp checkout (`bf2c86ddc`, tag ~`b9951`, binary
reports `version 10011`; 2 commits behind `origin/master`, both unrelated to
MTP) and the real `ds4_server.c` (antirez/ds4 `master`), 2026-07-14. Where the
issue's claims proved stale, the correction is called out.

**llama.cpp**

- **Detection key: `{arch}.nextn_predict_layers`** (`LLM_KV_NEXTN_PREDICT_LAYERS`,
  `llama-arch.cpp:202`), a `UINT32`. `> 0` ⇒ the file carries embedded MTP /
  NextN prediction layers (`n_layer_nextn`). This matches the issue and the
  upstream convention. Reads via our existing
  `header.u64(&[format!("{a}.nextn_predict_layers")])` pattern — no new GGUF
  plumbing.
- **Server enable flag is `--spec-type draft-mtp`** (`common/arg.cpp:3871`;
  `draft-mtp` confirmed present in the real `llama-server --help` type list;
  name map `common/speculative.cpp:35`; enum `COMMON_SPECULATIVE_TYPE_DRAFT_MTP`
  `common/common.h:173`). **Correction:** the issue (and a prior session note)
  treated `--mtp` as the user flag. `--mtp` is `set_examples({LLAMA_EXAMPLE_DOWNLOAD})`
  only (`arg.cpp:2812`) — it does **not** appear in `llama-server --help-full`;
  it is a *download-tool* convenience that also grabs the head. The server enable
  is `--spec-type draft-mtp`.
- **Draft-token count: `--spec-draft-n-max N`** (`arg.cpp:3797`, default **3**).
  Separate drafter: `--spec-draft-model` / `-md` / `--model-draft FNAME`
  (`arg.cpp:3862`).
- **Two model shapes.** *Embedded head* (Qwen3.5/3.6, GLM-4.x, DeepSeek-family
  with `nextn_predict_layers > 0`): `--spec-type draft-mtp` alone; the target's
  own nextn tensors are the drafter, no `--model-draft`. *Separate head*
  (Gemma-4 ships `mtp-*.gguf` siblings): needs `--spec-type draft-mtp
  --model-draft <path>`.
- **`--parallel 1` is NOT required (issue claim stale).** `server-context.cpp:370`:
  "MTP supports splitting"; no hard `n_seq_max == 1` guard for `draft-mtp`. Do
  not pin `--parallel 1`.
- **`--fit` is already MTP-aware (issue feature #4 largely handled upstream).**
  `server-context.cpp:1082-1093` reserves the MTP draft context
  (`LLAMA_CONTEXT_TYPE_MTP`) before fitting the target **when the spec type is
  set on argv**. Because llamastash delegates offload to `--fit`, emitting
  `--spec-type draft-mtp` *before* `--fit-ctx` gets MTP-aware fitting for free.
- **Live-tested failure mode (critical).** `llama-server --spec-type draft-mtp`
  on a non-MTP model (local `gemma-4-E2B`, arch `gemma4`, no nextn) exits:
  ```
  W llama_init_from_model: context type MTP requested but model doesn't contain MTP layers
  E common_speculative_init_result: failed to create MTP context
  E srv  llama_server: exiting due to model loading error
  ```
  ⇒ the flag must be gated on real capability; emitting it blind bricks the launch.
- **Live-tested success (embedded head).** Pulled `unsloth/Qwen3.5-4B-MTP-GGUF`
  (`Qwen3.5-4B-Q4_K_M.gguf`, arch `qwen35`, header `qwen35.nextn_predict_layers = 1`).
  `llama-server -m … --spec-type draft-mtp` (embedded head, **no** `--model-draft`)
  loaded in 3s and served `/v1/chat/completions`. MTP engaged:
  `draft acceptance = 0.652 (105 accepted / 161 generated), mean len = 2.94`.
  **Draft-acceptance surfaces two ways** (resolves M5's field question): the HTTP
  response `timings` object carries `draft_n` (drafted) + `draft_n_accepted`
  (accepted) — acceptance = `draft_n_accepted / draft_n`; the server log prints
  `slot print_timing: … draft acceptance = <rate> ( <acc> accepted / <gen> generated ), mean len = <n>`.

**ds4** (`ds4_server.c`, master)

- `--mtp <path>` → `engine.mtp_path` (**FILE** path, `need_arg`), `--mtp-draft <int>`
  → `mtp_draft_tokens` (default **1**), `--mtp-margin <float 0..1000>` →
  `mtp_margin` (default **3.0**) (arg block ~line 11557; defaults ~11518). All
  three parse in the current binary — confirming TODO.md and superseding the
  stale AGENTS.md note that says ds4 *rejects* `--mtp`/`--quality`.
- `/v1/models` still advertises the static `[deepseek-v4-flash, deepseek-v4-pro]`
  pair (lines ~11229) — no change to readiness/adoption facts.
- antirez's model card states the DeepSeek-V4 MTP sidecar "requires a specific
  loader" ⇒ treat DeepSeek-V4 MTP as **ds4-only**; do not attempt to route
  antirez's sidecar through llama.cpp's `--spec-type` path.

**Our codebase (no companion-download path exists)**

> **Refactor note (2026-07-15).** This plan predates commit `df93180`
> (`refactor(backend): pluggable backends behind one Backend trait +
> registry`), which moved backend-specific behavior behind the `Backend`
> trait (`backend/mod.rs`) and consolidated the launch path
> (`launch_service.rs` +769, `params.rs` +80, `ipc/status.rs` +285). The
> plan's design survives the refactor unchanged — every seam it needs still
> exists — but the anchors below are re-pinned to post-refactor lines and two
> seams are now *cleaner* (ds4 MTP auto-pair via `Backend::resolve_native_knobs`,
> KD7/D2; the formalized no-leak rule, KD9). See AGENTS.md "Adding a backend".

- Typed-knob machinery is centralized and compiler-guarded: `TypedKnobs`
  (`config/loader.rs`), `KnobField` (`flag_aliases.rs`), `KnobSpec` SPECS
  (`flag_aliases.rs`), `argvify` + `compose` (`params.rs:709`), `slot`/`slot_mut`
  (`loader.rs`). A new knob is ~6 edits, each fails to compile until wired (no
  wildcard match arms). `compose` is still the **llama.cpp** argv emitter —
  `LlamaCppBackend::process_spec` delegates to it, pinned by golden parity tests
  (`backend/llama_cpp.rs`) — so llama.cpp MTP flags emit here (KD9).
- `extras` + denylist: `FORBIDDEN_ADVANCED_PREFIXES` (`params.rs:35`),
  `is_forbidden_head_ext` (`params.rs:49`); each backend adds its own via the
  trait `forbidden_extra_heads()` (ds4 adds `--cors`/`--dist-`).
- `mmproj` is auto-detected **on disk** — at **discovery scan**
  (`detect_multimodal` → `find_mmproj`, `scanner.rs:234`/`:326`, which sets the
  multimodal badge) **and** again at launch, where `compose_and_spawn` resolves
  `launch_params.mmproj_path` (`launch_service.rs:479`, from `parsed.mmproj_path`
  or `find_mmproj` unless extras manage it) and `compose` emits `--mmproj`
  (`params.rs:718`) — but **never downloaded from HF**. (The sibling scan runs at
  scan time, so a capability badge can be computed pre-launch.) The pull path
  (`init/download.rs`: `select_files`, `download_repo`, `list_repo_files` via
  `hf_hub RepoInfo.siblings`) selects only base `.gguf` siblings + shards.
- GGUF KV read + arch-prefix resolution: `header.rs` (`u64`/`string`/`get`),
  `metadata.rs` (`general.architecture` → `{arch}` prefix), extend
  `ModelMetadata` (`metadata.rs:16`) + `summarise` (`metadata.rs:297`).
- Capability badges: `discovery/mod.rs:100` (`Multimodal::LEGEND`, 2 glyphs),
  rendered in `right_pane.rs` `render_header_name`, cached `metadata_cache.rs`,
  JSON `catalog.rs:128` (multimodal block). ds4 badge precedent: keyed on
  capability for a *selected* row and on `routed_backend` (generic discovery
  field from `auto_routes`/`routed_backend_for`, `catalog.rs:101`) for a
  *running* row.
- `status` per-model rows: `ipc/status.rs` — each row carries `resolved_backend`
  (`:105`/`:201`) + a `params` object serialising `knobs`/`backend_knobs`
  (`:178`)/`extras`; stability test in `status.rs`; CLI mirror
  `cli/output.rs::status_json`.
- Memory/fit: `--fit-ctx` emit (`params.rs`), local admission
  `admission.rs::project_demand:206` (with `is_sampled:140` /
  `effective_free_bytes:159` helpers), and KV geometry now behind the
  `Backend::kv_bytes` trait hook (`gguf::memory::kv_bytes` consults every
  backend, header-keyed) — so the MTP admission band is a generic
  `project_demand` addition, **not** a per-backend KV override (KD6).

## Requirements Trace

From issue #51:

- **M1** (feature 1): auto-detect MTP capability from `{arch}.nextn_predict_layers`
  → a `mtp_capable` header property surfaced through discovery.
- **M2** (feature 2): auto-enable MTP on llama.cpp with configurable draft
  settings — a tri-state enable knob (default `auto`-on-when-capable; launch
  knob only, no config-file entry — KD2) plus a draft-token knob mapping to
  `--spec-draft-n-max`.
- **M3** (feature 3): separate-head support — on-disk sibling detection
  (`mtp-*.gguf`) → `--model-draft`, and HF auto-download of the sidecar.
- **M4** (feature 4): VRAM/context fit awareness for MTP — delegated to
  llama.cpp's MTP-aware `--fit` (emit spec-type before `--fit-ctx`); a small
  local-admission band so the OOM gate isn't over-optimistic.
- **M5** (feature 5): TUI indicators — an MTP badge in the model list/right
  pane and MTP draft-acceptance (`timings.draft_n_accepted / timings.draft_n`)
  in the chat/logs view.

Beyond the issue:

- **M6** (TODO.md `DS4 › MTP auto-pairing`): expose ds4 `--mtp`/`--mtp-draft`/
  `--mtp-margin` as native knobs and auto-pair antirez's MTP sidecar from HF.
- **M7** (new ask, folded in): `pull` auto-detects and downloads the needed
  `mmproj` projector (and MTP head) companion for a GGUF — closing the standing
  mmproj HF gap with the same companion primitive M3 needs. **One-per-kind by
  default** (KD5 selection policy — repos ship several projector precisions;
  launch uses one), with an opt-in to fetch all variants.
- **M8**: keep `status`/CLI/docs in sync (AGENTS.md, README, usage,
  troubleshooting, config example, CHANGELOG, TODO strike, plan checkboxes).

## Scope Boundaries

- **Detection-gated, never blind.** `--spec-type draft-mtp` is emitted only
  when the file is genuinely MTP-capable (embedded `nextn > 0`, or a resolved
  separate head). No capability ⇒ no flag (the live-tested hard-fail).
- **MTP is output-equivalent** — auto-enable-when-capable is safe by default;
  a launch-time opt-out (`mtp: off`) exists for users who don't want it. No
  config-file entry (KD2).
- **DeepSeek-V4 MTP is ds4-only.** antirez's sidecar goes through ds4's
  `--mtp`; it is not fed to llama.cpp's `--spec-type` (card: "requires a
  specific loader"). The two backends' MTP paths stay separate.
- **No custom draft-model download for arbitrary speculative types.** Only the
  MTP head (embedded needs none; separate-head + ds4 sidecar are auto-paired).
  `draft-simple`/`eagle3`/`dflash` stay `extras`-only.
- **Companion download is opt-out, capped.** Reuses the existing per-file size
  cap (512 GiB). A `--no-companions` (or per-type) escape hatch for
  bandwidth-constrained pulls.
- **Acceptance-rate display is read-only** — surfaced from llama-server's
  response timings; no new sampling infra.
- **Fit stays delegated.** We do not re-implement MTP KV math for placement;
  llama.cpp's `--fit` owns it. Local admission gets a conservative band only.

## Key Technical Decisions

- **KD1 — Enable is a hard gate.** Resolve an effective MTP decision at launch
  (mirroring how `mmproj_path` is resolved in `launch_service.rs:445`):
  `mtp_effective = knob_state ∧ (embedded_capable ∨ separate_head_present)`.
  Emit nothing when not capable; when the user *forces* `mtp: on` on a
  non-capable model, **warn and skip** rather than emit-and-brick.
  *Capability is `nextn_predict_layers > 0` **or** a separate `mtp-*.gguf` head
  — never mmproj. The multimodal projector is an orthogonal companion; a
  model's multimodal status neither enables nor gates MTP.*
- **KD2 — MTP enable is a bespoke tri-state, NOT a `KnobValue<bool>`.** The
  states we want are `auto` (on when the model is MTP-capable per KD1), `on`
  (force), `off`. **`KnobValue` cannot express this:** its `Auto` variant means
  *"delegate to `--fit`; emit no flag"* (`loader.rs:511`), a fit-placement
  meaning that is nonsense for MTP — and `seed_layerless` only ever seeds `Auto`
  for `fit_governed()` fields (`params.rs:624`, `if !spec.field.fit_governed()`),
  which MTP is not. Model enable
  as a **dedicated `enum MtpEnable { Auto, On, Off }`** with its own resolver
  handling, not a generic knob slot. Default is `Auto`; the auto-on decision is
  resolved at launch from the model's capability (a per-model runtime property
  threaded via `LaunchParams`, not a static `defaults_table` row). **Launch /
  TUI / preset only — no `config.yaml` / `config.example.yaml` `mtp:` section, no
  `arch_defaults` row** (maintainer decision, 2026-07-14); it still persists via
  `presets` / `last_params` like any launch choice, but nothing new is
  hand-authored in config. **Consequence for B1:** only `spec_draft_n_max` (a
  plain `u32 → --spec-draft-n-max`, unset ⇒ llama.cpp's default of 3) fits the
  mechanical ~6-edit knob path; the enable tri-state is custom type + resolver
  work, costed separately.
- **KD3 — `--spec-type` accumulates; reconcile with user extras.** llama.cpp
  *appends* each `--spec-type` (`types.insert(...end())`, verified) rather than
  overriding, so emitting ours on top of a user's extra yields a duplicated /
  second type. Rule: **if `extras` already contains any `--spec-type`, defer
  entirely to the extra and emit none** — the user is driving speculative
  decoding by hand; do not silently merge a second type (which could conflict).
  Detect the extra before B3's emit.
- **KD4 — Separate-head detection reuses the mmproj sibling pattern.** A
  `find_mtp_head(model_path)` scan (`mtp-<stem>.gguf`, `<stem>-mtp.gguf`, quant
  fallbacks) alongside `find_mmproj`. On-disk presence makes a
  non-embedded model MTP-capable.
- **KD5 — One companion-download primitive, with a selection policy.** Extend
  `list_repo_files` + `select_files`/`download_repo` to also select companion
  siblings for the chosen base file. **Do not grab every variant** — repos ship
  multiple projector precisions (`mmproj-BF16/F16/F32`, e.g. the Qwen3.5-4B repo
  pulled for validation) and may ship multiple heads; downloading all wastes
  bandwidth on files launch never uses. Pick **one per companion kind** (default:
  the most-compatible, tie-broken by smallest), with an opt-in to fetch all.
  This is M3's sidecar fetch **and** M7's mmproj fetch in one place.
- **KD6 — Fit is delegated; admission gets a band.** Compose emits
  `--spec-type` before `--fit-ctx` so llama.cpp fits MTP-aware. Embedded MTP
  weight tensors are already counted by `weights_bytes`; add a small MTP
  compute/KV band to `project_demand` (`admission.rs:206`) so the local OOM
  gate isn't optimistic. Calibrate the band from the Phase A validation run.
- **KD7 — ds4 MTP rides the native-knob channel; auto-pair via
  `resolve_native_knobs`.** `mtp` (path), `mtp_draft` (int), `mtp_margin` (float)
  become three rows in `DS4_NATIVE_KNOBS` + `DS4_FLAG_MAP` (`backend/ds4/mod.rs`,
  ds4 grows 6→9), translated to `--mtp`/`--mtp-draft`/`--mtp-margin` by the
  existing `translate()` in `ds4_argv`. **Sidecar auto-pairing is an Auto knob,
  not a bespoke path:** resolve the sidecar path in `Ds4Backend::resolve_native_knobs`
  — the generic Auto-resolution hook `compose_and_spawn` already calls over every
  backend (`launch_service.rs:717`), the same seam ds4 uses today for
  `ssd_streaming`. An unset `mtp` knob resolves from the base model's repo/dir
  (downloaded via KD5); an explicit user value wins (mirrors the `ssd_streaming`
  user-override-wins check). The auto-set key is stripped from `last_params` so it
  re-resolves each launch. Keep DeepSeek-V4 MTP ds4-only.
- **KD8 — Badge keys on backend truth.** Selected row → capability prediction;
  running row → whether MTP actually resolved on for that launch (llama.cpp
  `draft-mtp` present, or ds4 `--mtp` passed) — same discipline as the ds4 badge.
- **KD9 — Respect the formalized backend no-leak rule (`df93180`, AGENTS.md
  "Adding a backend").** MTP spans both backends but no backend id-string / name
  may appear in code or comments outside the backend's own module + the registry.
  Concretely: the llama.cpp MTP flags (`--spec-type draft-mtp` / `--model-draft` /
  `--spec-draft-n-max`) emit in `compose` (`launch/params.rs`), llama.cpp's argv
  path — the same place `--mmproj` / `--jinja` live, naming no backend. The ds4
  MTP knobs + sidecar auto-pair live entirely in `backend/ds4/` (`DS4_NATIVE_KNOBS`
  + `DS4_FLAG_MAP` + `resolve_native_knobs`). **MTP capability detection
  (`nextn_predict_layers`, `find_mtp_head`) is generic and header-keyed** — a
  discovery/metadata property, not a backend branch — so it names no backend, and
  the enable gate (KD1) is resolved from that header property, not from which
  engine runs. No `== "ds4"` / `== "llamacpp"` in the MTP path.

## Open Questions

### Resolved during planning

- *Is `--mtp` the server flag?* No — `--spec-type draft-mtp` is. (Verified.)
- *Does MTP need `--parallel 1`?* No, not in current llama.cpp. (Verified.)
- *Who computes MTP VRAM overhead?* llama.cpp's `--fit`, when spec-type is on
  argv. (Verified from source.)
- *Is the non-capable failure graceful?* No — hard launch exit. Gate is
  mandatory. (Live-tested.)
- *Does mmproj already download from HF?* No — only on-disk detect. M7 is
  net-new. (Verified.)
- *Auto-enable default and its surface?* **Default `auto`-on-when-capable**;
  override is a **launch knob only, no config-file entry** (maintainer
  decision, 2026-07-14 — KD2).
- *Draft-acceptance field for M5?* **`timings.draft_n_accepted / timings.draft_n`**
  (HTTP), and the `draft acceptance = …` server-log line. Live-captured
  2026-07-14 against `unsloth/Qwen3.5-4B-MTP-GGUF` (see Problem Frame).

### Deferred to implementation

- **Admission band magnitude** — measure embedded-MTP resident delta on the
  validation model; pick a conservative constant/fraction.
- **Separate-head naming coverage** — enumerate real Gemma-4 / other sidecar
  filename patterns during M3 to make `find_mtp_head` robust.

## High-Level Technical Design

```
GGUF header ──nextn_predict_layers──► ModelMetadata.mtp (embedded_layers)
   │                                        │
   │  find_mtp_head(sibling)  ──────────────┤ (separate head on disk)
   ▼                                        ▼
DiscoveredModel.mtp_capable ─► catalog JSON / TUI badge / status

launch ──► resolve mtp_effective (knob ∧ capability) ─► LaunchParams.mtp
   │                                                        │
   ├─ llama.cpp compose: --spec-type draft-mtp [--model-draft H] [--spec-draft-n-max N]  (before --fit-ctx)
   └─ ds4 prepare_launch: --mtp <sidecar> [--mtp-draft N] [--mtp-margin F]

pull owner/repo[:file] ──► select base + companions (mmproj*, mtp head) ──► HF cache
```

## Implementation Units

### Phase A — detection substrate

Both capability signals resolve at **discovery scan** (not launch), mirroring
`detect_multimodal` (`scanner.rs:326`, which runs `find_mmproj` (`scanner.rs:234`)
at scan to set the multimodal badge). Scan-time is what lets the *selected-row*
badge (KD8) see capability before any launch.

- **A1. Read `nextn_predict_layers` (embedded head).** Add an `mtp` field
  (embedded layer count, `Option<u32>`; `0`/absent ⇒ none) to `ModelMetadata`
  (`metadata.rs:16`), populate in `summarise` (`metadata.rs:297`) via the
  arch-prefixed `u64` read. Cache in `CachedParse` (`metadata_cache.rs:34`) and
  surface on `DiscoveredModel`.
- **A2. `find_mtp_head(model_path)` (separate head), at scan time.** A sibling
  scan next to `find_mmproj` (`scanner.rs`: `mtp-<stem>.gguf` / `<stem>-mtp.gguf`
  / quant fallbacks), run in the same scan path as `detect_multimodal` and cached
  in `CachedParse`. **Also exclude MTP-head files from the launchable base-model
  catalog** — extend the companion filter (`is_projector_companion`,
  `scanner.rs:125`, applied at `:239`/`:264`/`:418`) so a `mtp-*.gguf` head does
  not list as a phantom model, exactly as mmproj files are excluded today.
  `mtp_capable = embedded ∨ separate-head-present`, surfaced in `catalog.rs:128`
  JSON (beside the multimodal block).
- **A3. Admission-band measurement (only remaining validation).** The enable
  path, `--spec-type draft-mtp`, "embedded head needs no `--model-draft`", and the
  acceptance fields are **already live-validated** (Problem Frame,
  `unsloth/Qwen3.5-4B-MTP-GGUF`, 2026-07-14). Remaining: measure the
  resident-memory delta (MTP-on vs MTP-off RSS on the same model) to calibrate the
  KD6 admission band — feeds F2.

### Phase B — llama.cpp MTP launch

- **B1. Launch params — two different shapes (KD2).**
  - `spec_draft_n_max` — a plain `u32 → --spec-draft-n-max`; fits the mechanical
    ~6-edit knob path (`TypedKnobs`, `KnobField` + `field_name` +
    `fit_governed=false`, `KnobSpec` SPECS row, `slot`/`slot_mut`, `argvify`,
    `apply_knob`).
  - `mtp` enable — a **dedicated `MtpEnable { Auto, On, Off }` tri-state**, NOT a
    `KnobValue` slot (KD2). Add the type + its CLI `start` flag / TUI control /
    preset-serde, resolved at launch (B2). Bespoke, not the mechanical path.
- **B2. Resolve effective MTP at launch** — in `compose_and_spawn`, right beside
  the `launch_params.mmproj_path` resolution (`launch_service.rs:479`): fold knob
  state + capability + separate-head into `LaunchParams.mtp`. Force-on a
  non-capable model ⇒ warn + skip (KD1).
- **B3. Compose** (`params.rs:709`, near the `--mmproj` emit at `:718`): emit
  `--spec-type draft-mtp` (merge-aware, KD3), `--model-draft <head>` if separate,
  `--spec-draft-n-max N` if set — positioned **before** `--fit-ctx` (KD6). Hard
  gate on `LaunchParams.mtp`. This is llama.cpp's argv path; naming no backend
  (KD9). Parity tests in `backend/llama_cpp.rs` pin `prepare_launch` to `compose`,
  so the new flags are covered there for free.
- **B4. Tests:** compose golden argv (embedded / separate / off / force-on
  non-capable warns); extras `--spec-type` dedup.

### Phase C — HF companion download (M3 sidecar + M7 mmproj)

- **C1.** In `download_repo`/`select_files` (`init/download.rs`), after base
  selection, filter the already-listed repo siblings for companions
  (`mmproj*.gguf`, MTP heads matching the base stem), apply the KD5
  one-per-kind selection, and add to the download set (respect the 512 GiB
  per-file cap). `--no-companions` opt-out; an `--all-companions`-style flag to
  fetch every variant. **Trust boundary:** companions are pattern-matched
  **within the same user-requested repo** — no cross-repo fetch (see Risks).
- **C2.** Cache-layout parity so discovery's on-disk `find_mmproj` /
  `find_mtp_head` locate the downloaded companions with no extra wiring.
- **C3. Tests:** repo listing → companion selection (mmproj-only repo,
  MTP-sidecar repo, base-only repo), opt-out path.

### Phase D — ds4 MTP auto-pairing (M6)

- **D1.** Add three rows to `DS4_NATIVE_KNOBS` + `DS4_FLAG_MAP`
  (`backend/ds4/mod.rs`): `mtp` (path) / `mtp_draft` (int) / `mtp_margin` (float)
  → `--mtp`/`--mtp-draft`/`--mtp-margin` (all `NativeKnobKind::FreeText`). The
  existing `translate()` in `ds4_argv` emits them — no `prepare_launch` change.
  ds4 grows **6→9** native knobs.
- **D2.** Auto-pair in `Ds4Backend::resolve_native_knobs` (the Auto hook, beside
  the existing `ssd_streaming` logic — KD7): when `mtp` is unset, resolve the
  sidecar path from the base model's repo/dir (downloaded via Phase C) and set it
  (recording the auto-set key so it re-resolves next launch); an explicit user
  value wins. Keep DeepSeek-V4 MTP ds4-only (scope).
- **D3.** Correct the now-**stale** ds4 in-code notes: the module doc
  (`backend/ds4/mod.rs:19-25`, "`--mtp`… ds4-CLI only… table is 6 entries, not
  8") and the `DS4_NATIVE_KNOBS` comment both say ds4-server rejects `--mtp` —
  false as of the 2026-07-10 build (verified; see `ds4-server-vs-cli-flags`
  memory). Update them, and bump the `native_knob_ids_are_unique_and_documented`
  test's `len == 6` assertion to `9`. Review `DS4_FORBIDDEN_EXTRA_HEADS` (no new
  forbidden heads expected). Tests: ds4 argv with/without paired sidecar;
  override precedence.

### Phase E — surfaces & docs (M5, M8)

- **E1. TUI badge** — MTP glyph in the capability `LEGEND`
  (`discovery/mod.rs:100`) and `render_header_name` (`right_pane.rs:501`);
  selected-row keys on capability, running-row on resolved MTP (KD8). Help-overlay
  legend updated.
- **E2. Acceptance rate — trace the data path first.** The numbers live in
  llama-server's HTTP response `timings.draft_n` / `timings.draft_n_accepted`
  (Problem Frame). Decide the carrier to the TUI — proxy passthrough of `timings`
  on the streamed response, a parse of the server-log `draft acceptance = …`
  line, or a new per-launch `status` field — and wire it before the chat/logs
  render. **Test dependency:** teach `fake_llama_server` (`tests/fixtures/`) to
  emit `timings.draft_n` / `draft_n_accepted` so the display has a fixture to
  assert against.
- **E3. status/CLI** — add an `mtp` indicator to running rows near the
  `resolved_backend` field (`ipc/status.rs:201`), mirror in
  `cli/output.rs::status_json`; update the per-row / key-set stability test.
- **E4. Docs** — README feature list; `docs/usage.md` MTP section per backend
  (llama.cpp `--spec-type draft-mtp`; ds4 `--mtp`; companion auto-download; the
  MTP enable knob is start-time only — document the flag/TUI, **not** a config
  key, per KD2); `docs/troubleshooting.md` (the non-capable hard-fail + fix);
  `docs/architecture.md`; `CHANGELOG.md`; strike TODO.md
  `DS4 › MTP auto-pairing`; correct the stale AGENTS.md ds4 `--mtp` note and
  add an MTP scope bullet; tick this plan's checkboxes.

### Phase F — fit / admission (M4)

- **F1.** Confirm compose order gives MTP-aware `--fit` (integration check).
- **F2.** Add the calibrated MTP admission band to `project_demand`
  (`admission.rs:206`) from A3's measurement.

## System-Wide Impact

- **Zero change when no MTP model is present** and no companion in a repo —
  byte-stable argv/wire/JSON for existing launches (new fields are additive,
  `Option`, omitted when absent).
- **`pull` behavior changes** (M7): multimodal/MTP repos now bring companions.
  This is the intended fix; documented + opt-out.
- **status/catalog JSON** gain additive `mtp_capable` / `mtp` fields — pin the
  stability tests.
- **ds4 native-knob count** 6→9; TUI ds4 knob panel and `backend_knobs` grow.

## Risks & Dependencies

- **Hard-fail if the gate is wrong** — mitigated by KD1 + golden tests + the
  force-on-warns path.
- **antirez sidecar in llama.cpp** — explicitly out of scope; ds4-only.
- **Acceptance-rate carrier** — the field names are verified
  (`timings.draft_n_accepted` / `draft_n`); the open piece is the TUI **data
  path** + a `fake_llama_server` fixture (E2), not the field itself.
- **Companion auto-fetch trust boundary** — `pull` now downloads files the user
  did not name. Constrain to **name-pattern matches within the same requested
  repo** (no cross-repo, no arbitrary path); companions are GGUFs parsed by
  llama.cpp / ds4 like any pulled model. Documented + `--no-companions` opt-out.
- **Companion download size** — one-per-kind selection (KD5) + 512 GiB per-file
  cap + opt-out.
- **Upstream flux** — llama.cpp spec flags and ds4's flag set both move; facts
  above are pinned to specific commits and must be re-checked at implementation
  start (`llama-server --help`, `ds4-server --help`).

## Documentation / Operational Notes

- Re-verify `--spec-type` / `nextn_predict_layers` against the `llama-server`
  the daemon resolves at implementation time; re-verify ds4 flags against the
  live `ds4-server --help`.
- Never fabricate MTP behavior from a fake server — validate llama.cpp MTP
  against a real MTP GGUF (A3) and ds4 MTP against a real `ds4-server`.

## Phased Delivery

A (detection) → B (llama.cpp launch) → C (companion download) → D (ds4 pairing)
→ E (surfaces + docs) → F (fit/admission). B depends on A; C is independent and
unblocks D and M7; E/F trail. Suggested PR cuts: {A+B+E-badge} as the issue-#51
core, {C+M7} as the companion-download PR, {D} as the ds4 PR, each with its doc
slice.

## Sources & References

- Issue #51 — https://github.com/llamastash/llamastash/issues/51
- llama.cpp MTP: `common/arg.cpp` (`--spec-type`:3871, `--mtp`:2812,
  `--spec-draft-n-max`:3797, `--model-draft`:3862), `common/speculative.cpp:31-35`,
  `common/common.h:173`, `src/llama-arch.cpp:202`, `tools/server/server-context.cpp:363-1093`
  (checkout `bf2c86ddc`, binary `version 10011`).
- ds4: `ds4_server.c` (`--mtp`/`--mtp-draft`/`--mtp-margin` ~11557, defaults ~11518;
  `/v1/models` ~11229) — antirez/ds4 master.
- Prior art in-repo: mmproj on-disk detect (`scanner.rs:234`), pull path
  (`init/download.rs`), typed knobs (`config/loader.rs`, `launch/flag_aliases.rs`,
  `launch/params.rs`), ds4 backend (`backend/ds4/`), ds4 plan
  (`docs/plans/2026-07-10-001-feat-ds4-backend-plan.md`).
- TODO.md — `DS4 › MTP auto-pairing`.
