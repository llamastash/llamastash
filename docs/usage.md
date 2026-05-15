# llamatui usage

This is the reference for the non-interactive CLI surface and the TUI keybindings. The runtime contract — exit codes, JSON shapes, env vars — is part of the public surface; pin against the documented forms rather than parsing human output.

## Concepts

**Single binary, three roles.** `llamatui` (no args) opens the TUI. `llamatui daemon ...` controls the background daemon. Every other subcommand (`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`) is a CLI client.

**Daemon on demand.** The first TUI or CLI client that runs auto-spawns the daemon if no socket is present. The daemon survives client exit; running models survive daemon shutdown via process detach. Pass `--no-spawn` to fail fast against a missing daemon (useful in scripts).

**Model references.** `start`, `stop`, `logs`, `presets`, `favorites` all accept the same model reference: an absolute path, a canonical model id, or a case-insensitive substring of the file name or its parent directory. Ambiguous references exit `66` with a disambiguation list.

## Configuration

llamatui reads `$XDG_CONFIG_HOME/llamatui/config.yaml` (macOS: `~/Library/Application Support/llamatui/config.yaml`). Fields:

```yaml
theme: macchiato            # macchiato | latte | gruvbox-dark | solarized-dark | mono
model_paths:                # Extra dirs to scan. Repeatable on the CLI as -p/--model-path.
  - /opt/llms
port_range:                 # Default 41100..=41300. Inclusive.
  start: 41100
  end: 41300
disable_scan: false         # Equivalent to LLAMATUI_NO_SCAN=1.
disable_default_cache_paths:
  huggingface: false
  ollama: false
  lmstudio: false
keybindings: {}             # Optional override map (planned).
```

### Environment variables

| Variable | Purpose |
|---|---|
| `LLAMATUI_CONFIG` | Override config-file path |
| `LLAMATUI_LLAMA_SERVER` | Path to `llama-server` |
| `LLAMATUI_NO_SCAN` | Skip filesystem scanning |
| `LLAMATUI_SOCKET` | Point a CLI at a non-default daemon socket |

## Top-level flags

These work on every subcommand (clap marks them `global`):

```
--config <PATH>            Path to YAML config (overrides LLAMATUI_CONFIG).
--llama-server <PATH>      Path to llama-server binary.
-p, --model-path <DIR>     Extra dir to scan. Repeatable.
--no-scan                  Disable filesystem scanning.
--no-spawn                 Fail fast if the daemon is not running.
-v, --verbose              Debug logging.
```

## Subcommands

### `llamatui list`

Print every discovered model.

```
llamatui list [--json] [--filter <PATTERN>]
```

- `--json` emits a stable JSON array; pin agents against this.
- `--filter` is a case-insensitive substring matched against name, path, arch, and quant.

### `llamatui start <model-ref>`

Launch a model. Layered resolution: catalog row → optional preset → per-invocation flags → trailing raw `llama-server` flags after `--`.

```
llamatui start <ref> [--preset NAME] [--ctx N] [--port N]
                     [--reasoning on|off] [--mode chat|embedding|rerank]
                     [-- <llama-server-flags>...]
```

Modes are strict: when the catalog reports `mode_hint = unknown` and no `--mode` is passed, the CLI exits `64` rather than silently defaulting to chat.

`--ctx` above the model's native context length is allowed (the supervisor still tries, per R12); a warning prints to stderr.

### `llamatui stop <target>` / `llamatui stop --all`

Stop a managed launch by `<launch_id>` (e.g. `L3`), by port, or — for unmanaged processes the daemon surfaced — by `ext-<pid>` or bare PID.

```
llamatui stop <target>     # exit 68 on failure, 66 on no match
llamatui stop --all [-y]   # confirms unless -y is set
```

### `llamatui status [target]`

Snapshot of daemon health, managed launches, external (unmanaged) `llama-server` processes, and the GPU backend. `--json` mirrors the daemon's `status` IPC shape and adds a `daemon` block:

```json
{
  "daemon": {"pid": 4242, "uptime_seconds": 90, "active_connections": 1},
  "models": [...],
  "external": [...],
  "gpu": "CpuOnly"
}
```

### `llamatui logs <target>`

Tail (or follow) a launch's log file.

```
llamatui logs <target> [-n N] [-f]
```

`-f` polls `logs_tail` and de-dupes against a rolling window. SIGINT exits cleanly with code `0`. `BrokenPipe` (e.g. piping to `head`) also exits `0`. Daemon disconnect during follow exits `65`.

### `llamatui presets <model-ref> <action>`

```
llamatui presets <ref> list [--json]
llamatui presets <ref> save <NAME> [--ctx N] [--port N]
                                   [--reasoning on|off] [--mode <m>]
                                   [-- <flags>...]
llamatui presets <ref> delete <NAME>
llamatui presets <ref> show <NAME>
```

`save` overwrites an existing preset (the response reports `replaced: <old-params>` so callers can audit). Presets live under `$XDG_STATE_HOME/llamatui/state.json`.

### `llamatui favorites`

```
llamatui favorites list [--json]
llamatui favorites add <ref>
llamatui favorites remove <ref>
```

### `llamatui daemon`

```
llamatui daemon start [--detach]
llamatui daemon stop
llamatui daemon status        # PID + uptime + connections + managed launches
```

`start --detach` double-forks into the background; without it the daemon stays in the foreground.

## TUI keybindings

These are the v1 defaults. Config-driven overrides are planned.

### Global / list focus

| Key | Action |
|---|---|
| `q` / `Ctrl+C` | Quit |
| `↑` / `k`, `↓` / `j` | Navigate |
| `PgUp` / `PgDn` | Page |
| `g` / `G` | Top / bottom |
| `/` | Open filter (Enter applies, Esc clears) |
| `f` | Toggle favorite on focused model |
| `Enter` | Open launch picker on focused model |
| `a` | Open advanced flags panel |
| `y` / `Y` / `p` | Yank URL / curl / model path |
| `t` | Cycle theme |
| `Tab` | Move focus to right pane |

### Launch picker

| Key | Action |
|---|---|
| `Enter` | Dispatch `start_model` with the picked params |
| `Tab` | Next field |
| `a` | Open advanced flags overlay |
| `Esc` | Cancel |

### Right pane

| Key | Action |
|---|---|
| `Tab` | Cycle tab (Logs → Chat / Embed / Rerank when Ready) |
| `Esc` / `Shift+Tab` | Return focus to the list |
| `s` | Toggle Logs auto-scroll |

### Chat tab (`Focus::ChatInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit prompt buffer |
| `Ctrl+Enter` | Send prompt |
| `Ctrl+r` | Toggle `<think>` block collapse |

### Embed tab (`Focus::EmbedInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit input |
| `Enter` | Call `/v1/embeddings` |

### Rerank tab (`Focus::RerankInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit current field |
| `Tab` | Stage candidate buffer, or cycle between Query and Candidate fields |
| `Enter` | Call `/v1/rerank` |
