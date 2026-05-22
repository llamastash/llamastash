# Changelog

All notable changes to llamastash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

Entries are one-line summaries of noteworthy changes; follow the linked commit or PR for the full story.

## [Unreleased]

_No changes yet._

## [0.0.1] — [Unreleased]

First publicly-installable release. Single `llamastash` binary acts as TUI, CLI, and daemon; distribution lands across Cargo, a Homebrew tap, and a GitHub-hosted install script, with a marketing site at [llamastash.cli.rs](https://llamastash.cli.rs).

- Daemon-on-demand over a `0600` Unix socket with peercred auth; supervises `llama-server` children through `Launching → Loading → Ready / Error → Stopping → Stopped` with three-factor orphan re-adoption.
- GGUF header parser and async scanner for HuggingFace / Ollama / LM Studio caches; model identity is `(canonical path, BLAKE3 of header)`.
- TUI with grouped list + favorites + filter, launch picker, advanced flag panel, clipboard yank, streaming Chat / Embed / Rerank / Logs right pane, five themes (Catppuccin Macchiato default).
- CLI: `list` / `start` / `stop` / `status` / `logs` / `presets` / `favorites` / `daemon` — every read+mutation command supports `--json`, with documented exit codes and an auto-spawn-daemon flow (`--no-spawn` to opt out).
- `llamastash init` first-run wizard (R48): detect → install `llama-server` per OS×GPU → recommend + pull a GGUF → write `config.yaml` with `arch_defaults` → smoke launch → TUI handoff. Per-step `--install` / `--model` / `--config-step` overrides, `--recommended` / `--json` / `--offline` modes, and `--revision <SHA>` to pin HF commits.
- TUI HuggingFace pull dialog (`d` from the model list) — three-stage Search → File picker → Confirm modal backed by HF Hub's `/api/models` (debounced via `FetchClient`, fit-aware ✓/⚠/✗/— glyph column, sharded-set collapse, byte-accurate progress strip, FIFO queue with one active pull). `Ctrl+X` cancels the active pull mid-chunk; `Ctrl+D` deletes the focused GGUF on idle rows only (HF-cache layout deletes the whole repo dir, constrained to the `~/.cache/huggingface/hub` tree). CLI `--offline` / `LLAMASTASH_OFFLINE` flows through every spawned HF task ([`#4`](../../pull/4)).
- Custom theme via the `custom_theme` config block — user-defined palette accepting `#RRGGBB` hex or ANSI names, inheriting unspecified slots from `base:`; joins the `t:theme` cycle once `theme: custom` is set.
- Custom keybindings via the `keybindings:` config block — every TUI `Action` accepts a Kdash-style key-spec override (`ctrl+q`, `shift+tab`, `f1`, …); overrides flow through to live HF-dialog / confirm-popup labels.
- `llamastash doctor` read-only diagnostic with stable finding ids under `--json` (R74); `llamastash pull <owner/repo[:filename.gguf]>` on `hf-hub` (R65); `llamastash recommend` shortcut for hardware-aware GGUF picks ([`adfef21`](../../commit/adfef21)).
- Path-A recommender with VRAM-fit hard filter and composite ranking (benchmark × tok/s × params × recency), backed by a bundled benchmark snapshot refreshed by daily CI and vendored [`whichllm`](https://github.com/Andyyyy64/whichllm) catalog discovery ([`ae94ee3`](../../commit/ae94ee3)).
- Built-in `(architecture, gpu_backend) → TypedKnobs` defaults table. A fresh install on any supported backend gets sensible launch flags without ever touching YAML. Coverage v1: `llama*`, `qwen2*`, `qwen3*`, `mistral`, `mixtral`, `gemma*`, `phi*`, `deepseek*`, `granite`, `falcon`, `stablelm`, `command-r`, plus a `*` fallback row. GPU backends seed `n_gpu_layers: 99` universally; `flash_attn: true` for flash-attn-eligible architectures on NVIDIA / Apple Metal.
- Layered launch-flag resolver with source labels — `preset > last_params > yaml arch_defaults > built-in table > llama-server`. Each resolved field carries a `LayerLabel` so the Settings tab can render per-row origin chips (`(user)`, `(last used)`, `(arch default)`, `(model default)`, `(server default)`). The yaml-vs-builtin split collapses into a single `(arch default)` chip — both are conceptually "the arch's known-good defaults"; yaml still wins per-field at resolve time. `(model default)` means the model file supplies the value (GGUF header for `ctx`, chat template for `reasoning`); `(server default)` means no flag is sent and llama-server's hardcoded fallback applies.
- Typed launch-knob editor in the Settings tab (replaces the freeform advanced-flags modal). Rows for `ctx`, `reasoning`, `n_gpu_layers`, `threads`, `cache_type_k/v`, `flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`, plus a free-text `extras` row. Up/Down moves rows; Left/Right cycles values through pinned preset lists; `e` opens inline edit for numeric / enum / extras rows; Enter commits an open edit, then launches. Backspace resets the focused row to inherit.
- Extras-row forbidden-flag inline warning. Tokens matching `--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*` surface a red `⚠ forbidden: …` line under the row; secret values are redacted before display. Mirrors the daemon's IPC-layer refusal so the user sees the same message in both places.
- IPC `start_model` and `last_params_list` swap to the typed shape: `knobs: TypedKnobs + extras: Vec<String>` instead of `advanced: Vec<String>`. The `params` block of every `last_params_list` row, `presets_list` row, and `status.models[].params` mirror the same structure. Pre-1.0 schema flip — dev installs with a populated `last_params` Vec will see `state.json` quarantined to `state.json.broken-<ts>` on first daemon boot and come up with defaults.
- Colored CLI output across every human-readable surface with `--no-colors` / `NO_COLOR` / non-TTY off-conditions; padded TTY tables for report commands; `--json` byte-stable regardless ([`96fed70`](../../commit/96fed70)).
- TUI `Ctrl+R` restarts the daemon preserving the parent dispatcher's resolved options; `Ctrl+Q` kills it; both stay discoverable via `?` only ([`adfef21`](../../commit/adfef21), [`0b6fc77`](../../commit/0b6fc77)).
- `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` env overrides for side-by-side daemons (alongside the existing `LLAMASTASH_SOCKET`).

## [0.0.1] — 2026-05-20

The first publicly-installable llamastash release. Bundles every commit since the project's inception under a single WIP release; the version reflects pre-1.0 status. Distribution lands across three channels (Cargo, Homebrew tap, GitHub-hosted install script) with end-to-end automated release-on-tag; a marketing site at [llamastash.cli.rs](https://llamastash.cli.rs) ships alongside.

### Added (interactive wizard + colored CLI)

- **Interactive `init` install picker offers a "Custom path…" option.** Selecting it prompts for an absolute path to an existing `llama-server` binary and routes it through the same `install_from_custom_path` adoption pipeline as `--install custom:PATH`. Closes the gap where users with self-built binaries had no way to point at them from the interactive wizard (the CLI flag worked but the menu didn't list it).
- **Interactive `init` wizard.** `llamastash init` now opens a `cliclack`-powered stepped wizard by default: install-method pick, model pick, config-write confirm. Pass `--recommended` to accept every hardware-aware default without prompting. Three per-step flags pre-answer individual prompts without skipping the rest: `--install <brew|gh-releases|existing|custom:PATH>`, `--model <recommended|none|owner/repo>`, `--config-step <write|skip>`. Non-TTY stdout auto-falls-back to recommended defaults with a single stderr warning. The unused `dialoguer` dep is removed; `cliclack` replaces it.
- **Colored CLI output.** Every human-readable surface now ships colored output by default — success-green, error-red, warning-yellow, dim-secondary. The new global `--no-colors` flag plus `NO_COLOR` env-var detection (per https://no-color.org) and non-TTY stdout detection are OR-ed together; any one silences ANSI. `--json` output is byte-stable regardless. Policy lives in `src/cli/colors.rs`, initialised once in `cli::dispatch`.

### Changed (wizard ergonomics + error reporting)

- **`llamastash init` shows live progress for long steps.** Every long-running phase the wizard runs (Homebrew install, GitHub Releases query + download + extract, HuggingFace per-file download, benchmark-snapshot fetch, smoke probe) now drives a `cliclack` spinner with a present-tense narration message that flips to a `✓` success line (or `✗` failure line) on completion. Replaces the previous "blinking cursor for several minutes" UX. Non-TTY runs fall back to static themed `cliclack::log` lines; `--json` mode emits no progress at all so the structured-stdout contract stays byte-stable.
- **Config-diff confirmation gets light syntax coloring.** The dry-run preview the wizard shows before writing `config.yaml` now colors the `+` / `~` markers, bold-cyans the dotted key path, and dims the "(no changes)" line. Honors the existing `--no-colors` / `NO_COLOR` / non-TTY downgrade rules.
- **Smoke probe step now narrates what it did and reports concrete numbers.** Success line shows peak memory estimate vs effective ceiling (e.g. `phase-1 fits (peak ~5.6 GiB vs ceiling 9.0 GiB); llama-server reports build 5037 (b00d09c)`) instead of the prior terse `phase-1 + --version OK (binary version:)`. `--verbose` now emits debug lines for each smoke sub-step (phase-1 inputs/result, `--version` spawn/return, peak vs ceiling). The version parser handles modern llama.cpp output (`version: NNNN (bHASH)`) which previously fell through to the regex and returned just `version:`.
- **`--verbose` now tees debug logs to stderr in addition to the log file.** The file logger remains the source of truth (full module surface at Debug level); the new stderr tee filters to `llamastash::*` records so dependency noise from hyper/reqwest/tokio doesn't drown out wizard-internal logs. Added `log::debug!` step boundaries in the init wizard so `--verbose` produces actually-useful output for a happy-path run.
- **CLI errors walk the full `std::error::Error::source()` chain.** `CliExit::prefix` (used by every wizard / pull / config error path) now appends every layer of the source chain into the message, so a wrapped hf-hub→reqwest→io error shows as `init download: hf-hub: request error: ... : connection reset by peer` instead of just the top-level wrapper. `DownloadError::Hub` now stores the `hf_hub::ApiError` directly (was a stringified `String`) so the chain isn't severed at the conversion boundary.

### Fixed

- **`llamastash init` model step no longer fails with "returned zero matching files" on sharded GGUF repos.** Three benchmark-snapshot entries (`Qwen/Qwen2.5-{7B,14B,32B}-Instruct-GGUF`) point at a single unsharded filename, but those repos only host the `q4_k_m` weights split across 2/3/5 shards. The download filter now falls back to the canonical `<stem>-NNNNN-of-NNNNN.<ext>` shard pattern when the exact pinned filename has no match, and pulls every shard. llama.cpp loads the shard set natively from the first shard, so the smoke probe and config write keep working unchanged.

### Added (init wizard, doctor, pull)

- **Interactive `init` wizard.** `llamastash init` now opens a `cliclack`-powered stepped wizard by default: install-method pick, model pick, config-write confirm. Pass `--recommended` to accept every hardware-aware default without prompting. Three per-step flags pre-answer individual prompts without skipping the rest: `--install <brew|gh-releases|existing|custom:PATH>`, `--model <recommended|none|owner/repo>`, `--config-step <write|skip>`. Non-TTY stdout auto-falls-back to recommended defaults with a single stderr warning. The unused `dialoguer` dep is removed; `cliclack` replaces it.
- **Colored CLI output.** Every human-readable surface now ships colored output by default — success-green, error-red, warning-yellow, dim-secondary. The new global `--no-colors` flag plus `NO_COLOR` env-var detection (per https://no-color.org) and non-TTY stdout detection are OR-ed together; any one silences ANSI. `--json` output is byte-stable regardless. Policy lives in `src/cli/colors.rs`, initialised once in `cli::dispatch`.

- **`llamastash init`** — first-run setup wizard (R48). Six-step flow: detect hardware + binary → install `llama-server` per OS×GPU class → recommend + download a starter GGUF → write `config.yaml` with `arch_defaults` → smoke launch → handoff to the TUI. `--recommended` accepts hardware-aware defaults; `--json` emits a structured summary; `--offline` disables outbound network. `--only`/`--skip` scope per-step re-runs (e.g. `init --only server` to re-install after a GPU swap).
- **`llamastash doctor`** — read-only diagnostic (R74). Re-runs detection, diffs against `_init_snapshot.json`, emits 0-6 findings with stable ids agents can branch on: `binary_missing`, `binary_digest_drift` (GH Releases only — brew installs carved out), `hardware_drift`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`. `--json` emits a stable envelope; output is always safe to paste into a public issue.
- **`llamastash pull <hf-repo>`** — HuggingFace pull primitive (R65), graduated from the v1 `unimplemented!` stub. Built on the [`hf-hub`](https://crates.io/crates/hf-hub) crate (0.5 line, which resolves the same `reqwest 0.12` we pin elsewhere — no transitive collision). Accepts `owner/repo` or `owner/repo:filename.gguf`; honors `HF_TOKEN`; refuses cache-file tokens with insecure modes; performs a disk-space precheck (R64) by HEAD-ing each filtered file via hf-hub's `Api::metadata`.
- **`arch_defaults` config block** — per-architecture launch defaults (`qwen2`, `llama`, …) merged into `LaunchParams.advanced` at start-model time, only for flags the caller has not already supplied. R69 precedence: preset > last-params > arch defaults > built-in.
- **`init_snapshot.json`** — sibling of `state.json` under the state dir. Records hardware vendor / VRAM / binary path + SHA-256 / install method / managed_keys with blake3 value digests. Atomic write + 0600 + parse-fail quarantine.
- **Bundled benchmark snapshot** — `data/benchmark-snapshot.json` ships in the binary via `include_str!` (2 MiB build-time cap, raised from 500 KiB by Unit 6 of [plan 2026-05-20-001](docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md) so the ~100-row live-discovery catalog fits without trimming task tiers). Daily CI workflow (`.github/workflows/regenerate-benchmark-snapshot.yml`) refreshes the rolling `snapshot-latest` Release asset; rollback-DoS gate via monotonic `bundle_date` + `min_version` ≤ build.
- **Path-A recommender** — VRAM-fit hard filter + composite ranker (benchmark × tok/s × params × recency) with per-pick justification (R58). Release-blocking 16/20 corpus check; weights tunable from the snapshot.
- **Network fetch substrate (`src/init/fetch.rs`)** — HTTPS-only `FetchClient` with host allowlist, redirect cap, body-size cap, IP-literal refusal-via-allowlist. Used by snapshot fetch, GH Releases install, and `llamastash pull`. `--offline` / `LLAMASTASH_OFFLINE` short-circuits before any DNS.
- **GH Releases install path (`src/init/install/`)** — fetches `ggml-org/llama.cpp` releases, picks the asset by `(os, arch, gpu)` suffix (Vulkan default for Linux+Nvidia per the Unit 1 spike's breaking finding — no CUDA prebuilt exists upstream), verifies SHA-256 from the asset's `digest` field, safe-extracts with archive-bomb defenses (entry count cap, total size cap, compression-ratio cap, hardlink + symlink + absolute-path + `..` refusal).
- **Exit codes 72/73/74** — `INIT_ABORTED` (integrity check failed, daemon stop/restart could not be coerced), `INIT_DOWNLOAD_FAILED` (wizard's download step), `INIT_SMOKE_FAILED` (probe phase). Distinct from `PULL_FAILED=69` so agents branch on cause.
- **Smoke phase 1 + `--version` probe (`src/init/smoke.rs`)** — pre-launch VRAM ceiling check + binary executes-cleanly probe with `env_clear()` minimal env. Phase 2 (daemon-mediated `/health` + `/v1/chat/completions`) is deferred to v2.1.

### Added (post-v2 — reproducibility + path-isolation env vars)

- **`init --revision <SHA-or-branch>`** — pin the HuggingFace commit the model resolves at via hf-hub's `Repo::with_revision`. Visible on every release binary (the flag is **not** gated behind `--features uat`); empty values are rejected at parse time. See `docs/usage.md §Pinning a HuggingFace revision`.
- **`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` env-var overrides** — direct overrides for `paths::state_dir()`, `paths::config_dir()`, and `paths::cache_dir()` mirroring the pre-existing `LLAMASTASH_SOCKET`. Empty values are treated as unset. Lets operators run side-by-side daemons without colliding on persisted state / config / cache paths. Documented in `docs/usage.md §Environment variables`.

### Internal

- **Vendored benchmark scrapers** — `scripts/benchmark_sources/{whichllm,open_llm_leaderboard,aider}.py` now run live against the Open LLM Leaderboard rows API and Aider's polyglot YAML in the daily snapshot regen cron, replacing the `TODO(unit7-v2-ga)` placeholders. Partial vendoring of [`Andyyyy64/whichllm`](https://github.com/Andyyyy64/whichllm) (MIT) pinned at commit `73cd92f`; deps pinned in `scripts/requirements.txt`. CI-only — R45 single-binary invariant preserved, no Rust artefact change.
- **Maintainer UAT command + nightly Metal CI lane** — new `--features uat` Cargo feature (off by default; release binaries never ship it) gates a hidden `llamastash uat --backend <X> --mode {warm|cold} --report-out <path>` subcommand that runs a 5-step lifecycle on real GPU hardware (doctor preflight → init → smoke chat → stop → doctor postrun) with cross-platform tempdir isolation (the new `LLAMASTASH_*_DIR` env vars + `HF_HOME`) and a structured JSON report. New `.github/workflows/uat-metal-nightly.yml` runs the UAT on Apple Silicon — daily at 09:00 UTC, on every `v*` release tag, and on-demand via `workflow_dispatch` — with rolling-issue failure routing. Release-PR template (`.github/PULL_REQUEST_TEMPLATE/release.md`) carries the UAT backends-checked checklist + `uat-caught` label for outcome-metric tracking. See [`docs/testing/hardware-uat.md`](docs/testing/hardware-uat.md), [`docs/runbooks/verify-uat-reintroduction.md`](docs/runbooks/verify-uat-reintroduction.md), and [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md).
- **PR-CI now compiles and tests `--features uat`** — `clippy` and `test` jobs in `.github/workflows/ci.yml` matrix on `features ∈ {test-fixtures, "test-fixtures,uat"}` so the maintainer-only UAT surface lint- and unit-tests on every PR (hermetic — pre-flight fails before any subprocess spawns, no GPU touched). The `release-readiness` job audits the R4 contract (release binary never carries `--features uat`), synthetic-exit-code docs sync, and the ce-review fix signatures in `uat-metal-nightly.yml`.

### Added (launcher + smoke-test + CLI)

- Daemon-on-demand architecture: single `llamastash` binary that acts as TUI, CLI, **and** daemon depending on the subcommand. Daemon owns `llama-server` children and persisted state; clients attach over a `0600` Unix socket authenticated via peer credentials.
- GGUF header parser with model identity = `(canonical path, BLAKE3 of header)`; KV-cache-aware memory estimator.
- Asynchronous filesystem scanner that surfaces HuggingFace, Ollama, and LM Studio caches plus user-configured roots; depth-limited HF watcher; per-file `(path, mtime, size)` metadata cache.
- Process supervisor: `Launching → Loading → Ready / Error → Stopping → Stopped` state machine; port allocator; `/health` probe; per-model log file plus 4K-line ring buffer; SIGTERM→SIGKILL stop semantics; orphan re-adoption with three-factor (PID alive + port listening + `/v1/models` path match) confirmation.
- Persisted state: favorites, presets, last-params, running snapshot. Temp-file + rename writes; corruption quarantine.
- Five themes — Catppuccin Macchiato (default), Catppuccin Latte, Gruvbox Dark, Solarized Dark, Monochrome.
- TUI: list pane with directory grouping + favorites + filter; launch picker pre-populated from `last_params`; advanced flag panel; clipboard yank (URL / curl / model path) with `arboard` + `wl-copy` / `xclip` / `xsel` fallbacks.
- TUI right pane: per-tab text input focus; streaming Chat tab with `<think>` collapse; Embed and Rerank one-shot tabs; live Logs tab tail with auto-scroll toggle.
- CLI: `list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `daemon` — `--json` everywhere relevant; documented exit codes; auto-spawn-daemon flow with `--no-spawn` opt-out.
- `status` IPC and CLI surface include a `daemon` health block (`pid`, `uptime_seconds`, `active_connections`).
- `stop_external` IPC for terminating unmanaged `llama-server` processes the daemon surfaced read-only.
- GPU detection: NVML on Linux + system_profiler on Apple Silicon, falling back to AMD `rocm-smi` shellout, then Vulkan, then CPU-only.

### Deferred to a later release
- HTTP and MCP server surfaces (R34).
- Smoke phase 2 (daemon-mediated `/health` + chat completion probe). 0.0.1 ships phase 1 + `--version`; phase 2 lands once the daemon stop+restart helpers are exported through the IPC surface.
- TUI `_init_snapshot`-aware maintenance nudge for doctor findings (open question in the plan; user-data-driven follow-up).
- Range-resume on partial HF downloads (requires a future hf-hub line that exposes a custom-`reqwest::Client` hook without a reqwest 0.13 transitive — see `docs/spikes/2026-05-19-hf-hub-client-injection.md`).

### Notes
- Commit `43cce21` (round-8 polish) describes the Shift key glyph
  as the Nerd Font codepoint `󰘶`. The shipped code never used that
  codepoint — `SHIFT_GLYPH` in `src/tui/keybindings.rs` is the
  standard Unicode `⇧` (U+21E7). The Nerd Font codepoints were
  scrubbed wholesale in the very next commit (`0ee01df`). No
  behaviour change; this note is for archaeology.

## How to read this file

Tagged releases land under their version heading; in-flight work accumulates under **Unreleased** until the next tag promotes it. llamastash is pre-1.0 / WIP; the entire pre-release history is bundled under the first publishable tag, [0.0.1], rather than backfilled into a series of synthetic tags. The ledger starts there.
