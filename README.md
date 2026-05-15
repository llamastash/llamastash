# llamatui

A fast, keyboard-driven TUI for launching and managing local `llama-server` (llama.cpp) instances.

> **Status: early development.** The v1 scope is in [`docs/brainstorms/llamatui-requirements.md`](docs/brainstorms/llamatui-requirements.md). The implementation plan is in [`docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md).

## What it does (v1, in progress)

- Discovers GGUF models on disk — including HuggingFace, Ollama, and LM Studio caches — and groups them by directory.
- Surfaces GGUF metadata (architecture, quantization, native context, KV-cache-aware memory estimates) so you can pick smart defaults.
- Launches `llama-server` with a tweakable launch picker (context length, reasoning, advanced flags), per-model named presets, favorites, and a filter.
- Manages multiple concurrent models with a health-probed status state machine; logs and a smoke-test prompt panel are one tab away.
- Pulls GGUFs directly from HuggingFace from inside the TUI or from the CLI.
- Exposes the same primitives as non-interactive `llamatui` subcommands so shell scripts and AI agents can drive it.

## Install

Coming soon: `cargo install llamatui`, Homebrew tap, and pre-built release binaries.

## CLI exit codes

Every non-interactive subcommand returns a documented exit code so agent scripts can branch on failure class. The codes are the public CLI contract — pin against numbers, not message text.

| Code | Meaning |
|------|---------|
| `0`  | Success |
| `64` | Usage error (missing required arg, invalid combination — clap-emitted) |
| `65` | Daemon unreachable (socket missing, peer hung up, timeout) |
| `66` | Model reference matched zero or multiple models (stderr lists candidates) |
| `67` | `start_model` failed at the supervisor (probe timeout, port allocation failure) |
| `68` | `stop_model` / `stop_all` failed |
| `69` | Reserved for `pull` (lands with R46 in v2) |
| `70` | `llama-server` binary not found (`--llama-server`, `LLAMATUI_LLAMA_SERVER`, or `$PATH`) |
| `71` | Unexpected error (catch-all) |

Set `LLAMATUI_SOCKET=/path/to/daemon.sock` to point a CLI at a non-default daemon socket without the `--socket-path` hidden flag dance.

## License

MIT © Deepu K Sasidharan
