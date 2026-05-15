# Changelog

All notable changes to llamatui will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

## [Unreleased]

### Added
- Daemon-on-demand architecture: single `llamatui` binary that acts as TUI, CLI, **and** daemon depending on the subcommand. Daemon owns `llama-server` children and persisted state; clients attach over a `0600` Unix socket authenticated via peer credentials.
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

### Deferred to v2
- HTTP and MCP server surfaces (R34).
- HuggingFace pull worker (R46). CLI `pull` subcommand scaffold is hidden from `--help` until then.

## How to read this file

Future releases will land under their own version heading once the project tags `v0.1.0` and beyond. Until then, every meaningful change appears under **Unreleased** so the file stays useful for in-progress users and reviewers.
