# TODO

Single index of outstanding work across plans, docs, and code. When you add a
TODO anywhere in the repo (a `TODO(...)` comment, an unchecked `- [ ]` in a
plan, a `todo:` frontmatter field on a spike), also add a one-line entry here
with a link back to the source. When you complete one, strike it from both
places.

## In-code TODOs

_None — the four vendoring items shipped 2026-05-19 via [`docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md`](docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md). The Open LLM Leaderboard + Aider polyglot adapters now run live against upstream in the daily CI cron at the pinned whichllm commit `73cd92f`; both `TODO(unit7-v2-ga)` placeholders in `scripts/regenerate-benchmark-snapshot.py` are gone._

## v2-GA blockers (must clear before v2 GA, not v2 launch)

- [x] **In progress**: ~~Remeasure per-backend VRAM overhead band on real CUDA / HIP / Vulkan / Metal hardware — [`docs/spikes/2026-05-19-vram-overhead-band.md`](docs/spikes/2026-05-19-vram-overhead-band.md) `todo:` frontmatter.~~ Harness ready: [`scripts/measure-overhead-band.sh`](scripts/measure-overhead-band.sh) + runbook at [`docs/runbooks/measure-vram-overhead-band.md`](docs/runbooks/measure-vram-overhead-band.md). Changing catalog of defaults shipped via [`docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md`](docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md).
- [ ] **Deferred (post-c80d638)**: Port whichllm's family-selection / lineage-demotion / generation-bonus logic so `init --only models --json` output matches `whichllm --json --top 10` byte-for-byte. Today 7/10 picks and 3/10 quants match — see [Post-plan refinements §Remaining gap](docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md#remaining-gap-deliberately-not-closed) in plan 2026-05-20-001.
- [ ] **Deferred (post-c80d638)**: Download-flow fallback for synthetic GGUF rows — when the catalog row's `gguf_publisher == "synthetic"` (official-org safetensors-only repo), try trusted converters (`bartowski/{name}-GGUF`, `unsloth/{name}-GGUF`, `lmstudio-community/{name}-GGUF`) before failing the download. Without this, `init --recommended` on a synthesized pick (e.g. Qwen3.6-27B) errors when the official repo doesn't ship GGUFs.

## v1+ release blockers

- [ ] Download fails for many models in `init --only models` (for example -> init download: HF tree listing for `Qwen/Qwen3-Next-80B-A3B-Instruct` returned zero matching files)
- [x] Remap Shift+Q to Ctrl+Q for killing deamon.
- [x] Remove Ctrl+R, Ctrl+Q from top bar hints.
- [x] **In progress**: init should show progress and text descriptions of what its doing (like installing llama.cpp via brew, Installed llama.cpp, downloading models, download complete, etc.) instead of just a blinking line.
- [x] **In progress**: Init install method doesnt offer custom path as option.
- [x] ~~Better/colorful/formatted CLI output for commands (daemon, list, status, presets, doctor etc).~~ Shipped via [`docs/plans/2026-05-20-002-feat-colorful-cli-output-plan.md`](docs/plans/2026-05-20-002-feat-colorful-cli-output-plan.md).
- [ ] **In progress**: Built in architecture defaults for all popular architectures, a default for all others. Advanced modal - replace free-text editor with typed key/value fields like settings; Its should be populated with architecture defaults for the model. keys = advanced options for the model, values = last settings or architecture default; pre-populate from the model's last params or architecture defaults and let users edit before launch. Requires a refactor of the advanced modal to support dynamic fields. Consider showing this inline in settings pane instead of a modal dialog, unless you think thats not good idea. Also provide a free text fields where user can enter arbitrary extra params that we won't show in the UI, for power users who want to use features we don't yet support in the UI.
- [x] ~~HuggingFace pull TUI dialog with search / sort / pagination (origin: R46, [`docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md)).~~ Shipped 2026-05-20 via [`docs/plans/2026-05-20-002-feat-hf-pull-tui-dialog-plan.md`](docs/plans/2026-05-20-002-feat-hf-pull-tui-dialog-plan.md) — `d` opens the Search / File picker / Confirm modal; downloads stream into the pinned status strip. 2026-05-21 follow-ups (multiple rounds): lowercase `d` binding (`Ctrl+D` now deletes; `Ctrl+X` cancels active download), `sort=trending` → `sort=trendingScore` root-cause fix (HF Hub renamed the value; legacy token returned HTTP 400), fit-glyph legend in picker, page indicator, byte-accurate progress, Esc walk-back contract, filter `Enter:launch` drills into focused row, edit-state-aware hint chips across all `InputField` surfaces, delete chip gated to idle rows (`NotLaunched` / `Stopped`), `delete_model_on_disk` constrained to the HF cache tree, CLI `--offline` threaded into dialog fetch, `InputField` modifier handling tightened, `→` removed from Models list.
  - [ ] **Deferred**: Models downloaded from HF can show ambiguously-named files (`model.gguf`, `ggml-model-q4_k_m.gguf`) when the publisher didn't name them descriptively. The "friendly display name" slice (`<repo> (<quant>)`) was attempted in the early branch and reverted in `2e11d65` after the first review — real catalogs use descriptive GGUF filenames so `file_stem()` is enough today. Revisit if real catalogs start hitting the ambiguity in practice.
  - [x] ~~**Migrate remaining text inputs to the modal `InputField` component** (chat composer, embed input, rerank query + candidate, advanced-panel free-text extras). Filter + HF search adopted the component 2026-05-21; the rest still use the raw `String + push/pop` path and won't honour the uniform `e:edit / Esc:stop / 2nd-Esc:clear` contract until migrated.~~ Shipped 2026-05-21 — every text input in the TUI now routes through `InputField`. See `src/tui/input_field.rs` for the contract.
  - [x] ~~**De-hardcode keybind labels app-wide.** App supports `keybindings:` config overrides via `Action::from_config_name`, but several hint strips / footers / docs still embed literal `Esc:close`, `Enter:open`, etc.~~ Shipped 2026-05-21 — HF dialog footer + header descriptions now resolve `Submit` / `Cancel` / `MoveUp` / `MoveDown` from the live keymap via a new `dialog_label` helper. Stage-internal chords (`o`, `n`, `p`, `e`) stay literal because they're component-internal, not actions. Other render surfaces (right pane bottom hints, models title strip, help bar, confirm overlay) were already binding-driven.
- [x] ~~if `--llama-server` is passed, add it as fallback in config file and use it when llama-server is not on path.~~ Shipped 2026-05-20 — `cli::dispatch` writes the resolved path to `config.yaml`'s `llama_server_path` key whenever the flag differs from the configured value (best-effort).
- [x] ~~best-model (find nicer alias) command. reuse `init --models` and just download the best model for current setup/hardware~~ Shipped 2026-05-20 — `llamastash recommend` wraps `init --only models --recommended` with the same `--json` / `--offline` / `--model` / `--revision` surface.
- [x] ~~`R:restart` daemon hotkey.~~ Shipped 2026-05-20 — `R` (Shift+r) triggers a confirmation popup, then the TUI's writer task issues `shutdown` and `start_detached`s a fresh daemon with the same `DaemonOptions` the parent CLI resolved.
- [ ] **Need brainstorm/plan**: Proxy router that maps a single endpoint to running models by model name. If the model isn't running, start it; if launch fails, fall back to a running model when one is available; otherwise error. Keep it OpenCode / π compatible so agents and tools can hit one URL.
- [ ] **Need brainstorm/plan**: Benchmark against ollama, LMStudio and other popular options.
- [x] ~~**Need brainstorm/plan**: Test strategy for Nvidia / AMD / Apple GPU support (origin: R34).~~ Shipped 2026-05-20 via [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md).
- [ ] `Hardware UAT report` GitHub issue template — deferred until first contributor wants to file one (origin §Acceptance checklist). Recreate the `uat-caught` label if it's ever deleted: `gh label create uat-caught --color B60205 --description "Release PR where UAT caught a regression that would otherwise have shipped"`.
- [ ] Cloud-runner re-evaluation — gated on user-base trigger (>500 installs + 3 RC cycles silence) per [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md) §Companion trigger.
- [ ] Lock in reference-model commit SHAs in `src/cli/uat/model.rs` — both `PRIMARY` and `FALLBACK` ship a `<TBD-locked-on-first-dry-run>` sentinel that the orchestrator surfaces as a `host.warnings` entry. First warm-mode dry-run on the maintainer's box lands the lock-in commit. Procedure: [`docs/runbooks/verify-uat-reintroduction.md`](docs/runbooks/verify-uat-reintroduction.md) §8b.
- [x] ~~Run the falsifying `uat-metal-spike.yml` workflow on macos-14, capture the outcome in the merge PR description, then `git rm .github/workflows/uat-metal-spike.yml` (and swap in `.github/workflows-fallback/macos-build-nightly.yml` if the spike proved Metal isn't exposed). Pre-merge step from [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md) §Approach. Procedure: [`docs/runbooks/verify-uat-reintroduction.md`](docs/runbooks/verify-uat-reintroduction.md) §8a.~~ Spike ran 2026-05-20 — [run 26181924383](https://github.com/llamastash/llamastash/actions/runs/26181924383) detected `cpu_only` despite `--backend apple_metal`, falsifying Metal exposure on headless `macos-14`. Tier 3 collapsed to `macos-build-nightly.yml` (cross-compile `aarch64-apple-darwin`); both Metal workflows removed. R3 in the plan restated.
- [ ] Skills.
- [ ] Readme and other docs sync.
- [ ] Audit (binary size, dependencies, test coverage, security, etc.).
- [ ] Release setup validation (website/CI/CD etc)
- [ ] Add llamastash to cli.rs https://github.com/zackify/cli.rs/pull/1/changes — Unit 7 cutover step, post-org-bootstrap.
- [ ] Write `docs/runbooks/secret-rotation.md` — operational steps for rotating `CRATES_IO_TOKEN` + `GH_BUMP_TOKEN`. Referenced from [`docs/runbooks/release-0.0.1-bootstrap.md`](docs/runbooks/release-0.0.1-bootstrap.md) §"Token rotation cadence".
- [ ] **Need brainstorm/plan**: Release blog.
- [ ] **Need brainstorm/research/plan**:Social promotion — research an approach for max reach.
- [x] ~~Release setup: website, brew tap, etc. (KDash-style).~~ shipped via [`docs/plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md`](docs/plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md) + [`docs/runbooks/release-0.0.1-bootstrap.md`](docs/runbooks/release-0.0.1-bootstrap.md). Org-admin bootstrap (creating repos, secrets, Pages) still pending — see runbook.
- [x] ~~Release pipeline like Kdash~~ shipped in `.github/workflows/release.yml` (kdash cd.yml lineage).

## v2+ roadmap

- [ ] **Need brainstorm/plan**: Migrate release pipeline secrets from PATs to a scoped GitHub App with OIDC. Eliminates `GH_BUMP_TOKEN` rotation and shrinks token blast radius. Deferred from 0.0.1 per the release-setup plan §"Token rotation surface".
- [ ] **Need brainstorm/plan**: Plan to prevent llama.cpp version drift/incompatibility issues. Should we bundle/fix version.
- [ ] **Need brainstorm/plan**: Windows support.
- [ ] **Need brainstorm/plan**: HTTP and MCP surfaces (origin: R34).
- [ ] **Need brainstorm/plan**: Anthropic API compatibility.
- [ ] **Need brainstorm/plan**: MLX and vLLM if cheap to add.
- [ ] **Need brainstorm/plan**: Docker-ready packaging.
- [ ] **Need brainstorm/plan**: Per-PID VRAM attribution via NVML's `nvmlDeviceGetComputeRunningProcesses` (Linux + Windows; AMD / Apple parity depends on upstream surface). Check ROCm and Metal for equivalents. Today the right-pane block title surfaces per-model RAM + CPU%; per-model VRAM is reported only at the host level.

## Active workstreams (unchecked plan units)

### ~~kdash-style dashboard UI — [`docs/plans/2026-05-16-001-feat-kdash-style-dashboard-ui-plan.md`](docs/plans/2026-05-16-001-feat-kdash-style-dashboard-ui-plan.md)~~

All 7 units shipped — verified 2026-05-19 against the tree:
`src/daemon/host_metrics.rs`, `GpuDevice.utilization_pct/temperature_c` in
`src/gpu/*`, `src/tui/{host_stats_pane,info_pane,logo_pane}.rs`,
`COMPACT_BANNER` in `src/banner.rs`, accent title bar in
`src/tui/render.rs`, and `latest_rss_bytes` / `latest_cpu_pct` plumbed
through `src/daemon/supervisor.rs` → `src/ipc/methods.rs` → `src/tui/app.rs`.

### ~~vendor benchmark scrapers — [`docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md`](docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md)~~

All 4 units shipped — verified 2026-05-19 against the tree:
`scripts/benchmark_sources/whichllm.py` (Unit 1, vendored at upstream
`73cd92f`); `scripts/benchmark_sources/open_llm_leaderboard.py`
(Unit 2); `scripts/benchmark_sources/aider.py` (Unit 3);
`scripts/regenerate-benchmark-snapshot.py` adapter wiring +
`BUNDLED_ID_TO_SOURCE_HF_ID` join + `_refresh_bundled_models` merge
(Unit 4). `scripts/requirements.txt` + updated `NOTICE` + adapter
README accompany the work.

Superseded 2026-05-20 by [`2026-05-20-001`](docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md):
the Open LLM Leaderboard + Aider adapters were collapsed into a single
`whichllm_combined.py` that delegates to
`whichllm.models.benchmark.fetch_benchmark_scores()` (all six upstream
sources + layered merge + lineage demotion). Net `-623` lines vendored.

### ~~live HF Hub snapshot discovery — [`docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md`](docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md)~~

All 7 units shipped 2026-05-20 plus post-plan refinements:
`scripts/benchmark_sources/hf_discovery.py` (Unit 3) wraps
`whichllm.models.fetcher.fetch_models()` with allowlist filter +
multi-quant emission + official-org variant synthesis;
`data/{task-hints,gguf-publisher-allowlist}.yaml` (Unit 4);
schema fields on `ModelEntry` (Unit 1); whichllm-aligned
`estimate_peak_bytes` (Unit 2); predicate-based corpus in
`tests/recommender_corpus.rs` (Unit 5); 2 MiB snapshot ceiling
(Unit 6); HF_TOKEN + lockstep version check in CI (Unit 7).

Follow-up commits `2dc70ff` (whichllm-combined scoring), `247f848`
(richer hardware banner), `0f89edd` (`--only models --json` listing,
top-N 5 → 10), `58ee985` (per-quant rows), `c80d638` (variant
synthesis + VRAM estimator port + ranking tuned to whichllm) closed
the gap so `init --only models --json` now produces 7/10 model
matches and 3/10 quant matches vs `whichllm --json --top 10` on a
64 GB shared-VRAM host. Remaining gap (family selection, additive
score shape, synthetic-row download fallback) tracked above in
v2-GA blockers.

### ~~init wizard / doctor / pull — [`docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md`](docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md)~~

All 13 units shipped — verified 2026-05-19 against the tree:

`docs/spikes/2026-05-19-*.md` for Unit 1;
`src/config/{loader,writer}.rs` + `managed_keys` for Unit 2;
`Init`/`Doctor`/`Pull` subcommands wired in
`src/cli/cli_args.rs` with `src/cli/{init,doctor,pull}.rs` shims for Unit 3;
`src/init/{fetch,fetch_policy}.rs` for Unit 4;
`src/init/{snapshot,benchmark}.rs` + `data/benchmark-snapshot.json` for Unit 5;
`src/init/recommender.rs` for Unit 6;
`scripts/regenerate-benchmark-snapshot.py` +
`.github/workflows/regenerate-benchmark-snapshot.yml` for Unit 7;
`src/init/install/{gh_releases,safe_extract}.rs` for Unit 8;
`src/init/download.rs` for Unit 9; `src/init/wizard.rs` for Unit 10;
`src/init/config_writer.rs` for Unit 11; `src/init/smoke.rs` for Unit 12;
`src/init/doctor.rs` for Unit 13.
