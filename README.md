# llamatui

A fast, keyboard-driven TUI **and** CLI for launching local `llama-server` (llama.cpp) instances.

> **Status: v1 work in progress.** Scope: [`docs/brainstorms/llamatui-requirements.md`](docs/brainstorms/llamatui-requirements.md). Implementation plan: [`docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md).

## Why

Heavy abstractions (Ollama, LM Studio) hide llama.cpp; raw `llama-server` use is tedious. llamatui is a fast, transparent launcher that is also a first-class shell-tool surface for agents — one binary, daemon on demand, same primitives in the TUI and the CLI.

## What it does (v1)

- **Discovers GGUF models on disk** — your own paths plus HuggingFace, Ollama, and LM Studio caches — grouped by directory with live filesystem watching.
- **Surfaces rich GGUF metadata** — architecture, quantization, native context length, KV-cache-aware memory estimates.
- **Launches `llama-server`** through a keyboard-driven picker (context length, reasoning toggle, advanced flags); supports named per-model **presets** and **favorites**.
- **Supervises multiple concurrent models** with a health-probed state machine. Running models survive TUI exit.
- **Smoke-tests models** via a right-pane Chat / Embed / Rerank tab that hits the same OpenAI-compatible endpoints any external client would use.
- **Exposes a complete non-interactive CLI** — `list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `daemon`. Every read command supports `--json`. Distinct exit codes per failure class.

HTTP and MCP surfaces are deferred to v2 (origin: R34). HuggingFace pull is also v2 (origin: R46).

## Install

> Pre-1.0 binaries are not yet published. Build from source for now.

```bash
git clone https://github.com/llamatui/llamatui
cd llamatui
cargo install --path .
```

`cargo install llamatui`, a Homebrew tap, and pre-built release binaries land alongside the first tagged release.

You also need `llama-server` on your `PATH` (or pointed at via `--llama-server <path>` / `LLAMATUI_LLAMA_SERVER`).

## Quickstart

```bash
# Open the TUI. Scans default caches; daemon auto-spawns on demand.
llamatui

# List discovered models (TSV by default, JSON for agents).
llamatui list
llamatui list --json | jq

# Launch a model by name, name substring, path, or canonical id.
llamatui start qwen-coder --ctx 16384 --reasoning on

# Drive a smoke-test request against the running endpoint.
curl -s http://127.0.0.1:41100/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model": "qwen-coder", "messages": [{"role": "user", "content": "hi"}]}'

# Stop it.
llamatui stop qwen-coder
```

Full subcommand reference: [`docs/usage.md`](docs/usage.md). Architecture and IPC contract: [`docs/architecture.md`](docs/architecture.md). When things go wrong: [`docs/troubleshooting.md`](docs/troubleshooting.md).

## CLI exit codes

Every non-interactive subcommand returns a documented exit code so agent scripts can branch on failure class. Pin against numbers, not message text — they are the public contract.

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

## Configuration

llamatui reads `$XDG_CONFIG_HOME/llamatui/config.yaml` (macOS: `~/Library/Application Support/llamatui/config.yaml`). Schema in [`docs/usage.md`](docs/usage.md). Environment variables:

| Variable | Purpose |
|---|---|
| `LLAMATUI_CONFIG` | Override config-file path |
| `LLAMATUI_LLAMA_SERVER` | Path to `llama-server` |
| `LLAMATUI_NO_SCAN` | Skip filesystem scanning |
| `LLAMATUI_SOCKET` | Point a CLI at a non-default daemon socket |

## Platforms

Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel). Windows is out of scope for v1.

## Related projects

- [`kdash`](https://github.com/kdash-rs/kdash) — Kubernetes dashboard TUI by the same author.
- [`jwt-ui`](https://github.com/jwt-rs/jwt-ui) — JWT decoder / encoder TUI by the same author.

## Contributing

Bug reports, design discussion, and PRs welcome. Start with [`CONTRIBUTING.md`](CONTRIBUTING.md) and the implementation plan referenced at the top of this file.

## License

MIT © Deepu K Sasidharan
