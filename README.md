# LlamaDash 🦙

A fast, keyboard-driven TUI **and** CLI for launching local `llama-server` (llama.cpp) instances.

> **Status: v2 work in progress.** v1 scope: [`docs/brainstorms/llamatui-requirements.md`](docs/brainstorms/llamatui-requirements.md), [v1 plan](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md). v2 scope: [v2 brainstorm](docs/brainstorms/2026-05-18-init-wizard-requirements.md), [v2 plan](docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md) — init wizard, doctor diagnostic, `llamadash pull` MVP, recommender.

## Why

Heavy abstractions (Ollama, LM Studio) hide llama.cpp; raw `llama-server` use is tedious. LlamaDash is a fast, transparent launcher that is also a first-class shell-tool surface for agents — one binary, daemon on demand, same primitives in the TUI and the CLI.

## What it does (v1)

- **Discovers GGUF models on disk** — your own paths plus HuggingFace, Ollama, and LM Studio caches — grouped by directory with live filesystem watching.
- **Surfaces rich GGUF metadata** — architecture, quantization, native context length, KV-cache-aware memory estimates.
- **Launches `llama-server`** through a keyboard-driven picker (context length, reasoning toggle, advanced flags); supports named per-model **presets** and **favorites**.
- **Supervises multiple concurrent models** with a health-probed state machine. Running models survive TUI exit.
- **Smoke-tests models** via a right-pane Chat / Embed / Rerank tab that hits the same OpenAI-compatible endpoints any external client would use.
- **Exposes a complete non-interactive CLI** — `list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `daemon`, `init`, `doctor`, `pull`. Every read command supports `--json`. Distinct exit codes per failure class.

## What's new in v2

- **`llamadash init`** — first-run setup wizard: detects hardware, installs `llama-server` (brew on macOS, GH Releases prebuilt on Linux), picks a starter GGUF tuned to your VRAM via a built-in recommender (Open-LLM-Leaderboard + Aider benchmarks bundled, daily-CI-refreshed), downloads via the [`hf-hub`](https://crates.io/crates/hf-hub) crate, writes a tuned `config.yaml`. By default it runs **interactively** through a [`cliclack`](https://crates.io/crates/cliclack)-powered stepped wizard; pass `--recommended` (or the hidden `--yes` alias preserved for legacy scripts) to accept every hardware-aware default without prompting. Three per-step flags pre-answer individual prompts without skipping the rest: `--install <brew|gh-releases|existing|custom:PATH>`, `--model <recommended|none|owner/repo>`, `--config-step <write|skip>`. `--json` / `--offline` / `--only` / `--skip` still apply for agent use and post-GPU-swap re-runs.
- **Colored CLI output** — every human-readable surface now renders success in green, errors in red, warnings in yellow, secondary text dim. Disable globally with `--no-colors`, `NO_COLOR=1`, or by piping stdout to a non-terminal; `--json` output is never colored.
- **`llamadash doctor`** — read-only diagnostic. Compares current state against the recorded init snapshot, emits typed findings (`binary_missing`, `hardware_drift`, `snapshot_stale`, …) each with a `→ fix with: llamadash init --only X` hint.
- **`llamadash pull <hf-repo>`** — graduated from the v1 stub. Downloads any GGUF repo into the canonical HF cache layout `llamadash list` already scans.
- **GPU-aware `arch_defaults` config block** — per-architecture launch flags (`qwen2`, `llama`, …) merged into your launch only when you haven't already supplied the flag yourself.

## Roadmap (post-v2)
- Custom themes
- HTTP and MCP surfaces (origin: R34).
- Anthropic API compatibility
- Maybe MLX and vLLM if it's easy to add
- Docker Ready
- **Per-PID VRAM attribution via NVML.** Today the right-pane block title surfaces per-model RAM + CPU%; per-model VRAM is reported only at the host level. Post-v2 unlocks per-launch VRAM via NVML's `nvmlDeviceGetComputeRunningProcesses` (Linux + Windows; AMD/Apple parity depends on upstream surface).
- Smoke phase 2 (daemon-mediated `/health` + chat-completion probe) — v2 ships phase 1 + `--version`.
- Range-resume on partial HF downloads (waits on a future `hf-hub` line that exposes a custom-`reqwest::Client` hook without a reqwest 0.13 transitive).

## Install

Pick whichever channel you prefer. All three install the same binary.

```bash
# macOS + Linux, one-shot
curl -fsSL https://llamadash.cli.rs/install.sh | sh

# Homebrew (macOS + Linuxbrew)
brew install llamadash-rs/llamadash/llamadash

# From crates.io (any platform with a Rust toolchain)
cargo install llamadash
```

The marketing site at [llamadash.cli.rs](https://llamadash.cli.rs) is a content-verified mirror of the install script published with each GitHub Release; for the most paranoid path, run the equivalent `curl ... github.com/llamadash-rs/llamadash/releases/latest/download/install.sh` directly.

You also need `llama-server` on your `PATH` (or pointed at via `--llama-server <path>` / `LLAMADASH_LLAMA_SERVER`). `llamadash init` will offer to install it for you on first run.

> **macOS release tarballs are not codesigned for 0.2.0.** The `curl | sh`, `brew`, and `cargo install` paths all avoid Gatekeeper quarantine. The only path that hits the quarantine flag is hand-unzipping a tarball from the GitHub Releases page; `xattr -d com.apple.quarantine ./llamadash` clears it once if you do.

### Build from source

```bash
git clone https://github.com/llamadash-rs/llamadash
cd llamadash
cargo install --path .
```

## Quickstart

```bash
# Open the TUI. Scans default caches; daemon auto-spawns on demand.
llamadash

# List discovered models (TSV by default, JSON for agents).
llamadash list
llamadash list --json | jq

# Launch a model by name, name substring, path, or canonical id.
llamadash start qwen-coder --ctx 16384 --reasoning on

# Drive a smoke-test request against the running endpoint.
curl -s http://127.0.0.1:41100/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model": "qwen-coder", "messages": [{"role": "user", "content": "hi"}]}'

# Stop it.
llamadash stop qwen-coder
```

Full subcommand reference: [`docs/usage.md`](docs/usage.md). Architecture and IPC contract: [`docs/architecture.md`](docs/architecture.md). When things go wrong: [`docs/troubleshooting.md`](docs/troubleshooting.md).

## CLI exit codes

Every non-interactive subcommand returns a documented exit code so agent scripts can branch on failure class. Pin against numbers, not message text — they are the public contract.

| Code | Meaning                                                                                  |
| ---- | ---------------------------------------------------------------------------------------- |
| `0`  | Success                                                                                  |
| `64` | Usage error (missing required arg, invalid combination — clap-emitted)                   |
| `65` | Daemon unreachable (socket missing, peer hung up, timeout)                               |
| `66` | Model reference matched zero or multiple models (stderr lists candidates)                |
| `67` | `start_model` failed at the supervisor (probe timeout, port allocation failure)          |
| `68` | `stop_model` / `stop_all` failed                                                         |
| `69` | `pull` download failed (transport, checksum, or HF cache write)                          |
| `70` | `llama-server` binary not found (`--llama-server`, `LLAMADASH_LLAMA_SERVER`, or `$PATH`) |
| `71` | Unexpected error (catch-all)                                                             |
| `72` | `init` aborted before substantive work — failed precondition, integrity check, or rate-limited GH API. Safe to re-run. |
| `73` | `init` download failed mid-step — disk space, transport, or HF cache write. Partial state recorded; re-run picks up where it stopped. |
| `74` | `init` smoke-launch failed — phase-1 dry-run exceeded VRAM ceiling, or `--version` probe returned non-zero. Binary is installed; re-run smoke with `init --only smoke` (v2.1) or use `llamadash doctor` to diagnose. |

> **Note on sysexits.h**: the numbers above are deliberately reused from `<sysexits.h>` for familiarity, but LlamaDash's _meanings_ diverge from the standard ones. Scripts that import `EX_NOHOST` (68) expecting "host unreachable" will get our "stop failed"; `EX_DATAERR` (65) is reused for "daemon unreachable", not "data error". Branch on LlamaDash's table above, not the libc constants.

## Configuration

LlamaDash reads `$XDG_CONFIG_HOME/llamadash/config.yaml` (macOS: `~/Library/Application Support/llamadash/config.yaml`). A fully-annotated sample lives at [`config.example.yaml`](config.example.yaml) — copy it to the path above and edit. The full schema reference is in [`docs/usage.md`](docs/usage.md#configuration).

Quick tour of the top-level keys:

| Key                           | What it controls                                                                                                                                                          |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `theme`                       | Built-in palette: `macchiato` (default), `latte`, `gruvbox-dark`, `solarized-dark`, `mono`. Set to `custom` to use the `custom_theme` block. Cycle live with `t:theme`.   |
| `custom_theme`                | User-defined palette. Inherits unspecified slots from `base:` (default macchiato). Accepts `#RRGGBB` hex or ANSI names. Once defined, `Custom` joins the `t:theme` cycle. |
| `model_paths`                 | Extra directories to scan for `.gguf` files. Merged with `-p/--model-path` and `LLAMADASH_MODEL_PATHS`.                                                                   |
| `disable_default_cache_paths` | Per-bucket toggles (`huggingface`, `ollama`, `lm_studio`) for the auto-walked caches.                                                                                     |
| `disable_scan`                | Skip filesystem scanning entirely. Same as `--no-scan` / `LLAMADASH_NO_SCAN=1`.                                                                                           |
| `port_range`                  | Inclusive `{start, end}` TCP range the supervisor picks from. Default `41100..=41300`.                                                                                    |
| `llama_server_path`           | Absolute path to `llama-server`. Overridable by `--llama-server` and `LLAMADASH_LLAMA_SERVER`.                                                                            |
| `probe_timeout_secs`          | Health-probe deadline per launch. Default `120`. Bump for 70B+ on slow disks.                                                                                             |
| `keybindings`                 | Action-name → key-spec overrides. Kdash-style dialect (`ctrl+q`, `shift+tab`, `f1`, …).                                                                                   |

Environment variables:

| Variable                 | Purpose                                    |
| ------------------------ | ------------------------------------------ |
| `LLAMADASH_CONFIG`       | Override config-file path                  |
| `LLAMADASH_LLAMA_SERVER` | Path to `llama-server`                     |
| `LLAMADASH_NO_SCAN`      | Skip filesystem scanning                   |
| `LLAMADASH_SOCKET`       | Point a CLI at a non-default daemon socket |
| `LLAMADASH_OFFLINE`      | Disable outbound network for `init`, `pull`, and `doctor` fetch paths. Accepts `true` / `false` when bound via clap's `--offline` flag; the runtime `fetch::offline_requested` check also accepts `1` / `yes` for compatibility with scripts that follow the `XDG`/`gh` convention. Equivalent to `--offline`. |
| `HF_TOKEN`               | HuggingFace API token. Read by `init` and `pull` only; never propagated into spawned `llama-server` children. Cache-file (`~/.cache/huggingface/token`) source is refused if its mode is group/world-readable. |
| `HF_ENDPOINT`            | Override the HuggingFace API endpoint host. Must be `https://` and on the HF-allowlist (`huggingface.co` and its LFS CDN); non-allowlisted values are refused. Default: `https://huggingface.co`. |

### Default scan paths

When `model_paths` and `--model-path` are empty, LlamaDash walks these caches automatically. Each bucket is independently toggleable via `disable_default_cache_paths.<bucket>: true` in `config.yaml`, or globally via `--no-scan` / `LLAMADASH_NO_SCAN=1`.

| Bucket      | Linux                                             | macOS                                                    |
| ----------- | ------------------------------------------------- | -------------------------------------------------------- |
| HuggingFace | `~/.cache/huggingface/hub`                        | `~/Library/Caches/huggingface/hub`                       |
| Ollama      | `~/.ollama/models`                                | `~/.ollama/models`                                       |
| LM Studio   | `~/.lmstudio/models`, `~/.cache/lm-studio/models` | `~/Library/Caches/LMStudio/models`, `~/.lmstudio/models` |

Files anywhere under these roots that end in `.gguf` (and aren't `.gguf.part`) get parsed and added to the catalog.

## Platforms

Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel). Windows is out of scope for v1.

## Related projects

- [`kdash`](https://github.com/kdash-rs/kdash) — Kubernetes dashboard TUI by the same author.
- [`jwt-ui`](https://github.com/jwt-rs/jwt-ui) — JWT decoder / encoder TUI by the same author.

## Contributing

Bug reports, design discussion, and PRs welcome. Start with [`CONTRIBUTING.md`](CONTRIBUTING.md) and the implementation plan referenced at the top of this file.

## AI Usage

Multiple AI Coding Harnesses and LLMs were heavily used to create this project.

## License

MIT © Deepu K Sasidharan
