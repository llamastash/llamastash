# TODO

Single index of outstanding work across plans, docs, and code. When you add a
TODO anywhere in the repo (a `TODO(...)` comment, an unchecked `- [ ]` in a
plan, a `todo:` frontmatter field on a spike), also add a one-line entry here
with a link back to the source. When you complete one, strike it from both
places.

## In-code TODOs

_None — the four vendoring items shipped 2026-05-19 via [`docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md`](docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md). The Open LLM Leaderboard + Aider polyglot adapters now run live against upstream in the daily CI cron at the pinned whichllm commit `73cd92f`; both `TODO(unit7-v2-ga)` placeholders in `scripts/regenerate-benchmark-snapshot.py` are gone._

## v2-GA blockers (must clear before v2 GA, not v2 launch)

- [ ] Remeasure per-backend VRAM overhead band on real CUDA / HIP / Vulkan / Metal hardware — [`docs/spikes/2026-05-19-vram-overhead-band.md`](docs/spikes/2026-05-19-vram-overhead-band.md) `todo:` frontmatter. Harness ready: [`scripts/measure-overhead-band.sh`](scripts/measure-overhead-band.sh) + runbook at [`docs/runbooks/measure-vram-overhead-band.md`](docs/runbooks/measure-vram-overhead-band.md)

## v1+ release blockers

- [ ] /ce:review from commit 5f290d7e27fe0204faf5394895c891f6150dce6b onwards except 07ce5abdede7d9c63e2e4352b34de9c8d5566e40 as thats already reviewed.
- [ ] if `--llama-server` is passed, add it as fallback in config file and use it when llama-server is not on path.
- [ ] Better/colorful/formatted CLI output for commands.
- [ ] Readme and other docs sync.
- [ ] best-model (find nicer alias) command. reuse init and just download the best model for current setup/hardware
- [ ] `R:restart` daemon hotkey.
- [ ] **Need brainstorm/plan**: Advanced modal - replace free-text editor with typed key/value fields like settings; keys = advanced options for the model, values = current settings; pre-populate from the model's last params and let users edit before launch. Requires a refactor of the advanced modal to support dynamic fields.
- [ ] **Need brainstorm/plan**: HuggingFace pull TUI dialog with search / sort / pagination (origin: R46, [`docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md)).
- [ ] **Need brainstorm/plan**: Proxy router that maps a single endpoint to running models by model name. If the model isn't running, start it; if launch fails, fall back to a running model when one is available; otherwise error. Keep it OpenCode / π compatible so agents and tools can hit one URL.
- [ ] **Need brainstorm/plan**: Test strategy for Nvidia / AMD / Apple GPU support (origin: R34).
- [ ] Skills.
- [ ] **In progress**: Release setup: website, brew tap, etc. (KDash-style).
- [ ] Release pipeline like Kdash
- [ ] **Need brainstorm/plan**: Release blog.
- [ ] **Need brainstorm/research/plan**:Social promotion — research an approach for max reach.

## v2+ roadmap

- [ ] **Need brainstorm/plan**: HTTP and MCP surfaces (origin: R34).
- [ ] **Need brainstorm/plan**:Anthropic API compatibility.
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
