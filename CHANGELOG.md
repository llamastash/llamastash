# Changelog

All notable changes to llamadash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

## [Unreleased]

### Added (v2 — init wizard, doctor, pull)

- **`llamadash init`** — first-run setup wizard (R48). Six-step flow: detect hardware + binary → install `llama-server` per OS×GPU class → recommend + download a starter GGUF → write `config.yaml` with `arch_defaults` → smoke launch → handoff to the TUI. `--yes` accepts hardware-aware defaults; `--json` emits a structured summary; `--offline` disables outbound network. `--only`/`--skip` scope per-step re-runs (e.g. `init --only server` to re-install after a GPU swap).
- **`llamadash doctor`** — read-only diagnostic (R74). Re-runs detection, diffs against `_init_snapshot.json`, emits 0-6 findings with stable ids agents can branch on: `binary_missing`, `binary_digest_drift` (GH Releases only — brew installs carved out), `hardware_drift`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`. `--json` emits a stable envelope; output is always safe to paste into a public issue.
- **`llamadash pull <hf-repo>`** — HuggingFace pull primitive (R65), graduated from the v1 `unimplemented!` stub. Built on the [`hf-hub`](https://crates.io/crates/hf-hub) crate (0.5 line, which resolves the same `reqwest 0.12` we pin elsewhere — no transitive collision). Accepts `owner/repo` or `owner/repo:filename.gguf`; honors `HF_TOKEN`; refuses cache-file tokens with insecure modes; performs a disk-space precheck (R64) by HEAD-ing each filtered file via hf-hub's `Api::metadata`.
- **`arch_defaults` config block** — per-architecture launch defaults (`qwen2`, `llama`, …) merged into `LaunchParams.advanced` at start-model time, only for flags the caller has not already supplied. R69 precedence: preset > last-params > arch defaults > built-in.
- **`init_snapshot.json`** — sibling of `state.json` under the state dir. Records hardware vendor / VRAM / binary path + SHA-256 / install method / managed_keys with blake3 value digests. Atomic write + 0600 + parse-fail quarantine.
- **Bundled benchmark snapshot** — `data/benchmark-snapshot.json` ships in the binary via `include_str!` (500 KiB build-time cap). Daily CI workflow (`.github/workflows/regenerate-benchmark-snapshot.yml`) refreshes the rolling `snapshot-latest` Release asset; rollback-DoS gate via monotonic `bundle_date` + `min_version` ≤ build.
- **Path-A recommender** — VRAM-fit hard filter + composite ranker (benchmark × tok/s × params × recency) with per-pick justification (R58). Release-blocking 16/20 corpus check; weights tunable from the snapshot.
- **Network fetch substrate (`src/init/fetch.rs`)** — HTTPS-only `FetchClient` with host allowlist, redirect cap, body-size cap, IP-literal refusal-via-allowlist. Used by snapshot fetch, GH Releases install, and `llamadash pull`. `--offline` / `LLAMADASH_OFFLINE` short-circuits before any DNS.
- **GH Releases install path (`src/init/install/`)** — fetches `ggml-org/llama.cpp` releases, picks the asset by `(os, arch, gpu)` suffix (Vulkan default for Linux+Nvidia per the Unit 1 spike's breaking finding — no CUDA prebuilt exists upstream), verifies SHA-256 from the asset's `digest` field, safe-extracts with archive-bomb defenses (entry count cap, total size cap, compression-ratio cap, hardlink + symlink + absolute-path + `..` refusal).
- **Exit codes 72/73/74** — `INIT_ABORTED` (integrity check failed, daemon stop/restart could not be coerced), `INIT_DOWNLOAD_FAILED` (wizard's download step), `INIT_SMOKE_FAILED` (probe phase). Distinct from `PULL_FAILED=69` so agents branch on cause.
- **Smoke phase 1 + `--version` probe (`src/init/smoke.rs`)** — pre-launch VRAM ceiling check + binary executes-cleanly probe with `env_clear()` minimal env. Phase 2 (daemon-mediated `/health` + `/v1/chat/completions`) is deferred to v2.1.

### Added (v1 — launcher + smoke-test + CLI)

- Daemon-on-demand architecture: single `llamadash` binary that acts as TUI, CLI, **and** daemon depending on the subcommand. Daemon owns `llama-server` children and persisted state; clients attach over a `0600` Unix socket authenticated via peer credentials.
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

### Deferred to v2.1+
- HTTP and MCP server surfaces (R34).
- Smoke phase 2 (daemon-mediated `/health` + chat completion probe). v2 ships phase 1 + `--version`; phase 2 lands once the daemon stop+restart helpers are exported through the IPC surface.
- TUI `_init_snapshot`-aware maintenance nudge for doctor findings (open question in the v2 plan; user-data-driven follow-up).
- Range-resume on partial HF downloads (requires a future hf-hub line that exposes a custom-`reqwest::Client` hook without a reqwest 0.13 transitive — see `docs/spikes/2026-05-19-hf-hub-client-injection.md`).

### Notes
- Commit `43cce21` (round-8 polish) describes the Shift key glyph
  as the Nerd Font codepoint `󰘶`. The shipped code never used that
  codepoint — `SHIFT_GLYPH` in `src/tui/keybindings.rs` is the
  standard Unicode `⇧` (U+21E7). The Nerd Font codepoints were
  scrubbed wholesale in the very next commit (`0ee01df`). No
  behaviour change; this note is for archaeology.

## How to read this file

Future releases will land under their own version heading once the project tags `v0.1.0` and beyond. Until then, every meaningful change appears under **Unreleased** so the file stays useful for in-progress users and reviewers.
