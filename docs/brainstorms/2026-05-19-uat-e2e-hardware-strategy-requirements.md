---
title: UAT / E2E strategy + GPU vendor hardware testing
date: 2026-05-19
status: requirements (ready for /ce:plan)
origin: TODO.md — "Test strategy for Nvidia / AMD / Apple GPU support (origin: R34)"
related:
  - docs/brainstorms/llamatui-requirements.md
  - docs/brainstorms/2026-05-18-init-wizard-requirements.md
  - tests/ (existing fixture-based integration suite)
  - src/gpu/{nvidia,amd,vulkan,metal}.rs
  - .github/workflows/ci.yml
revision: 3 (focused refine after round-2 document-review: factual bugs F1-F4 + sprawl trims S1-S5)
---

# UAT / E2E strategy for LlamaDash, with per-vendor GPU coverage

## Problem

The project ships a single binary that detects GPU hardware (`src/gpu/{nvidia,amd,vulkan,metal}.rs`), launches `llama-server`, and reports backend-specific telemetry (`gpu_backend`, `gpu_mem_*`, `gpu_temp_c`). Today's automated tests run only against a `fake_llama_server` fixture on `ubuntu-latest` / `macos-latest`. That suite is good at catching protocol, IPC, lifecycle, and UI regressions, but it provably **cannot** catch:

- A broken NVML probe surfacing the wrong VRAM total on real CUDA hardware.
- A `rocm-smi` parse regression that returns `0%` utilization on real ROCm cards.
- A Metal device-count off-by-one on a multi-GPU Mac.
- A Vulkan-only fallback that miscategorizes a real Intel iGPU.
- An end-to-end `init → install llama-server → pull GGUF → launch → smoke chat` flow that breaks on a specific backend.

There is also no UAT contract for what a maintainer should manually verify before tagging a release.

### Why this over "just rely on RC + community soak"?

A reasonable alternative: tag every release as `rc` for ≥ 48h and let early adopters with diverse hardware catch regressions on their own boxes. For a pre-1.0 project with an established user base, that would be plenty.

LlamaDash is pre-1.0 with **no established user base yet** (pre-publish to crates.io, pre-tap, pre-binaries). The expected contributor-UAT response rate on the first several RCs is ~0. Until that changes, the project has nobody to soak with — so the maintainer is the only source of real-hardware signal that exists. Once a user base forms, this calculation shifts and the community path becomes the primary defense; the maintainer-run UAT then degrades into a pre-major-release sanity check rather than a per-release gate (see [Outcome metric](#outcome-metric--when-to-retire-or-reshape)).

## Goal

Define a **tiered test strategy** that:

1. Preserves the existing fast, fixture-based per-PR signal (today's `cargo test --features test-fixtures`).
2. Adds an executable **UAT command** the maintainer runs on each local hardware box before tagging a release, producing a structured report.
3. Adds **one automated Metal lane** via the free GitHub macOS Apple Silicon runner on a nightly cron — *conditional on a falsifying spike confirming Metal actually works in that environment* (see Tier 3).
4. Documents how community contributors can run the same UAT and submit results, becoming the primary defense once the user base materializes.

**Opportunity cost (acknowledged):** This is multi-day work that competes with TODO.md v1+ items (proxy router, restart hotkey, advanced modal refactor, HF pull TUI, best-model command, release setup). The maintainer is choosing this as the next investment over those because GPU-vendor regressions are harder for users to diagnose and report cleanly than feature gaps — a broken `gpu_backend` reading produces confused bug reports across vendors, while a missing feature produces a clean "I want X" issue. Re-evaluate if a non-GPU regression class proves to be the bigger pain.

**Non-goal for this round:** paid cloud GPU runners. Re-evaluate per the [Outcome metric](#outcome-metric--when-to-retire-or-reshape) trigger.

## Users and stakeholders

- **Maintainer** — runs the UAT pre-release on owned hardware (NVIDIA CUDA Linux, AMD ROCm Linux, Apple Silicon Mac, Vulkan-fallback Linux), pastes JSON report into the release PR. Subject to the [Degraded gate policy](#degraded-gate-policy) below.
- **Contributors** — optionally run the UAT on their hardware against a release-candidate tag; post the JSON report on a tracking issue.
- **CI** — runs the nightly Metal UAT lane (after the spike confirms it's real coverage) and updates a single rolling tracking issue when it breaks.

## The three tiers

### Tier 1 — Unit + fixture e2e (per-PR, today)

No new work. This is what `cargo test --features test-fixtures` already does. Keep the bar where it is: every PR runs the full suite on `ubuntu-latest` + `macos-latest`. Augment opportunistically:

- Each `src/gpu/*.rs` backend module must keep its `#[cfg(test)] mod tests` for **parse-only** cases (synthetic `nvidia-smi` output, synthetic `rocm-smi` JSON, synthetic Metal `system_profiler` blobs).
- Continue extending `tests/*.rs` integration suites that exercise the daemon + CLI + TUI via `fake_llama_server`.

### Tier 2 — Manual hardware UAT command (pre-release, all backends)

A new scripted UAT command that runs the **user lifecycle** on whichever real hardware is present, against a real `llama-server` and a real (small) GGUF, and emits a structured pass/fail report.

#### UAT lifecycle (one run per backend)

Isolation strategy: the UAT sets a fresh tempdir as the state-dir + runtime-dir for the lifetime of its child processes. The mechanism is platform-specific:

- **Linux:** set `XDG_STATE_HOME=<tempdir>` and `XDG_RUNTIME_DIR=<tempdir>` — `paths::state_dir()` and the daemon socket path already honor those.
- **macOS:** the `directories` crate ignores XDG vars on macOS, so XDG-only isolation silently writes into `~/Library/Application Support/llamadash` and the maintainer's real socket. The UAT must also set `LLAMADASH_SOCKET=<tempdir>/daemon.sock` (the existing override at `paths::runtime_socket_path`), plus a **new** `LLAMADASH_STATE_DIR=<tempdir>` env var that `paths::state_dir()` must be taught to consult. The state-dir env-var addition is a planning prerequisite — Tier 3 cannot land on macOS until it exists.

This is the minimum change set: one new env var (Linux + macOS), no CLI flag additions on `init`/`doctor`/`start`/`status`.

5 steps, in order, each timed and recorded. Step 2 (`init`) already does install + pull + smoke probe — earlier drafts double-counted that as 4 separate steps. The condensed flow keeps coverage without scaffolding:

1. **`doctor` (pre-flight)** — capture baseline `gpu_backend`, hardware fingerprint, available disk/VRAM. Records what backend the rest of the UAT is exercising.
2. **`init --recommended --json [--skip install]`** — full init wizard: hardware detect → `llama-server` install (skipped via `--skip install` when the binary is pre-staged; see [Operating modes](#operating-modes)) → recommender → `pull` a deliberately tiny GGUF (target: ≤ 1 GB) → write `config.yaml` → smoke probe. Includes a `status --json` sub-assertion that `gpu_backend` matches Step 1's reading.
3. **Smoke chat against the started model** — single non-empty completion via the OpenAI-compatible endpoint. Assert HTTP 200 and `> 0` tokens. Also assert (after ~1.5s sampler tick) that `latest_rss_bytes` and `latest_cpu_pct` are populated — folded into this step rather than a standalone "telemetry check".
4. **`stop <model>`** — graceful shutdown.
5. **`doctor` (post-run)** — confirm no orphaned `llama-server` PIDs, no stale lockfiles in the tempdir.

Failure short-circuits subsequent dependent steps; the report records every step's intended state.

#### Operating modes

The UAT has two modes, selected by a `--mode {warm|cold}` flag on the `uat` subcommand:

- **`--mode warm`** (default for per-release runs): the UAT passes `--skip install` to `init`. Assumes `llama-server` is already on PATH and the reference GGUF is in the HF cache. This is the fast (≤ 5 min) per-backend gate. Does not exercise the install path.
- **`--mode cold`**: the UAT omits `--skip install`, so `init` runs the full brew / GH-Releases install path. Slower (~10-20 min depending on cache state) and exercised at least once per backend per release cycle, plus implicitly by Tier 3's nightly CI lane. Catches install-path regressions the warm mode skips.

The maintainer's release-PR checklist must include at least one cold run across the four backends per minor release (any backend); patch releases may rely on the most recent cold run if it's < 30 days old. This is the [degraded-gate policy's](#degraded-gate-policy) extension for install-path coverage.

#### Degraded gate policy

The release gate is one human running UAT on four boxes — a fragile setup. Explicit fallback policy:

- **Max report age per backend:** ≤ 14 days old at release time, *or* the release notes must list that backend as "untested this release" with a link to the most recent passing report.
- **Box unavailable (hardware fail, OS upgrade, vacation):** ship without that backend's UAT; mark it explicitly in release notes ("This release was not UAT-tested on AMD ROCm. Most recent confirmed-good: vX.Y.Z."). Do not block the release on a single unavailable box.
- **All four backends unavailable simultaneously:** delay the release or cut a patch-only release explicitly scoped to non-GPU changes.

This is honor-system; no workflow enforcement. Acknowledged tradeoff: the gate is voluntary infrastructure and may decay over time. The [Outcome metric](#outcome-metric--when-to-retire-or-reshape) is the trigger that flags decay before it becomes invisible.

#### Report contract

By default UAT emits **both** a TTY-pretty summary to stdout AND a structured JSON report. `--quiet` skips stdout; `--report-out -` redirects JSON to stdout instead of a file. The default-both behavior is the contract — solo modes are opt-in.

JSON shape mirrors the existing `GpuInfo` enum (tagged union) so per-backend fields stay correct without a lossy scalar projection:

```jsonc
{
  "schema_version": 1,                    // additive-only within v1; consumers must ignore unknown fields
  "started_at": "<rfc3339>",
  "duration_secs": 142.3,
  "host": {
    "os": "linux",
    "arch": "x86_64",
    "kernel": "...",
    "llamadash_version": "0.4.0-rc1",
    "llama_server_version": "..."
  },
  "backend": {
    "expected": "apple_metal",            // normalized to the GpuInfo discriminant; the CLI's `--backend metal` resolves to "apple_metal" here
    "detected": {                         // serialized verbatim from src/gpu/mod.rs::GpuInfo (tag = "backend", rename_all = "snake_case")
      "backend": "nvidia",
      "devices": [
        { "name": "NVIDIA GeForce RTX 4090", "total_memory_bytes": 25_769_803_776, "used_memory_bytes": 0, "utilization_pct": null, "temperature_c": null }
      ]
      // apple_metal:  { "backend": "apple_metal", "total_memory_bytes": 38_654_705_664 }
      // cpu_only:     { "backend": "cpu_only" }
      // unknown:      { "backend": "unknown", "devices": [...] }
    }
  },
  "steps": [
    { "name": "doctor_preflight", "verdict": "pass", "duration_ms": 312, "observed": { /* ... */ } },
    { "name": "init",             "verdict": "pass", "duration_ms": 135120 },
    { "name": "smoke_chat",       "verdict": "pass", "duration_ms": 5801 },
    { "name": "stop",             "verdict": "pass", "duration_ms": 612 },
    { "name": "doctor_postrun",   "verdict": "pass", "duration_ms": 1455 }
    // sum: 143,300ms ≈ duration_secs above
  ],
  "verdict": "pass",
  "failure_summary": null                 // populated with { step, exit_code, message } when verdict=fail
}
```

Duration numbers above are illustrative, not budget commitments. See [Performance budgets](#performance-budgets).

Compatibility commitment: schema is additive-only within v1. Renames or removals bump to v2 and the issue template embeds the version it was generated against so future readers know which schema applies.

#### Exit codes

`0` = pass, `1` = any UAT failure. The JSON report's `failure_summary.step` and `exit_code` carry the phase information; granular exit-code mapping is YAGNI for a tool whose only consumer today is a human reading the JSON. Revisit only if a script consumes the exit status directly.

Existing `cli::exit_codes` constants (72-74 for init, 70 for binary-not-found, etc.) are not touched.

#### Invocation surface

`llamadash uat ...` as a hidden subcommand gated behind a new `--features uat` Cargo feature. The release binary on crates.io / Homebrew bottle ships without the feature, so `uat` is unreachable from a user install. `cargo run --features uat -- uat --backend nvidia` is the maintainer's invocation.

Why this over `cargo xtask uat`: the repo is currently a single-crate manifest (no `[workspace]` table). Adopting `xtask` would require converting to a workspace just to add one dev command, with no second dev binary on the horizon to amortize the cost. A `--features uat` subcommand achieves the same "not reachable from a release binary" guarantee with `#[cfg(feature = "uat")]` gating around the subcommand handler and its module. Revisit `xtask` if/when a second dev-only command appears.

#### Pre-staged inputs per backend (warm mode only)

Warm mode assumes the maintainer has done a one-time per-box setup, documented in `docs/testing/hardware-uat.md`:

- The vendor toolkit `llama-server` needs (CUDA driver, ROCm runtime, Xcode CLT, Vulkan loader).
- `llama-server` itself installed and on PATH.
- A pre-fetched reference GGUF in the HF cache layout (so Step 2's `pull` is a no-op on warm runs).

Cold mode does none of this assumption — it exercises install + pull from scratch.

**Reference model contract.** Specific model picked in planning. Constraints:

- ≤ 1 GB on disk (post-Q4 quantization).
- Loads in ≤ 3 GB of usable GPU memory at `--ctx 2048`, *including* unified-memory and iGPU configurations.
- Supports OpenAI-compatible chat completion meaningfully.

**Supply-chain posture.** Pin by HuggingFace commit SHA (requires adding a `--revision <sha>` flag to `llamadash pull` and threading it through `init`'s pull invocation — see [Planning prerequisites](#planning-prerequisites)). Define one fallback model meeting the same constraint envelope; if the primary fails to fetch, the UAT command falls back automatically and records `model_used: "<fallback-name>"` in the report.

If both primary and fallback fail to fetch, the UAT errors loudly — the maintainer falls back to manually staging a model and re-runs. No mirror, no BLAKE3 cache verification, no license-redistribution path. Revisit if HF unavailability actually breaks a release UAT run.

### Tier 3 — Nightly automated Metal lane (GH Actions)

**Prerequisite: a falsifying spike.** Before this lands, prove that GH-hosted `macos-14` runners actually expose Metal to a headless process. Land a throwaway `.github/workflows/uat-metal-spike.yml` that loads a small GGUF with `llama-server -ngl 99` and asserts (a) `gpu_backend` resolves to `apple_metal`, not `cpu_only`, and (b) layers actually offload. Capture the result in the PR description and delete the spike workflow in the same PR that lands the real Tier 3 workflow. If the spike fails, Tier 3 collapses to "macOS build verification" and is honestly labeled as such.

Assuming the spike passes, job spec:

- One job, `uat-metal-nightly`, `runs-on: macos-14` (pinned, not `macos-latest` — the alias has flipped between Intel and Apple Silicon historically and the UAT must structurally fail on Intel).
- **Trigger:** `schedule:` cron only (no `push:` / `pull_request:` / `workflow_dispatch:` triggers). Scheduled workflows on GitHub always run against the default branch.
- **Caching:** `actions/cache` for the HF model cache + brew bottles + Cargo target dir.
- **Setup phases (untimed):** brew install `llama.cpp` happens *before* the UAT command. brew flakes are the #1 macOS CI failure mode and must not be classified as UAT regressions.
- **UAT invocation:** `cargo run --features uat -- uat --backend metal --mode warm --report-out uat-metal.json`. Warm mode is used because the brew install (step above) pre-stages `llama-server`; the UAT itself shouldn't re-run install on every nightly. Tier 3 implicitly exercises the brew install path via its setup phase, so the cold-mode coverage need is partially met.
- **Pre-flight assertion:** the UAT must assert `backend.detected.backend == "apple_metal"` *before* any other step runs, so a runner-image regression (Metal not available) fails loudly and immediately with a clear message.
- **Failure routing:** single rolling tracking issue (`#uat-metal-status` or similar). The workflow looks up an open issue with label `uat-metal-status`; if none exists, it creates one. On each failure, append a comment with the failure classification (pre-flight assertion / brew install / model load / smoke chat / other) so interleaved failure modes remain attributable rather than overwriting each other. Close the issue on next success via `gh issue close`. Workflow needs `permissions: issues: write` granted explicitly.

Why nightly not per-PR: avoids burning macOS CI minutes (billed 10× on private repos; unmetered for public repos) on every PR for a regression class that historically fires rarely.

## Performance budgets

Different cadences have different cache realities; one budget doesn't fit both:

- **Tier 2 warm-mode local runs:** target < 5 minutes per backend wall-clock. Assumes `llama-server` pre-installed, GGUF pre-fetched. The bulk is `init`'s recommender + smoke probe + the chat round-trip. The 5-min target is a starting point — calibrate against p95 of the first 4 dry runs across the maintainer's hardware; the iGPU/Vulkan box is structurally slower and may justify a per-backend budget if it routinely overshoots.
- **Tier 2 cold-mode local runs:** target < 15 minutes per backend wall-clock. Includes install + pull + everything warm-mode covers.
- **Tier 3 nightly CI:** target < 20 minutes wall-clock total, including brew install (only on cache miss), HF model fetch (only on cache miss), and Rust build. Cache hits should bring this under 8 minutes.

For Tier 3, set `timeout-minutes: 30` on the job — a passive ceiling that prevents runaway minute burn without converting "slower than usual" into a tracked regression in the rolling issue.

## What gets verified across the layers

| Concern | Tier 1 (fixtures, per-PR) | Tier 2 (manual UAT, pre-release) | Tier 3 (nightly Metal CI) |
|--------|---------------------------|----------------------------------|---------------------------|
| Protocol / IPC / lifecycle | ✅ | (incidental) | (incidental) |
| GPU module parse logic | ✅ (synthetic input) | — | — |
| GPU detection against real silicon | — | ✅ all 4 backends *(see caveats below)* | ✅ Metal only (after spike passes) |
| `llama-server` install path | — | ✅ cold mode (≥ 1× per minor release per [Operating modes](#operating-modes)) | ⚠️ via brew setup phase, untimed |
| HF pull against real network | — | ✅ all 4 backends | ✅ macOS |
| Real model launch + chat | — | ✅ all 4 backends | ✅ Metal only |
| `doctor` against real drift | — | ✅ | ✅ |
| Release-blocking signal | — | ✅ (manual gate, subject to [degraded-gate policy](#degraded-gate-policy)) | ⚠️ informational only — never blocks release |

**Vulkan coverage caveat.** "Vulkan-fallback Linux" is *one box with one device*. Vulkan-the-codepath covers Intel iGPU, AMD-without-ROCm, NVIDIA-via-Mesa, Intel Arc dGPU, and others. A single UAT box validates whichever silicon it has; the rest of the Vulkan matrix is uncovered until contributors run UAT on theirs. Release notes should say "Vulkan: tested on <specific device family>", not "Vulkan: passed".

## Community-driven coverage (the primary defense, eventually)

Once a user base materializes, the contributor UAT path becomes the breadth defense for unowned hardware (Windows CUDA, Intel Arc, exotic AMD). The plan:

- A `Hardware UAT report` GitHub issue template that embeds the JSON report shape and schema_version.
- Tag a release as `rc` for ≥ 48h before promoting, giving contributors a window to run UAT on hardware the maintainer doesn't own.
- Until the user base exists: release notes explicitly list untested backends as "untested" rather than implying coverage.

## Outcome metric — when to retire or reshape

The release gate is voluntary infrastructure and will decay without an outcome signal. Two paired metrics:

**Value metric (does it catch real bugs?)** Within 6 months of Tier 2 shipping: if ≥ 1 release PR carries the `uat-caught` label *and* the catch is **not** also reachable by Tier 1 fixtures (a fixture-reachable catch should land as a Tier 1 extension, not credit Tier 2), keep the gate. Otherwise reshape or retire.

**Compliance metric (is it actually running?)** Track via the release-PR template, which includes a checklist line "UAT run attached for backends: [ ] NVIDIA [ ] AMD [ ] Apple Silicon [ ] Vulkan". Count over the 6-month window: if < 50% of releases have ≥ 2 backends checked, the gate has decayed in practice and the value metric is uninterpretable — reshape (smaller scope) before re-evaluating.

Use `gh pr list --label uat-caught --state merged` to surface the catch list at the 6-month mark. No separate tracked file.

**Companion trigger for cloud runners:** revisit the cloud-runner non-goal once the project has > 500 reported installs (gh-releases download count + cargo install metrics) AND ≥ 3 RC cycles in that period have closed without contributor UAT reports for non-maintainer hardware. The "after 3 RC cycles" alone is meaningless while the user base is ~0.

## Success criteria

- A maintainer can run **one command per local box** before a release and get a pass/fail verdict + a JSON report they can attach to the release PR. Wall-clock target: < 5 min per backend (warm mode) / < 15 min (cold mode).
- The nightly Metal lane runs on `main` and upserts a single rolling tracking issue when broken, with per-failure comments preserving signal across interleaved failure modes. (Conditional on the [Tier 3 spike](#tier-3--nightly-automated-metal-lane-gh-actions) passing.)
- The fixture suite (Tier 1) is unchanged in shape.
- A contributor with hardware the maintainer doesn't own can follow `docs/testing/hardware-uat.md` end-to-end without asking for help.
- The UAT entry point does not appear in the default release binary's `--help` (feature-gated; absent without `--features uat`). The release workflow audits its build invocation to confirm the feature is never enabled in shipped binaries.
- The release-PR template carries the UAT checklist line and the `uat-caught` label is applied when a UAT run catches a regression that isn't fixture-reachable — enabling the 6-month value + compliance evaluation.

## Out of scope

- Paid cloud GPU runners.
- Windows CUDA UAT automation.
- Performance benchmarking (tokens/sec across backends).
- A web dashboard / aggregator for community UAT reports.
- Automated rollback if Tier 3 fails.
- An `xtask` workspace member (revisit if a second dev-only command appears).

## Planning prerequisites

Items that must land *as part of* (or before) the UAT implementation, surfaced explicitly because they're load-bearing for the spec above:

1. **`LLAMADASH_STATE_DIR` env var** consulted by `util::paths::state_dir()` on all platforms. Required for macOS UAT isolation; without it, Tier 3 Metal lane writes into the runner's real `~/Library/Application Support/llamadash` and Tier 2 on the maintainer's Mac collides with their daily-driver state.
2. **`--revision <sha>` flag on `llamadash pull`**, plumbed through `init`'s internal pull invocation so `init --recommended --revision <sha>` honors it. Required for HF commit-SHA pinning; without it, the UAT's reference model is branch-tracked and silently moves under it.
3. **Release-PR template** containing the UAT backends-checked checklist and an `uat-caught` label hint. Required for the compliance + value metric tracking; without it, the 6-month retire-or-reshape evaluation has no signal.

## Open questions for the planning phase

1. **Reference model** — which tiny GGUF satisfies the [reference model contract](#pre-staged-inputs-per-backend-warm-mode-only)? Likely a Q4 quantization of a ≤ 1B-parameter model from the existing recommender corpus. Pick the specific model + commit SHA in planning. Pick a fallback meeting the same envelope.
2. **Per-backend budget calibration** — after the first 4 dry runs, decide whether the < 5-min warm-mode target holds across all backends or whether the Vulkan/iGPU lane needs a separate budget.

## Acceptance checklist for the plan

A `/ce:plan` derived from this doc should produce implementation units that cover:

- [ ] **Planning prerequisite work:** `LLAMADASH_STATE_DIR` env var added to `paths::state_dir()`; `--revision <sha>` flag added to `llamadash pull` and threaded through `init`'s pull invocation; release-PR template added with UAT checklist + `uat-caught` label registered. (See [Planning prerequisites](#planning-prerequisites).)
- [ ] New `--features uat` Cargo feature wiring a hidden `llamadash uat` subcommand. `cfg(feature = "uat")` gates the subcommand variant in `Subcommands`. No `xtask`.
- [ ] UAT command: cross-platform state-dir/socket isolation (Linux XDG + macOS `LLAMADASH_STATE_DIR` + `LLAMADASH_SOCKET`), the 5-step lifecycle, `--mode {warm|cold}` flag (default warm), `--backend <name>` normalization to `GpuInfo` discriminant, structured JSON report (verbatim `GpuInfo` serialization for `backend.detected`), 0/1 exit codes with `failure_summary.exit_code` carrying the failing child's exit status, automatic fallback model on primary fetch failure.
- [ ] Reference GGUF + fallback model selection in planning, both pinned by HuggingFace commit SHA. License redistribution not required (no mirror).
- [ ] `docs/testing/hardware-uat.md` — per-backend one-time setup, how to run warm and cold modes, the [Degraded gate policy](#degraded-gate-policy) and cold-mode cadence requirement, how to attach the JSON to a release PR, how to apply the `uat-caught` label.
- [ ] Falsifying spike before Tier 3 lands — throwaway `.github/workflows/uat-metal-spike.yml` proving Metal is reachable on `macos-14`. Capture result in the PR description; delete the spike workflow in the same PR that lands the real one. No separate spike doc.
- [ ] New GH Actions workflow `.github/workflows/uat-metal-nightly.yml` per Tier 3 (conditional on spike result). `timeout-minutes: 30` on the job; `permissions: issues: write`; `actions/cache` for HF cache + brew bottles + Cargo target dir; brew install in a separate untimed setup step.
- [ ] Release workflow audit: confirm `--features uat` is never passed during shipped-binary builds.
- [ ] Acceptance verification: maintainer dry-runs the UAT in warm mode on each **available** backend before merging the implementation PR. Any backend whose box is temporarily unavailable is recorded in the PR description and runs in a follow-up before the next release — mirroring the [Degraded gate policy](#degraded-gate-policy) rather than introducing a stricter merge gate.
- [ ] `TODO.md` entry struck; new TODOs added for: `Hardware UAT report` issue template (deferred until first contributor wants to submit one), cloud-runner re-evaluation (gated on user-base trigger).
- [ ] `docs/usage.md` and `docs/architecture.md` left untouched (UAT is dev-only).
