---
title: "feat: UAT / E2E strategy with per-vendor GPU hardware coverage"
type: feat
status: active
date: 2026-05-19
origin: docs/brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md
revision: 2 (refined after document-review: addressed 3 P1s + cheap P2s, collapsed 10 units → 7)
---

# UAT / E2E strategy with per-vendor GPU hardware coverage

## Overview

Introduces a tiered test strategy that layers real-hardware UAT coverage on top of the existing fixture-based test suite, addressing the gap that no test in `cargo test --features test-fixtures` ever runs against an actual GPU. Three concrete deliverables:

1. A maintainer-invoked `llamadash uat` subcommand (hidden, feature-gated) that runs a 5-step lifecycle on real silicon and emits a structured JSON report.
2. A nightly GitHub Actions Metal lane on `macos-14` that runs the same UAT command on Apple Silicon, gated behind a falsifying spike (run as a pre-merge step in the same PR that lands the nightly workflow).
3. Three prerequisite changes (path-isolation env vars, `init --revision <sha>` flag, release-PR template) that make the UAT command possible without touching the user-facing CLI surface.

The maintainer runs the command on each of four owned backends (NVIDIA CUDA, AMD ROCm, Apple Silicon, Vulkan-fallback) before tagging a release, subject to a degraded-gate policy when boxes are unavailable.

## Problem Frame

Today the GPU detection modules in `src/gpu/{nvidia,amd,vulkan,metal}.rs` are unit-tested with synthetic input (parsed `nvidia-smi` output, fake `rocm-smi` JSON, etc.) but never exercised against real silicon. The `init → install llama-server → pull GGUF → launch → smoke chat` flow likewise runs only against `tests/fixtures/fake_llama_server.rs`. The result: a class of regressions — NVML probe drift, ROCm parse changes, Metal device-count off-by-one, broken GH-Releases install URLs, Vulkan iGPU miscategorization — cannot be caught before users hit them.

There is also no contract for what a maintainer should verify before tagging a release. Verification is currently ad-hoc, and the project's CI on `ubuntu-latest` / `macos-latest` does not exercise any real GPU codepath.

(See origin: `docs/brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md` § Problem.)

## Requirements Trace

- **R1.** Preserve the existing fixture-based per-PR suite unchanged (origin §Goal·1). *No implementation work — preserved by absence of changes to the per-PR test suite. Verified at Unit 7's final cross-cut check.*
- **R2.** Ship a `llamadash uat` command the maintainer runs on each local backend pre-release, emitting both TTY-pretty stdout output AND a JSON report (origin §Goal·2, §Tier 2).
- **R3.** Add a nightly Metal lane via GH Actions `macos-14`, conditional on a falsifying spike proving Metal is reachable (origin §Goal·3, §Tier 3).
- **R4.** UAT does not appear in the default release binary's `--help` — feature-gated behind `--features uat` (origin §Invocation surface).
- **R5.** UAT uses cross-platform tempdir isolation, including state, runtime socket, cache/logs, and the HuggingFace cache (origin §UAT lifecycle, §Planning prerequisites·1 — scope expanded after plan-review found `cache_dir()` / `log_dir()` / `hf_cache_dir()` bypass `state_dir()`).
- **R6.** Reference GGUF pinned by HuggingFace commit SHA via a new `init --revision` flag, with one fallback model meeting the same constraint envelope (origin §Reference model contract, §Planning prerequisites·2).
- **R7.** Release-PR template carries a UAT backends-checked checklist; `uat-caught` label enables 6-month outcome-metric evaluation (origin §Planning prerequisites·3, §Outcome metric).
- **R8.** Tier 3 failure routing: single rolling tracking issue (`uat-metal-status` label) with per-failure comments for signal preservation across interleaved failure modes (origin §Tier 3 Failure routing).
- **R9.** Cold-mode coverage (full install path) runs at least once per minor release per the degraded-gate policy; warm mode is the default for per-release runs (origin §Operating modes, §Degraded gate policy).
- **R10.** `TODO.md` R34 line is struck; `docs/usage.md` and `docs/architecture.md` are left untouched (UAT is dev-only) (origin §Acceptance checklist).

## Scope Boundaries

Non-goals (carried verbatim from origin §Out of scope):
- Paid cloud GPU runners.
- Windows CUDA UAT automation.
- Performance benchmarking (tokens/sec across backends).
- Web dashboard / aggregator for community UAT reports.
- Automated rollback if Tier 3 fails.
- An `xtask` workspace member (feature-flag pattern chosen instead).
- A `Hardware UAT report` GitHub issue template — explicitly deferred until a contributor actually wants to file one (origin §Acceptance checklist `TODO.md` line).
- Mirror of the reference GGUF to a GitHub Release asset, BLAKE3 cache verification, license-redistribution audit — all dropped in favor of commit-SHA pin + fallback (origin §Supply-chain posture).
- A `pull --revision <sha>` user-facing flag. The UAT consumes init only; adding `--revision` to `llamadash pull` would be speculative API surface with no real consumer today. Revisit if a real `pull` user requests it.

## Context & Research

### Relevant Code and Patterns

- **Feature gating** — `Cargo.toml` already has `[features] test-fixtures = []`; the `fake_llama_server` `[[bin]]` uses `required-features = ["test-fixtures"]`. Mirror this exact pattern for `uat`.
- **Env-var override for path resolution** — `src/util/paths.rs::runtime_socket_path()` honors `LLAMADASH_SOCKET` as override #1 before falling through to `XDG_RUNTIME_DIR` / `$TMPDIR`. The new env vars (`LLAMADASH_STATE_DIR`, `LLAMADASH_CACHE_DIR`) follow the same shape inside `state_dir()` and `cache_dir()`.
- **Hidden CLI subcommands** — `daemon start` accepts a hidden `--state-dir` flag for internal re-exec hand-off (per AGENTS.md); pattern is `#[arg(hide = true)]` on individual args. For an entire hidden subcommand variant: `#[command(hide = true)]` plus `#[cfg(feature = "uat")]` on the variant.
- **Pull internals already track revision** — `src/init/download.rs:112` has `pub revision: String` on its result struct and line 366 captures the SHA into the result. However, `DownloadOptions` (line 188) does NOT carry a revision *input* field, and `download_repo` (line 313) calls `api.model(spec.repo_id.clone())` which short-circuits to `Repo::new(..., "main")`. The revision-threading work surfaces a new input through `DownloadOptions` and the call site, not just the CLI.
- **Init wizard's model-pick branches** — `src/init/wizard.rs::pick_model` has multiple branches; UAT uses the `ModelOverride::Paste` branch (`init --model owner/repo`) to bypass the recommender, ensuring the pinned model is what actually gets pulled regardless of detected hardware.
- **Existing CI shape** — `.github/workflows/ci.yml` runs on `ubuntu-latest` + `macos-latest` matrix; `release.yml` builds with `cargo build --release ... --bin llamadash` (no `--features` flag, so audit criterion is structurally satisfied — just needs an explicit comment per R10).
- **Test fixture pattern** — Integration tests like `tests/cli_init_parse.rs`, `tests/init_orchestration.rs`, `tests/daemon_lifecycle_test.rs` show the per-test `unique_temp_dir(label)` isolation pattern; UAT tests follow the same convention.

### Institutional Learnings

`docs/solutions/` directory does not currently exist in the repo. The macOS XDG-not-honored gotcha (uncovered during plan-review) is exactly the kind of post-incident learning worth recording in a future `docs/solutions/macos-state-dir-xdg-gotcha.md` once Unit 1 lands.

### External References

External research deliberately skipped. The codebase has strong local patterns for every surface this plan touches. The novel external dependency — whether GH Actions `macos-14` reliably exposes Metal — is what the Tier 3 pre-merge spike determines.

## Key Technical Decisions

- **Env-var isolation over CLI flag plumbing.** UAT sets four env vars (`LLAMADASH_STATE_DIR`, `LLAMADASH_CACHE_DIR`, `LLAMADASH_SOCKET`, `HF_HOME`) — plus Linux's `XDG_*` equivalents redundantly — to isolate state, runtime socket, cache/logs, and the HF cache. Adding `--state-dir` flags across `init`/`doctor`/`start`/`status` would have been the alternative; the four env vars + two new `paths::*` overrides win on surface-area. **Note:** the initial brainstorm proposed only `LLAMADASH_STATE_DIR` + `LLAMADASH_SOCKET`; plan-review caught that `cache_dir()` (logs) and `hf_cache_dir()` (GGUF downloads) bypass `state_dir()` entirely, so the isolation contract was incomplete. The expanded set is the honest minimum.
- **Feature flag over xtask.** `#[cfg(feature = "uat")]` on a `Command` variant achieves the same "not in release binary" guarantee as `cargo xtask` without converting the single-crate manifest to a workspace. (Origin §Invocation surface.)
- **2 exit codes (0/1) instead of 7.** The JSON `failure_summary.{step, exit_code}` carries phase information; granular CLI exit codes are YAGNI for a maintainer-invoked tool. The `failure_summary.exit_code` carries the failing *child wrapper's* exit code as-is (e.g., `72` for an `init`-step failure — init's `INIT_*` codes), not the deeper failure-class code. (Origin §Exit codes.)
- **Tagged-union JSON for `backend.detected`.** Mirrors `GpuInfo`'s serde shape verbatim (`tag = "backend"`) rather than a lossy scalar projection. (Origin §Report contract.)
- **Cleanup contract: Drop-guard-driven, fail-safe-preserve.** UAT acquires the isolation tempdir via a `TempdirGuard` struct that holds a `preserve: bool` flag initialized to `true`. On a successful return from the orchestrator, the guard's `preserve` is set to `false` and Drop removes the directory. On any other path — error return, panic (caught via `std::panic::catch_unwind` around the orchestrator body), SIGINT (trapped via `tokio::signal::ctrl_c`), or implicit drop from a thread crash — Drop honors `preserve = true` and the tempdir survives for post-mortem. Child processes (`llama-server`, especially) are explicitly killed in the guard's Drop *before* tempdir teardown so they don't keep writing to a path that's about to disappear or persist beyond the orchestrator.
- **Throwaway-spike pattern (now as a pre-merge step, not a separate unit).** The Tier 3 PR ships with a throwaway spike workflow (`uat-metal-spike.yml`); maintainer runs it via `workflow_dispatch`; result is captured in the PR description; spike workflow is deleted in the same PR before merge. If the spike fails, U7 ships a build-only macOS lane (pre-authored as the "spike-fail branch" of U7's deliverable) and R3 is honestly restated.
- **Single rolling tracking issue with per-failure comments.** Avoids per-failure issue spam *and* preserves signal across interleaved failure modes (brew flake vs. real Metal regression) that a body-overwrite scheme would lose. (Origin §Failure routing.)
- **Degraded-gate policy is honor-system.** No workflow enforcement of UAT-pre-release. The compliance metric (release-PR checklist via `.github/PULL_REQUEST_TEMPLATE/release.md`) + value metric (`uat-caught` label) catch decay at the 6-month outcome-metric review, which is itself anchored by a date-tagged GH issue created in Unit 7 so the review surfaces in normal triage instead of being a TODO that never fires.

## Open Questions

### Resolved During Planning

- **Entry-point form** — resolved to feature flag during brainstorm round 2 (origin §Invocation surface).
- **State-dir isolation primitive** — resolved to env-var; plan-review extended scope from 1 var to 4 (state, cache, socket, HF cache).
- **Exit-code granularity** — resolved to 0/1 only.
- **Tier 3 failure routing** — resolved to single rolling issue with per-failure comments.
- **Schema versioning** — kept `schema_version: 1` with additive-only commitment; dropped `uat_run_id` UUID.
- **`--revision` plumbing path** — resolved: `Option<String>` added to `DownloadOptions` (not `RepoSpec`), threaded into `download_repo` which calls `api.repo(Repo::with_revision(...))` when `Some`. UAT invokes `init --recommended` is NOT used — UAT calls `init --model <repo>/<filename> --revision <sha>` to use the `ModelOverride::Paste` branch and bypass the recommender. The recommender's pick may differ from UAT's pinned model on a given box; bypass is intentional.
- **PR template form** — resolved to multi-template (`.github/PULL_REQUEST_TEMPLATE/release.md` + `default.md`) from day 1, so non-release PRs aren't polluted with UAT checkboxes.

### Deferred to Implementation

- **Reference GGUF + fallback specifics.** Selected during Unit 4 execution. Constraints from origin: ≤ 1 GB on disk (post-Q4 quantization), loads in ≤ 3 GB usable GPU memory at `--ctx 2048` *including unified-memory and iGPU*, supports OpenAI-compatible chat completion. Candidate families: Qwen2.5-0.5B-Instruct-GGUF (Apache 2.0, ~400 MB Q4_K_M), SmolLM2-360M-Instruct (Apache 2.0), TinyLlama-1.1B-Chat-v1.0-GGUF (Apache 2.0, ~668 MB Q4_K_M). Final pick + commit SHA captured in `src/cli/uat/model.rs` constants.
- **Per-backend warm-mode budget calibration.** After Unit 4 first dry runs across the maintainer's four boxes, decide whether the 5-min target holds uniformly or the Vulkan/iGPU lane needs a separate budget. Recorded in `docs/testing/hardware-uat.md` § Performance.
- **Cold-mode dry-run timing.** Plan-review flagged that no unit *tests* cold mode end-to-end before "done"; deferred because the first cold run requires either a wiped box (impractical for the maintainer's daily-driver hardware) or trust in the brew/GH-Releases install path. Treated as a release-day discovery risk; mitigated by Tier 3's nightly Metal lane implicitly exercising the brew install path. Re-evaluate if the first real cold-mode release surfaces a regression.
- **Fallback-error discrimination policy.** Plan-review flagged that fallback-on-any-fetch-error masks transient HF blips and SHA mismatches alike. For v1, fallback fires on any `init` failure during the model-pull phase; the report's `host.warnings` makes the substitution visible. Tighten only if the first real fallback firing turns out to have been a transient that should have been retried — captured as a future iteration, not a v1 blocker.
- **Cron time.** Recommend `'0 9 * * *'` (09:00 UTC nightly); final pick during Unit 7 implementation.

## Implementation Units

7 units across 4 phases (collapsed from a prior 10-unit / 5-phase form after plan-review flagged that 4+5, 8+9+10 were over-decomposed for a single-maintainer cadence).

### Phase 1 — Prerequisite plumbing (3 units, parallel)

- [ ] **Unit 1: Add `LLAMADASH_STATE_DIR` + `LLAMADASH_CACHE_DIR` env vars to `paths::state_dir()` + `paths::cache_dir()`**

**Goal:** Make `state_dir()` and `cache_dir()` honor env-var overrides as override #1 before falling through to the existing `ProjectDirs` resolution. Required for cross-platform UAT isolation; without it, the UAT writes daemon logs into the maintainer's real `~/Library/Caches/llamadash/logs` (since `log_dir()` derives from `cache_dir()`) AND pollutes the real HF cache (mitigated separately via the UAT-set `HF_HOME` in Unit 4).

**Requirements:** R5.

**Dependencies:** None.

**Files:**
- Modify: `src/util/paths.rs` (extend `state_dir()` and `cache_dir()` with the env-var override pattern; `log_dir()` inherits the cache override automatically since it's `cache_dir().join("logs")`)
- Test: `src/util/paths.rs` inline `#[cfg(test)] mod tests` — matches the project's convention from AGENTS.md.

**Approach:**
- Mirror `runtime_socket_path()`'s `LLAMADASH_SOCKET` override pattern (`src/util/paths.rs:73-79`): check the env var first, return the `PathBuf` verbatim if set and non-empty.
- Apply the same empty-string defensive check (treat empty env value as unset).
- Update the module-level docs at `src/util/paths.rs:1-8` to enumerate all four overrides (`LLAMADASH_STATE_DIR`, `LLAMADASH_CACHE_DIR`, `LLAMADASH_SOCKET`, plus `HF_HOME` which already works via `hf-hub`).

**Patterns to follow:**
- `src/util/paths.rs::runtime_socket_path()` for the env-var-first override pattern.
- Existing module-level docs at `src/util/paths.rs:1-8`.

**Test scenarios:**
- Happy path: with `LLAMADASH_STATE_DIR=/tmp/uat-abc`, `state_dir()` returns `Some(PathBuf::from("/tmp/uat-abc"))`; similarly for `LLAMADASH_CACHE_DIR` → `cache_dir()`.
- Happy path: `log_dir()` returns `<cache-override>/logs` when `LLAMADASH_CACHE_DIR` is set, confirming the chain.
- Edge case: empty env var (`LLAMADASH_STATE_DIR=""`) treated as unset; falls through.
- Edge case: unset env vars → behavior matches today byte-for-byte (Linux returns XDG paths, macOS returns `~/Library/...`).
- Edge case: paths with spaces and unicode returned verbatim without mangling.
- Audit (manual, recorded in PR description): grep `src/` for any other call sites that compute state-dir / cache-dir / log-dir-like paths independently of `paths::*` — should be none for state, none for log. `hf_cache_dir` (`src/init/download.rs:239`) intentionally bypasses `paths::cache_dir()` because it honors `HF_HOME` per HF convention; this is by design, not a regression, and the UAT sets `HF_HOME` separately in Unit 4.

**Verification:**
- New unit tests pass.
- `cargo clippy --all-targets --features test-fixtures -- -D warnings` clean.
- `cargo test --features test-fixtures` (full suite) passes with zero new failures vs. pre-plan baseline.

- [ ] **Unit 2: Add `--revision <sha>` flag to `llamadash init`, threaded through to `DownloadOptions`**

**Goal:** Expose HuggingFace commit-SHA pinning on the `init` surface (UAT's actual consumer) and plumb it through `init::download` to `hf-hub`'s `Repo::with_revision`. **No** `pull --revision` flag — that was originally planned but plan-review flagged it as speculative API surface with no real user; cut from scope.

**Requirements:** R6.

**Dependencies:** None.

**Files:**
- Modify: `src/cli/cli_args.rs` (`InitArgs` struct around line 345 — add `#[arg(long, value_name = "SHA")] pub revision: Option<String>`)
- Modify: `src/init/download.rs` — add `pub revision: Option<String>` to `DownloadOptions` (line 188 struct), use it at the `api.model(spec.repo_id.clone())` call site (line 313) to switch to `api.repo(Repo::with_revision(spec.repo_id.clone(), RepoType::Model, sha))` when `Some`.
- Modify: `src/init/wizard.rs` — thread the `revision` value from `InitArgs` into the `DownloadOptions` constructor on the `pick_model` paste-branch path (and any other branch that reaches `download_repo`).
- Modify: `src/cli/init.rs` (thin shim — passes the new field through).
- Test: `tests/init_revision_test.rs` (new integration test, follows `tests/cli_init_parse.rs` shape).
- Test: inline `#[cfg(test)] mod tests` additions to `src/cli/cli_args.rs` for arg parsing.

**Approach:**
- When `revision: None`, behavior matches today (HEAD of the default branch).
- When `revision: Some(sha)`, `download_repo` constructs the `ApiRepo` via `Repo::with_revision` — `hf-hub` 0.5 API.
- The result-struct `revision` field at `src/init/download.rs:112,366` already gets populated from the API response; no change to capture.
- The wizard's recommender branch can pass `None` for revision (recommender pick is HEAD-tracked); only the explicit `ModelOverride::Paste` branch (used by UAT) needs the revision plumbed.

**Patterns to follow:**
- Existing `InitArgs` flag boilerplate (`InitArgs.json`, `InitArgs.offline`).
- `tests/cli_init_parse.rs` for the parse-only test pattern.
- `tests/cli_integration_test.rs` for a real-pull integration test pattern (uses HTTP fixtures, not live HF).

**Test scenarios:**
- Happy path (parse): `llamadash init --recommended --revision abc1234` parses with `revision == Some("abc1234")`.
- Happy path (parse): `llamadash init --model qwen/qwen2.5-0.5b-instruct-GGUF --revision abc1234` parses with both fields set.
- Happy path (behavior): with `--revision` set on the Paste branch, `download_repo` constructs `ApiRepo` with the supplied revision; without it, falls back to default-branch HEAD.
- Edge case: `--revision ""` (empty) rejected at parse time via clap's value parser.
- Edge case: invalid SHA (`--revision not-a-real-sha`) surfaces a transport error from `hf-hub` with a clean error message and the existing `INIT_DOWNLOAD_FAILED` (73) exit code (already in `cli::exit_codes`).
- Integration: pulling a known-good repo with a known SHA returns a result whose `revision` field matches the input SHA.

**Verification:**
- `cargo test --features test-fixtures init_revision` passes.
- `cargo test --features test-fixtures init` continues to pass (no regression on the wizard's existing tests).
- `llamadash init --help` shows the new flag.
- `docs/usage.md` is NOT updated in this unit (UAT is the primary consumer; the flag is borderline-user-visible). If a future doc-sync review surfaces it as user-relevant, add then.

- [ ] **Unit 3: Cargo `uat` feature + hidden `llamadash uat` subcommand scaffold**

**Goal:** Add the `uat` Cargo feature, register the hidden subcommand variant, and wire a stub handler. Lets Unit 4 land without re-doing the dispatcher wiring.

**Requirements:** R4.

**Dependencies:** None.

**Files:**
- Modify: `Cargo.toml` (`[features]` block — add `uat = []`)
- Modify: `src/cli/cli_args.rs` (`Command` enum at line 105 — add `#[cfg(feature = "uat")] Uat(UatArgs)` variant + the `UatArgs` struct)
- Modify: `src/cli/mod.rs` (dispatcher at line 62 — add the `#[cfg(feature = "uat")] Some(Command::Uat(args)) => uat::handle(args, &cli, resolved_config).await` arm)
- Create: `src/cli/uat/mod.rs` — stub `pub async fn handle(...)` returning `Ok(CliExitCode::Success)` so the binary stays compilable. Submodules added in Unit 4; only the entrypoint exists in this unit.
- Test: inline `#[cfg(test)] mod tests` in `src/cli/cli_args.rs` — parse test gated on `#[cfg(feature = "uat")]`.

**Approach:**
- `UatArgs` flag surface: `--backend <name>` (clap `ValueEnum`: `nvidia | amd | apple_metal | vulkan` — `metal` is **not** an accepted alias; the canonical `GpuInfo` discriminant is the only spelling), `--mode {warm|cold}` (default `warm`), `--report-out <path>` (`Option<PathBuf>`). Consume the existing global `cli.quiet` flag (`src/cli/cli_args.rs:52`); do **not** declare a UAT-local `--quiet` — clap's debug-assert rejects duplicate long names even on disjoint subcommand scopes (same pattern as `--config-step` documented on `InitArgs::config_choice`).
- The subcommand variant uses both `#[cfg(feature = "uat")]` AND `#[command(hide = true)]`.

**Patterns to follow:**
- `Command::Pull(PullArgs)` and `src/cli/pull.rs` as the "thin shim" template.
- `Cargo.toml`'s existing `test-fixtures = []` feature for the exact `[features]` syntax.
- `cli_args.rs` value-enum patterns: `ConfigOverride` (`src/cli/cli_args.rs:441-446`, `#[derive(...ValueEnum)] #[clap(rename_all = "lower")]`). **Not** `InstallOverride`, which is a plain enum with a custom `parse_install_override` function.

**Test scenarios:**
- Parse (`#[cfg(feature = "uat")]`): `llamadash uat --backend nvidia` parses with defaults (`mode == Warm`, `report_out == None`).
- Parse: `llamadash --quiet uat --mode cold --report-out /tmp/r.json` parses; `--quiet` is the global flag.
- Parse: `--backend metal` rejected at parse time (clap value-enum).
- Build invariant: `cargo build` (no features) compiles without including the UAT module.

**Verification:**
- `cargo build --features uat` succeeds.
- `cargo build` (no features) succeeds and the resulting binary's `--help` does not list `uat`.
- `cargo run --features uat -- uat --help` shows the subcommand (only when explicitly invoked by name).
- `cargo clippy --all-targets --features uat -- -D warnings` clean.

### Phase 2 — UAT command (1 unit)

- [ ] **Unit 4: UAT lifecycle + isolation + report + reference model**

**Goal:** Implement the 5-step UAT lifecycle (doctor preflight → init → smoke chat → stop → doctor postrun) with cross-platform tempdir isolation, the structured JSON report, the reference model + fallback, and a fail-safe cleanup contract that survives Ctrl-C and panic. The bulk of the UAT command's real work; was previously split into U4+U5 — combined because U5 modified files U4 created and they ship together.

**Requirements:** R2, R5, R6.

**Dependencies:** Unit 1 (uses `LLAMADASH_STATE_DIR` / `LLAMADASH_CACHE_DIR`), Unit 2 (uses `init --revision`), Unit 3 (uses the subcommand scaffold).

**Files:**
- Create: `src/cli/uat/isolation.rs` (the `TempdirGuard` Drop-guard + env-var configuration for child processes — Linux sets `XDG_STATE_HOME` + `XDG_CACHE_HOME` + `XDG_RUNTIME_DIR`; macOS additionally sets `LLAMADASH_STATE_DIR` + `LLAMADASH_CACHE_DIR` + `LLAMADASH_SOCKET`. **All platforms** set `HF_HOME=<tempdir>/hf` to isolate the GGUF cache.)
- Create: `src/cli/uat/lifecycle.rs` (the 5-step orchestrator + per-step `Verdict` capture + signal-handling integration via `tokio::signal::ctrl_c`)
- Create: `src/cli/uat/report.rs` (JSON shape mirroring `GpuInfo`'s serde tagged-union; `host` block carries `model_used: String` and `warnings: Vec<String>` for non-fatal anomalies like fallback-substitution and preserved-tempdir path)
- Create: `src/cli/uat/model.rs` (constants for primary + fallback model identity — repo, filename, commit SHA, expected size)
- Modify: `src/cli/uat/mod.rs` (replace stub `handle()` with real orchestration; wraps the body in `tokio::select!` against `ctrl_c` and propagates the cancellation through the `TempdirGuard` Drop)
- Test: `tests/uat_lifecycle_test.rs` (new integration test, follows `tests/init_orchestration.rs` shape — exercises the lifecycle against `fake_llama_server` so it runs in CI without real hardware)

**Execution note:** Start by writing the integration test against `fake_llama_server` — it gives the orchestrator a concrete contract and prevents drift from the documented 5-step shape during iteration.

**Approach:**
- **Isolation:** the `TempdirGuard` struct holds the tempdir's `PathBuf` + a `preserve: AtomicBool` initialized to `true`. Its `Drop` impl: (1) explicitly stop child `llama-server` (SIGTERM with a short grace period, then SIGKILL); (2) if `preserve == false`, remove the tempdir; otherwise leave it in place and emit one line to stderr with the preserved path. The orchestrator calls `guard.release_on_success()` (flips `preserve` to `false`) only after the entire lifecycle returns `Ok(Verdict::Pass)`.
- **Panic safety:** wrap the lifecycle body in `std::panic::catch_unwind` (or `tokio::task::spawn_blocking` for async safety) so a panic doesn't bypass the Drop. A panic results in `verdict: "fail"`, `failure_summary.message: "orchestrator panicked at <location>"`, and tempdir preserved.
- **SIGINT:** `tokio::select!` between the lifecycle and `tokio::signal::ctrl_c()`. On SIGINT, write a partial report to the configured `--report-out` (so the maintainer has something to read), set `verdict: "interrupted"`, mark in-flight step as `interrupted`, then return — Drop runs, child is killed, tempdir preserved.
- **Lifecycle:** orchestrator spawns child processes (`init --model <repo>/<file> --revision <sha>`, `start`, smoke-chat via `reqwest`, `stop`, `doctor`) with isolation env vars set via `Command::env()`. Smoke chat hits the OpenAI-compatible endpoint directly.
- **Report:** both stdout (TTY-pretty) and file output (`--report-out`) are produced by default; `--quiet` (global) suppresses stdout; `--report-out -` redirects JSON to stdout (mutually exclusive with `--quiet`).
- **`backend.detected`:** serialize an actual `GpuInfo` value verbatim via `serde_json::to_value(&gpu_info)`.
- **`backend.expected`:** pass through the CLI's `--backend` value as-is (no normalization needed since Unit 3 restricts the CLI to canonical discriminants).
- **Failure short-circuits:** if step N fails, steps N+1..5 marked `skipped`; `verdict: "fail"`; `failure_summary` populated with the failing child's exit code as-is (e.g., `73` if init's download failed).
- **Reference model selection:** UAT calls `init --model <PRIMARY.repo>/<PRIMARY.filename> --revision <PRIMARY.revision>` to bypass the recommender (the recommender's hardware-based pick may differ from UAT's pinned model). On any init failure during the model-pull phase, retry with `FALLBACK` constants; record `model_used: "<actual-repo>"` and append a `warnings` entry. If both fail, exit 1 with `failure_summary.step == "init"` and `failure_summary.message` listing both attempts.
- **Cold vs warm mode:** in warm mode, the UAT passes `--skip install` to `init`; in cold mode, the UAT omits `--skip install` so init exercises the full brew/GH-Releases install path.

**Patterns to follow:**
- `tests/init_orchestration.rs` and `tests/cli_integration_test.rs` for the spawn-llamadash-as-subprocess + assert-on-JSON pattern.
- `src/daemon/supervisor.rs` for the `Command::env()` spawning pattern (UAT does NOT use `setsid` — children are explicitly killed by `TempdirGuard::Drop`).
- `serde_json::to_value()` + `serde_json::Value` manipulation as used in `src/cli/output.rs::status_json`.
- `tokio::select!` with `tokio::signal::ctrl_c` — search the codebase for prior usage; if absent, this becomes the first reference.

**Test scenarios:**
- Happy path (fixture-backed warm): UAT against `fake_llama_server` returns `verdict: "pass"`, all 5 steps `pass`, JSON parses cleanly with all top-level fields (`schema_version`, `started_at`, `duration_secs`, `host`, `backend`, `steps`, `verdict`, `failure_summary: null`).
- Happy path: `--report-out /tmp/r.json` writes the JSON file AND emits the TTY summary; file contents match stdout structurally.
- Happy path: `--report-out -` redirects JSON to stdout; exit 0.
- Edge case: global `--quiet` suppresses TTY output; combined with `--report-out -` is rejected at parse time as mutually exclusive.
- Edge case: isolation tempdir created, populated, removed on success.
- Edge case: isolation tempdir preserved on failure (for debugging); `host.warnings` includes "preserved tempdir at <path>".
- Edge case (macOS, gated on `cfg(target_os = "macos")`): isolation sets `LLAMADASH_STATE_DIR`, `LLAMADASH_CACHE_DIR`, `LLAMADASH_SOCKET`, `HF_HOME`. Child processes see them.
- Edge case (Linux): isolation sets `XDG_STATE_HOME` + `XDG_CACHE_HOME` + `XDG_RUNTIME_DIR` + `HF_HOME`.
- Error path: `init` step fails (failure-injection env var that `fake_llama_server` recognizes). Subsequent steps marked `skipped`; `failure_summary.step == "init"`; `failure_summary.exit_code == 72` or `73` (init's own exit codes); top-level UAT exit code `1`; tempdir preserved.
- Error path: smoke chat returns HTTP 500. `stop` and `doctor_postrun` still run (cleanup); verdict `fail`; tempdir preserved.
- Error path (cleanup): SIGINT mid-lifecycle. Test sends SIGINT to the UAT process during step 2; assert (a) UAT exits within 5s, (b) `--report-out` file exists and `verdict == "interrupted"`, (c) tempdir preserved, (d) no orphan `fake_llama_server` PID remains (test parent reaps).
- Error path (cleanup): orchestrator panic. Test triggers a panic via a debug-only env var; assert (a) UAT exits with code 1, (b) report shows `failure_summary.message` mentioning "orchestrator panicked", (c) tempdir preserved.
- Error path (cleanup): child outlives parent attempt. Test forces orchestrator to exit while `fake_llama_server` is still running; assert `TempdirGuard::Drop` killed the child (verifiable by tracking the spawned PID).
- Fallback: primary fetch fails (mocked) → fallback succeeds → report `model_used: "<fallback-repo>"`; `host.warnings` includes "primary model fetch failed: <reason>; used fallback".
- Fallback: both fetches fail → exit 1; `failure_summary.message` lists both attempts.
- Integration: real HF fetch with the pinned SHA returns a file whose downloaded revision matches the input SHA.

**Verification:**
- `cargo test --features 'uat test-fixtures' uat_lifecycle` passes on both Linux and macOS.
- One real end-to-end dry run on the maintainer's local hardware in warm mode succeeds. (Cold mode is exercised at release time — not as a unit-acceptance gate, per the deferred-implementation note.)
- `cargo test --features test-fixtures` (full suite, no `uat`) passes — confirms Unit 1 + 2 + 3 didn't regress the existing suite.

### Phase 3 — Docs + PR infrastructure (2 units)

- [ ] **Unit 5: Hardware UAT setup + run docs** *(follows Phase 2)*

**Goal:** Write `docs/testing/hardware-uat.md` so a contributor or the maintainer can set up a fresh box and run the UAT without asking for help.

**Requirements:** R2, R9.

**Dependencies:** Unit 4.

**Files:**
- Create: `docs/testing/hardware-uat.md`

**Approach:**
Content sections:
1. **Per-backend one-time setup** — vendor toolkits (CUDA driver, ROCm runtime, Xcode CLT, Vulkan loader), `llama-server` install, HF cache pre-population. One subsection per backend.
2. **Running the UAT** — invocation examples for warm and cold mode; what each step does; what the JSON report looks like; how to interpret a fail; the `--report-out` / TTY contract.
3. **The degraded-gate policy** — max 14-day report age, what to do when a box is unavailable, cold-mode cadence requirement (≥ 1× per minor release).
4. **Attaching the report to a release PR** — the release PR is opened via `gh pr create --template release.md`; the UAT checklist is filled in; JSON reports attached; `uat-caught` label applied if a UAT failure surfaced a real regression.
5. **Per-backend budget calibration** — placeholder for measured p95 numbers, filled in after the first 4 dry runs.
6. **`uat-caught` label recreation** — the `gh label create` command (in case the label is ever deleted).

**Patterns to follow:**
- `docs/architecture.md` and `docs/usage.md` for the project's prose voice.
- `docs/spikes/2026-05-19-*.md` for the data-first / observed-numbers style.

**Test scenarios:** None — docs.

**Verification:**
- Docs render correctly on GitHub.
- Maintainer reads cold and runs the UAT on one backend without questions.

- [ ] **Unit 6: Release-PR template + `uat-caught` label** *(fully independent — may ship at any time)*

**Goal:** Wire the compliance + value metric tracking infrastructure: multi-template PR form so release PRs carry the UAT checklist while regular PRs aren't polluted, plus the `uat-caught` label registered for outcome-metric evaluation.

**Requirements:** R7.

**Dependencies:** None.

**Files:**
- Create: `.github/PULL_REQUEST_TEMPLATE/release.md` — release-only template with the UAT backends-checked checklist.
- Create: `.github/PULL_REQUEST_TEMPLATE/default.md` — minimal "## Summary / ## Test plan" for all other PRs.

**Side-effects (run once on the repo):**
- `gh label create uat-caught --color B60205 --description "Release PR where UAT caught a regression that would otherwise have shipped"`.

**Approach:**
- `release.md` content:
  ```
  ## Release summary
  Version: vX.Y.Z

  ### UAT
  - [ ] NVIDIA CUDA (warm)
  - [ ] AMD ROCm (warm)
  - [ ] Apple Silicon Metal (warm)
  - [ ] Vulkan fallback (warm)
  - [ ] ≥ 1 backend run in cold mode this cycle (state which)

  Backends not covered this release (with reason): _none / list_
  Attach JSON reports as files or paste verbatim below.
  ```
- `default.md` content: minimal summary + test plan; no UAT section.
- The multi-template form (rather than a single `pull_request_template.md`) avoids the "UAT checkboxes on every PR → trained ignorance" failure mode flagged by plan-review.
- Release PRs are opened via `gh pr create --template release.md` (documented in Unit 5).

**Patterns to follow:**
- GitHub's `.github/PULL_REQUEST_TEMPLATE/` directory convention.
- `CONTRIBUTING.md`'s prose voice.

**Test scenarios:** None — config.

**Verification:**
- Opening a fresh PR with `gh pr create --template release.md` renders the release template.
- Opening a fresh PR with no template flag (or `--template default.md`) renders the minimal template — no UAT section.
- `gh label list | grep uat-caught` confirms the label exists.

### Phase 4 — Tier 3 nightly + final wrap-up (1 unit)

- [ ] **Unit 7: Tier 3 spike + nightly Metal workflow + TODO/AGENTS/CHANGELOG/release-audit wrap-up + outcome-review reminder**

**Goal:** The single PR that proves Metal works in CI, lands the nightly workflow, and closes out the plan. Was previously U8 + U9 + U10 — combined because (a) U8's spike workflow is explicitly deleted in U9's PR per the throwaway pattern, (b) U10's wrap-up is 5 lines across 4 files that belong with the feature-completion PR.

**Requirements:** R1 (final cross-cut check), R3, R8, R10.

**Dependencies:** Unit 4 (UAT command must work locally first), Unit 6 (`uat-caught` label referenced by docs).

**Files:**
- Create: `.github/workflows/uat-metal-spike.yml` — throwaway workflow run via `workflow_dispatch`; deleted in the same PR before merge.
- Create: `.github/workflows/uat-metal-nightly.yml` — the real nightly lane (lands only if the spike passed).
- Modify: `TODO.md` (strike R34 line; add deferred follow-ups: `Hardware UAT report` issue template, cloud-runner re-evaluation gated on user-base trigger).
- Modify: `.github/workflows/release.yml` (one comment line confirming absence of `--features uat` is the contract).
- Modify: `AGENTS.md` (one-paragraph note under "Build, test, lint" mentioning the new `uat` feature, dev-only status, pointer to `docs/testing/hardware-uat.md`).
- Modify: `CHANGELOG.md` (one-line entry under `[Unreleased]`: "Internal: maintainer UAT command + nightly Metal CI lane for real-hardware coverage (dev-only, behind `--features uat`)").

**Side-effects (run once on the repo, as part of this PR):**
- `gh issue create --title "[outcome-review] UAT pre-release gate: keep, reshape, or retire?" --label outcome-review --body "Due: 2026-11-19. Review criteria per docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md §Outcome metric: count of release PRs with uat-caught label AND not also fixture-reachable. <50% of release PRs with ≥2 backends checked = compliance has decayed, reshape first. Surface this in normal triage; do not let the date slip silently."` — the date-anchored issue is the calendar mechanism that prevents the 6-month review from being a TODO that never fires.

**Approach (Spike phase, before merging):**
- `uat-metal-spike.yml`: `runs-on: macos-14`, trigger `workflow_dispatch:` only. Steps: checkout → install Rust → `brew install llama.cpp` → `cargo run --features uat -- uat --backend apple_metal --mode warm --report-out spike.json` → upload `spike.json` artifact.
- Maintainer triggers via `gh workflow run uat-metal-spike.yml`. Outcome captured verbatim in the PR description.
- **Spike-pass branch:** keep `uat-metal-nightly.yml` as drafted below; delete `uat-metal-spike.yml` from the PR.
- **Spike-fail branch (pre-authored fallback so reshape doesn't happen under PR-review pressure):** replace `uat-metal-nightly.yml` with a build-only `macos-build-nightly.yml` that runs `cargo build --release --target aarch64-apple-darwin` nightly and uploads the binary as an artifact. Update R3 in this PR's description from "Add a nightly Metal lane" to "Add a nightly macOS build lane (Metal lane deferred — see spike artifact)". Either way, delete `uat-metal-spike.yml`.

**Approach (Nightly workflow, spike-pass branch):**
- Single job `uat-metal-nightly`, `runs-on: macos-14`, `timeout-minutes: 30` (passive ceiling, not a tracked failure).
- Trigger: `schedule:` cron only (`'0 9 * * *'` — 09:00 UTC). Scheduled workflows always run against the default branch, structurally enforcing R3's "runs on main".
- Concurrency: `concurrency: { group: uat-metal-nightly, cancel-in-progress: true }`.
- Permissions: `permissions: { contents: read, issues: write }`.
- Caching: `actions/cache@v4` keyed on a hash of `Cargo.lock` + the reference-model SHA, scoped to `~/.cache/huggingface/hub`, `~/Library/Caches/Homebrew/downloads`, `target/`.
- Setup steps: checkout → install Rust → restore cache → `brew list llama.cpp >/dev/null 2>&1 || brew install llama.cpp` (the `brew list` short-circuit is what skips reinstall on a warm Cellar; the cache only avoids redownload).
- UAT invocation: `cargo run --features uat -- uat --backend apple_metal --mode warm --report-out uat-metal.json`. Upload `uat-metal.json` as artifact.
- Failure routing via shell-out (`gh` CLI): look up open issue with `label:uat-metal-status`; if none, `gh issue create --label uat-metal-status --title "UAT Metal nightly status"`. On failure: `gh issue comment <num> --body "Run <url> failed at <step>. Verdict: <classification>. Report: <artifact-url>."`. On success when issue is open: `gh issue close <num> --comment "Restored to green on run <url>."`.
- Job summary: render the UAT JSON's verdict + failed step into the GH Actions step summary.

**Approach (Wrap-up):**
- `TODO.md` strike pattern: wrap the R34 line in `~~...~~` per existing convention.
- Add new TODOs:
  - `Hardware UAT report` issue template — deferred until first contributor wants to file.
  - Cloud-runner re-evaluation — gated on user-base trigger (>500 installs + 3 RC cycles silence).
  - Outcome-metric review — tracked by the `outcome-review` labeled issue created in this PR's side-effects.

**Patterns to follow:**
- `.github/workflows/regenerate-benchmark-snapshot.yml` for the cron + gh-CLI-from-workflow pattern.
- `actions/cache@v4` canonical usage.
- `.github/workflows/release.yml`'s `actions/upload-artifact@v4` usage.
- `CHANGELOG.md`'s existing entry voice.

**Test scenarios:** None for the YAML — verification is observational.

**Verification:**
- Spike phase: `gh workflow run uat-metal-spike.yml` completes; outcome captured in PR description.
- Spike-pass: first scheduled nightly run completes within 30 min and posts the UAT report as artifact.
- Forced-failure test (before merge): on the PR branch, temporarily break the UAT (e.g., set `--backend nvidia` to force a pre-flight mismatch) and run via `workflow_dispatch` to verify the rolling-issue creation + commenting path works end-to-end. Restore before merging.
- Auto-close test: after the forced failure, run a clean nightly and confirm the issue closes with the recovery comment.
- Final cross-cut (R1): `cargo test --features test-fixtures` (full suite) passes byte-for-byte with the pre-plan baseline. No new failures introduced anywhere across U1-U7.
- `grep "R34" TODO.md` returns the struck line, not an active checkbox.
- `grep -F "features uat" .github/workflows/release.yml` returns the audit comment line.
- `gh issue list --label outcome-review` returns the date-anchored review issue.

## System-Wide Impact

- **Interaction graph:** The UAT command spawns child `llamadash` processes (init, start, stop, doctor) and a separate `llama-server` process. Unit 1's two new env vars (`LLAMADASH_STATE_DIR`, `LLAMADASH_CACHE_DIR`) affect every code path that calls `paths::state_dir()` or `paths::cache_dir()` — verify via grep during U1 execution that no other code computes these paths independently (the audit step in U1's Test scenarios).
- **Path-isolation surface area (honest accounting):** UAT must set FOUR env vars (`LLAMADASH_STATE_DIR`, `LLAMADASH_CACHE_DIR`, `LLAMADASH_SOCKET`, `HF_HOME`) plus Linux XDG equivalents. The brainstorm's "one new env var" framing was incomplete; plan-review surfaced the gap. `hf_cache_dir` (`src/init/download.rs:239`) intentionally honors `HF_HOME` independent of `paths::cache_dir()` — that's by HF convention, not a bug, and UAT sets `HF_HOME` separately.
- **Error propagation:** UAT child-process failures surface their exit codes via `failure_summary.exit_code`, not remapped to UAT's own 0/1. The 0/1 contract is for the UAT process; the report carries the richer signal.
- **State lifecycle risks:** The `TempdirGuard` Drop pattern is load-bearing: tempdir preserved on any non-success path (failure, panic, SIGINT); child `llama-server` killed before tempdir teardown; preserved tempdir path emitted to stderr so the maintainer knows where to look. Without the guard, a panic mid-lifecycle would either leak the tempdir or destroy diagnostic state.
- **API surface parity:** `--revision` flag added to `init` only. Not added to `pull` (speculative surface, no consumer). If a future use case justifies it, add then.
- **Integration coverage:** The Tier 3 nightly is the first time real-`llama-server` + real-Metal is exercised in CI. Fixture-based tests (Tier 1) cannot prove the GPU codepaths. The fixture suite is unchanged in shape per R1.
- **Unchanged invariants:** `docs/usage.md`, `docs/architecture.md`, `README.md` — explicitly NOT updated (UAT is dev-only). Release binary's `--help` unchanged. Existing `cli::exit_codes` constants untouched (no new codes added).

## Risks & Dependencies

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| GH Actions `macos-14` doesn't expose Metal to a headless process | Med | High (kills Tier 3) | Pre-merge spike (Unit 7 spike-phase). Pre-authored spike-fail branch: macOS build-only lane replaces the nightly; R3 restated honestly. |
| `TempdirGuard` Drop semantics fail under some edge (e.g., process killed via SIGKILL) | Low | Med (lost diagnostics) | Drop guards work for everything except SIGKILL by design; documented in U4 approach. For SIGKILL, the tempdir is naturally preserved (the OS doesn't clean it up — that's actually fine for forensics, even though no kill-handler ran). Trade-off accepted. |
| Path-isolation env vars insufficient (some `paths::*` call site doesn't go through `state_dir()` / `cache_dir()`) | Low | High (UAT contaminates real cache) | U1 audit step explicitly greps for bypass paths. `hf_cache_dir` is the only documented bypass and `HF_HOME` handles it. New audit lands in U1's PR description. |
| Reference GGUF disappears from HuggingFace | Low | Med (delays UAT runs) | Commit-SHA pin + fallback model (Unit 4). If both fail, maintainer stages manually and re-runs. Acceptable per origin's degraded-gate policy. |
| Maintainer skips UAT runs → gate becomes ceremony | Med | Med | Compliance metric (release-PR checklist via `release.md`) + value metric (`uat-caught` label) + 6-month outcome-metric review *anchored by a date-tagged GH issue created in U7* so the review surfaces in normal triage. Honor-system tradeoff acknowledged. |
| Fallback-on-any-fetch-error masks transient HF blips → maintainer thinks they tested primary | Low | Med | `host.warnings` surfaces the substitution in the TTY summary. Treated as v1 acceptable; tighten fallback discrimination (transient vs. durable) if a real release post-mortem traces a missed regression to silent fallback. |
| Cold-mode never actually exercised before a release attempt | Med | Med | Tier 3's nightly Metal lane implicitly exercises the brew install path (untimed setup step). For other backends, the first real cold run is at release time; this is accepted v1 risk. Add a cold-mode dry-run gate to U4 only if release-day breakage actually occurs. |
| Rolling-issue churn during interleaved failure modes (brew flake vs. real Metal regression) | Low | Low | Per-failure *comments* (not issues), with failure classification (`pre-flight assertion | brew install | model load | smoke chat | other`) so signal stays attributable. |

## Documentation Plan

- **Per-change docs sync:** Each unit's PR updates `CHANGELOG.md` under `[Unreleased]` only if user-visible. Most are dev-only; only the new `init --revision` flag is borderline-user-visible and CHANGELOG-worthy (U2).
- **`AGENTS.md`** updated in Unit 7 to note the new dev-only surface.
- **`docs/testing/hardware-uat.md`** is the new authoritative doc (Unit 5).
- **`README.md`, `docs/usage.md`, `docs/architecture.md`** — explicitly NOT updated.

## Sources & References

- **Origin document:** [`docs/brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md`](../brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md)
- **Project conventions:** [`AGENTS.md`](../../AGENTS.md), [`CONTRIBUTING.md`](../../CONTRIBUTING.md)
- **Related code:**
  - `src/util/paths.rs` (state-dir / cache-dir / log-dir / socket-path resolution; env-var override pattern at lines 73-79)
  - `src/cli/cli_args.rs` (`Command` enum at line 105, `PullArgs` at 321, `InitArgs` at 345, `ConfigOverride` at 441-446 as the ValueEnum template)
  - `src/init/download.rs` (line 112 revision tracking, line 188 `DownloadOptions`, line 313 `api.model(...)` call site to update, line 366 capture)
  - `src/init/wizard.rs` (`pick_model` Paste-branch where UAT's `--revision` flows)
  - `src/gpu/mod.rs` (GpuInfo tagged-union with `tag = "backend"`)
  - `src/daemon/supervisor.rs` (Command::env() spawning model)
  - `Cargo.toml` (`[features] test-fixtures = []` as the mirror pattern)
- **Existing test patterns:**
  - `tests/init_orchestration.rs` (spawn-llamadash-subprocess test pattern)
  - `tests/cli_init_parse.rs` (clap arg-parse-only tests)
  - `tests/fixtures/fake_llama_server.rs` (HTTP fixture for OpenAI-compatible endpoints)
- **Existing CI workflows:**
  - `.github/workflows/ci.yml` (matrix + Swatinem/rust-cache pattern)
  - `.github/workflows/regenerate-benchmark-snapshot.yml` (cron + gh-CLI-from-workflow pattern)
  - `.github/workflows/release.yml` (build invocation that must not gain `--features uat`)
- **External:** `hf-hub` 0.5 docs for `Repo::with_revision` API; `actions/cache@v4` for Tier 3 caching.
