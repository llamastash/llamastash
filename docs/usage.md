# LlamaDash usage

This is the reference for the non-interactive CLI surface and the TUI keybindings. The runtime contract — exit codes, JSON shapes, env vars — is part of the public surface; pin against the documented forms rather than parsing human output.

## Concepts

**Single binary, three roles.** `llamadash` (no args) opens the TUI. `llamadash daemon ...` controls the background daemon. Every other subcommand (`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`) is a CLI client.

**Daemon on demand.** The first TUI or CLI client that runs auto-spawns the daemon if no socket is present. The daemon survives client exit; running models survive daemon shutdown via process detach. Pass `--no-spawn` to fail fast against a missing daemon (useful in scripts).

**Model references.** `start`, `stop`, `logs`, `presets`, `favorites` all accept the same model reference: an absolute path, a canonical model id, or a case-insensitive substring of the file name or its parent directory. Ambiguous references exit `66` with a disambiguation list.

## Configuration

LlamaDash reads `$XDG_CONFIG_HOME/llamadash/config.yaml` (macOS: `~/Library/Application Support/llamadash/config.yaml`). A fully-annotated sample lives at [`config.example.yaml`](../config.example.yaml) — copy it to the path above and edit.

Resolution order (highest wins): `--config <PATH>` → `LLAMADASH_CONFIG` env var → the XDG path above.

All keys are optional; missing keys fall back to defaults. Unknown top-level keys are ignored (forward-compat); unknown *values* within a known key error noisily.

### Schema

```yaml
# Built-in: macchiato (default) | latte | gruvbox-dark |
# solarized-dark | mono. Use `custom` to activate `custom_theme:`.
theme: macchiato

# Optional user-defined palette. Active when `theme: custom`. Every
# slot is optional and inherits from `base` (default macchiato).
custom_theme:
  base: macchiato
  is_dark: true
  bg: "#1A1B26"
  fg: "#C0CAF5"
  accent: "#BB9AF7"
  on_accent: "#1A1B26"
  panel_title: "#FFC777"
  label: "#7DCFFF"
  muted: "#565F89"
  selection: "#283457"
  highlight: "#FFC777"
  success: "#9ECE6A"
  warning: "#FF9E64"
  error: "#F7768E"
  status_loading: "#FFC777"
  status_ready: "#9ECE6A"
  status_error: "#F7768E"
  status_stopped: "#565F89"
  status_external: "#7DCFFF"

model_paths:                # Extra dirs to scan. Repeatable on the CLI as -p/--model-path.
  - /opt/llms

port_range:                 # Default 41100..=41300. Inclusive.
  start: 41100
  end: 41300

llama_server_path: /usr/local/bin/llama-server  # Overridable by --llama-server / env var.

disable_scan: false         # Equivalent to LLAMADASH_NO_SCAN=1.
disable_default_cache_paths:
  huggingface: false
  ollama: false
  lm_studio: false

probe_timeout_secs: 120     # Per-launch health-probe deadline.

keybindings:                # Action-name → key-spec overrides.
  quit: ctrl+q
  cycle_theme: T
  toggle_help: f1
```

### Custom theme

Set `theme: custom` and define a `custom_theme:` block to ship a personal palette. The slot list mirrors the internal `Palette` struct so every visible region is rebindable:

| Slot | What it paints |
|---|---|
| `bg` | Panel background (the root paint between bordered Blocks) |
| `fg` | Primary text |
| `accent` | Panel borders + active tab strip |
| `on_accent` | Text drawn on top of `accent` (title bar). Pin to a dark colour on mono-style themes where `bg` is `reset`. |
| `panel_title` | Block-title text — ` Host `, ` Daemon `, ` Models ` |
| `label` | In-panel label prefixes (`CPU`, `socket`, …) and list group headers (`★ Favorites`, folder paths) |
| `muted` | Secondary text + hint separators |
| `selection` | Reserved surface tone (used by future overlays) |
| `highlight` | Selected-row background in the Models list. Set to `reset` to fall back to `Modifier::REVERSED`. |
| `success` / `warning` / `error` | Per-state row colours + gauge tiers |
| `status_loading` / `status_ready` / `status_error` / `status_stopped` / `status_external` | Status-glyph colours in the model list |

Colour syntax (case-insensitive):

- 6-digit hex with leading `#`: `"#1A1B26"`, `"#c0caf5"` — quote in YAML since `#` starts a comment.
- ANSI names: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`/`grey`, `darkgray`, `lightred`, `lightgreen`, `lightyellow`, `lightblue`, `lightmagenta`, `lightcyan`, `white`.
- `reset` / `default` — fall through to the terminal's default colour.

Missing slots inherit from the `base:` theme (defaults to macchiato). Bad colour values log a warning and the slot keeps the base value rather than dropping the whole palette.

Once defined, the `Custom` theme joins the `t:theme` cycle alongside the built-ins.

### Custom keybindings

Each entry in `keybindings:` rebinds one action. Action names accept both snake_case and kebab-case. The key spec dialect:

- Bare characters: `q`, `?`, `/`, `Q` (uppercase implies `shift+`).
- Modifier chains: `ctrl+q`, `shift+tab`, `alt+enter`, `ctrl+shift+r`. Recognised modifiers: `ctrl`/`control`, `shift`, `alt`/`meta`, `super`/`cmd`.
- Named keys: `enter`/`return`, `esc`/`escape`, `tab`, `backtab`, `space`, `backspace`/`bs`, `up`/`down`/`left`/`right`, `home`, `end`, `pgup`/`pageup`, `pgdn`/`pagedown`, `delete`/`del`, `insert`/`ins`, `f1`–`f12`.

Override semantics mirror kdash: the action's existing default binding(s) are removed across every focus that used the action, and the new binding is inserted in those same focuses. Any binding that previously used the new key spec in those focuses is dropped to keep dispatch unambiguous. Unknown action names and unparseable specs log a warning at startup; the rebind is dropped, the rest of the keymap survives.

| Action name | Default key | Where it fires |
|---|---|---|
| `quit` | `q`, `ctrl+c` | List focus |
| `move_up` / `move_down` | `↑`/`k`, `↓`/`j` | List, right pane |
| `page_up` / `page_down` | `PgUp` / `PgDn` | List |
| `go_top` / `go_bottom` | `g` / `G` | List |
| `open_filter` | `/` | List |
| `clear_filter` | `Esc` | Filter input |
| `toggle_favorite` | `f` | List |
| `open_launch_picker` | `Enter` | List |
| `open_advanced_panel` | `a` | List, launch picker |
| `submit` | `Enter` | Filter, picker, advanced |
| `cancel` | `Esc` | Picker, advanced |
| `yank_url` / `yank_curl` / `yank_path` | `y` / `Y` / `p` | List |
| `cycle_theme` | `t` | List |
| `toggle_help` | `?` | List, right pane |
| `stop_model` | `s` | List |
| `kill_daemon` | `Q` (shift+q) | List — triggers a confirmation popup |
| `focus_list` | `Esc`, `Shift+M` | Right pane / tab inputs |
| `focus_logs_tab` | `Shift+L` | List, right pane — gated on a running model |
| `focus_chat_tab` | `Shift+C` | List, right pane — mode-appropriate (Chat / Embed / Rerank), gated on a running model |
| `focus_settings_tab` | `Shift+S` | List, right pane — always available |
| `next_focus` / `prev_focus` | `Tab` / `Shift+Tab`, `→`/`l`, `←`/`h` | Cross-pane navigation |
| `enter_edit` / `exit_edit` | `e` / `Esc` | Right pane → tab input |
| `send_chat` | `Enter` (Shift+Enter inserts newline on kitty-protocol terminals) | Chat input |
| `toggle_think_collapse` | `Ctrl+r` | Chat input |
| `toggle_auto_scroll` | `s` | Right pane (Logs) |
| `stage_rerank_candidate` | `Tab` | Rerank input |

### Environment variables

| Variable | Purpose |
|---|---|
| `LLAMADASH_CONFIG` | Override config-file path |
| `LLAMADASH_LLAMA_SERVER` | Path to `llama-server` |
| `LLAMADASH_NO_SCAN` | Skip filesystem scanning |
| `LLAMADASH_SOCKET` | Point a CLI at a non-default daemon socket |

## Top-level flags

These work on every subcommand (clap marks them `global`):

```
--config <PATH>            Path to YAML config (overrides LLAMADASH_CONFIG).
--llama-server <PATH>      Path to llama-server binary.
-p, --model-path <DIR>     Extra dir to scan. Repeatable.
--no-scan                  Disable filesystem scanning.
--no-spawn                 Fail fast if the daemon is not running.
-v, --verbose              Debug logging.
```

## Subcommands

### `llamadash list`

Print every discovered model.

```
llamadash list [--json] [--filter <PATTERN>]
```

- `--json` emits a stable JSON array; pin agents against this.
- `--filter` is a case-insensitive substring matched against name, path, arch, and quant.

### `llamadash start <model-ref>`

Launch a model. Layered resolution: catalog row → optional preset → per-invocation flags → trailing raw `llama-server` flags after `--`.

```
llamadash start <ref> [--preset NAME] [--ctx N] [--port N]
                     [--reasoning on|off] [--mode chat|embedding|rerank]
                     [-- <llama-server-flags>...]
```

Modes are strict: when the catalog reports `mode_hint = unknown` and no `--mode` is passed, the CLI exits `64` rather than silently defaulting to chat.

`--ctx` above the model's native context length is allowed (the supervisor still tries, per R12); a warning prints to stderr.

### `llamadash stop <target>` / `llamadash stop --all`

Stop a managed launch by `<launch_id>` (e.g. `L3`), by port, or — for unmanaged processes the daemon surfaced — by `ext-<pid>` or bare PID.

```
llamadash stop <target>     # exit 68 on failure, 66 on no match
llamadash stop --all [-y]   # confirms unless -y is set
```

### `llamadash status [target]`

Snapshot of daemon health, managed launches, external (unmanaged) `llama-server` processes, and the GPU backend. `--json` mirrors the daemon's `status` IPC shape and adds a `daemon` block:

```json
{
  "daemon": {"pid": 4242, "uptime_seconds": 90, "active_connections": 1},
  "models": [...],
  "external": [...],
  "gpu": "CpuOnly"
}
```

### `LlamaDash logs <target>`

Tail (or follow) a launch's log file.

```
LlamaDash logs <target> [-n N] [-f]
```

`-f` polls `logs_tail` and de-dupes against a rolling window. SIGINT exits cleanly with code `0`. `BrokenPipe` (e.g. piping to `head`) also exits `0`. Daemon disconnect during follow exits `65`.

### `llamadash presets <model-ref> <action>`

```
llamadash presets <ref> list [--json]
llamadash presets <ref> save <NAME> [--ctx N] [--port N]
                                   [--reasoning on|off] [--mode <m>]
                                   [-- <flags>...]
llamadash presets <ref> delete <NAME>
llamadash presets <ref> show <NAME>
```

`save` overwrites an existing preset (the response reports `replaced: <old-params>` so callers can audit). Presets live under `$XDG_STATE_HOME/llamadash/state.json`.

### `llamadash favorites`

```
llamadash favorites list [--json]
llamadash favorites add <ref>
llamadash favorites remove <ref>
```

### `llamadash daemon`

```
llamadash daemon start [--detach]
llamadash daemon stop
llamadash daemon status        # PID + uptime + connections + managed launches
```

`start --detach` double-forks into the background; without it the daemon stays in the foreground.

## TUI keybindings

These are the defaults. Override any binding via the `keybindings:` block in `config.yaml` — see [Custom keybindings](#custom-keybindings) above for the dialect and the action-name table.

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
| `Shift+M` / `Shift+L` / `Shift+C` / `Shift+S` | Jump focus to Models / Logs / Chat / Settings respectively. `L` and `C` only fire when the focused model is running. |

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
| `Esc` / `Shift+Tab` / `Shift+M` | Return focus to the list |
| `Shift+L` / `Shift+C` / `Shift+S` | Jump to Logs / Chat / Settings tab. `L` and `C` are gated on a running model. |
| `s` | Toggle Logs auto-scroll |

### Chat tab (`Focus::ChatInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit prompt buffer |
| `Enter` | Send prompt |
| `Shift+Enter` | Insert newline (only on kitty-protocol terminals; collapses to send elsewhere) |
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
