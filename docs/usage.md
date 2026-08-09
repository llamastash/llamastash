# LlamaStash usage

This is the reference for the non-interactive CLI surface and the TUI keybindings. The runtime contract — exit codes, JSON shapes, env vars — is part of the public surface; pin against the documented forms rather than parsing human output.

## Concepts

**Single binary, three roles.** `llamastash` (no args) opens the TUI. `llamastash daemon ...` controls the background daemon. Every other subcommand (`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`) is a CLI client.

**Daemon on demand.** The first TUI or CLI client that runs auto-spawns the daemon if no socket is present. The daemon survives client exit; running models survive daemon shutdown via process detach. Pass `--no-spawn` to fail fast against a missing daemon (useful in scripts).

**Model references.** `start`, `stop`, `logs`, `presets`, `favorites` all accept the same model reference: an absolute path, a canonical model id, or a case-insensitive substring of the file name or its parent directory. Ambiguous references exit `66` with a disambiguation list.

## Platform requirements

LlamaStash runs on Linux (x86_64, aarch64), macOS (Apple Silicon, Intel), and Windows (x86_64).

**Windows.**

- **OS:** 64-bit Windows 11, or Windows 10 version 1809 (build 17763) or newer.
- **Terminal:** **Windows Terminal is recommended** for the TUI — it renders truecolor themes and the Unicode status/severity glyphs correctly. The legacy console (`conhost.exe`, the default window for `cmd.exe` and Windows PowerShell) is supported on 1809+ via ConPTY/VT, but glyph and color fidelity are lower. The `?` help overlay, theme cycling, and all chords work in either host.
- **PowerShell:** Windows PowerShell 5.1 (preinstalled) or PowerShell 7+.
- **Visual C++ Redistributable:** the bundled `llama-server` needs the Microsoft Visual C++ 2015–2022 Redistributable (x64). If `start` reaches `error` immediately with a `0xC0000005` crash in `MSVCP140.dll`/`VCRUNTIME140.dll`, install/update it with `winget install --id Microsoft.VCRedist.2015+.x64`.
- **GPU host panel:** vendor, VRAM total, and the unified-memory marker are detected via DXGI/D3D12. Live GPU utilization and temperature are not sampled on Windows yet, so those rows show `—`.

## Configuration

LlamaStash reads `$XDG_CONFIG_HOME/llamastash/config.yaml` on Linux (fallback `~/.config/llamastash/config.yaml`), `~/Library/Application Support/llamastash/config.yaml` on macOS, and `%APPDATA%\llamastash\config\config.yaml` on Windows. A fully-annotated sample lives at [`config.example.yaml`](../config.example.yaml) — copy it to the path above and edit. Run `llamastash config` to open the active path in `$EDITOR`, or `llamastash config bindings` to print every effective keybinding as YAML.

Resolution order (highest wins): `--config <PATH>` → `LLAMASTASH_CONFIG` env var → the platform path above.

All keys are optional; missing keys fall back to defaults. Unknown top-level keys are ignored (forward-compat); unknown _values_ within a known key — and unknown keys inside a `deny_unknown_fields` block like `[proxy]` — are rejected **loudly**: the command prints `config error: …` to stderr and exits `64` (`USAGE`) rather than silently using defaults. `init` (which rewrites the file) and `doctor` (which diagnoses setup) are exempt so a broken config can always be repaired. A _missing_ config file is not an error.

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

model_paths: # Extra dirs to scan. Repeatable on the CLI as -p/--model-path.
  - /opt/llms

port_range: # Default 41100..=41300. Inclusive.
  start: 41100
  end: 41300

backend: # Per-engine config, one block per backend. llama.cpp is the
         # always-on default (no enable toggle); lemonade + ds4 are
         # optional, each default-on when its own binary resolves.
  llamacpp:
    servers: # Build/binary variants. First = default (auto/no-device launches),
             # and the target of --llama-server / LLAMASTASH_LLAMA_SERVER. Each is
             # probed with --list-devices; every entry is its own selectable
             # server (no dedup across builds — CUDA/ROCm/Vulkan builds all list).
      - binary: /usr/local/bin/llama-server
      - binary: /opt/builds/cuda/llama-server
        name: cuda # Optional; else auto-derived (<backend>·<gpu_backend>).
    fit_ctx_floor: 16384 # Min --fit-ctx window. Env: LLAMASTASH_FIT_CTX_FLOOR.
    strict_fit: false # Refuse (vs degrade) an unplaceable --fit. Env: LLAMASTASH_STRICT_FIT.
    jinja: true # Emit --jinja every launch (tool calling). Config-only.
  ds4: # See §"ds4 backend" below.
    # servers: [{ binary: /opt/ds4/ds4-server }] # ds4-server path; else PATH.
    # enabled: # tri-state: unset=auto, true=force on, false=force off.
  lemonade:
    # servers: [{ binary: /opt/lemonade/lemond }] # lemond path; else PATH.
    # enabled: # tri-state (see ds4).
    # port: 13305 # lemond umbrella port.

disable_scan: false # Equivalent to LLAMASTASH_NO_SCAN=1.
disable_default_cache_paths:
  huggingface: false
  ollama: false
  lm_studio: false

gpu: # GPU probe tuning. Config-only; no CLI/env surface.
  enable_vulkan_probe: true  # Skip Vulkan fallback probe (vulkaninfo) when false.
  reprobe_interval_secs: 60  # Full vendor re-probe interval (0 = disabled).

daemon: # Lifecycle + host-metrics sampling. Config-only.
  idle_timeout_secs: 0      # Auto-shutdown after N idle seconds (0 = never).
  probe_interval_secs: 1    # Host-metrics tick interval (1..=60, factory 1; 0 resets to 1).

probe_timeout_secs: 120 # Per-launch health-probe deadline.

mouse_focus: false # Opt into mouse capture for click-to-focus / click-to-tab. Default off keeps native terminal text selection.

ascii_glyphs: false # Render the TUI with the 7-bit ASCII glyph fallback (status dots, severity markers, box borders) for fonts that show the Unicode set as tofu. `LLAMASTASH_ASCII=1` wins over this.

left_pane_ratios: [65, 100, 50, 35, 0] # Left (Models list) width % that `Alt+L` cycles through in wide mode; the right pane takes the remainder. 100 hides the right pane, 0 hides the list. Slot 0 is the startup default; the pick is session-only. At most 5 slots (extras ignored), each clamped 0..=100.

proxy: # OpenAI-compat proxy router. See §"Proxy
  enabled: true # (OpenAI-compatible listener)" below for
  ollama_compat:
    false # Opt in for full Ollama drop-in identity
    # ("Ollama is running" on `GET /`, default
    # port 11434). Off → "LlamaStash is
    # running", default port 11435.
  # port: 11435             # Pin to override the mode default.

keybindings: # Action-name → key-spec overrides.
  quit: ctrl+q
  cycle_theme: T
  toggle_help: f1
```

### Custom theme

Set `theme: custom` and define a `custom_theme:` block to ship a personal palette. The slot list mirrors the internal `Palette` struct so every visible region is rebindable:

| Slot                                                                                      | What it paints                                                                                              |
| ----------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `bg`                                                                                      | Panel background (the root paint between bordered Blocks)                                                   |
| `fg`                                                                                      | Primary text                                                                                                |
| `accent`                                                                                  | Panel borders + active tab strip                                                                            |
| `on_accent`                                                                               | Text drawn on top of `accent` (title bar). Pin to a dark colour on mono-style themes where `bg` is `reset`. |
| `panel_title`                                                                             | Block-title text — `Host`, `Daemon`, `Models`                                                               |
| `label`                                                                                   | In-panel label prefixes (`CPU`, `socket`, …) and list group headers (`★ Favorites`, folder paths)           |
| `muted`                                                                                   | Secondary text + hint separators                                                                            |
| `selection`                                                                               | Reserved surface tone (used by future overlays)                                                             |
| `highlight`                                                                               | Selected-row background in the Models list. Set to `reset` to fall back to `Modifier::REVERSED`.            |
| `success` / `warning` / `error`                                                           | Per-state row colours + gauge tiers                                                                         |
| `status_loading` / `status_ready` / `status_error` / `status_stopped` / `status_external` | Status-glyph colours in the model list                                                                      |

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

Override semantics mirror kdash: the action's existing default binding(s) are removed and the new binding is inserted with the same focus scope. Any binding that previously used the new key spec in those scopes is dropped to keep dispatch unambiguous. Unknown action names and unparseable specs log a warning at startup; the rebind is dropped, the rest of the keymap survives.

The keybinding scheme follows two policies:

- **Destructive actions live behind `Ctrl`** (stop, kill, restart, delete, cancel-download).
- **Cross-pane navigation lives behind `Shift`** (`Shift+M/L/C/E/R/S/P` jump to surfaces; `Shift+Tab` reverses pane cycle).

Bare letters are for tool actions (`f` favorite, `e` edit, `u/c/p` yank, `t` theme, `q` quit).

| Action name                             | Default key(s)                    | Where it fires                                                                     |
| --------------------------------------- | --------------------------------- | ---------------------------------------------------------------------------------- |
| `quit`                                  | `q` · `ctrl+c`                    | Nav focuses                                                                        |
| `toggle_help`                           | `?`                               | Nav focuses                                                                        |
| `cycle_theme`                           | `t`                               | Nav focuses                                                                        |
| `cycle_theme_prev`                      | `shift+t`                         | Nav focuses — walks the theme list in reverse                                      |
| `restart_daemon`                        | `ctrl+r`                          | Nav focuses — confirmation popup                                                   |
| `kill_daemon`                           | `ctrl+k`                          | List — confirmation popup                                                          |
| `stop_model`                            | `ctrl+s`                          | Nav focuses — confirmation popup                                                   |
| `delete_model`                          | `ctrl+d`                          | List — confirmation popup (refuses on a running launch)                            |
| `cancel_download`                       | `ctrl+x`                          | Nav focuses — confirmation popup                                                   |
| `move_up` / `move_down`                 | `↑` · `k`, `↓` · `j`              | Nav focuses, HF dialog                                                             |
| `page_up` / `page_down`                 | `PgUp` / `PgDn`                   | List                                                                               |
| `go_top` / `go_bottom`                  | `g` · `Home`, `G` · `End`         | List                                                                               |
| `open_filter`                           | `/`                               | List                                                                               |
| `clear_filter`                          | `Esc`                             | Filter input                                                                       |
| `toggle_favorite`                       | `f`                               | List                                                                               |
| `open_launch_picker`                    | `Enter`                           | List                                                                               |
| `open_hf_dialog`                        | `shift+p`                         | List — "Pull" mnemonic                                                             |
| `submit`                                | `Enter`                           | Filter, right pane, embed, rerank, confirm popup, HF dialog                        |
| `cancel`                                | `Esc`                             | Confirm popup, HF dialog                                                           |
| `yank_url` / `yank_curl` / `yank_path`  | `u`, `c` · `y`, `p`               | Nav focuses — `y` is a vi-style alias for `c`                                      |
| `next_focus` / `prev_focus`             | `Tab` · `l`, `Shift+Tab` · `h`    | Universal pane cycle (TUI focuses); vi aliases are nav-only                        |
| `focus_list`                            | `Esc` · `Shift+M`                 | Right pane / tab inputs                                                            |
| `focus_logs_tab`                        | `Shift+L`                         | Nav focuses — gated on a running model                                             |
| `focus_chat_tab`                        | `Shift+C` · `Shift+E` · `Shift+R` | Nav focuses — picks mode-appropriate tab (Chat / Embed / Rerank), gated on running |
| `focus_settings_tab`                    | `Shift+S`                         | Nav focuses — always available                                                     |
| `next_field` / `prev_field`             | `↓` / `↑`                         | Rerank input — cycles Query / Candidate                                            |
| `cycle_value_next` / `cycle_value_prev` | `→` / `←`                         | Right pane (Settings) — cycles the focused row's value (incl. the preset row, and the `server` row when a model has >1 compatible build) |
| `save_preset`                           | `Ctrl+P`                          | Save the settings in view as a named preset (name prompt → confirm). Settings pane always (the form, or a running model); Models list only on a running row |
| `enter_edit` / `exit_edit`              | `e` / `Esc`                       | Right pane → tab input                                                             |
| `send_chat`                             | `Enter`                           | Chat input                                                                         |
| `insert_newline`                        | `Shift+Enter`                     | All input focuses (kitty-protocol terminals only)                                  |
| `toggle_think_collapse`                 | `r`                               | Right pane (Chat tab)                                                              |
| `toggle_auto_scroll`                    | `s`                               | Right pane (Logs)                                                                  |
| `toggle_device`                         | `Space`                           | Right pane (Settings, launch picker Device row)                                    |

The "nav focuses" alias means `List` + `RightPane`; "input focuses" means `ChatInput` + `EmbedInput` + `RerankInput`; "TUI focuses" is both groups combined.

### Environment variables

| Variable                            | Purpose                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| ----------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `LLAMASTASH_CONFIG`                 | Override config-file path (single-file knob; the daemon writes here)                                                                                                                                                                                                                                                                                                                                                                            |
| `LLAMASTASH_CONFIG_DIR`             | Override the directory `paths::config_dir()` resolves to; `user_config_file()` becomes `<dir>/config.yaml`. Empty value = unset                                                                                                                                                                                                                                                                                                                 |
| `LLAMASTASH_STATE_DIR`              | Override the directory `paths::state_dir()` resolves to (state.json, daemon.pid, init_snapshot.json, runtime.json). Empty value = unset                                                                                                                                                                                                                                                                                                         |
| `LLAMASTASH_CACHE_DIR`              | Override the directory `paths::cache_dir()` resolves to; `log_dir()` inherits as `<dir>/logs`. Empty value = unset                                                                                                                                                                                                                                                                                                                              |
| `LLAMASTASH_LLAMA_SERVER`           | Path to `llama-server` (sets the first `backend.llamacpp.servers[]` entry)                                                                                                                                                                                                                                                                                                                                                                      |
| `LLAMASTASH_NO_SCAN`                | Skip filesystem scanning                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `LLAMASTASH_IPC_URL`                | Point a CLI/TUI at a non-default daemon control plane (verbatim URL, e.g. `http://127.0.0.1:48134`). Must be set together with `LLAMASTASH_IPC_TOKEN`; partial overrides are rejected. Bypasses `runtime.json` lookup entirely.                                                                                                                                                                                                                 |
| `LLAMASTASH_IPC_TOKEN`              | Bearer token for the control-plane URL. See `LLAMASTASH_IPC_URL`.                                                                                                                                                                                                                                                                                                                                                                               |
| `LLAMASTASH_OFFLINE`                | Refuse any outbound network from `init` / `pull` / `recommend` (equivalent to `--offline` on those subcommands). Truthy values `1` / `true` / `yes` (case-insensitive) enable it; `0`, an empty value, and unset leave it off.                                                                                                                                                                                                                  |
| `LLAMASTASH_DEFAULT_LAUNCH_MODE`    | Seed mode for knobs no layer supplied: `auto` (default — delegate to `--fit`) or `inherited` (leave unset, llama-server's own default). Overrides `default_launch_mode` in config. Invalid values are logged and ignored.                                                                                                                                                                                                                       |
| `LLAMASTASH_FIT_CTX_FLOOR`          | `--fit-ctx` floor in tokens passed to fit-capable `llama-server` (overrides `backend.llamacpp.fit_ctx_floor`). Validated `1..=1048576`; a non-numeric or out-of-range value is logged and the factory `16384` is used.                                                                                                                                                                                                                          |
| `LLAMASTASH_STRICT_FIT`             | Set to `"1"` to refuse (rather than degrade) a launch `--fit` could not place as requested. OR-ed with the `backend.llamacpp.strict_fit` config field.                                                                                                                                                                                                                                                                                         |
| `LLAMASTASH_ASCII`                  | Render the TUI with the 7-bit ASCII glyph fallback instead of the default Unicode house style (status dots, severity markers, gauge bars, box borders, the logo banner). Truthy values `1` / `true` / `yes` enable it; this **wins over** the `ascii_glyphs` config field. For terminals / fonts that show the Unicode set as tofu. Keyboard-symbol hint labels (`↑ ↓ ⏎ ⇧ ↹`) stay Unicode — they're present in every monospace terminal font.   |
| `HF_HOME`                           | Honored by `init::download::hf_cache_dir()` per HuggingFace convention; controls where pulled GGUFs land                                                                                                                                                                                                                                                                                                                                        |
| `NO_COLOR`                          | Any non-empty value disables ANSI styling on every human-readable output (per [no-color.org](https://no-color.org/)). An empty value (`NO_COLOR=`) does **not** disable.                                                                                                                                                                                                                                                                        |
| `LLAMASTASH_BENCH_DISABLE_DEFAULTS` | **Maintainer / bench-internal.** When set to `"1"`, the launch-knob resolver skips presets, last-used, yaml-arch, and compiled-in arch defaults — only knobs the caller explicitly supplied land on the wire. Used by `scripts/bench/` to make `llamastash start` produce byte-identical argv to raw `llama-server` for fair Suite-A overhead comparison. **Do not set in normal use** — it disables the auto-tuning the launcher exists to do. |

The three `LLAMASTASH_*_DIR` overrides make it possible to run side-by-side daemons (each writes its own `runtime.json` under its state dir) without colliding on state / cache / config paths.

### Pinning a HuggingFace revision

`llamastash init --recommended --model owner/repo --revision <SHA-or-branch>` threads the `--revision` value into hf-hub's `Repo::with_revision` so the byte-stream resolves at the supplied commit. Empty values are rejected at parse time. Use this when you need a reproducible model download — agents pinning environments should always pass a SHA rather than relying on the repo's default branch.

### Preferring a Vulkan `llama-server` build

LlamaStash does **not** block you from using a Vulkan-built
`llama-server` on hardware that normally probes as another backend
(for example an AMD ROCm machine). If `init` already installed a model
or pulled one into the cache, you can point launches at a Vulkan build
by overriding the binary path:

```bash
# One-off run
LLAMASTASH_LLAMA_SERVER=/path/to/llama.cpp/build-vulkan/bin/llama-server \
  llamastash start qwen

# Or set it once in config.yaml
backend:
  llamacpp:
    binary: /path/to/llama.cpp/build-vulkan/bin/llama-server
```

This changes the **runtime binary**, not the detected host backend. So
`init`, host metrics, and UAT preflight may still report the machine as
`amd` / `nvidia` while the actual launched server is the Vulkan build.
That combination already works as long as the Vulkan binary itself can
load the model on your system.

## Top-level flags

These work on every subcommand (clap marks them `global`):

```
--config <PATH>            Path to YAML config (overrides LLAMASTASH_CONFIG).
--llama-server <PATH>      Path to llama-server binary.
-p, --model-path <DIR>     Extra dir to scan. Repeatable.
--no-scan                  Disable filesystem scanning.
--no-spawn                 Fail fast if the daemon is not running.
--no-colors                Disable ANSI styling on human-readable output.
--mouse-focus              Opt into TUI mouse capture (click-to-focus / click-to-tab). ORs with `mouse_focus` in `config.yaml`; there's no negating counter-flag.
-v, --verbose              Debug logging.
```

The colored-output policy OR-es three off-conditions: `--no-colors`, `NO_COLOR` env (non-empty), or non-TTY stdout. Any one silences colors. `--json` output is byte-stable regardless — pin agents against `--json`, not against the human form. `--help` follows the same policy: it shows styled section headers and flags on a TTY and stays plain bytes when piped, `NO_COLOR` is set, or `--no-colors` is passed.

Report-style commands (`list`, `status`, `presets list`, `favorites list`, `last-params`, `daemon status`) render padded + colored tables on a TTY and plain tab-separated rows when piped. The padded form is purely a human affordance; the TSV path stays byte-stable so existing `awk -F\t` / `column -t` pipelines keep working unchanged. Action-style commands (`daemon start/stop`, `start`, `stop`) keep their single-line shape but pick up value-color highlights on launch-id / port / pid / state when colors are enabled.

## Subcommands

### `llamastash config`

Opens the active config-file path in the executable named by `$EDITOR` and waits for it to exit. The same `--config <PATH>` and `LLAMASTASH_CONFIG` resolution order applies. It can open a malformed or missing config file so you can repair or create it.

`llamastash config bindings` prints every effective binding as a `keybindings:` YAML block in stable key order. Configured bindings replace their default values; every unset action prints its default primary key. Redirect it to copy the bindings to another config: `llamastash config bindings > bindings.yaml`. The config format accepts one key spec per action, so actions with multiple default aliases export their primary key.

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
llamastash start <ref> [--preset NAME] [--ctx N] [--port N] [--wait]
                     [--reasoning on|off] [--mode chat|embedding|rerank]
                     [--backend auto|ds4|llamacpp|lemonade] [--server <id>]
                     [--<advanced-knob> ...] [-- <llama-server-flags>...]
```

`--backend` defaults to `auto` (picks the engine by model identity — a DeepSeek-V4 GGUF routes to the [ds4 backend](#ds4-backend) when available, everything else to llama.cpp). Override it to force a specific engine.

`--server <id>` picks a specific **server** — one build/binary of a backend (`llamacpp-vulkan`, `llamacpp-cuda`, `ds4` or a named `ds4-rocm`). It determines which binary spawns and, when `--backend` is unset, which backend runs the model (the server's owning backend). Server ids auto-derive as `<backend>-<compute>` from each build's own device names (or the bare backend id for a device-less engine like ds4/lemonade), overridable with a per-server `name:`; list them from `status` (the `servers` array; `status --json` mirrors it). A `--device <selector>` already implies its owning server, so `--server` is for picking a build with no device pin. The pick persists in `last_params`, so a relaunch reuses it — in the TUI it reopens the launch picker's `server` row on that build.

Every typed knob the Settings editor exposes is also a first-class `start` flag — `--n-gpu-layers`, `--threads`, `--device`, `--tensor-split`, `--main-gpu`, `--split-mode`, `--flash-attn`, `--cache-type-k`/`-v`, `--batch-size`, `--mlock`, … Run `start --help` for the full list under **Advanced launch params** (the flags are generated from the same spec table the TUI uses, so the two surfaces can't drift). Booleans take `--flash-attn` (= on) or `--flash-attn=false`. Anything `start` doesn't recognise as a knob — including `llama-server`'s single-dash shorts like `-ngl` — still works verbatim after `--`. A knob set both inline and after `--` resolves to the `--` value.

Modes are strict: when the catalog reports `mode_hint = unknown` and no `--mode` is passed, the CLI exits `64` rather than silently defaulting to chat.

`--ctx` above the model's native context length is allowed (the supervisor still tries, per R12); a warning prints to stderr. When `--preset` and inline knobs are combined, the inline knobs layer onto the preset — they override only the fields they set, leaving the rest of the preset intact.

#### Auto launch mode (default)

By default LlamaStash does **not** pin GPU layers or context size. It delegates GPU/CPU placement and context sizing to llama-server's `--fit`, so an oversized model loads partially offloaded instead of OOMing, and keeps memory-budget authority itself: a launch that would not fit the sampled free memory is refused before spawn (with the demand, the effective free, and what to do about it) rather than letting two concurrent models exhaust the pool. This requires a fit-capable `llama-server`.

Every knob has three states:

- a pinned value (`--n-gpu-layers 50`, `--ctx 16384`) — used verbatim;
- `auto` (`--n-gpu-layers auto`, `start --ctx auto`, or the Auto stop in the TUI knob cycle) — delegated to `--fit`;
- unset (Inherited) — falls through presets / arch defaults / the server default.

`backend.llamacpp.fit_ctx_floor` (default 16384) is the minimum context `--fit` is told to keep. Set `default_launch_mode: inherited` to opt the whole machine back to the pre-Auto behavior (knobs you never touch fall through to llama-server's own defaults instead of `--fit`). See the config schema and the environment-variable table above for `default_launch_mode`, `backend.llamacpp.fit_ctx_floor`, and `backend.llamacpp.strict_fit`.

#### `--wait` (block until the launch settles)

`start` is fire-and-forget by default: it returns as soon as the daemon accepts the launch, while the model is still loading. Pass `--wait` to block until the launch reaches a terminal state (Ready / Error / Stopped) and report the fit-resolved context:

- **Ready** prints a `ready → ctx=N` follow-up under the headline (`N (clamped to fit-ctx floor)` when memory pressure clamped the window down to `fit_ctx_floor`).
- **Error** prints `failed → <cause>` and exits `67` (`LAUNCH_FAILED`), so scripts can branch on a load that was accepted but never came up.
- A 15-minute safety ceiling caps the wait; the daemon's own size-scaled probe budget normally flips a stuck load to Error well before that, after which it prints `waiting timed out → still loading; check llamastash status`.

`--wait --json` emits a single combined object — the launch fields plus `state`, `resolved_ctx`, `ctx_clamped`, and `cause` (on error) — instead of the immediate accept-time object.

### `llamastash stop <target>` / `llamastash stop --all`

Stop a managed launch by `<launch_id>` (e.g. `L3`), by port, by a case-insensitive substring of the running model's file name or parent dir (e.g. `stop qwen`), or — for unmanaged processes the daemon surfaced — by `ext-<pid>` or bare PID. A name substring that matches more than one running launch exits `66` with the candidate launch ids.

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
  "gpu": "CpuOnly",
  "proxy": {"enabled": true, "listen": "127.0.0.1:11434", "status": "listening", "bind_error": null, "ui_url": "http://127.0.0.1:11434/ui/"}
}
```

The `proxy` block is documented in detail under [Proxy → Is the proxy up?](#is-the-proxy-up).

On a host where more than one GPU backend reports a device (e.g. an
NVIDIA card seen via CUDA plus an AMD card via ROCm), `gpu` serialises
as a `multi` snapshot (`{"backend":"multi","devices":[…]}`) and the
`host` block carries a `gpu_devices` array with one per-device row
(name, backend, utilisation, temperature, memory) so dashboards can
render each card separately. Single-backend hosts keep the existing
per-vendor shape.

### `LlamaStash logs <target>`

Tail (or follow) a launch's log file. `<target>` is a `<launch_id>` (e.g. `L3`), a port, or a case-insensitive substring of the running model's file name / parent dir (e.g. `logs qwen`). An ambiguous name exits `66` with the matching launch ids.

```
LlamaStash logs <target> [-n N] [-f]
```

`-f` polls `logs_tail` and de-dupes against a rolling window. SIGINT exits cleanly with code `0`. `BrokenPipe` (e.g. piping to `head`) also exits `0`. Daemon disconnect during follow exits `65`.

### `llamastash presets <model-ref> <action>`

```
llamastash presets <ref> list [--json]
llamastash presets <ref> save <NAME> [--ctx N]
                                   [--reasoning on|off] [--mode <m>]
                                   [-- <flags>...]
llamastash presets <ref> delete <NAME>
llamastash presets <ref> show <NAME>
```

Named launch presets for a model. `save` is create-or-update (the response reports `replaced: <old-params>` so callers can audit). `list` shows the model's **effective** set; each row carries `source: "config"` and `is_default`. Apply one at launch with `llamastash start <ref> --preset <NAME>`.

Presets live in `config.yaml` under a `presets:` key, the single writable source. `save` / `delete` write there comment-safely. `state.json` does not carry or import presets.

A `presets:` key is classified per-resolution against your discovered models: a key that names a model (by file basename, or full path) is **per-model**; otherwise it is read as a GGUF `general.architecture` id and applies to **every model of that arch**. A model's effective set is its per-model entries ∪ its arch entries; the per-model entry wins on a name collision. The CLI writes per-model keys only — arch presets are hand-authored.

A `default:` under a key is the model's **standing launch config** (hand-edited; there is no set-default command). It auto-applies whenever you launch without picking something: a plain `llamastash start <model>` with no `--preset`, and proxy auto-start, both launch with the default. Precedence is `your inline flags > default preset > last-used params > arch defaults > fit`, so the default overrides your last manual launch but last-used still fills any knob the default leaves unset. Two reserved forms: `default: <name>` applies that preset; `default: auto` launches **pure fit** (ignores last-used and the default). With no `default:` set, last-used remains the implicit default (unchanged behavior).

Picking a preset explicitly (`start --preset <name>`, or the TUI cycle) overrides the default for that launch. `start --preset auto` is the clean per-launch "ignore everything, fit fresh" gesture. In the TUI, the preset cycle (`last used → auto → named…`) marks whichever stop is the configured default with `(default)` and opens on it, and the preset row shows the count of available presets (`preset (N)`).

An entry knob set to `auto` delegates that knob to llama-server's `--fit` (e.g. `n_gpu_layers: auto`); `auto` is a reserved token, so to pin a knob to the *literal* string value `auto`, use the escape `{ value: auto }`. The app writes entries in block style (flow `{ ctx: 8192 }` is also accepted when you hand-author). Presets carry no `port` (it is per-launch, auto-assigned). Changes the CLI/TUI make are live immediately; hand-edits to `config.yaml` need a `llamastash daemon restart` to be picked up. See `config.example.yaml` for the full shape.

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
llamastash daemon start [--foreground|-f]
llamastash daemon stop  [--force|-f]
llamastash daemon status [--json]   # PID + uptime + connections + managed launches
```

`daemon start` detaches into the background by default and returns once the socket is bound. Pass `--foreground` (or `-f`) to keep the daemon attached to the terminal — useful when a process supervisor (systemd, runit, container `CMD`) owns the lifecycle and needs to see stdout/stderr directly.

`daemon stop` calls the IPC `shutdown` RPC, then waits (up to 10 s) for the daemon process to actually exit before printing `daemon: stopped` — so `daemon stop && daemon start` never races the dying daemon's lockfile or its managed `lemond` umbrella. If teardown outlives the wait it falls back to `daemon: shutdown requested (still exiting, pid N)`. When `runtime.json` is missing (the IPC channel can't be opened because a stale daemon from an older version is holding the lockfile) pass `--force` (or `-f`) to fall back to a `SIGTERM` on the PID recorded in `daemon.pid`. The CLI auto-detects this state on every command and prints the exact `kill` / `--force` invocation needed.

`daemon status --json` emits the raw `version` IPC response (the same `{name, version, protocol_version, pid, uptime_seconds, connections}` object an agent would get by hitting the UDS directly). The plain form is a human key/value block and is not a stable machine contract — agents should always use `--json`.

## MTP speculative decoding

**MTP (multi-token prediction)** speeds up decoding by letting the model guess several tokens ahead and verifying them in one forward pass — roughly a **2x decode speedup** at high draft acceptance. It is **output-equivalent** to normal decoding (the model still verifies every token), so it is safe to leave on.

llamastash **auto-detects and enables it** for capable models. A model is MTP-capable when either:

- it carries an **embedded** draft head (`{arch}.nextn_predict_layers > 0` — Qwen3.5/3.6, GLM-4.x, DeepSeek), or
- a **separate** draft head sits next to it (the Gemma-4 shape, `mtp-*.gguf`, or a head named like a quant such as DeepSeek-V4's `…-MTP-Q4K-Q8_0-F32.gguf`).

Heads are identified by what is inside the file, not by its name, because the name is genuinely ambiguous: plenty of published *models* wear `-MTP-` to advertise embedded draft layers. A head is excluded from the model list and paired with the model it drafts for; a model that merely says MTP in its name stays launchable.

The `↯` glyph next to a model title (TUI) and the `mtp` block in `status` tell you whether MTP is capable and running: `enable` is your intent (auto/on/off), `active` is whether the serving backend actually dispatched with MTP, and `acceptance` is the latest draft-acceptance rate it reports (present once the model has served enough tokens; a backend that publishes no acceptance figures leaves it null).

### Controlling it

```bash
llamastash start <model>                 # auto: MTP on when capable (default)
llamastash start <model> --mtp off       # never use MTP for this launch
llamastash start <model> --mtp on        # force on (warns + skips if not capable)
llamastash start <model> --mtp-draft-n 5  # tokens drafted per step (backend default when unset)
```

`--mtp` is a **launch-only** setting (there is no `config.yaml` key for it); it persists in `last_params` and presets like any other launch choice. `--mtp-draft-n` works whichever backend serves the model. The TUI launch picker shows the same control as an `mtp` cycle row (auto/on/off), but only for MTP-capable models. Forcing it on a model that has no draft head **warns and skips** rather than failing the launch (emitting the flag blind is a hard server error). If you drive speculative decoding yourself through the `-- <extras>` tail, llamastash defers entirely and adds nothing.

Under the hood, each backend maps this onto its own flags — the serving backend enables speculation with the resolved draft head (and `--mtp-draft-n` when set), emitted **before** the fit step so context reservation stays MTP-aware. **DeepSeek-V4 on the ds4 backend** uses ds4's own `mtp` / `mtp_draft` / `mtp_margin` native knobs, auto-pairing a sidecar found next to the model. ds4 publishes no draft-acceptance figure, so `acceptance` stays null on a ds4 launch even while MTP is active.

ds4 cannot stream weights from disk and speculate at the same time — `ssd_streaming` and an MTP draft head are mutually exclusive in ds4-server. llamastash reconciles them before launching: whichever of the two it enabled on your behalf gives way (an auto-paired sidecar is dropped so a memory-pressured launch can still stream; auto-streaming is skipped so a head you asked for survives), and it refuses the launch up front when you set both explicitly. Watch for the notice in either direction.

### Getting the companion files

`llamastash pull <repo>` now also fetches a model's companion siblings — the **mmproj** projector (multimodal) and any **MTP draft head** — so a pulled model arrives ready to launch:

```bash
llamastash pull owner/repo:model.gguf                 # base + one companion per kind (default)
llamastash pull owner/repo:model.gguf --no-companions # base file only
llamastash pull owner/repo:model.gguf --all-companions # every projector precision / head
```

## ds4 backend

> **⚠️ Experimental.** ds4 support is new and lightly road-tested (validated on a single Strix Halo / ROCm host). Its behaviour, config keys, and defaults may change between releases. llama.cpp is the stable default and runs DeepSeek-V4 too on a current build (**b9840+**), so ds4 is never required — if anything here misbehaves, force llama.cpp with `--backend llamacpp` or `backend.ds4.enabled: false`.

[ds4](https://github.com/antirez/ds4) (antirez's DwarfStar) is a third backend: a direct, process-per-model engine that runs the `ds4-server` binary for the DeepSeek-V4 Flash/PRO GGUFs at [huggingface.co/antirez/deepseek-v4-gguf](https://huggingface.co/antirez/deepseek-v4-gguf). It is the purpose-built engine for those files (disk KV cache, SSD streaming); a current llama.cpp (**b9840+**) also runs DeepSeek-V4, so ds4 is preferred, never required.

> **Minimum llama.cpp version for these GGUFs.** DeepSeek-V4 support landed in llama.cpp **b9840** ([ggml-org/llama.cpp#24162](https://github.com/ggml-org/llama.cpp/pull/24162), merged 2026-06-29). On **b9840 or newer** — a release binary or a source build from that merge onward — llama.cpp loads antirez's Flash/PRO GGUFs; on anything older it fails immediately with `error loading model: unknown model architecture: 'deepseek4'`. This matters because ds4's "falls back to llama.cpp, never a refusal" (below) only degrades gracefully when your llama.cpp is new enough — an older `llama-server` turns that fallback into a hard load error. Point `backend.llamacpp.servers` at a b9840+ build if you rely on the fallback. (Note: on the llama.cpp backend, Flash Attention is currently auto-disabled for the deepseek4 graph; it loads and runs without it.)

**You supply the binary.** LlamaStash does not install ds4-server — build it from the repo (`git clone https://github.com/antirez/ds4 && cd ds4 && make`) and either put `ds4-server` on `PATH` or point `backend.ds4.servers` at it. ds4 is **default-on the moment the binary resolves**; it stays completely dormant when it doesn't (no discovery, no new JSON fields on other rows).

Enable / configure:

```yaml
backend:
  ds4:
    # binary: /opt/ds4/ds4-server   # explicit path; else `ds4-server` on PATH
    # enabled:                       # tri-state:
    #   (unset)  auto — on when the binary is found (the default)
    #   true     force on
    #   false    force off even when the binary is present
```

`--ds4` on `daemon start` and `LLAMASTASH_DS4=1` also force ds4 on (OR-merged with the config, and carried through the detached daemon re-exec).

### Which GGUFs run on ds4

Routing is automatic and keys on a header-level compatibility predicate — arch `deepseek4` **plus** ds4's quant contract (routed-expert tensors `ffn_*_exps` in `IQ2_XXS` / `Q2_K` / `Q4_K`, every other tensor in `F32` / `F16` / `Q8_0` / `I32`). Both published Flash/PRO variants pass; a generic third-party `deepseek4` K-quant does not and stays an ordinary llama.cpp model.

- A **compatible** GGUF launches on ds4 when ds4 is available and the mode is chat/completions.
- Otherwise it **falls back to llama.cpp** — never a refusal, on a **b9840+** llama.cpp (see the version note above); an older `llama-server` fails the load with `unknown model architecture: 'deepseek4'`.
- `start <model> --backend ds4` forces ds4 (it surfaces its own error if the file is a mismatch); `--backend llamacpp` forces llama.cpp on a compatible file. `--backend` accepts `auto` (default) | `ds4` | `llamacpp` | `lemonade`.
- `--mode embedding` / `--mode rerank` on a compatible model routes to llama.cpp — ds4 serves chat/completions only.
- The split PRO half-files (`…-Layers00-30.gguf` / `…-Layers-31-output.gguf`) are refused before spawn with "ds4 distributed mode unsupported"; use a single-file DeepSeek-V4 GGUF. Single-file PRO quants (e.g. the `…-Pro-IQ2XXS-…-Instruct` variants) are fine.

### ds4 native knobs

ds4-server takes nine backend-specific tunables that have no llama.cpp equivalent. Set them per-launch in the TUI launch picker or persist them in a preset; ds4 honors exactly one typed knob from the shared set — `ctx` (→ `--ctx`).

| Knob             | ds4-server flag      | What it does |
| ---------------- | -------------------- | ------------ |
| `power`          | `--power`            | GPU duty-cycle target, 1–100 (ds4 default 100) |
| `tokens`         | `--tokens`           | Default max output tokens when a client omits a limit |
| `threads`        | `--threads`          | CPU helper-thread count for host-side work |
| `kv_disk_dir`    | `--kv-disk-dir`      | Directory for ds4's persistent disk KV cache (see privacy note below) |
| `kv_disk_space_mb` | `--kv-disk-space-mb` | Disk KV cache budget in MB (ds4 default 4096 when enabled) |
| `ssd_streaming`  | `--ssd-streaming`    | Stream weights from disk (below-RAM-floor mode; skips the admission gate). Mutually exclusive with `mtp` |
| `mtp`            | `--mtp`              | Path to the MTP draft-head sidecar (auto-paired from a sibling when unset; see [MTP speculative decoding](#mtp-speculative-decoding)) |
| `mtp_draft`      | `--mtp-draft`        | Tokens drafted per step (also set by the neutral `--mtp-draft-n`) |
| `mtp_margin`     | `--mtp-margin`       | Acceptance margin for the draft verifier |

Any other ds4-server flag (`--kv-cache-*`, `--prefill-chunk`, …) rides the free-form extras tail after `--`, e.g. `start <model> -- --prefill-chunk 512`. The loopback/credential denylist still applies, extended for ds4 with `--cors` and `--dist-` — those are stripped/refused.

### Oversized models and below-floor hardware

The DeepSeek-V4 GGUFs are 81–300+ GB; the practical RAM floor is roughly 128 GB on CUDA/ROCm and 96 GB on Metal. On a box below the floor, full residency out-of-memories. LlamaStash handles this for you: when a ds4 launch's resident estimate (~1.25× the weights, covering the expert cache + KV) exceeds free memory, it **auto-enables `ssd_streaming`** before spawn and prints a one-line notice (`ds4 needs ~N GiB resident but only M is free — enabled SSD streaming`). ds4-server then streams weights from disk under a bounded cache instead of OOM-killing mid-load. Set the **`ssd_streaming` native knob** yourself to force streaming on, or `ssd_streaming: false` to force full residency and skip the auto-enable. The knob is also the one launch where the pre-spawn admission gate is skipped (the on-disk size no longer maps to memory demand); this bypass keys on the native knob only — an extras-spelled `--ssd-streaming` still hits the admission gate. DeepSeek-V4's KV cache is modeled from the header (its two-tier compressed cache, ~0.5 GiB at 16k ctx and ~11 GiB at 1M for Flash), so the admission estimate is realistic at long context; the auto-streaming notice above is the memory signal to watch when residency is tight.

Streaming rules out MTP speculation (ds4-server refuses the pair, and only after loading the whole model). When both would apply, llamastash drops whichever it enabled itself and says so; setting `ssd_streaming: true` and an `mtp` head together is refused before the load.

### The ds4 `/v1/models` menu

ds4-server advertises a **static two-entry list** on `/v1/models` — both `deepseek-v4-flash` and `deepseek-v4-pro` — no matter which GGUF is loaded, so a direct `curl` shows two models with one running. It is a fixed menu, not a report of the resident model. `/v1/chat/completions` serves the loaded model and **echoes back the `model` name you send** (no alias rewrite). LlamaStash's proxy publishes your real catalog (by file name) on its own `/v1/models` and forwards your request model verbatim, so through the proxy you request — and get back — the name you used. The right pane marks a ds4-routed model with a ` ds4 ` chip (backend identity only; no model-id remap to disclose).

### kv-disk cache privacy

`--kv-disk-dir` is ds4's own persistent cache, reused across restarts. LlamaStash never subdir-mangles or cleans it — it is entirely ds4-owned state. It durably holds conversation-derived data under ds4's own permissions (umask) at exactly the path you type, without any of LlamaStash's `0600` state-file hygiene. **Point it at a private, user-owned directory.**

## Proxy (OpenAI-compatible listener)

The daemon binds a single OpenAI-compatible HTTP proxy on `127.0.0.1:11435` (default mode) so any agent that speaks the OpenAI REST shape — OpenCode, Pi (pi.dev), the OpenAI SDKs, Cline, llm-cli — can talk to every discovered model through one stable URL. The default port is `11435` (one above Ollama's `11434`) so llamastash co-exists with an installed Ollama daemon without a collision. If the base port is taken the listener walks up to `11440` and binds the first free slot — the actual address is reported via `llamastash status` / the TUI Daemon pane under `proxy.listen`.

The installable Agent Skills bundle for this flow lives under [`skills/llamastash/`](https://github.com/llamastash/llamastash/tree/main/skills/llamastash). Claude Code, OpenClaw, OpenCode, and similar harnesses can install it by copying that directory into their configured skills path.

The proxy resolves `body.model` against the same fuzzy matcher `llamastash start <ref>` uses, forwards the request byte-for-byte to the matching `llama-server` child, and streams the response back. If the named model isn't running, the proxy auto-starts it (replaying `last_params`, else `arch_defaults`). A model that is already *loading* is waited on rather than started again, no matter which surface launched it, so a request arriving mid-load never yields a second copy of the same model. The launch mode follows the endpoint that triggered it (`/v1/embeddings` starts the model in embedding mode, `/v1/rerank` in rerank mode), then the recorded `last_params` mode, then the GGUF's own hint; a model whose hint says `chat` is never started in embedding or rerank mode, since that would lock it out of chat completions for its whole lifetime. If the launch fails and another model is already Ready, the proxy falls back to it and stamps `x-llamastash-served-by` + `x-llamastash-fallback-reason: launch_failed` headers on the response. Substitution is observable; no extra round-trip is needed to discover what served the request. The full mechanism — coalesced launches, family-MRU fallback selection, scope boundaries — is documented in [`docs/plans/2026-05-21-001-feat-proxy-router-plan.md`](https://github.com/llamastash/llamastash/blob/main/docs/plans/2026-05-21-001-feat-proxy-router-plan.md).

Routes served: `/v1/models`, `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/rerank`, the OpenAI `/v1/responses` (+ `/v1/responses/input_tokens`), and the Anthropic `/v1/messages` + `/v1/messages/count_tokens`.

### Anthropic-shape clients (Claude Code)

llama-server speaks the Anthropic Messages API natively, so the proxy forwards `/v1/messages` and `/v1/messages/count_tokens` on the same path as the OpenAI routes — no body translation. Point Claude Code (or anything that drives the Anthropic shape) at the proxy with `ANTHROPIC_BASE_URL` (no `/v1` suffix — the SDK appends `/v1/messages` itself):

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:11435 \
  ANTHROPIC_AUTH_TOKEN=llamastash \
  ANTHROPIC_MODEL=<discovered-model> \
  ANTHROPIC_SMALL_FAST_MODEL=<discovered-model> \
  claude
```

- **Set both model vars** to a discovered model name (not a `claude-*` name) so Claude Code's main and background calls both resolve through the proxy.
- **`llamastash init` writes these for you.** Its **Claude Code** integration drops a sourceable `~/.config/llamastash/claude-code.sh` with the `ANTHROPIC_*` exports (separate from the OpenAI `env.sh`); `source ~/.config/llamastash/claude-code.sh && claude` opts Claude Code into the proxy **for that shell only**. It deliberately does *not* write Claude Code's global `~/.claude/settings.json` (whose `env` block applies to every session) — so bare `claude` keeps using your real Anthropic models.
- **Auth.** Anthropic clients send the key in the `x-api-key` header; the proxy accepts it alongside `Authorization: Bearer` and browser `Basic`. On the keyless loopback default no key is needed (the token value is ignored, but Claude Code still wants one set). When you set `proxy.api_key` (or `LLAMASTASH_PROXY_API_KEY`), auth is enforced and `init`'s generated `env.sh` / `claude-code.sh` carry that real key (mode `0o600`) — so a client only authenticates once the script is sourced into its environment.
- **Tool calling** needs the backend launched with `--jinja`, which is on by default (`backend.llamacpp.jinja: true` in `config.yaml`; the reasoning toggle also forces it). Set `backend.llamacpp.jinja: false` only if you don't need tool use. Basic chat / streaming work either way. Some model templates (e.g. certain Qwen GGUFs) fail llama-server's tool-parser generation with `System message must be at the beginning`; override with `start <model> -- --chat-template-file <tool-compatible.jinja>` (or the crude `--chat-template chatml`), or use a GGUF whose template is tool-compatible.
- Compatibility is best-effort (it's llama-server's translation, not a full Anthropic spec implementation) — verify your client end-to-end.

### Web UI (`/ui`)

Open `http://127.0.0.1:11435/ui/` in a browser (swap in the actual `proxy.listen` port if it roamed) to use the running model's stock llama.cpp web UI through the proxy — one stable address, so you never have to look up the ephemeral backend port. Chat history persists across model switches because it's keyed to the browser origin, which never changes.

- **One model running:** `/ui/` opens its UI directly.
- **Several running:** `/ui/` shows a small chooser; pick one and the browser reloads onto it. The pick is remembered in a `ls_ui_target` cookie (scoped to `/ui`), so assets and chat requests stay pinned to that model. The chooser lists **running** models only; start a stopped one from the TUI / `llamastash start <model>` first.
- **None running:** `/ui/` shows a "no model running" page pointing you at the TUI / CLI.

**Switching models.** Once you've picked a model, `/ui/` keeps forwarding to it (that's the cookie pin). To pick a different one, open `http://127.0.0.1:11435/ui/switch` — it always re-shows the chooser and marks the model you're currently on. Bookmark it; the stock chat UI has no in-page switcher and llamastash deliberately doesn't inject one. You can also jump straight to a specific model with `http://127.0.0.1:11435/ui/?target=<launch-id>` (the `L1` / `L2` ids from `llamastash status`), which re-pins and reloads — this is exactly what the chooser links do under the hood.

`/ui` is reachable over [LAN](#lan-access-opt-in-behind-a-key) too. A browser can't send a bearer header by navigating, so when a key is configured the proxy answers `/ui` with `WWW-Authenticate: Basic`: the browser prompts once, you paste the proxy key as the **password** (any username), and it's remembered per-origin. Same key as the API path, no login page, no key-in-URL. On the keyless loopback default there's no prompt. As with the API, LAN mode is plaintext HTTP (no TLS yet), so the key crosses the wire as base64 — keep it on a trusted network.

### Ollama drop-in mode (opt-in)

The official `ollama` CLI (and other Ollama-Go-based clients) issue a `HEAD /` handshake before any `/api/*` call and bail when the body isn't the literal `"Ollama is running"`. Default mode answers that probe with `"LlamaStash is running"` so the identity is honest; opt in to full Ollama impersonation when the goal is "this tool that natively speaks Ollama just works":

| Source | Form                                         |
| ------ | -------------------------------------------- |
| CLI    | `llamastash daemon start --ollama-compat`    |
| Config | `proxy.ollama_compat: true` in `config.yaml` |
| Env    | `LLAMASTASH_OLLAMA_COMPAT=1`                 |

The three are OR-ed; any one of them turns compat mode on. Effects:

- `GET /` returns the byte-exact `"Ollama is running"` string Go-clients sometimes strcmp against.
- Default port shifts from `11435` → `11434` (Ollama's well-known port). Stop your real Ollama daemon first, or pin `proxy.port: <N>` (CLI: `--proxy-port N`) to avoid the collision.
- Everything else — OpenAI compat `/v1/...`, Ollama discovery `/api/...`, headers, error envelope — is identical to default mode.

Default mode (no compat) is fine when clients reach `/api/tags` directly without doing the handshake (`ollama-python`'s default code path, most IDE plugins, curl scripts). Compat mode is required when the client is `ollama` CLI or links the Ollama-Go SDK.

### LAN access (opt-in, behind a key)

By default the proxy binds `127.0.0.1` and runs keyless — same-machine threat model. To reach your models from another box, bind a routable address:

| Source | Form |
| ------ | ---- |
| CLI    | `llamastash daemon start --proxy-host 0.0.0.0` |
| Config | `proxy.host: 0.0.0.0` in `config.yaml` |
| Env    | `LLAMASTASH_PROXY_HOST=0.0.0.0` |

CLI beats env beats config. A specific NIC IP or an IPv6 address (`::`) work too. Only the proxy data plane moves — the control plane and `llama-server` children stay loopback.

Because an open proxy on the network would let anyone drive your GPU, a non-loopback bind **requires** a bearer key:

- On the first LAN-enabled `daemon start`, llamastash generates an `sk-llamastash-…` key, writes it to `proxy.api_key` in your config (atomic, mode `0600`), and prints it once. Send it as `Authorization: Bearer <key>`:

  ```bash
  curl http://<box-ip>:11434/v1/chat/completions \
    -H "Authorization: Bearer sk-llamastash-…" \
    -H "Content-Type: application/json" \
    -d '{"model":"<discovered-name>","messages":[{"role":"user","content":"hi"}]}'
  ```

- The daemon **refuses** to bind a non-loopback address with no key (`status.proxy.status: "refused_insecure"`; the daemon and control plane keep running). Resolve it by letting the CLI provision a key, setting `proxy.api_key`, or passing `--insecure-no-auth` / `proxy.insecure_no_auth: true` to deliberately run an unauthenticated LAN proxy. A loud warning prints either way.
- A configured key is enforced on every data route (`/v1/*`, `/api/*`) and the web UI (`/ui*`); the liveness probes `GET /` and `GET /health` stay open. API clients send `Authorization: Bearer <key>`; a browser hitting `/ui` gets a `WWW-Authenticate: Basic` challenge and pastes the **same key as the password** (see [Web UI](#web-ui-ui)). `LLAMASTASH_PROXY_API_KEY` overrides the config key for the process and is never written back to disk (containers / secret managers).

> **No TLS yet.** LAN mode is plaintext HTTP, so the bearer key is visible to anyone sniffing the network. Keep it on a trusted LAN, or put a TLS-terminating reverse proxy (caddy, nginx, …) in front. Native TLS is a planned follow-up.

### Connecting an agent

Set the OpenAI base URL to `http://127.0.0.1:11435/v1` (default mode) or `http://127.0.0.1:11434/v1` (Ollama-compat mode). On the default loopback bind the proxy ignores authentication, so any string works as the API key. If you exposed the proxy on the LAN ([LAN access](#lan-access-opt-in-behind-a-key)), put your `sk-llamastash-…` key in the client's API-key field instead: OpenAI-compatible clients send the API key as `Authorization: Bearer <key>`, which is exactly what the proxy validates, so no client-side change is needed beyond the key value. (For API clients the proxy expects `Authorization: Bearer`, not Azure-style `api-key:` headers — browsers hitting `/ui` get an `Authorization: Basic` challenge instead; Ollama-native clients hitting `/api/*` send no key, so they get a `401` once auth is on.) The base-URL pattern works with any OpenAI-compatible client; the standard env var names across the ecosystem are:

| Client                    | Env var(s)                                                                                           |
| ------------------------- | ---------------------------------------------------------------------------------------------------- |
| OpenAI SDK (Python, Node) | `OPENAI_BASE_URL` (Python) / `OPENAI_API_BASE` (legacy) and `OPENAI_API_KEY`                         |
| OpenCode                  | `OPENAI_API_BASE` and `OPENAI_API_KEY`, or the equivalent `openai.api_base` field in its config file |
| Pi (pi.dev)               | `OPENAI_API_BASE_URL` and `OPENAI_API_KEY` (their "OpenAI-compatible" guide)                         |
| Cline / llm-cli           | `OPENAI_BASE_URL` (or their tool-specific equivalent) and any key                                    |
| Claude Code (Anthropic)   | `ANTHROPIC_BASE_URL` (proxy origin **without** `/v1`) + `ANTHROPIC_AUTH_TOKEN`; see [Anthropic-shape clients](#anthropic-shape-clients-claude-code) |

Verify the exact env var name against the client's current docs if you're automating — names drift. The manual smoke runbook at [`tests/proxy_real_client_smoke.md`](https://github.com/llamastash/llamastash/blob/main/tests/proxy_real_client_smoke.md) carries the maintainer's verified OpenCode + Pi sequences.

#### OpenCode setup

Point OpenCode at the proxy's current `proxy.listen` address. The
default is `http://127.0.0.1:11435/v1`, but if that port is busy
llamastash will roam up to the next free port (for example `11436`), so
check `llamastash status --json | jq -r .proxy.listen` first.

```json
"llamastash": {
  "npm": "@ai-sdk/openai-compatible",
  "name": "llamastash proxy (local)",
  "options": {
    "baseURL": "http://127.0.0.1:11436/v1"
  },
  "models": {
    "Qwen3.6-27B-Q4_K_M": {
      "name": "Qwen3.6 27B Q4_K_M (via llamastash)",
      "limit": {
        "context": 262144,
        "output": 16384
      }
    },
    "Qwen3.6-27B-Q6_K": {
      "name": "Qwen3.6 27B Q6_K (via llamastash)",
      "limit": {
        "context": 262144,
        "output": 16384
      }
    }
  }
}
```

The model keys must match what you send in `body.model`; llamastash
will resolve that name against the catalog and auto-start the target if
needed.

##### Auto-populating the model list (avoid hand-listing)

Maintaining that `models` map by hand is the tedious part. Two ways to skip it:

**Generate it from `llamastash list --json`.** OpenCode has no native
`/v1/models` auto-discovery yet, and the proxy's `/v1/models` stays
OpenAI-standard (`id` / `object` / `created` / `owned_by`) with **no
capability field**, so nothing downstream can tell a chat model from an
embedding or reranker off that endpoint alone. `list --json` *does* carry a
per-model `mode_hint` (under the nested `metadata` block), so generate the block
from it and filter to just the chat models:

```bash
BASE="http://$(llamastash status --json | jq -r .proxy.listen)/v1"
llamastash list --json | jq --arg base "$BASE" '{
  provider: { llamastash: {
    npm: "@ai-sdk/openai-compatible",
    name: "llamastash (local)",
    options: { baseURL: $base },
    models: ( .models
      | map(select(.metadata.mode_hint == "chat"))
      | map({ (.name | sub("\\.gguf$"; "")):
              { name: (.name | sub("\\.gguf$"; "")),
                limit: { context: .metadata.native_ctx } } })
      | add )
  }}}'
```

The `.gguf` suffix is stripped so the keys match the ids the proxy advertises
on `/v1/models` (what `body.model` resolves against). Pipe the output into
`~/.config/opencode/opencode.json` (or `jq`-merge it into an existing file),
and re-run when your catalog changes — an alias or a `make` target keeps it a
one-liner. On an auth-enforced proxy add your `proxy.api_key` as `apiKey`
**inside `options`** (see the auth note below).

**Or discover dynamically.** The third-party
[`opencode-models-discovery`](https://github.com/yuhp/opencode-models-discovery)
plugin queries `/v1/models` at OpenCode startup, so new models appear without a
re-run. Because `/v1/models` has no type field, it can only separate chat from
embed/rerank by **name pattern** (`excludeBy` on ids like `embed` / `rerank` /
`whisper`), not the exact `mode_hint` the generator above uses.

> **Auth posture.** On the default loopback bind the proxy has **no authentication** — the threat model is "same machine, any UID can issue requests," so don't run llamastash on a shared host. Exposing it on the LAN ([LAN access](#lan-access-opt-in-behind-a-key)) requires a bearer key, which llamastash auto-provisions and enforces; the daemon refuses a non-loopback bind with no key unless you pass `--insecure-no-auth`. TLS is still a deferred follow-up, so LAN mode is plaintext (trusted network or reverse proxy). The control plane and `llama-server` children always stay loopback regardless.

### Is the proxy up?

```bash
llamastash status --json | jq .proxy
```

`host` is the bound IP (derived from `listen`); `auth` is `"enforced"` when a bearer key is required, `"none"` on the keyless loopback default, or `"required"` for `refused_insecure`. The key itself is never reported. Shape, all five states:

```json
// Listening on the configured port (keyless loopback default):
{ "enabled": true,  "listen": "127.0.0.1:11435", "host": "127.0.0.1", "status": "listening",       "auth": "none",     "bind_error": null, "ui_url": "http://127.0.0.1:11435/ui/" }
// Listening on the LAN with a bearer key required:
{ "enabled": true,  "listen": "0.0.0.0:11434",   "host": "0.0.0.0",   "status": "listening",       "auth": "enforced", "bind_error": null, "ui_url": "http://0.0.0.0:11434/ui/" }
// Config has proxy.enabled: false:
{ "enabled": false, "listen": null,              "host": null,        "status": "disabled",        "auth": "none",     "bind_error": null, "ui_url": null }
// All six ports in the scan range (port..=port+5) taken:
{ "enabled": true,  "listen": "127.0.0.1:11439", "host": "127.0.0.1", "status": "port_in_use",     "auth": "none",     "bind_error": null, "ui_url": null }
// Bind failed for some other reason (EACCES, EADDRNOTAVAIL, …):
{ "enabled": true,  "listen": "127.0.0.1:80",    "host": "127.0.0.1", "status": "unbound",         "auth": "none",     "bind_error": "permission denied", "ui_url": null }
// Non-loopback host requested with no key and no --insecure-no-auth (daemon stays up, proxy skipped):
{ "enabled": true,  "listen": "0.0.0.0:11434",   "host": "0.0.0.0",   "status": "refused_insecure", "auth": "required", "bind_error": "refused to bind a non-loopback proxy without authentication; set proxy.api_key or pass --insecure-no-auth", "ui_url": null }
```

The same block is on the IPC `status` method response. The TUI's Daemon info pane shows the proxy state on row 3 as `proxy <status> <addr>` (an authed LAN listener adds `(auth)`); a toast fires on the transition into `port_in_use` or `refused_insecure`. `proxy.enabled: false` renders the row as `proxy disabled`.

### Endpoints

The proxy speaks HTTP/1.1 only on `127.0.0.1:<port>` (no h2c upgrade, no ALPN-negotiated HTTP/2 — the underlying hyper build is feature-gated to `http1`). It answers exactly the surfaces below. Anything else — including `/v1/messages`, MCP, websocket transports, or native llama.cpp routes like `/completion` — returns 404.

| Method | Path                   | Behavior                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ------ | ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET`  | `/health`              | `{"status":"ok","models_loaded":<N>,"models_discovered":<M>}`. Cheap liveness probe; counts come from the supervisor registry (`models_loaded` = Ready) and the catalog (`models_discovered`). **Always returns 200** — the listener being up is the only signal this endpoint encodes. It does NOT report degraded states (zero Ready models, partial supervisor failures, etc.); poll `/v1/models` or `llamastash status --json` if you need that. |
| `GET`  | `/v1/models`           | OpenAI-shape `{"object":"list","data":[…]}` listing every discovered model. Each row carries `id` (the discovered display name), `object: "model"`, `created: 0` (no stable epoch — the catalog has no creation timestamp; documented choice), `owned_by: "llamastash"`. Sorted by `id` so the output is byte-stable across calls.                                                                                                                   |
| `POST` | `/v1/chat/completions` | OpenAI chat completions. Streaming (`stream: true`) is byte-piped end-to-end — SSE chunks reach the agent in the same order with the same framing the upstream `llama-server` emitted.                                                                                                                                                                                                                                                               |
| `POST` | `/v1/completions`      | OpenAI text completions. Same forwarding semantics.                                                                                                                                                                                                                                                                                                                                                                                                  |
| `POST` | `/v1/embeddings`       | OpenAI embeddings. JSON pass-through.                                                                                                                                                                                                                                                                                                                                                                                                                |
| `POST` | `/v1/rerank`           | llama.cpp's rerank endpoint (also exposed under the `/v1/` prefix for client uniformity). JSON pass-through.                                                                                                                                                                                                                                                                                                                                         |
| `GET`  | `/api/tags`            | **Ollama compat — discovery.** Ollama-shape `{"models":[{name, model, modified_at, size, digest, details:{format,family,parameter_size,quantization_level,…}}]}` projection of the discovered catalog. Sorted alphabetically by `name`. Empty catalog → `{"models":[]}`. See [Ollama-compat surface](#ollama-compat-surface).                                                                                                                        |
| `GET`  | `/api/version`         | **Ollama compat.** `{"version":"<crate-version>"}` — same value `status.daemon.build` surfaces.                                                                                                                                                                                                                                                                                                                                                      |
| `GET`  | `/api/ps`              | **Ollama compat.** Currently-Ready supervisors in Ollama's running-list shape (`{models:[…{expires_at, size_vram, …}]}`). `expires_at` is a far-future placeholder until idle-TTL eviction lands (R34 deferred); `size_vram` is `0` until per-PID VRAM attribution lands.                                                                                                                                                                            |
| `POST` | `/api/show`            | **Ollama compat.** `{"model":"<name>"}` or `{"name":"<name>"}` body → per-model metadata in Ollama shape (`{modelfile, parameters, template, details, model_info, capabilities}`). Same fuzzy resolver as `/v1/chat/completions`.                                                                                                                                                                                                                    |

Request body cap: **2 MiB**, enforced via `http-body-util::Limited` before forwarding. Anything larger returns HTTP 413. OpenAI chat completion requests are typically well under 1 MiB even with long histories; the cap is intentional rather than implicit.

### Ollama-compat surface

The four `/api/*` endpoints above let Ollama-shape discovery libraries — `ollama-python`'s default code path, IDE plugins that probe `GET /api/tags` to detect Ollama, `OLLAMA_HOST`-based env discovery in agent frameworks — recognise llamastash as Ollama-compatible. Once recognised, clients fall through to the OpenAI-compat surface (`/v1/chat/completions` etc.) for actual inference, which already works against llamastash without further changes. This unlocks OOB compatibility with anything that "speaks Ollama" for discovery but uses OpenAI shape for completions — the most common pattern in the agent ecosystem.

The Ollama **inference** endpoints (`POST /api/chat`, `POST /api/generate`, `POST /api/embed`) are **not** implemented in v1. They emit a different request/response shape than OpenAI compat (newline-delimited JSON streaming, different field names) and would require request/response body translation — incompatible with the proxy's current byte-pure forward path. Tracked in TODO §R2 as a brainstorm/plan item. For now, point Ollama-shape _inference_ clients at `OLLAMA_HOST=http://127.0.0.1:11434` and they will discover models via `/api/tags`, then fall through to the OpenAI-compat completion endpoints on those same client libraries that support both shapes (most do).

A few field-level details where llamastash's projection diverges from Ollama's:

- **`digest`** — Ollama uses `sha256:<hex>`; llamastash uses `blake3:<hex>` derived from the canonical path string of the discovered file. The value is stable across `/api/tags` and `/api/ps` for the same model — both endpoints hash the same path — so clients can join the two endpoints by digest. It is **not** the GGUF header BLAKE3 that `ModelId` carries internally; re-reading the header on every `/api/tags` row would brick discovery, and the catalog doesn't cache the header hash today. Lifting the digest to the truthful header BLAKE3 is tracked in [TODO §R2](https://github.com/llamastash/llamastash/blob/main/TODO.md) ("Ollama-compat digest from cached header BLAKE3"). Clients that round-trip the digest opaquely keep working; clients that _validate_ the algorithm see the truthful `blake3:` tag rather than a misleading `sha256:` prefix on a non-SHA-256 hash.
- **`size`** — Ollama returns the on-disk file size; llamastash returns `weights_bytes` (the GGUF tensor footprint), typically within a few KiB of the full file size. `0` when discovery couldn't parse the header.
- **`modified_at`** — llamastash doesn't track file mtime in the catalog. Emits `"1970-01-01T00:00:00Z"` (Unix epoch) as a placeholder so clients displaying this see a clearly-not-now sentinel.
- **`/api/ps` `expires_at`** — far-future placeholder (`"9999-12-31T23:59:59Z"`) while idle-TTL eviction is deferred (R34).
- **`/api/ps` `size_vram`** — always `0` until per-PID VRAM attribution lands (R2 brainstorm).

`POST /api/show` resolves the model reference (`body.model` or `body.name`) with the same fuzzy matcher `/v1/chat/completions` uses against `body.model`. Identical names work across both APIs — model `llama3:8b` resolves the same way on `/v1/...` and `/api/...`.

Hop-by-hop headers (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `Upgrade`, `Proxy-*`) are stripped in both directions. The upstream's response is streamed back unchanged otherwise — same status, same body bytes, same SSE timing modulo network scheduling.

### Response headers

On the happy path no `x-llamastash-*` headers are emitted; the response is byte-equivalent to what the upstream `llama-server` returned. The fallback path (launch failed → served from a different Ready model) tags the response with two headers so clients can audit:

| Header                         | Value                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `x-llamastash-served-by`       | The display name of the model that actually answered (e.g. `qwen2-7b-instruct-q4_k_m`). Only emitted on the fallback branch.                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `x-llamastash-fallback-reason` | Stable wire label. v1 emits `launch_failed` for **in-family** substitution (the picked supervisor's arch matches the requested model's arch — graceful degradation, response shape is what the client asked for) and `family_mismatch` for **cross-arch** fallback (the picked supervisor's arch differs from the request, or one side has no arch metadata — response shape is _not_ what the client asked for; embedding / rerank requests answered by a chat model will return chat-shaped output). Clients that care about output-shape parity should branch on this header. |

Family selection prefers the _requested_ model's `general.architecture` (matched exactly against running models' arch metadata), then falls through to any-MRU among Ready models. A model without arch metadata (synthetic GGUFs, etc.) skips the family-prefer step and goes straight to any-MRU, but the fallback reason still surfaces as `family_mismatch` so the client sees that the arch comparison was not satisfied.

### Error envelope

Every non-2xx response carries an OpenAI-shaped JSON body:

```json
{
  "error": {
    "type": "<wire-label>",
    "code": "<sub-discriminator>",
    "message": "<human-readable>",
    "matches": ["..."],
    "running": ["..."]
  }
}
```

`code` is present only when the sub-discriminator adds information beyond `type`. `matches` appears on disambiguation errors; `running` appears on `launch_failed` 503s. Other fields are omitted from the JSON when unset.

| HTTP | `type`                                                       | When                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ---- | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 400  | `invalid_request` (`code: model_required`, `param: "model"`) | `body.model` missing or empty.                                                                                                                                                                                                                                                                                                                                                                                                                             |
| 400  | `ambiguous_model`                                            | Fuzzy match returned >1 candidate. `matches` lists the candidate names; the client retries with a tighter reference.                                                                                                                                                                                                                                                                                                                                       |
| 400  | `invalid_request`                                            | Request body wasn't valid JSON, or the HTTP method couldn't be translated for forwarding.                                                                                                                                                                                                                                                                                                                                                                  |
| 404  | `model_not_found`                                            | Fuzzy match returned zero candidates. `matches` is omitted from the body when empty (the field is `Option`-shaped and serialised with `skip_serializing_if`).                                                                                                                                                                                                                                                                                              |
| 404  | `not_found`                                                  | No such route (unknown path _or_ wrong HTTP method on a known path — e.g. `GET /v1/chat/completions`).                                                                                                                                                                                                                                                                                                                                                     |
| 413  | `payload_too_large`                                          | Request body exceeded 2 MiB.                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 502  | `upstream_unreachable`                                       | The model was Ready a moment ago but the connect to `llama-server` failed (process exited between snapshot and forward, kernel-level refusal, …). The agent sees this rather than a hanging socket.                                                                                                                                                                                                                                                        |
| 503  | `launch_failed`                                              | Auto-start failed and no Ready models exist for fallback. `running: []` is always present on this arm. The list reflects models that were **in `Ready` state at the moment the proxy snapshotted the supervisor registry for fallback** — models in `Launching` / `Loading` are not included, so an empty list does not mean "the daemon has nothing alive," only "no candidate was available for instant fallback." Retry once the slow launch completes. |

Upstream non-2xx responses (e.g. `llama-server` returns 500 for a malformed completion request) are passed through verbatim — same status code, same body bytes; the OpenAI-shape envelope above only covers errors the proxy itself emits. Mid-stream upstream death: once headers are sent the routing decision is committed; if the upstream stream errors after that point, the proxy closes its connection to the agent (the agent sees a truncated SSE / chunked body) — no retry, no fallback.

### Configuration

```yaml
proxy:
  enabled:
    true # Default true. false => the daemon runs but no
    # listener is bound; status.proxy.status = "disabled".
  ollama_compat:
    false # Default false. true => GET / returns "Ollama is running"
    # (Go-client handshake) and the default port shifts to
    # 11434. See "Ollama drop-in mode" above. CLI: --ollama-compat;
    # env: LLAMASTASH_OLLAMA_COMPAT=1. All three sources are OR-ed.
  # port: 11435          # Pin to override the mode default. Omitted = derived from
  # ollama_compat (11434 when true, 11435 when false).
  # Loopback only — there is no `host` knob; LAN binding is
  # a deferred follow-up.
```

Unknown keys inside `[proxy]` are **rejected loudly** (`#[serde(deny_unknown_fields)]`) — a typo never silently falls back to defaults. The top-level config still tolerates unknown keys for forward-compat. There is no `host`, no `api_key`, no `tls_*`, no fallback-tuning knob; these are all deferred per the plan's Scope Boundaries.

`llamastash daemon start --proxy-port <PORT>` overrides the mode default for that daemon process — CLI flag beats config beats mode default. `--proxy-port 0` binds an ephemeral port; the actual address is reported via `llamastash status --json | jq .proxy.listen`. The flag survives the default detached start (the re-exec'd child receives it on its argv). `--ollama-compat` is similarly propagated.

Port collision (Ollama-compat mode against a running Ollama on `11434`, another listener on the base port, …) leaves the daemon up and reports `proxy.status: "port_in_use"`. Edit `proxy.port` and restart the daemon, or restart with `--proxy-port <free-port>`. The proxy does not auto-roam outside the `base..=base+5` scan window — that would break the "single stable URL" contract.

## Setup subcommands

These three are first-run and admin surfaces. They're separated from the runtime CLI above because they touch durable state on disk (the `llama-server` binary, the snapshot file, the user's config) and have their own exit-code contract.

### `llamastash init`

Six-step first-run wizard: detect hardware → install `llama-server` → pick + download a starter GGUF → write `config.yaml` with `arch_defaults` → smoke launch → handoff. Interactive by default (built on `cliclack`); per-step pre-answer flags let agents drive every prompt non-interactively.

```
llamastash init [--recommended] [--yes] [--json] [--offline]
               [--only <STEPS>] [--skip <STEPS>]
               [--install <CHOICE>] [--model <CHOICE>]
               [--config-step <CHOICE>]

llamastash init <step> [flags]   # run one step; <step> = server | models | config | integrations
```

Each step is also a first-class subcommand. `llamastash init server` is sugar for `llamastash init --only server`, with that step's pre-answer flag carried on the subcommand itself; the global flags (`--recommended`, `--json`, `--offline`, `--no-tui`) work on either side of it:

| Subcommand                 | Equivalent to                    | Step flag           |
| -------------------------- | -------------------------------- | ------------------- |
| `init server`              | `init --only server`             | `--install`         |
| `init models`              | `init --only models`             | `--model`, `--revision` |
| `init config`              | `init --only config`             | `--config-step`     |
| `init integrations`        | `init --only integrations`       | `--integrations`    |

Examples: `llamastash init server --install gh-releases`, `llamastash init models --json`, `llamastash init config --config-step write`. Bare `llamastash init` (no subcommand) still runs the full wizard and honors the `--only` / `--skip` flags.

| Flag                     | Effect                                                                                                                                                                  |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--recommended`          | Accept the hardware-aware default for every prompt; no prompts fire. Canonical form.                                                                                    |
| `--yes`                  | Hidden alias for `--recommended`. Preserved for script and agent compatibility.                                                                                         |
| `--json`                 | Emit a structured summary (schema: `schema_version`, `steps_ran`, `steps_skipped`, `install`, `model`, `config`, `smoke`, `hardware`) and skip all human prose.         |
| `--offline`              | Refuse outbound network. Useful for `--only config` / `--only server` reruns where the model and snapshot are already cached. `LLAMASTASH_OFFLINE=1` is equivalent.     |
| `--only <STEPS>`         | Comma-separated list of `server,models,config,integrations` (other names rejected). Only the listed steps run. Or run one step as a subcommand: `init server`.            |
| `--skip <STEPS>`         | Inverse of `--only`. Mutually exclusive with it (clap refuses both).                                                                                                    |
| `--install <CHOICE>`     | Pre-answer the install-method prompt. Values: `brew`, `gh-releases`, `existing`, `custom:<PATH>`. Override beats `--recommended`.                                       |
| `--model <CHOICE>`       | Pre-answer the model-pick prompt. Values: `recommended`, `none`, `<owner>/<repo>[:<filename>.gguf]`.                                                                    |
| `--config-step <CHOICE>` | Pre-answer the config-write confirm. Values: `write`, `skip`. (Named `--config-step` rather than `--config` because the top-level `--config <PATH>` is already global.) |

The three per-step flags are **advisory, not authoritative**: supplying `--install brew` for a step that `--skip server` already excludes emits one stderr warning and proceeds. Conflicting axes don't abort.

Non-interactive contract: when stdout isn't a terminal and `--recommended` is not set, the wizard emits one consolidated stderr warning, then the install + model steps use recommended defaults silently. The config-write step refuses to proceed without explicit consent — pass `--recommended`, `--config-step write`, or `--config-step skip`. Without that consent the wizard aborts with exit `72` after persisting whatever durable state earlier steps already wrote (so `doctor` sees the partial baseline).

### `llamastash doctor`

Read-only diagnostic (its one write is the memory-drift baseline refresh). Re-runs hardware detection, diffs against `_init_snapshot.json`, and emits findings with stable ids agents can branch on: `binary_missing`, `binary_digest_drift` (skipped on brew installs — routine `brew upgrade` legitimately rotates the digest), `hardware_drift`, `memory_drift`, `gtt_hint`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`, plus two configured-server advisories — `server_binary_missing` (Warning: a `backend.<id>.servers[].binary` path no longer resolves) and `servers_configured` (Info: a summary of the resolvable servers and their device counts; silent when no `servers:` are configured). When the local benchmark snapshot looks stale, `doctor` probes the latest remote (the same one the recommender prefers) before judging `snapshot_stale`, so it only fires when no fresher snapshot is actually reachable; `LLAMASTASH_OFFLINE` skips that probe.

```
llamastash doctor [--json]
```

`doctor` **always exits 0** — findings are informative, not a failure signal. Branch on a non-empty `findings` array (or filter for `severity == "error"`) to escalate, not on the exit code. This makes `doctor` safe to run unconditionally from health-check loops without `set -e` blowing up.

Each `--json` finding carries `{id, severity, message, fix_hint, safe_to_log}`. `safe_to_log: true` on every finding means the output is safe to paste into a public issue.

`--json` (schema `2`) also carries a `hardware` section — the same live snapshot the init banner and `status` render: `cpu_brand`, `cpu_cores`, `mem_total_bytes`, `disk_free_bytes`, `gpu_backend`, `unified`, `uma_class_source` (how the unified-vs-discrete verdict was reached), `gpu_pool_total_bytes` (raw GPU memory ceiling — carve-out + GTT on a UMA APU), and the `uma_carve_bytes` / `uma_shared_bytes` composition. Two of the findings read this section: `memory_drift` fires when the GPU pool grows (info) or shrinks (warning) past `max(5%, 512 MiB)` versus the recorded baseline (doctor re-stamps the baseline after it fires); `gtt_hint` fires on Linux unified hosts whose GTT is still at the amdgpu default (~half of RAM), pointing at the `amdgpu.gttsize` ceiling.

### `llamastash recommend`

Shortcut for `init --only models` that ranks the top picks for this hardware and lets the user choose from them interactively. Useful when `llama-server` is already installed and the user just wants weights. The picker shows up to 10 ranked candidates from the `init::recommender` (default `DEFAULT_TOP_N`); pass `--model recommended` if you want it to short-circuit to the top entry without prompting. Besides the ranked picks, the list offers **Paste an HF repo id…** (type an `owner/repo` slug) and **Search HuggingFace by name…** (online only) — the latter prompts for a query, runs a live HF search, and lets you pick from the results (each row shows params · approx size · downloads); the chosen repo flows through the same download path as a pasted slug.

```
llamastash recommend [--json] [--offline] [--model <CHOICE>] [--revision <SHA>]
```

| Flag               | Effect                                                                                                                                   |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `--json`           | Same `{"steps_ran": ["detect","models"], "model": {...}, "recommendations": [...], ...}` shape as `init --only models --json`.           |
| `--model <CHOICE>` | Pre-answer the picker. Values: `recommended` (auto-pick top entry), `none`, `<owner>/<repo>`. Omit to get the interactive top-10 picker. |
| `--revision <SHA>` | Pin the HF revision; honored only on `<owner>/<repo>` paste branch.                                                                      |
| `--offline`        | Refused — recommend always needs network. Kept for `init` parity.                                                                        |

### `llamastash pull <repo>`

HuggingFace pull primitive. Built on the `hf-hub` crate. Accepts `<owner>/<repo>` (downloads every GGUF file in the repo) or `<owner>/<repo>:<filename>.gguf` (single file). Honors `HF_TOKEN` for gated repos.

```
llamastash pull <repo> [--json] [--offline]
```

`--json` emits `{"repo", "revision", "files": [...], "total_bytes"}`. Exit `69` on any failure (network, disk, integrity).

`pull` performs a disk-space precheck by HEADing each file before download, so an out-of-space failure surfaces before any bytes hit disk. It refuses to write the HF token to disk in cache-file modes that would persist it insecurely.

## Exit codes

Source of truth: `src/cli/exit_codes.rs`. Codes are part of the public CLI contract; pin against them rather than parsing human error strings.

| Code | Constant               | Meaning                                                                                                                                                |
| ---- | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `0`  | `SUCCESS`              | Success                                                                                                                                                |
| `64` | `USAGE`                | Bad CLI usage — missing required arg, invalid flag combination, or config-load error. Clap also emits this on its own.                                 |
| `65` | `DAEMON_UNREACHABLE`   | Daemon socket missing, peer hung up, or call timed out                                                                                                 |
| `66` | `MODEL_NOT_FOUND`      | Model reference matched zero or multiple catalog rows; stderr carries a disambiguation hint                                                            |
| `67` | `LAUNCH_FAILED`        | Daemon accepted `start_model` but the supervisor failed (probe timeout, port allocation, etc.)                                                         |
| `68` | `STOP_FAILED`          | `stop` couldn't reach the target (daemon error or process gone)                                                                                        |
| `69` | `PULL_FAILED`          | `pull` couldn't complete (network, integrity, disk space)                                                                                              |
| `70` | `BINARY_NOT_FOUND`     | `llama-server` not on PATH, no `--llama-server` flag, `LLAMASTASH_LLAMA_SERVER` unset                                                                  |
| `71` | `UNKNOWN`              | Catch-all for unexpected errors that don't map to a documented class                                                                                   |
| `72` | `INIT_ABORTED`         | `init` aborted before smoke — integrity check failed, archive defenses tripped, user declined confirm, or non-TTY config step without explicit consent |
| `73` | `INIT_DOWNLOAD_FAILED` | `init`'s model-download step failed (distinct from `PULL_FAILED` so agents branch on cause)                                                            |
| `74` | `INIT_SMOKE_FAILED`    | `init`'s smoke phase failed (binary doesn't run cleanly under `--version`)                                                                             |

`doctor` always exits `0` — severity lives in the findings array.

## TUI keybindings

These are the defaults. Override any binding via the `keybindings:` block in `config.yaml` — see [Custom keybindings](#custom-keybindings) above for the dialect and the action-name table.

### Global / list focus

| Key                                           | Action                                                                                                                                                                                                   |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `q` / `Ctrl+C`                                | Quit                                                                                                                                                                                                     |
| `↑` / `k`, `↓` / `j`                          | Navigate                                                                                                                                                                                                 |
| `PgUp` / `PgDn`                               | Page                                                                                                                                                                                                     |
| `g` / `G`                                     | Top / bottom                                                                                                                                                                                             |
| `/`                                           | Open filter (predicate applies live as you type; `Enter` drills into the focused result by opening the launch picker; `Esc` walks back: exit edit → clear → close)                                       |
| `f`                                           | Toggle favorite on focused model                                                                                                                                                                         |
| `Enter`                                       | Open launch picker on focused model                                                                                                                                                                      |
| `u` / `c` / `p`                               | Yank URL / curl / model path. `y` is a vi-style alias for `c`.                                                                                                                                           |
| `t` / `Shift+T`                               | Cycle theme forward / backward                                                                                                                                                                           |
| `Alt+L` (`⌥L` on macOS)                       | Cycle the left/right pane split through `left_pane_ratios` (wide mode; session-only). `100` hides the right pane, `0` hides the list.                                                                    |
| `Tab` / `Shift+Tab`                           | Move focus across panes (`h` / `l` do the same — Left/Right arrows are intentionally unbound on Models to avoid an asymmetric pane-jump)                                                                 |
| `Shift+M` / `Shift+L` / `Shift+C` / `Shift+S` | Jump focus to Models / Logs / Chat / Settings respectively. `L` and `C` only fire when the focused model is running.                                                                                     |
| `Shift+P`                                     | Open the HuggingFace pull dialog (Models list focus only — search + sort + paginate, download via the pinned status strip). "P" for Pull.                                                                |
| `Ctrl+P`                                      | Save the launch settings in view (the Settings form's knobs, or a running model's live knobs) as a named preset in `config.yaml` — prompts for a name, then an overwrite confirm if it already exists. "P" for Preset.                                                              |
| `Ctrl+S`                                      | Stop the focused running launch (any nav focus; opens a confirmation popup)                                                                                                                              |
| `Ctrl+R`                                      | Restart the daemon (any nav focus; opens a confirmation popup)                                                                                                                                           |
| `Ctrl+K`                                      | Kill the daemon entirely (List focus; opens a confirmation popup)                                                                                                                                        |
| `Ctrl+D`                                      | Delete the focused model from disk (idle rows only: `NotLaunched` / `Stopped` — opens a confirmation popup; HF-cache models remove the entire `models--<owner>--<repo>` directory to reclaim blob bytes) |
| `Ctrl+X`                                      | Cancel the currently-active HF download (any focus; opens a confirmation popup; queued pulls stay in line — press again on the next promoted pull)                                                       |

### Mouse focus (opt-in)

Mouse capture is **off by default** so the terminal keeps native click-and-drag text selection — useful for copying paths, logs, or curl strings out of the dashboard. Two ways to opt in:

- Per-run: `llamastash --mouse-focus`.
- Always-on: set `mouse_focus: true` in `config.yaml`, or alias the binary in your shell rc — `alias llamastash='llamastash --mouse-focus'`.

The CLI flag and the config knob are OR-ed; either source is sufficient. There's no negating counter-flag because the default is already the conservative "off" path.

When enabled, left-click moves focus and the wheel replays the `↑`/`↓` action in the current focus — i.e. whatever pressing `k` / `j` (or arrows) would do right now. Drag / Up / Moved are filtered out at the input thread so a user holding the terminal's bypass modifier (Shift on iTerm2 / Alacritty / foot / wezterm, Option on Apple Terminal) can still highlight text for native copy.

| Gesture                                                                           | Action                                                                                                                                                                                                                                                                                        |
| --------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Left-click on the Models list                                                     | Focus → `List`                                                                                                                                                                                                                                                                                |
| Left-click on the right pane (body, not a tab label)                              | Focus → `RightPane` (keyboard still drives `e` to enter Chat/Embed/Rerank text input)                                                                                                                                                                                                         |
| Left-click on a tab label (`Settings`/`Logs`/`Chat`/`Embed`/`Rerank`)             | Switch `right_tab` + focus → `RightPane`                                                                                                                                                                                                                                                      |
| Wheel up/down                                                                     | Same as pressing `↑`/`↓`: moves the list cursor in `List` focus, scrolls the active buffer in Logs / Chat / Embed / Rerank, cycles fields in the Settings form (scrolls the read-only running view). To scroll Logs without leaving an input, click the right pane first to land focus there. |
| Drag / Up / Moved                                                                 | Filtered out — preserves terminal text selection during drag and prevents mouse-motion events from saturating the event channel.                                                                                                                                                              |
| Any mouse event while a modal owns input (HF dialog, confirm popup, help overlay) | Ignored — modals own their own dismissal contract; a stray click cannot confirm a destructive action.                                                                                                                                                                                         |

### HuggingFace pull dialog (`Focus::HfDialog`, `Shift+P` from the Models list)

Three-stage modal: **Search → File picker → Confirm**. Search runs live against the public `/api/models` endpoint (300 ms debounce); paste an `owner/repo[:filename]` slug + Enter to bypass search. Each search row carries two size columns — `params` (model parameter count, e.g. `35B`) and `size` (approximate download size, the representative GGUF file HF parsed, e.g. `5.3G`); the exact per-quant size lands in the File picker.

| Key                         | Action                                                                                                                                                                                                   |
| --------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `e`                         | Enter edit mode on the search field (auto-enabled on dialog open). Resting Esc clears the buffer; a further Esc closes the dialog.                                                                       |
| (alphanumerics / Backspace) | Mutate the search query while editing                                                                                                                                                                    |
| `↑` / `↓`                   | Move the row cursor                                                                                                                                                                                      |
| `o`                         | Cycle sort (Downloads → Likes → Recently Updated → Trending → File size → Params → Repo name). The first four are server-side; File size / Params / Repo name reorder the current page in memory (HF can't sort by these). Resets to page 1. Only fires while the search field is resting. |
| `n` / `p`                   | Next / previous page (only fires while the search field is resting; `‹›` chevrons next to `page N` indicate when they're available)                                                                      |
| `Enter`                     | Search → drill into the focused repo's files; FilePicker → confirm the chosen file; Confirm → enqueue the pull on the download strip                                                                     |
| `Esc`                       | Walk back one layer: editing → exit edit · resting+content → clear · resting+empty → close (in-flight downloads keep running). In the FilePicker / Confirm stages, Esc steps back to the previous stage. |
| `Ctrl+X`                    | Cancel the currently-active HF download (also reachable from anywhere outside the dialog)                                                                                                                |

### Launch picker (Settings tab)

The Settings tab hosts the typed-knob launch editor. Each row shows
the resolved value plus a `(source)` chip indicating where the value
came from in the precedence chain (`(user)`, `(last used)`, `(arch
default)`, `(built-in)`, `(model default)`).

| Key       | Action                                                         |
| --------- | -------------------------------------------------------------- |
| `↑` / `↓` | Move between editor rows                                       |
| `←` / `→` | Cycle the focused row's value (on the `device` row: walk the GPU cursor) |
| `Space`   | Toggle the cursor GPU on the multi-GPU `device` row             |
| `e`       | Open inline edit on a numeric / enum / extras row              |
| `Enter`   | Commit an open inline edit; otherwise dispatch `start_model`   |
| `Esc`     | Cancel an open inline edit, or return focus to the Models list |

Knob set, grouped into labelled clusters in display order:

| Group                                        | Knobs                                              |
| -------------------------------------------- | -------------------------------------------------- |
| Context                                      | `ctx`, `reasoning`                                 |
| GPU / CPU offload                            | `n_gpu_layers`, `n_cpu_moe`                        |
| Multi-GPU placement _(multi-GPU hosts only)_ | `device`, `tensor_split`, `main_gpu`, `split_mode` |
| Attention & KV cache                         | `flash_attn`, `cache_type_k`, `cache_type_v`       |
| Throughput                                   | `threads`, `parallel`, `batch_size`, `ubatch_size` |
| Memory loading                               | `mlock`, `no_mmap`                                 |
| Advanced                                     | `rope_freq_scale`, `keep`, `extras`                |

Groups are ordered by how often a knob is typically changed; related
knobs sit together. (This display order is independent of the order
flags are emitted on the `llama-server` argv.) Booleans cycle
`default ↔ on ↔ off`; enums cycle their allowed set (the standard
llama-server cache types `f32` / `f16` / `bf16` / `q8_0` / `q4_0` /
`q4_1` / `iq4_nl` / `q5_0` / `q5_1` for `cache_type_k` / `cache_type_v`,
`none` / `layer` / `row` for `split_mode`).
`e` enters free-form numeric / enum / text edit mode for any row whose
preset list doesn't cover the value the user wants — cache-type rows
also accept a custom quant identifier from a modified llama-server build
(e.g. `fp4`, `turbo_quant`) this way, and `--cache-type-k` / `-v` on
`start` accept the same.

**GPU/CPU offload split.** `n_gpu_layers` offloads N layers to the GPU
(rest on CPU); `n_cpu_moe` keeps the first N layers' MoE expert weights
on CPU — the lever for big MoE models that don't fit VRAM. On
multi-GPU hosts, `tensor_split` (e.g. `3,1`) sets an uneven split
across heterogeneous cards, `main_gpu` picks the primary GPU, and
`split_mode` chooses `none|layer|row`. For per-tensor placement beyond
these, `--override-tensor` works through the `extras` row.

The `device` row (`--device` / `-d`) pins a model to a chosen subset of
GPUs instead of letting `llama-server` split it across every visible
card. In the TUI it uses the same `◀ ▶` single-stop style as the other
knobs, with a `[ ]` checkbox in front of each stop: `←/→` walk a cursor
through the devices the selected server reports via `--list-devices`
(one shown at a time, e.g. `[x] ROCm0  ·  2 of 3` — the selector, its
checkbox, and how many of the N GPUs are on), and `Space` toggles the
shown GPU in or out of the selection (a `Space:choose` hint surfaces
while the row is active). Every box ticked (`· all`) is the llama-server
default — no `--device` flag — and clearing the last box snaps back to
it; Backspace resets the row. Selectors are passed through verbatim
(comma-joined for a multi-GPU pick, e.g. `ROCm0,ROCm1`), so only devices
the server's binary exposes are offered — the list rescopes when you
cycle the `server` row. On the CLI, `start --device ROCm0,ROCm1` takes
the same comma-separated list.

The whole **Multi-GPU placement** group (`device`, `tensor_split`,
`main_gpu`, `split_mode`) — and the matching `Device` column in the
model list — appear **only when more than one GPU device is detected**.
Single-GPU and CPU-only hosts never see them, so the launcher stays
uncluttered when there's no device choice to make. In the model list the
`Device` column reads `all` for a running launch that targets every GPU
(no `--device`), so it never blanks out inconsistently next to launches
that pinned a selector. Once a model is running, the read-only Settings
view shows a `server` row naming the build that served it (when the
model has more than one compatible server). The bottom `extras` row holds the free-form argv tail for
flags the typed editor doesn't model; forbidden flags
(`--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`) surface a
red inline warning with secret values redacted.

### Precedence chain

When the daemon composes the argv for `start_model`, it walks the
following layers top-down per typed knob; the first `Some` wins:

```
preset       (R21)
  └─ last_params  (R20)
       └─ config.yaml arch_defaults
            └─ built-in (architecture, gpu_backend) table
                 └─ llama-server defaults
```

User-supplied `knobs` in the IPC request body sit above `last_params`
on the chain. The Settings tab renders the source label so the
inheritance is visible at the row level.

### Right pane

| Key                                                       | Action                                                                                    |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `Tab` / `Shift+Tab`                                       | Cycle pane focus (universal across the TUI; `l` / `h` are vi aliases)                     |
| `↑` / `↓` (or `k` / `j`)                                  | Settings tab: move between editor rows. Logs tab: scroll the buffer.                      |
| `←` / `→`                                                 | Settings tab: cycle the focused row's value through its preset list (no-op on other tabs) |
| `Esc` / `Shift+M`                                         | Return focus to the Models list                                                           |
| `Shift+L` / `Shift+C` / `Shift+S` / `Shift+E` / `Shift+R` | Jump to Logs / Chat / Settings tab. `L` and `C/E/R` are gated on a running model.         |
| `s`                                                       | Toggle Logs auto-scroll (toasts `auto-scroll on` / `off`)                                 |
| `c` (or `y`)                                              | Logs tab: copy the full log buffer to clipboard                                           |
| `r`                                                       | Chat tab: toggle `<think>` block collapse (toasts `reasoning shown` / `collapsed`)        |
| `Ctrl+S`                                                  | Stop the focused running launch (confirmation popup)                                      |
| `e`                                                       | Enter edit mode on the active tab's input field                                           |

### Chat tab (`Focus::ChatInput`)

| Key                         | Action                                                                         |
| --------------------------- | ------------------------------------------------------------------------------ |
| (alphanumerics / Backspace) | Edit prompt buffer                                                             |
| `Enter`                     | Send prompt                                                                    |
| `Shift+Enter`               | Insert newline (only on kitty-protocol terminals; collapses to send elsewhere) |

### Embed tab (`Focus::EmbedInput`)

| Key                         | Action                                         |
| --------------------------- | ---------------------------------------------- |
| (alphanumerics / Backspace) | Edit input                                     |
| `Enter`                     | Call `/v1/embeddings`                          |
| `Shift+Enter`               | Insert newline (kitty-protocol terminals only) |
| `Tab` / `Shift+Tab`         | Cycle pane focus                               |

### Rerank tab (`Focus::RerankInput`)

| Key                         | Action                                                                                       |
| --------------------------- | -------------------------------------------------------------------------------------------- |
| (alphanumerics / Backspace) | Edit current field                                                                           |
| `↓` / `↑`                   | Cycle Query ↔ Candidate field                                                                |
| `Enter`                     | Query field → call `/v1/rerank`. Candidate field → stage the buffer onto the candidate list. |
| `Shift+Enter`               | Insert newline (kitty-protocol terminals only)                                               |
| `Tab` / `Shift+Tab`         | Cycle pane focus (universal; not field cycle)                                                |

## Toasts

Transient status messages (yank confirmations, "nothing to stop" hints,
no-op cycle attempts, theme changes, and toggle-state changes such as
`auto-scroll on/off` or `reasoning shown/collapsed`) surface as a short
toast string in the bottom-right of the active panel. Toasts:

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
