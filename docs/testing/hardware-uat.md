# Hardware UAT — set up & run

This is the maintainer-facing guide for the `llamastash uat` subcommand
(behind `--features uat`, hidden from `--help` on every release
binary). It targets four backends — NVIDIA CUDA, AMD ROCm, Apple
Silicon Metal, Vulkan fallback — and runs a 5-step lifecycle on real
hardware, emitting a JSON report you attach to the release PR.

The fixture suite (`cargo test --features test-fixtures`) gives the
fast per-PR signal; UAT is the slower per-release gate that catches
the regression classes fixtures provably can't:

- NVML probe drift surfacing wrong VRAM total on real CUDA cards.
- `rocm-smi` parse regressions returning `0%` utilization on real ROCm.
- Metal device-count off-by-one on multi-GPU Macs.
- Broken GH-Releases install URLs.
- Vulkan iGPU miscategorization.

Origin & plan:

- Brainstorm — `docs/brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md`
- Plan — `docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`

---

## 1. Per-backend one-time setup

The UAT needs the vendor toolkit `llama-server` requires, plus
`llama-server` itself on `PATH`. Cold mode (see §2) exercises the
install path; warm mode assumes you've already done these steps.

### NVIDIA CUDA (Linux)

```sh
# CUDA driver / runtime — Ubuntu example, adapt for your distro.
sudo apt-get install -y nvidia-driver-555 cuda-toolkit-12

# Verify the toolkit is on PATH for llama.cpp build / load.
nvidia-smi
nvcc --version
```

llama.cpp install:

```sh
# Prefer the official release asset:
gh release download -R ggerganov/llama.cpp llamacpp-linux-x86_64.tar.gz
# Or build from source against CUDA — depends on whether your driver
# is current enough for the prebuilt binary's link target.
```

### AMD ROCm (Linux)

```sh
# AMD's installer drops rocm-smi, rocminfo into PATH:
sudo apt-get install -y rocm-libs rocminfo rocm-smi
rocm-smi
```

llama.cpp via Homebrew on Linux (linuxbrew):

```sh
brew install llama.cpp
```

### Apple Silicon Metal (macOS-14+)

```sh
# Xcode Command Line Tools provide Metal headers.
xcode-select --install

# llama.cpp via Homebrew — same pattern as Linux.
brew install llama.cpp
```

### Vulkan fallback (Linux)

```sh
# Vulkan loader + tools. The UAT exercises whichever silicon the
# Vulkan loader actually picks — usually the iGPU on hosts without
# a discrete card, or the discrete card when no vendor toolkit is
# installed.
sudo apt-get install -y libvulkan1 vulkan-tools mesa-vulkan-drivers
vulkaninfo --summary
```

llama.cpp must be built with `-DGGML_VULKAN=on`; the upstream prebuilt
asset under llama.cpp's GH Releases includes a Vulkan-capable build on
Linux x86_64.

### HuggingFace cache pre-population (all backends)

Warm mode assumes the reference GGUF is already in the cache. Pull
it once, ideally pinned to the SHA captured in
`src/cli/uat/model.rs`:

```sh
# Primary (Qwen2.5-0.5B-Instruct-GGUF / Q4_K_M ~400 MB)
llamastash pull Qwen/Qwen2.5-0.5B-Instruct-GGUF:qwen2.5-0.5b-instruct-q4_k_m.gguf
# Fallback (SmolLM2-360M-Instruct-GGUF / Q4_K_M ~270 MB)
llamastash pull HuggingFaceTB/SmolLM2-360M-Instruct-GGUF:smollm2-360m-instruct-q4_k_m.gguf
```

The constants in `model.rs` currently ship a `<TBD-locked-on-first-dry-run>`
SHA — the first real warm-mode dry-run lands the lock-in commit.
Until then the UAT resolves the repo's default branch.

---

## 2. Running the UAT

### Warm mode (per-release default)

```sh
# 5-min budget per backend; assumes llama-server + GGUF pre-staged.
cargo run --features uat -- uat --backend nvidia      --mode warm --report-out uat-nvidia.json
cargo run --features uat -- uat --backend amd         --mode warm --report-out uat-amd.json
cargo run --features uat -- uat --backend apple_metal --mode warm --report-out uat-metal.json
cargo run --features uat -- uat --backend vulkan      --mode warm --report-out uat-vulkan.json
```

Each command:

- Creates a fresh tempdir under `$TMPDIR` (`/tmp` on Linux, `$TMPDIR`
  on macOS) keyed by `<backend>-<mode>-<pid>-<nanos>`. Set as
  `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET` /
  `HF_HOME` for the child processes so the run never touches your
  real daily-driver state.
- Runs the 5-step lifecycle (`doctor_preflight` → `init` → `smoke_chat`
  → `stop` → `doctor_postrun`).
- Writes the JSON report to the `--report-out` path (`-` redirects to
  stdout — mutually exclusive with the global `--quiet`).
- Prints the TTY-pretty summary to stdout unless `--quiet`.
- Exits 0 on `verdict: "pass"`, 1 otherwise.

### Cold mode (≥ 1 backend per minor release)

```sh
# 15-min budget; exercises the full brew / GH-Releases install path.
cargo run --features uat -- uat --backend apple_metal --mode cold --report-out uat-metal-cold.json
```

Cold mode is the only path that catches install-side regressions.
The plan's degraded-gate policy (origin §Degraded gate policy)
requires at least one cold-mode run per minor release on any of the
four backends. Patch releases may rely on the most recent cold run
< 30 days old.

### What each step does

| Step | Action | Failure means |
|---|---|---|
| `doctor_preflight` | `gpu::probe()` snapshot; assert `--backend` matches the detected discriminant | Wrong driver / runner image; UAT halts immediately so a non-Metal macOS-14 image doesn't masquerade as a green run. |
| `init` | `llamastash init --recommended --model <repo>:<file> --revision <sha>` with the isolation env vars; runs install too in cold mode. | Install path broke, GGUF fetch failed (HF outage / wrong SHA), recommender mis-selected. Fallback model runs automatically on primary failure; substitution surfaces in `host.warnings`. |
| `smoke_chat` | Parses `status --json` for the running model + port, then POSTs `/v1/chat/completions` and asserts non-empty content. | Model loaded but failed to respond — GPU OOM, slot exhaustion, sampler bug. |
| `stop` | `llamastash stop --all --yes`. | Daemon stop_model failed or the supervisor lost track of the child. |
| `doctor_postrun` | `llamastash doctor --json`; records `finding_count`. | Doctor itself failed; finding count > 0 is informational, not a fail. |

### Interpreting a fail

`failure_summary.step` says which step short-circuited.
`failure_summary.classification` carries the stable snake_case enum
agents pattern-match on (see §UAT failure classifications).
`failure_summary.exit_code` is the **failing child's** exit code
verbatim — e.g. `73` for `INIT_DOWNLOAD_FAILED`. The exit code IS NOT
remapped to a UAT-specific code; consult `src/cli/exit_codes.rs` for
the meaning, **except** for the synthetic codes the UAT itself emits
when a step never spawned a subprocess (see next subsection).

### UAT synthetic exit codes

These codes appear in `failure_summary.exit_code` when the failing
step doesn't run a subprocess, or when the orchestrator wraps the
subprocess outcome (timeout / SIGINT). They sit outside the
`<sysexits.h>` 64-78 band so they never collide with `init`'s 72-74
or with a legitimate child exit code.

| Code | Constant in `src/cli/uat/lifecycle.rs` | Meaning |
|------|---------------------------------------|---------|
| `10` | `PREFLIGHT_MISMATCH_CODE` | `doctor_preflight` saw `expected` ≠ `detected` GPU backend |
| `11` | `SMOKE_HTTP_ERROR_CODE` | `smoke_chat` could not reach the model's HTTP endpoint or got a non-2xx |
| `12` | `SMOKE_PARSE_ERROR_CODE` | `smoke_chat` could not parse the model's response or the `status --json` body |
| `13` | `SMOKE_STATUS_ERROR_CODE` | `smoke_chat`'s `llamastash status --json` probe failed |
| `124` | `TIMEOUT_CODE` | Subprocess exceeded its per-step budget; followed shell convention (`timeout(1)`) |
| `130` | `SIGINT_CODE` | The maintainer Ctrl-C'd the UAT; `verdict` becomes `interrupted` |

### UAT failure classifications

`failure_summary.classification` is the stable snake_case enum the
nightly workflow's rolling-issue comment routes on. New variants are
additive within `schema_version: 1`; a rename bumps the schema.

| Value | Where it comes from |
|-------|---------------------|
| `backend_mismatch` | `doctor_preflight` — `expected` ≠ `detected` |
| `init_install` / `init_download` / `init_other` | `init` failures, classified by the child's exit code (74 / 73 / other) |
| `smoke_http` / `smoke_parse` / `smoke_status` | `smoke_chat` failure mode |
| `stop_failed` | `stop` step couldn't shut the daemon's children down |
| `doctor_postrun_failed` | `doctor_postrun` itself spawn/exit failed |
| `timeout` | Any subprocess exceeded its per-step budget |
| `interrupted` | SIGINT during the run |
| `other` | Catch-all (panic-safe envelope, unclassifiable child exit) |

When a run is not `verdict: pass`, the tempdir is **preserved** and
the path is recorded in `host.warnings` ("preserved tempdir at ...").
Inspect it for:

- `state/` — the daemon's `state.json` snapshot at fail time.
- `cache/logs/` — `llama-server` per-launch log files.
- `hf/hub/` — the partial / completed model download.

---

## 3. Degraded gate policy

The release gate is one human running UAT on four boxes — fragile by
design. Explicit fallback policy (matches origin §Degraded gate
policy verbatim):

- **Max report age per backend**: ≤ 14 days at release time, or list
  the backend as "untested this release" with a link to the most
  recent passing report.
- **Box unavailable** (hardware fail, OS upgrade, vacation): ship
  without that backend; mark explicitly in release notes ("vX.Y.Z
  was not UAT-tested on AMD ROCm. Most recent confirmed-good: v…").
- **All four boxes unavailable simultaneously**: delay the release
  or cut a patch-only release explicitly scoped to non-GPU changes.
- **Cold mode**: ≥ 1 backend per minor release. Patch releases may
  rely on the most recent cold run if it's < 30 days old.

This is honor-system. No workflow enforces it; the compliance metric
is the release-PR checklist (`release.md`) + the `uat-caught` label
at the 6-month outcome review.

---

## 4. Attaching the report to a release PR

```sh
gh pr create --template release.md
```

The release template (`.github/PULL_REQUEST_TEMPLATE/release.md`)
carries the UAT backends-checked checklist. Fill in:

- Tick each backend you ran (or mark explicitly untested + reason).
- State which backend got the cold-mode coverage for this cycle.
- Attach `uat-*.json` as file attachments **or** paste each verbatim
  under the corresponding checklist line.

If a UAT run caught a regression that would otherwise have shipped,
apply the `uat-caught` label so the 6-month outcome-metric review has
signal. The label tracks the value-metric for retire-or-reshape
decisions:

```sh
# One-time recreation (the side-effect runs once, but in case the
# label is ever deleted):
gh label create uat-caught \
  --color B60205 \
  --description "Release PR where UAT caught a regression that would otherwise have shipped"
```

---

## 5. Per-backend budget calibration

The 5-min warm-mode target is a starting point pending p95 from the
maintainer's four boxes. After the first 4 dry runs across NVIDIA /
AMD / Metal / Vulkan, fill the table below with observed p95 numbers
so future runs can flag drift:

| Backend | Observed p95 (warm) | Observed p95 (cold) | Notes |
|---|---|---|---|
| NVIDIA CUDA | _TBD_ | _TBD_ | |
| AMD ROCm | _TBD_ | _TBD_ | |
| Apple Silicon Metal | _TBD_ | _TBD_ | |
| Vulkan fallback | _TBD_ | _TBD_ | iGPU lane is structurally slower; per-backend budget may be needed. |

The iGPU / Vulkan box is the most likely to need a separate budget if
it routinely overshoots the global 5-min target.

---

## 6. Troubleshooting

### "preserved tempdir at /tmp/llamastash-uat-..."

Expected on every non-`pass` run — the orchestrator preserves the
sandbox so you can inspect it. Clean up manually once you've
finished investigating:

```sh
rm -rf /tmp/llamastash-uat-<backend>-<mode>-<pid>-<nanos>
```

### Pre-flight failure on a host you know has the GPU

`gpu::probe()` runs four sub-probes in order: NVIDIA → AMD → Metal →
Vulkan. The first that succeeds wins. If your discrete NVIDIA + AMD
Radeon machine reports `amd`, that's because `nvidia-smi` failed; check
the NVML driver / `nvidia-smi --query-gpu` output manually.

### Reference model SHA still placeholder

The `model.rs` constants carry `<TBD-locked-on-first-dry-run>` until
the first warm-mode dry run lands the real SHA. The orchestrator
surfaces this with the warning `"reference model SHA unlocked
(placeholder)"`. To lock it in:

1. Run warm-mode UAT against the maintainer's box.
2. Capture the resolved SHA from the HF response (visible in
   `~/.cache/huggingface/hub/models--Qwen--Qwen2.5-0.5B-Instruct-GGUF/refs/main`
   or the report's `host.warnings` once we wire it through).
3. Replace the constant in `src/cli/uat/model.rs` and PR the change.

### Empty `models[]` from `status --json` during smoke_chat

Init's smoke probe passed but the daemon didn't keep the model
started. Likely cause: a transient sampler crash, or the supervisor's
ready-probe timed out post-init. Inspect `cache/logs/<short-id>-*.log`
in the preserved tempdir for the `llama-server` stderr.
