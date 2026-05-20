# LlamaStash usage

This is the reference for the non-interactive CLI surface and the TUI keybindings. The runtime contract — exit codes, JSON shapes, env vars — is part of the public surface; pin against the documented forms rather than parsing human output.

## Concepts

**Single binary, three roles.** `llamastash` (no args) opens the TUI. `llamastash daemon ...` controls the background daemon. Every other subcommand (`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`) is a CLI client.

**Daemon on demand.** The first TUI or CLI client that runs auto-spawns the daemon if no socket is present. The daemon survives client exit; running models survive daemon shutdown via process detach. Pass `--no-spawn` to fail fast against a missing daemon (useful in scripts).

**Model references.** `start`, `stop`, `logs`, `presets`, `favorites` all accept the same model reference: an absolute path, a canonical model id, or a case-insensitive substring of the file name or its parent directory. Ambiguous references exit `66` with a disambiguation list.

## Configuration

LlamaStash reads `$XDG_CONFIG_HOME/llamastash/config.yaml` (macOS: `~/Library/Application Support/llamastash/config.yaml`). A fully-annotated sample lives at [`config.example.yaml`](../config.example.yaml) — copy it to the path above and edit.

Resolution order (highest wins): `--config <PATH>` → `LLAMASTASH_CONFIG` env var → the XDG path above.

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

disable_scan: false         # Equivalent to LLAMASTASH_NO_SCAN=1.
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
| `next_focus` / `prev_focus` | `→`/`l`, `←`/`h` | Cross-pane navigation (arrows + vim-style; `Tab` is reserved for field cycling) |
| `next_field` / `prev_field` | `Tab` / `Shift+Tab` | Right pane (Settings tab cycles ctx/reasoning/advanced) · Rerank input (cycles Query/Candidate) |
| `enter_edit` / `exit_edit` | `e` / `Esc` | Right pane → tab input |
| `send_chat` | `Enter` (Shift+Enter inserts newline on kitty-protocol terminals) | Chat input |
| `toggle_think_collapse` | `Ctrl+r` | Chat input |
| `toggle_auto_scroll` | `s` | Right pane (Logs) |
| `stage_rerank_candidate` | `Tab` | Rerank input — stages the candidate buffer and advances to the next field |

### Environment variables

| Variable | Purpose |
|---|---|
| `LLAMASTASH_CONFIG` | Override config-file path |
| `LLAMASTASH_LLAMA_SERVER` | Path to `llama-server` |
| `LLAMASTASH_NO_SCAN` | Skip filesystem scanning |
| `LLAMASTASH_SOCKET` | Point a CLI at a non-default daemon socket |
| `LLAMASTASH_OFFLINE` | Refuse any outbound network from `init` / `pull` / `doctor` (equivalent to `--offline` on those subcommands) |
| `NO_COLOR` | Any non-empty value disables ANSI styling on every human-readable output (per [no-color.org](https://no-color.org/)). An empty value (`NO_COLOR=`) does **not** disable. |

## Top-level flags

These work on every subcommand (clap marks them `global`):

```
--config <PATH>            Path to YAML config (overrides LLAMASTASH_CONFIG).
--llama-server <PATH>      Path to llama-server binary.
-p, --model-path <DIR>     Extra dir to scan. Repeatable.
--no-scan                  Disable filesystem scanning.
--no-spawn                 Fail fast if the daemon is not running.
--no-colors                Disable ANSI styling on human-readable output.
-v, --verbose              Debug logging.
```

The colored-output policy OR-es three off-conditions: `--no-colors`, `NO_COLOR` env (non-empty), or non-TTY stdout. Any one silences colors. `--json` output is byte-stable regardless — pin agents against `--json`, not against the human form.

## Subcommands

### `llamastash list`

Print every discovered model.

```
llamastash list [--json] [--filter <PATTERN>]
```

- `--json` emits a stable JSON array; pin agents against this.
- `--filter` is a case-insensitive substring matched against name, path, arch, and quant.

### `llamastash start <model-ref>`

Launch a model. Layered resolution: catalog row → optional preset → per-invocation flags → trailing raw `llama-server` flags after `--`.

```
llamastash start <ref> [--preset NAME] [--ctx N] [--port N]
                     [--reasoning on|off] [--mode chat|embedding|rerank]
                     [-- <llama-server-flags>...]
```

Modes are strict: when the catalog reports `mode_hint = unknown` and no `--mode` is passed, the CLI exits `64` rather than silently defaulting to chat.

`--ctx` above the model's native context length is allowed (the supervisor still tries, per R12); a warning prints to stderr.

### `llamastash stop <target>` / `llamastash stop --all`

Stop a managed launch by `<launch_id>` (e.g. `L3`), by port, or — for unmanaged processes the daemon surfaced — by `ext-<pid>` or bare PID.

```
llamastash stop <target>     # exit 68 on failure, 66 on no match
llamastash stop --all [-y]   # confirms unless -y is set
```

### `llamastash status [target]`

Snapshot of daemon health, managed launches, external (unmanaged) `llama-server` processes, and the GPU backend. `--json` mirrors the daemon's `status` IPC shape and adds a `daemon` block:

```json
{
  "daemon": {"pid": 4242, "uptime_seconds": 90, "active_connections": 1},
  "models": [...],
  "external": [...],
  "gpu": "CpuOnly"
}
```

### `LlamaStash logs <target>`

Tail (or follow) a launch's log file.

```
LlamaStash logs <target> [-n N] [-f]
```

`-f` polls `logs_tail` and de-dupes against a rolling window. SIGINT exits cleanly with code `0`. `BrokenPipe` (e.g. piping to `head`) also exits `0`. Daemon disconnect during follow exits `65`.

### `llamastash presets <model-ref> <action>`

```
llamastash presets <ref> list [--json]
llamastash presets <ref> save <NAME> [--ctx N] [--port N]
                                   [--reasoning on|off] [--mode <m>]
                                   [-- <flags>...]
llamastash presets <ref> delete <NAME>
llamastash presets <ref> show <NAME>
```

`save` overwrites an existing preset (the response reports `replaced: <old-params>` so callers can audit). Presets live under `$XDG_STATE_HOME/llamastash/state.json`.

### `llamastash favorites`

```
llamastash favorites list [--json]
llamastash favorites add <ref>
llamastash favorites remove <ref>
```

### `llamastash last-params [<ref>]`

Surfaces the daemon's record of "what params did I last successfully start this model with" so an operator (or agent) can relaunch with the same shape via `start`. No `<ref>` lists every recorded model; with a ref, the output is filtered to that model.

```
llamastash last-params [<ref>] [--json]
```

`--json` wraps rows in `{"last_params": [...]}`. Exit `64` if `<ref>` resolves to a model with no recorded params yet — launch it once to populate.

### `llamastash daemon`

```
llamastash daemon start [--detach]
llamastash daemon stop
llamastash daemon status        # PID + uptime + connections + managed launches
```

`start --detach` double-forks into the background; without it the daemon stays in the foreground.

## Setup subcommands

These three are first-run and admin surfaces. They're separated from the runtime CLI above because they touch durable state on disk (the `llama-server` binary, the snapshot file, the user's config) and have their own exit-code contract.

### `llamastash init`

Six-step first-run wizard: detect hardware → install `llama-server` → pick + download a starter GGUF → write `config.yaml` with `arch_defaults` → smoke launch → handoff. Interactive by default (built on `cliclack`); per-step pre-answer flags let agents drive every prompt non-interactively.

```
llamastash init [--recommended] [--yes] [--json] [--offline]
               [--only <STEPS>] [--skip <STEPS>]
               [--install <CHOICE>] [--model <CHOICE>]
               [--config-step <CHOICE>]
```

| Flag | Effect |
|---|---|
| `--recommended` | Accept the hardware-aware default for every prompt; no prompts fire. Canonical form. |
| `--yes` | Hidden permanent alias for `--recommended`. Preserved for backward compatibility with scripts and agents that already pass it. |
| `--json` | Emit a structured summary (schema: `schema_version`, `steps_ran`, `steps_skipped`, `install`, `model`, `config`, `smoke`, `hardware`) and skip all human prose. |
| `--offline` | Refuse outbound network. Useful for `--only config` / `--only server` reruns where the model and snapshot are already cached. `LLAMASTASH_OFFLINE=1` is equivalent. |
| `--only <STEPS>` | Comma-separated list of `server,models,config` (other names rejected). Only the listed steps run. |
| `--skip <STEPS>` | Inverse of `--only`. Mutually exclusive with it (clap refuses both). |
| `--install <CHOICE>` | Pre-answer the install-method prompt. Values: `brew`, `gh-releases`, `existing`, `custom:<PATH>`. Override beats `--recommended`. |
| `--model <CHOICE>` | Pre-answer the model-pick prompt. Values: `recommended`, `none`, `<owner>/<repo>[:<filename>.gguf]`. |
| `--config-step <CHOICE>` | Pre-answer the config-write confirm. Values: `write`, `skip`. (Named `--config-step` rather than `--config` because the top-level `--config <PATH>` is already global.) |

The three per-step flags are **advisory, not authoritative**: supplying `--install brew` for a step that `--skip server` already excludes emits one stderr warning and proceeds. Conflicting axes don't abort.

Non-interactive contract: when stdout isn't a terminal and `--recommended` is not set, the wizard emits one consolidated stderr warning, then the install + model steps use recommended defaults silently. The config-write step refuses to proceed without explicit consent — pass `--recommended`, `--config-step write`, or `--config-step skip`. Without that consent the wizard aborts with exit `72` after persisting whatever durable state earlier steps already wrote (so `doctor` sees the partial baseline).

### `llamastash doctor`

Read-only diagnostic. Re-runs hardware detection, diffs against `_init_snapshot.json`, and emits 0-6 findings with stable ids agents can branch on: `binary_missing`, `binary_digest_drift` (skipped on brew installs — routine `brew upgrade` legitimately rotates the digest), `hardware_drift`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`.

```
llamastash doctor [--json]
```

`doctor` **always exits 0** — findings are informative, not a failure signal. Branch on a non-empty `findings` array (or filter for `severity == "error"`) to escalate, not on the exit code. This makes `doctor` safe to run unconditionally from health-check loops without `set -e` blowing up.

Each `--json` finding carries `{id, severity, message, fix_hint, safe_to_log}`. `safe_to_log: true` on every v2 finding means the output is safe to paste into a public issue.

### `llamastash pull <repo>`

HuggingFace pull primitive. Built on the `hf-hub` crate. Accepts `<owner>/<repo>` (downloads every GGUF file in the repo) or `<owner>/<repo>:<filename>.gguf` (single file). Honors `HF_TOKEN` for gated repos.

```
llamastash pull <repo> [--json] [--offline]
```

`--json` emits `{"repo", "revision", "files": [...], "total_bytes"}`. Exit `69` on any failure (network, disk, integrity).

`pull` performs a disk-space precheck by HEADing each file before download, so an out-of-space failure surfaces before any bytes hit disk. It refuses to write the HF token to disk in cache-file modes that would persist it insecurely.

## Exit codes

Source of truth: `src/cli/exit_codes.rs`. Codes are part of the public CLI contract; pin against them rather than parsing human error strings.

| Code | Constant | Meaning |
|---|---|---|
| `0` | `SUCCESS` | Success |
| `64` | `USAGE` | Bad CLI usage — missing required arg, invalid flag combination, or config-load error. Clap also emits this on its own. |
| `65` | `DAEMON_UNREACHABLE` | Daemon socket missing, peer hung up, or call timed out |
| `66` | `MODEL_NOT_FOUND` | Model reference matched zero or multiple catalog rows; stderr carries a disambiguation hint |
| `67` | `LAUNCH_FAILED` | Daemon accepted `start_model` but the supervisor failed (probe timeout, port allocation, etc.) |
| `68` | `STOP_FAILED` | `stop` couldn't reach the target (daemon error or process gone) |
| `69` | `PULL_FAILED` | `pull` couldn't complete (network, integrity, disk space) |
| `70` | `BINARY_NOT_FOUND` | `llama-server` not on PATH, no `--llama-server` flag, `LLAMASTASH_LLAMA_SERVER` unset |
| `71` | `UNKNOWN` | Catch-all for unexpected errors that don't map to a documented class |
| `72` | `INIT_ABORTED` | `init` aborted before smoke — integrity check failed, archive defenses tripped, user declined confirm, or non-TTY config step without explicit consent |
| `73` | `INIT_DOWNLOAD_FAILED` | `init`'s model-download step failed (distinct from `PULL_FAILED` so agents branch on cause) |
| `74` | `INIT_SMOKE_FAILED` | `init`'s smoke phase failed (binary doesn't run cleanly under `--version`) |

`doctor` always exits `0` — severity lives in the findings array.

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
| `Tab` / `Shift+Tab` | Move focus across panes (arrows / `h` / `l` do the same) |
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
| `Tab` / `Shift+Tab` | In the Settings tab, cycles through form fields (ctx → reasoning → advanced). In other right-pane tabs, no-op — use arrows / `h` / `l` to navigate panes. |
| `→` / `l`, `←` / `h` | Cycle pane focus |
| `Esc` / `Shift+M` | Return focus to the Models list |
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
| `Tab` | Stage candidate buffer, or cycle to the next field (Query ↔ Candidate) |
| `Shift+Tab` | Cycle back to the previous field |
| `Enter` | Call `/v1/rerank` |

## Toasts

Transient status messages (yank confirmations, "nothing to stop" hints,
no-op cycle attempts, theme changes) surface as a short toast string in
the bottom-right of the active panel. Toasts:

- auto-clear after ~3 seconds (`TOAST_TTL` in `src/tui/app.rs`);
- stack one-at-a-time — a newer toast replaces the previous one
  rather than queueing;
- never appear over a modal popup (confirm dialog, help overlay,
  advanced flags) — those overlays paint on top, and the toast
  surfaces again once the overlay is dismissed.

A "terminal too small" placeholder takes over the whole frame when
the terminal drops below the rendering floor (40×10). The display
shows the current size + required minimum so resizing the window
gives immediate feedback.
