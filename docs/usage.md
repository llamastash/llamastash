# LlamaStash usage

This is the reference for the non-interactive CLI surface and the TUI keybindings. The runtime contract — exit codes, JSON shapes, env vars — is part of the public surface; pin against the documented forms rather than parsing human output.

## Concepts

**Single binary, three roles.** `llamastash` (no args) opens the TUI. `llamastash daemon ...` controls the background daemon. Every other subcommand (`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`) is a CLI client.

**Daemon on demand.** The first TUI or CLI client that runs auto-spawns the daemon if no socket is present. The daemon survives client exit; running models survive daemon shutdown via process detach. Pass `--no-spawn` to fail fast against a missing daemon (useful in scripts).

**Model references.** `start`, `stop`, `logs`, `presets`, `favorites` all accept the same model reference: an absolute path, a canonical model id, or a case-insensitive substring of the file name or its parent directory. Ambiguous references exit `66` with a disambiguation list.

**Launch names.** One model can run several times at once, each under a name you choose: `start qwen3 --name coder`, then `start qwen3 --name writer`. The launch is then addressable as `<model-ref>@<name>` everywhere a reference is taken (`stop qwen3@coder`, `logs qwen3@coder`, `show qwen3@coder`) and as `<model-id>@<name>` in a request's `body.model` on the proxy. A bare `stop coder` works too when exactly one live launch answers to that name; a launch that failed to load keeps its row but stops holding the address, so the name reaches the copy that is actually running and only falls back to the failed one when nothing else answers. Names are unique per model, case-insensitive, limited to letters/digits/`-`/`_` (so the address always parses back), live only as long as the launch, and are refused for backends that serve every model from one shared process (Lemonade), where a second launch is not a second instance.

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

backend: # Per-engine config, one block per backend. llama.cpp is the
         # always-on default (no enable toggle); lemonade, vllm and sglang
         # are optional, each default-on when its own binary resolves.
  llamacpp:
    servers: # Build/binary variants. First = default (auto/no-device launches),
             # and the target of --llama-server / LLAMASTASH_LLAMA_SERVER. Each is
             # probed with --list-devices; every entry is its own selectable
             # server (no dedup across builds — CUDA/ROCm/Vulkan builds all list).
             # Either binary shape works: the standalone llama-server, or the
             # unified `llama` app, which is launched as `llama serve ...`.
      - binary: /usr/local/bin/llama-server
      - binary: /opt/builds/cuda/llama-server
        name: cuda # Optional; else auto-derived (<backend>·<gpu_backend>).
    fit_ctx_floor: 16384 # Min --fit-ctx window. Env: LLAMASTASH_FIT_CTX_FLOOR.
    strict_fit: false # Refuse (vs degrade) an unplaceable --fit. Env: LLAMASTASH_STRICT_FIT.
    jinja: true # Emit --jinja every launch (tool calling). Config-only.
    map_anthropic_effort: true # Map Anthropic output_config.effort to the engine kwarg. Config-only.
    slot_save: # Keep the prompt cache across an unload. See §"Keeping the prompt cache across an unload".
      enabled: false # Config-only.
      max_gib: 16 # Total size of saved files; oldest deleted first.
      max_age_secs: 86400 # 0 = no age limit.
      min_tokens: 2048 # Smaller slots are not saved.
  lemonade:
    # servers: [{ binary: /opt/lemonade/lemond }] # lemond path; else PATH.
    # enabled: # tri-state: unset=auto, true=force on, false=force off.
    # port: 13305 # lemond umbrella port.
  generic: # Any other OpenAI-compatible server (ds4, gufo, Halogen). See §"Generic backend".
    # servers: [{ name: ds4, model: "DeepSeek-V4-*", binary: /opt/ds4/ds4-server, ... }]

disable_scan: false # Equivalent to LLAMASTASH_NO_SCAN=1.
disable_default_cache_paths:
  huggingface: false
  ollama: false
  lm_studio: false

gpu: # GPU probe tuning. Config-only; no CLI/env surface.
  enable_vulkan_probe: true # Skip the vulkaninfo fallback probe when false.
  reprobe_interval_secs: 60 # Full vendor re-probe period (0 = probe only at start).

daemon: # Launch ports, health probing, lifecycle. Config-only.
  port_range: # Ports the supervisor picks from when launching a server.
    start: 41100
    end: 41300
  probe_timeout_secs: 120 # Per-launch health-probe deadline.
  idle_timeout_secs: 0 # Shut down after N idle seconds (0 = never).
  metrics_interval_secs: 1 # Host-metrics tick (1..=60; 0 resets to 1).
  # preload:                       # Models to start at daemon boot, in order.
  #   - model.gguf                 # list name, path, <model>@<preset>, or a launch file
  #   - other.gguf@long-ctx

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
| `LLAMASTASH_LLAMA_SERVER`           | Path to `llama-server`, or to the unified `llama` binary (sets the first `backend.llamacpp.servers[]` entry)                                                                                                                                                                                                                                                                                                                                                                      |
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

The colored-output policy OR-es three off-conditions: `--no-colors`, `NO_COLOR` env (non-empty), or non-TTY stdout. Any one silences colors. `--json` output is byte-stable regardless — pin agents against `--json`, not against the human form. A command run with `--json` that fails prints `{"error": {"code": <exit code>, "message": "..."}}` on stdout instead of the `✗` line on stderr, and exits with the same code. `--help` follows the same policy: it shows styled section headers and flags on a TTY and stays plain bytes when piped, `NO_COLOR` is set, or `--no-colors` is passed.

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

- `--json` emits a stable JSON array; pin agents against this. Rows are byte-identical to the IPC `list_models` rows — a single `CatalogRow` serde impl (`src/launch/resolve.rs`) is the only definition of the wire shape, serialized by the daemon and deserialized by both CLI and TUI.
- `--filter` is a case-insensitive substring matched against name, path, arch, and quant.

Row shape:

- Top level: `name`, `repo`, `path`, `parent`, `source`, `backend`, `supported_backends`, `split_siblings`, `parse_error`, `display_label`, plus `model_id` only when set and a CLI-only `status` object on a running row (`state`, `port`, `launch_id`, `name` when the launch has one, `device` — the raw `--device` selector, `null` when the launch took the backend default, which the table's DEVICE column renders as `all`).
- `launches` — present alongside `status` on a running row: one object per live launch of that model, same shape as `status`. `status` is the first of them and keeps its pre-existing meaning, so `models[i].status.state` still pins. Read `launches` when a model may be running more than once.
- `metadata` — GGUF-derived: `arch`, `quant`, `native_ctx`, `mode_hint`, `parameter_label`, `weights_bytes`, `total_parameters`, `tokenizer_kind`, `has_chat_template`, `has_reasoning_hint`. (These are **not** top-level keys; read `has_reasoning_hint`, there is no `reasoning_hint` alias.)
- `mtp` — `{embedded_layers, separate_head}`; `multimodal` — `{vision, audio}`.

A model running more than once gets **one row per launch**, and a named launch renders `<model>@<name>` in NAME. That joined string is the address: paste it into a client's `model` field, or hand it to `stop` / `logs` / `show`.

The table columns are `NAME [REPO] ARCH PARAMS QUANT CTX SIZE MODE [BACKEND] STATUS [DEVICE]` — the TUI Models list shows the same set minus `REPO`, which is CLI-only (the TUI groups rows under a repo section header instead), with `DEVICE` gated on the same "some single server offers more than one device" rule the TUI uses (`cli::resolve::multi_device`). `MODE` shows the catalog's mode hint (`chat` / `embedding` / `rerank`). `REPO` is where the model lives in short form — `unsloth/Qwen3.8-27B-GGUF` for an HF or LM Studio cache entry, the parent directory's name for anything else; it is empty for a source that names its own origin (Ollama, Lemonade), and the column is dropped when no row has one. It is also the prefix of the repo-qualified id the proxy publishes when two models share a file name (see [Model ids on the proxy](#model-ids-on-the-proxy)). `BACKEND` appears only when some model is served by more than one backend (or a non-default one); `DEVICE` appears only on multi-GPU hosts and reads `all` for a running launch that targets every GPU (no `--device`), the explicit selector when pinned, and `?` otherwise — matching the TUI's Device column. When piped, the same columns print as tab-separated rows.

### `llamastash show <model-ref>`

Everything LlamaStash knows about one model: catalog row, GGUF metadata, on-disk size (per shard), the yaml + built-in arch defaults a launch would resolve, last-used launch params, and live running state.

```
llamastash show <model-ref> [--json]
```

`<model-ref>` also takes a launch: `show qwen3@coder` scopes the running section to that one launch, and `show coder` / `show L3` / `show 41100` resolve through the live launches the way `stop` and `logs` do.

`--json` builds on the **same catalog-row shape as `list --json`** (nested `metadata`, `multimodal`, `mtp`, `supported_backends`, `split_siblings`; `model_id` omitted when unset). The envelope **is** the serialized `CatalogRow` with four show-only sections layered on top (`src/cli/show.rs::assemble_envelope`) — never a second hand-built projection:

- `size` — `weights_bytes`, `shard_count`, `on_disk_total_bytes`, and a per-shard `shards` breakdown.
- `arch_defaults` — the `yaml` and `builtin` knob sets for this (arch, GPU backend) pair.
- `last_params` — the params of the last successful launch (`null` when never launched).
- `running` — an array with one object per live launch (`launch_id`, `name`, `preset`, `state`, `port`, `resolved_ctx`, `ctx_clamped`), empty when nothing is running. `preset` is the preset that launched that copy (`null` when none was in play). The human output prints one `running` block per launch, headed by its `<model>@<name>` address, and shows the `preset` row only when there is one.

The human output shows the same content as aligned key/value sections, including `multimodal` (`vision + audio`) and `mtp` (`embedded (N layers)` / `separate head`) rows under `metadata`.

### `llamastash start <model-ref> | llamastash run <model-ref>`

Launch a model. `run` is a visible alias for `start` — same flags, same behavior; it exists as the shorter way to say "launch this". Layered resolution: catalog row → optional preset → per-invocation flags → trailing raw `llama-server` flags after `--`.

```
llamastash start <ref> [--name LABEL] [--preset NAME] [--ctx N] [--port N] [--wait] [--force]
                     [--reasoning on|off] [--mode chat|embedding|rerank]
                     [--backend auto|llamacpp|lemonade|vllm|sglang|generic] [--server <id>]
                     [--<advanced-knob> ...] [-- <llama-server-flags>...]
```

`--name <label>` names this launch, so the same model can run several times at once and each copy stays addressable as `<model-ref>@<label>`. A name is trimmed and limited to letters, digits, `-` and `_` — anything else (a space, an `@`) is a usage error at parse time, because the address would not parse back to this launch; the daemon enforces the same rule for raw JSON-RPC callers. A second live launch of the *same* model under the *same* name is refused, and the refusal names the launch already holding it (`name `coder` is already running as L3`); the same name on a *different* model is fine. `--json` reports the accepted name back as `launch_name`, with or without `--wait`. Without `--name`, a launch from a named preset (`--preset coder`, a launch file, a TUI preset stop) takes the preset's name, so it gets the same `<model-ref>@coder` address a proxy request for that preset would auto-start; running that preset a second time on the same model is refused like a duplicate `--name`, so pass `--name` for another copy. Because a reference is only read as `<model>@<name>` when the name half follows that same rule, a mistyped `qwen3@my coder` is treated as a plain model reference and simply misses, rather than starting anything. Names are not config: they live as long as the launch and are gone once it stops. A `llamastash daemon stop` stops every managed launch with the daemon, so nothing is left to name; if the daemon *crashes*, its `llama-server` children keep serving and the next start surfaces each as a read-only `external` row that still carries its name — `status` shows `<model>@<name>` and `stop <name>` reaches it, but it is not re-published on `/v1/models` (routing needs a supervisor, and there is none), so the next proxy request for that address starts a fresh launch beside it.

`--backend` defaults to `auto` (picks the engine by model identity: a GGUF runs on llama.cpp, a safetensors repo on vLLM or SGLang). Override it to force a specific engine.

`--server <id>` picks a specific **server** — one build/binary of a backend (`llamacpp-vulkan`, `llamacpp-cuda`, `vllm`, or a generic entry's `generic-<name>`). It determines which binary spawns and, when `--backend` is unset, which backend runs the model (the server's owning backend). Server ids auto-derive as `<backend>-<compute>` from each build's own device names (or the bare backend id for a device-less engine like vLLM or Lemonade), overridable with a per-server `name:`; list them from `status` (the `servers` array; `status --json` mirrors it). A `--device <selector>` already implies its owning server, so `--server` is for picking a build with no device pin. The pick persists in `last_params`, so a relaunch reuses it — in the TUI it reopens the launch picker's `server` row on that build.

Every knob any backend declares is a first-class `start` flag — `--n-gpu-layers`, `--threads`, `--device`, `--tensor-split`, `--main-gpu`, `--split-mode`, `--flash-attn`, `--cache-type-k`/`-v`, `--batch-size`, `--load-mode`, and the same for every other backend's own tunables. The flag is spelled the way the engine spells it. Run `start --help` for the full list, grouped by the backend that declares each; `llamastash knobs` lists them with value ranges and choices. Flags, editor rows and preset keys are all generated from one declaration per knob, so no surface can be missing one. Booleans take `--flash-attn` (= on) or `--flash-attn=false`. Anything `start` doesn't recognise as a knob — including `llama-server`'s single-dash shorts like `-ngl` — still works verbatim after `--`. A knob set both inline and after `--` resolves to the `--` value.

Modes are strict: when the catalog reports `mode_hint = unknown` and no `--mode` is passed, the CLI exits `64` rather than silently defaulting to chat. Otherwise the mode resolves as `--mode` > a preset's `mode:` pin > the model's own GGUF hint > chat, and the last two rungs are resolved by the daemon, so the same order applies to a plain `start`, the TUI, and proxy auto-start alike.

`--ctx` above the model's native context length is allowed (the supervisor still tries, per R12); a warning prints to stderr. When `--preset` and inline knobs are combined, the inline knobs layer onto the preset — they override only the fields they set, leaving the rest of the preset intact.

#### Model loading (`load-mode`)

`load-mode` picks how the weights are brought in: `auto` (the engine default —
mmap unless a device can't), `none` (no mmap, the old `--no-mmap`), `mmap`,
`mlock` (locked in RAM, and *not* mmapped), `mmap+mlock`, or `dio` (DirectIO
where the build has it).

llama.cpp deleted `--mmap` / `--no-mmap` / `--mlock` / `-dio` on 2026-09-09 in
favour of one `--load-mode` flag, and both spellings are still in the field — a
current stock build commonly sits beside older fork builds pinned for one model.
LlamaStash probes each configured server's `--help` once at daemon start and
emits whichever spelling that binary takes, so the same preset launches on both.
Nothing to configure; `llamastash status` lists the servers it probed.

A `config.yaml` still carrying `no-mmap: true` or `mlock: true` is rewritten on
the next `daemon start` (`no-mmap` → `load-mode: none`, `mlock` → `load-mode:
mlock`, both → `mlock`), with a `.pre-knobs.bak` copy beside it.

#### Auto launch mode (default)

By default LlamaStash does **not** pin GPU layers or context size. It delegates GPU/CPU placement and context sizing to llama-server's `--fit`, so an oversized model loads partially offloaded instead of OOMing, and keeps memory-budget authority itself: a launch that would not fit the sampled free memory is refused before spawn (with the demand, the effective free, and what to do about it) rather than letting two concurrent models exhaust the pool. This requires a fit-capable `llama-server`.

Every knob has three states:

- a pinned value (`--n-gpu-layers 50`, `--ctx 16384`) — used verbatim;
- `auto` (`--n-gpu-layers auto`, `start --ctx auto`, or the Auto stop in the TUI knob cycle) — delegated to `--fit`;
- unset (Inherited) — falls through presets / arch defaults / the server default.

`backend.llamacpp.fit_ctx_floor` (default 16384) is the minimum context `--fit` is told to keep. Set `default_launch_mode: inherited` to opt the whole machine back to the pre-Auto behavior (knobs you never touch fall through to llama-server's own defaults instead of `--fit`). See the config schema and the environment-variable table above for `default_launch_mode`, `backend.llamacpp.fit_ctx_floor`, and `backend.llamacpp.strict_fit`.

#### Launch files (`llamastash run qwen3.8.yml`)

The positional also takes a `.yaml`/`.yml` file naming **one** model and the presets to run it with:

```
llamastash run qwen3.8.yml                 # the file's `default:` preset
llamastash run qwen3.8.yml --preset slow   # a named entry from the file
llamastash run qwen3.8.yml --ctx 4096      # flags still layer on top
```

The file is a `presets:` block in `config.yaml`'s own shape, narrowed to one model — no new preset types:

```yaml
presets:
  Qwen3-8B-Q4_K_M.gguf:      # model key: substring / exact name / catalog path
    default: fast            # optional; required with >1 entry and no --preset
    entries:
      fast:
        knobs:
          n_gpu_layers: 99
          ctx_size: 32768
        backend: llamacpp
        extras: ["--rope-freq-base", "1000000"]
      slow:
        knobs:
          n_gpu_layers: 40
```

It is **not** self-contained: the model key resolves against the running daemon's catalog exactly like a `start` argument, so a key that matches nothing exits `66`. **Arch keys do not carry over.** In `config.yaml` a top-level key that names no discovered model is read as an arch id; a launch file resolves catalog-only, so a block copied out of a config under `qwen3` exits `66` (no match, or an ambiguity list) rather than applying to every Qwen. Name one model. And it writes nothing — no `config.yaml` entry, no `state.json`, no restart. `presets save` still targets `config.yaml`; a launch file is an input, never an output.

Detection reads the positional, not the alias: a value ending in `.yaml`/`.yml` **that exists as a file** is a launch file, so `start file.yml` behaves identically, and a model actually named `foo.yml` that isn't on disk still resolves as a model reference.

The file's preset is a self-contained baseline, identical to `--preset <name>`: the daemon skips both the default-preset and last-used layers, so nothing leaks in from a previous run. Per-flag layering is unchanged — `--ctx 4096` overrides the file's `ctx_size` and leaves its other knobs alone. Extras are the one asymmetry, as everywhere else on the CLI: flags after `--` **replace** the file's whole `extras:` list rather than merging into it.

Validation is stricter than `config.yaml`'s, because a hand-authored file has no inspection surface — you never see its parsed form. Each of these exits `64` with the problem named:

| The file | Result |
|---|---|
| no `presets:` entries, or 2+ model keys | must name exactly one model |
| a model key with no `entries:` | must define at least one preset |
| 2+ entries, no `default:`, no `--preset` | ambiguous; the message names the entries |
| `default:` naming no entry | dangling default (`config.yaml` ignores this one silently; a launch file does not) |
| `--preset auto`, or `default: auto` | `auto` means "apply no preset", which a launch file cannot mean |
| a knob no backend declares | names it and points at `llamastash knobs` |
| a knob value the backend can't parse (`ctx_size: 8k`) | names it (`config.yaml` drops a bad value as silently as a bad id) |
| a field no preset block reads (`backendd:`, or `mode:` written beside `knobs:` instead of inside it) | names it and lists what a preset takes |

The last three rows are the deliberate difference from `config.yaml`, where each of them is dropped with a line in the daemon's log file and the launch goes ahead without it. A misspelled `backend:` is the worst of the set: it silently picks a different engine and still reports success. Only the selected entry is checked — a typo in an entry you didn't launch stays silent, though a bad field on the `default:` / `entries:` block itself is always caught.

YAML merge keys work: `<<: *base` is expanded before the file is validated or parsed, so a shared anchor contributes its knobs to the entry that merges it, and a typo riding in through an anchor is caught like any other. Merges are shallow, per key, as YAML defines them — an entry with its own `knobs:` map replaces the anchor's rather than extending it.

#### `--force` (launch through an admission refusal)

A launch whose projected demand exceeds the sampled free memory is refused before spawn, with the demand, the effective free, and what to do about it. `--force` turns that refusal into a warning and launches anyway:

```
llamastash start <model> --force
  ! --force overrode the memory admission gate — launch refused: needs 90.6 GiB but only 16.9 GiB is free ...
```

Use it when you know the projection is wrong for your setup — an engine build that holds less than LlamaStash models, weights that page in from a filesystem the gate cannot see. If the projection was right, the host runs out of memory and the kernel's OOM killer picks the victim, which may well be a different process.

The warning reaches every output path: the human line above (on both plain `start` and `--wait`), and a `warnings` array in `--json`. Only `--quiet` drops the human line, and it still rides the JSON. A forced launch also holds its projected demand on the reservation ledger, so a second launch started while it loads is priced against the memory it is taking rather than against a free reading that has not caught up yet.

`--force` overrides only LlamaStash's own admission gate. It does not skip validation, mode resolution, or a backend's refusal — including a backend's own memory refusal, like a KV-cache cap that cannot be met, which has its own override knob named in the error. And the proxy's auto-start path can never set it: a request from the network must not be able to OOM the host.

#### `--wait` (block until the launch settles)

`start` is fire-and-forget by default: it returns as soon as the daemon accepts the launch, while the model is still loading. Pass `--wait` to block until the launch reaches a terminal state (Ready / Error / Stopped) and report the fit-resolved context:

- **Ready** prints a `ready → ctx=N` follow-up under the headline (`N (clamped to fit-ctx floor)` when memory pressure clamped the window down to `fit_ctx_floor`).
- **Error** prints `failed → <cause>` and exits `67` (`LAUNCH_FAILED`), so scripts can branch on a load that was accepted but never came up.
- A 15-minute safety ceiling caps the wait; the daemon's own size-scaled probe budget normally flips a stuck load to Error well before that, after which it prints `waiting timed out → still loading; check llamastash status`.

`--wait --json` emits a single combined object — the launch fields plus `state`, `resolved_ctx`, `ctx_clamped`, and `cause` (on error) — instead of the immediate accept-time object.

Both `--json` shapes carry a `warnings` array when the daemon raised any advisory (dropped knobs, an admission bypass, a `--force` override), and omit the field entirely when it did not.

### `llamastash stop <target>` / `llamastash stop --all`

Stop a managed launch by `<launch_id>` (e.g. `L3`), by port, by its launch name (`stop coder`, or `stop qwen3@coder` to qualify it when two models share a name), by a case-insensitive substring of the running model's file name or parent dir (e.g. `stop qwen`), or, for unmanaged processes the daemon surfaced, by `ext-<pid>` or bare PID. An exact launch name is tried before the path substring, the way an exact launch id is. Anything that matches more than one running launch exits `66` with the candidate launch ids.

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

Each row in `models` carries `name` when the launch was started with `--name`, and `preset` when the launch resolved one — an explicit `--preset` / launch file / TUI preset stop, a `<model>@<preset>` auto-start address, or the model's config `default:`. Both keys are omitted, not nulled, when they don't apply; `preset` is the preset that actually launched this copy, unlike the sibling `default` field, which is the model's configured default either way. The human table has no MODEL column, so its NAME cell renders `<model>@<name>` for a named launch and the model alone otherwise: two different models both named `coder` stay tellable apart in the command you reach for to work out what to stop.

The `proxy` block is documented in detail under [Proxy → Is the proxy up?](#is-the-proxy-up).

On a host where more than one GPU backend reports a device (e.g. an
NVIDIA card seen via CUDA plus an AMD card via ROCm), `gpu` serialises
as a `multi` snapshot (`{"backend":"multi","devices":[…]}`) and the
`host` block carries a `gpu_devices` array with one per-device row
(name, backend, utilisation, temperature, memory) so dashboards can
render each card separately. Single-backend hosts keep the existing
per-vendor shape.

### `LlamaStash logs <target>`

Tail (or follow) a launch's log file. `<target>` is a `<launch_id>` (e.g. `L3`), a port, a launch name (`logs coder` / `logs qwen3@coder`), or a case-insensitive substring of the running model's file name / parent dir (e.g. `logs qwen`). An ambiguous name exits `66` with the matching launch ids. Each launch writes its own file, so two launches of one model never interleave.

```
LlamaStash logs <target> [-n N] [-f]
```

`-f` polls `logs_tail` and de-dupes against a rolling window. SIGINT exits cleanly with code `0`. `BrokenPipe` (e.g. piping to `head`) also exits `0`. Daemon disconnect during follow exits `65`.

### `llamastash presets <model-ref> <action>`

```
llamastash presets <ref> list [--json]
llamastash presets <ref> save <NAME> [--ctx N]
                                   [--reasoning on|off] [--mode <m>]
                                   [--idle-ttl SECONDS] [--preload]
                                   [-- <flags>...]
llamastash presets <ref> delete <NAME>
llamastash presets <ref> show <NAME>
```

Named launch presets for a model. `save` is create-or-update (the response reports `replaced: <old-params>` so callers can audit). `list` shows the model's **effective** set; each row carries `source: "config"` and `is_default`. Apply one at launch with `llamastash start <ref> --preset <NAME>`.

Presets live in `config.yaml` under a `presets:` key, the single writable source. `save` / `delete` write there comment-safely. `state.json` does not carry or import presets.

A `presets:` key is classified per-resolution against your discovered models: a key that names a model (by file basename, or full path) is **per-model**; otherwise it is read as a GGUF `general.architecture` id and applies to **every model of that arch**. A model's effective set is its per-model entries ∪ its arch entries; the per-model entry wins on a name collision. The CLI writes per-model keys only — arch presets are hand-authored.

A key may also use `*` (any run of characters, `/` included) and `?` (exactly one) to cover a family of models: `Qwen3.8-27B-*` catches every quant of one model, `unsloth/*` catches a whole HF repo, `*-Q4_K_M` catches one quant everywhere. The pattern is matched, case-insensitively and anchored at both ends, against the model's file name, its extensionless form, its full path, and both of those prefixed with the `REPO` label `list` shows. A wildcard key is always per-model — it is never read as an arch id, since an architecture id cannot contain `*` or `?`. Precedence within the per-model layer is **exact key > wildcard key**, for both individual entries and `default:`, so a wildcard is a family fallback that a key naming one model overrides. Wildcard keys are hand-authored; `presets save` always writes the model's own name.

A `default:` under a key is the model's **standing launch config** (hand-edited; there is no set-default command). It auto-applies whenever you launch without picking something: a plain `llamastash start <model>` with no `--preset`, and proxy auto-start, both launch with the default. Precedence is `your inline flags > default preset > last-used params > arch defaults > fit`, so the default overrides your last manual launch but last-used still fills any knob the default leaves unset. Two reserved forms: `default: <name>` applies that preset; `default: auto` launches **pure fit** (ignores last-used and the default). With no `default:` set, last-used remains the implicit default (unchanged behavior).

Picking a preset explicitly (`start --preset <name>`, or the TUI cycle) overrides the default for that launch. `start --preset auto` is the clean per-launch "ignore everything, fit fresh" gesture. In the TUI, the preset cycle (`last used → auto → named…`) marks whichever stop is the configured default with `(default)` and opens on it, and the preset row shows the count of available presets (`preset (N)`).

Alongside its knobs an entry may pin launch **identity**: `mode:` (`chat` / `embedding` / `rerank`), `backend:`, and `server:` (a build id, as shown on the TUI's Server row). These say *what runs* rather than how it is tuned, and they apply on every surface: `start --preset`, a `default:` preset on plain `start` and on proxy auto-start, and the TUI preset cycle. An explicit `--mode` / `--backend` / `--server` still wins over the pin, and a pinned or last-used `server:` of another backend is dropped when `--backend` names a different one. A `mode:` pin also answers a model whose GGUF hint is `unknown`, which `start` would otherwise refuse with "pass `--mode`". Only a pinned preset carries a mode forward; a one-off `start --mode embedding` is not remembered for the next plain launch, so an embedding request can never lock a chat model out of chat.

An entry knob set to `auto` delegates that knob to llama-server's `--fit` (e.g. `n_gpu_layers: auto`); `auto` is a reserved token, so to pin a knob to the *literal* string value `auto`, use the escape `{ value: auto }`. The app writes entries in block style (flow `{ ctx: 8192 }` is also accepted when you hand-author). Presets carry no `port` (it is per-launch, auto-assigned). Changes the CLI/TUI make are live immediately; hand-edits to `config.yaml` need a `llamastash daemon restart` to be picked up. See `config.example.yaml` for the full shape. On the first `daemon start` after upgrading, an older `config.yaml` is rewritten in place into the `knobs:` shape with a `.pre-knobs.bak` copy beside it; the daemon logs what it migrated. Comments above a key survive that rewrite, comments *between* two knobs inside a migrated entry do not (the entry body is regenerated), which is what the backup is for. Residency keys (`idle_ttl_secs`, `preload`) are entry policy like `backend:` / `server:`, so they are carried through a migration verbatim and never read as the old shape on their own. Until that first start, a read that does not reach the daemon (`--no-spawn`) sees an unmigrated entry's knobs as empty.

#### Preset residency

Beside its launch settings an entry can pin how long it stays loaded and whether it starts on its own. These are residency policy, not knobs, so they sit next to `knobs:` (`--idle-ttl` / `--preload` on `presets save`, and both show in `presets list` under `TTL` / `PRELOAD` and in `--json` as `idle_ttl_secs` / `preload`):

- `idle_ttl_secs: N` — this preset's launches are unloaded after `N` idle seconds instead of the global `proxy.idle_ttl_secs`. Useful when load times differ a lot: a model that takes 80 s to read off disk should not expire on the same clock as one that takes 4 s.
- `idle_ttl_secs: 0` — never unload this preset's launches. Note the sweep only ever considers **proxy auto-started** launches: anything you `llamastash start` (or the TUI launches) is exempt from eviction whatever its preset says, so a TTL is about models the proxy brought up on a request.
- `preload: true` — start this preset when the daemon boots. Same as naming it in `daemon.preload`, and the same exemption applies: a preloaded launch is manual intent, so it stays up until you stop it. The preset's key has to name **one** model: a preloaded launch can never be unloaded, so an arch or wide-glob key (which would pin every model it matches) is skipped at boot with a line in the daemon log — name the model, or list them under `daemon.preload` in the order you want them.

A re-save that says nothing about residency keeps whatever the name already pins, even when the save lands under the model's own key and shadows an arch or glob entry of the same name; `--no-idle-ttl` / `--no-preload` are the only way to drop a pin. The one exception is `preload`: a family key's `preload: true` is not carried onto a single-model key, because boot refuses to preload a family and a preloaded launch can never be unloaded — so a `Ctrl+P` recapture cannot start loading your model at boot behind your back. The sweep reads the TTL from the daemon's live preset store on every pass, so `presets save --idle-ttl` moves a running launch's deadline without a relaunch; a hand edit to `config.yaml` needs `daemon restart`, like any other hand edit. When a request cannot be admitted because the host is full, the daemon first tries to make room by unloading idle auto-started launches, least-recently-used first (see §Proxy); launches pinned to `idle_ttl_secs: 0`, launches with a request in flight, and manual or preloaded launches are never picked.

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

Each row's `params` object carries a `knobs` map — every knob the launch dispatched with, keyed by its declared id and holding a scalar or the bare string `auto`. One map for every backend; omitted when empty. The same field rides the `start_model` IPC request body. See [`docs/architecture.md` § The knob registry](architecture.md).

### `llamastash daemon`

```
llamastash daemon start [--foreground|-f]
llamastash daemon restart [--foreground|-f]
llamastash daemon stop  [--force|-f]
llamastash daemon status [--json]   # PID + uptime + connections + managed launches
```

`daemon start` detaches into the background by default and returns once the socket is bound. Pass `--foreground` (or `-f`) to keep the daemon attached to the terminal — useful when a process supervisor (systemd, runit, container `CMD`) owns the lifecycle and needs to see stdout/stderr directly.

Without `llama-server`, `daemon start` refuses unless another backend is enabled (Lemonade, vLLM, SGLang, or a `backend.generic` entry). On such a host those backends launch as usual and only llama.cpp launches fail. `--force` starts the daemon either way.

`daemon restart` is `stop` followed by `start`: it shuts the running daemon down over IPC, waits for that process to exit, then brings a new one up. It takes the same flag set as `daemon start` (`--proxy-port`, `--ollama-compat`, `--proxy-host`, the backend opt-ins, `--force`, `--foreground`), and that is how the new daemon is configured — those flags are per-invocation, so repeat the ones the running daemon was started with. Use it after hand-editing `config.yaml`, which the daemon only reads at boot. Three differences from typing the pair yourself. The new daemon's flags and `config.yaml` are resolved before the running daemon is touched, so a bad flag or a broken config file leaves the running daemon up. With nothing running it is just `start`. And if the old daemon is still exiting when the stop window closes, `restart` fails instead of starting on top of a daemon that has not let go of the lockfile (`daemon stop --force`, then retry). The fail-fast backend check is the exception: it runs in the start half, because the running daemon legitimately holds the ports that check probes. Running models go down with the old daemon and are not brought back — `start` what you need afterwards.

`daemon stop` calls the IPC `shutdown` RPC, then waits for the daemon process to actually exit before printing `daemon: stopped` — up to 10 s, or the longest managed-launch stop grace plus 5 s when that is longer — so `daemon stop && daemon start` never races the dying daemon's lockfile or its managed `lemond` umbrella. If teardown outlives the wait it falls back to `daemon: shutdown requested (still exiting, pid N)`. When `runtime.json` is missing (the IPC channel can't be opened because a stale daemon from an older version is holding the lockfile) pass `--force` (or `-f`) to fall back to a `SIGTERM` on the PID recorded in `daemon.pid`. The CLI auto-detects this state on every command and prints the exact `kill` / `--force` invocation needed. A `runtime.json` left behind by a crash — a handshake with no process holding the lock — is cleared by `stop` and `restart`, which then report `daemon: not running`.

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

`--mtp` is a **launch-only** setting (there is no `config.yaml` key to set it globally), but it persists in `last_params` and in named presets like any other launch choice, so `mtp: off` / `mtp: on` in a preset entry's `knobs:` map pins it — including under `default:`, where it now applies to a plain `start` and a TUI launch that left the row alone. That matters most for pinning MTP **off** on a model where speculation costs more than it saves. `--mtp-draft-n` works whichever backend serves the model, and rides the `mtp-draft-n` knob, so `-- --mtp off` / `-- --mtp-draft-n 5` in the extras tail work too. The TUI launch picker shows the same control as an `mtp` row cycling inherited → auto → on → off, but only for MTP-capable models; it shows your intent, not the resolved answer (`status`'s `active` reports that). Forcing it on a model that has no draft head **warns and skips** rather than failing the launch (emitting the flag blind is a hard server error). If you drive speculative decoding yourself through the `-- <extras>` tail, llamastash defers entirely and adds nothing.

Under the hood, each backend maps this onto its own flags — the serving backend enables speculation with the resolved draft head (and `--mtp-draft-n` when set), emitted **before** the fit step so context reservation stays MTP-aware. A generic server entry has no MTP automation: `--mtp` does nothing there, and you pass the engine's own draft-head flag through a knob. For DeepSeek-V4 on ds4 that's the `mtp-model` knob, set in a preset (see [Running ds4 as a generic server](#running-ds4-as-a-generic-server)).

#### DSpark speculative decoding

DSpark is ds4's second speculative engine for DeepSeek-V4 Flash: a support model that reads the target's hidden states and proposes up to five tokens per step, which the Flash model then verifies. It replaces the one-stage MTP head for that run rather than stacking with it, and it takes the same `--mtp` slot: `mtp-model` points at the support GGUF, and `dspark` turns the runtime on. With the ds4 generic entry from [Running ds4 as a generic server](#running-ds4-as-a-generic-server):

```yaml
presets:
  DeepSeek-V4-Flash-*-0731.gguf:
    entries:
      ds4-dspark:
        server: generic-ds4
        knobs:
          dspark: true
          mtp-model: /path/to/DeepSeek-V4-Flash-DSpark-support-0731.gguf
```

`ds4-server` refuses `--dspark` without an `--mtp` file, and only after the full weight load, so always set both.

**Measure before you trust it.** DSpark is experimental, and on current ds4 builds it is often a net decode *loss* even at high acceptance. The per-accepted-token replay ds4 runs to preserve greedy identity can cancel the whole speculative saving (upstream ds4 issues [#695](https://github.com/antirez/ds4/issues/695), [#731](https://github.com/antirez/ds4/issues/731), [#733](https://github.com/antirez/ds4/issues/733) report this on Metal and M3 Ultra at 70-83% acceptance; measured here on ROCm/gfx1151 at 80% acceptance, 13.7 t/s falls to 7.0 t/s). ds4 also emits no acceptance figure through its API, so llamastash cannot surface one. Check it yourself with `DS4_DSPARK_STATS=1` on the ds4 binary (counters flush on clean exit) or `DS4_DSPARK_PROBE=1` for per-cycle stage status.

**Not every ds4 build implements it.** DSpark needs two GPU kernels that some backends stub out. On ROCm they returned failure until [ds4#761](https://github.com/antirez/ds4/pull/761), so DSpark loaded, logged `DSpark target-hidden capture enabled`, and then proposed nothing all session with no warning. A run showing `proposed=0` / `accept_rate=0.00%` in the stats above means the kernels are missing, not that the model is unsuited.

Three further constraints come from ds4 itself, not llamastash: the support file is **checkpoint-specific** (a Flash 0731 support model pairs only with a Flash 0731 model, never an older one), decoding must be **greedy** — sampled requests ignore proposals — and DeepSeek-V4 PRO is unsupported. Speedup is workload-dependent: predictable continuations like code benefit most, while low-yield prompts can come out no faster or slightly slower, since drafting and verification are not free. ds4 publishes its acceptance counters only behind debug env vars, so llamastash reports no DSpark acceptance figure.

### Getting the companion files

`llamastash pull <repo>` now also fetches a model's companion siblings — the **mmproj** projector (multimodal) and any **MTP draft head** — so a pulled model arrives ready to launch:

```bash
llamastash pull owner/repo:model.gguf                 # base + one companion per kind (default)
llamastash pull owner/repo:model.gguf --no-companions # base file only
llamastash pull owner/repo:model.gguf --all-companions # every projector precision / head
```

## vLLM backend

**Experimental.** vLLM serves **safetensors HuggingFace repos** — the non-GGUF half of your cache. A GGUF still binds llama.cpp; vLLM claims repos the GGUF scanner does not. Setup, the ROCm container recipe, and the full knob table are in **[vLLM setup](vllm-setup.md)**.

Enable/disable follows the same tri-state as the other detected backends: unset means on-when-found, `backend.vllm.enabled: false` forces off, and `daemon start --vllm` / `LLAMASTASH_VLLM=1` force on over it.

```bash
llamastash status --json | jq '.backends[] | select(.id == "vllm")'
llamastash list                       # safetensors rows show BACKEND=vllm
llamastash start owner/repo --ctx 4096
```

Three behaviours differ from the GGUF backends and are worth knowing:

- **A vLLM row's path is a directory**, not a weight file — the resolved HF snapshot. `list` shows the repo id as the name, since the directory basename is an opaque revision hash.
- **Detection never runs the binary.** vLLM builds its argument parser through a device probe and fails with `Failed to infer device type` on a host with no usable accelerator, so LlamaStash checks only that the configured path exists. That is also why a container wrapper script works as the `binary`.
- **Startup is slow and readiness waits for it.** Engine init (memory profiling plus KV-cache build) ran 10-27 s on a 0.5B and takes longer on real models. Readiness requires `/v1/models` to advertise the model, not just an answering port.

`--ctx` maps to vLLM's own `--max-model-len`, which is the knob's declared name. Nine further vLLM tunables are declared (`kv-cache-memory-bytes`, `gpu-memory-utilization`, `max-num-seqs`, `tensor-parallel-size`, `dtype`, `kv-cache-dtype`, `quantization`, `enforce-eager`, `trust-remote-code`), each reachable from the CLI, the TUI and presets alike. Flags that start extra processes or listeners, like pipeline and data parallelism (`--data-parallel-*`, `-dp`, `-dpm` and the other short aliases), are refused in the extras tail; the full list is in [vLLM setup](vllm-setup.md). The rest of vLLM's ~240 flags ride the `-- <extras>` tail.

**On unified-memory hosts (APUs), the KV cache is capped automatically.** GPU memory is system RAM there, and vLLM sizes its KV cache against the pool rather than the model — the default has exhausted RAM and frozen a 121 GB machine. When neither `kv_cache_memory_bytes` nor `gpu_memory_utilization` is set, the launcher caps the cache from live free memory, keeping a reserve that covers the engine's own footprint as well as the OS, and passes a `--gpu-memory-utilization` sized to the launch so vLLM's startup check lets the capped launch through. See [vLLM setup](vllm-setup.md#notes-and-limitations).

`backend.vllm.cors` controls cross-origin access, defaulting to `true` because that is vLLM's own behaviour (it allows any origin and offers no switch but `--allowed-origins`). The proxy relays those headers onto its stable port, so while it is on, any page you visit can read completions off the loopback listener. Set it to `false` to pin `--allowed-origins '[]'`.

Known gap: multi-GPU device selection is not wired.

## SGLang backend

**Experimental.** SGLang serves the same **safetensors HuggingFace repos** vLLM does, through `sglang serve`. Setup, the container recipe, the knob table and the guard are in **[SGLang setup](sglang-setup.md)**.

Enable/disable follows the same tri-state: unset means on-when-found, `backend.sglang.enabled: false` forces off, and `daemon start --sglang` / `LLAMASTASH_SGLANG=1` force on over it.

```bash
llamastash status --json | jq '.backends[] | select(.id == "sglang")'
llamastash start owner/repo --backend sglang --ctx 4096
```

With vLLM installed too, a repo lists both engines in `supported_backends` and an `auto` launch picks vLLM; `--backend sglang` selects SGLang. Detection, the directory-shaped row and the slow readiness window are as for vLLM.

`--ctx` maps to `--context-length`. Eight further tunables are declared (`mem-fraction-static`, `max-total-tokens`, `enable-unified-memory`, `max-running-requests`, `quantization`, `trust-remote-code`, `tool-call-parser`, `reasoning-parser`); the rest ride the `-- <extras>` tail minus a denylist that keeps the launch loopback-only, single-host and on the HTTP server the proxy forwards to.

**On unified-memory hosts the KV pool is capped in tokens.** SGLang has no byte-level cap — `--mem-fraction-static` is a fraction of the whole pool — so when neither `max_total_tokens` nor `mem_fraction_static` is set, the launcher divides the shared byte budget by the model's KV bytes per token, read from the repo's `config.json`, and passes `--max-total-tokens`. A repo whose attention geometry cannot be read is refused, naming the override. See [SGLang setup](sglang-setup.md#notes-and-limitations).

`resolved_ctx` on a running SGLang row is read from `/get_server_info`, since SGLang's `/v1/models` carries no context field. There is no `cors` key: SGLang allows every origin and exposes no flag to narrow it.

## Generic backend

Runs any OpenAI-compatible server llamastash has no dedicated backend for, declared under `backend.generic.servers` in `config.yaml`. llamastash reserves the port, polls the readiness path, routes the proxy, and stops the process. It knows nothing else about the engine: flags, env and weights live in the entry or in a wrapper script you write. Config-only on purpose, since `binary` runs as you; no CLI flag or IPC method sets one. Entries are read at process start, so run `llamastash daemon restart` (and reopen the TUI) after editing them.

Tested on 2026-09-26 with gufo `d9a84f1`, Halogen `0.14.0` (Docker image) and CIRU `3cf984c` on a Strix Halo host. Full configs are in § Examples below.

Two shapes:

- **With `model`**: the entry runs catalog GGUFs. `model` is a preset-key glob (`*Flash-Next*`), a path, or a model id, and may match several models. The entry becomes the server `generic-<name>` on each matching row: `list` shows `llamacpp|generic`, and the TUI Server row, `start <model> --server generic-<name>` and a preset's `server:` pick it. llama.cpp stays the default; a plain `start` runs the entry only when the last launch of that model did. The model id is the GGUF's own id, and `{model}` carries its path (shard 1 of a split set).
- **Without `model`**: the entry is its own row, published as `name`. Use this for engines whose weights are not a catalog GGUF (Halogen `.hgn`, CIRU's own package).

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Server id suffix (`generic-<name>`) with `model`; the row's model id without it. No `@`, `/` or spaces. |
| `binary` | yes | Absolute or `~/` path. |
| `ready` | yes | HTTP path that returns 200 once the model is loaded (gufo `/ready`, llama-server `/health`, Halogen `/v1/models`). |
| `model` | no | Catalog GGUF(s) this entry runs, as above. Requires `{model}` in `args` or `env`. |
| `args` | no | argv after `binary`, with placeholders. |
| `knobs` | no | Knob declarations, below. |
| `env` | no | Extra env for the child, with placeholders. |
| `memory_gib` | no | Admission demand for a row without `model` (a `model` launch is sized from its GGUF). Unset means no memory gating. |
| `stop_grace_secs` | no | Minimum SIGTERM-to-SIGKILL grace on every stop path: `stop`, `stop --all`, idle eviction, daemon shutdown. A shorter `--grace` is raised to it. |
| `ready_timeout_secs` | no | Readiness timeout, replacing the default probe budget. |
| `modes` | no | The endpoints the server answers: any of `chat`, `embedding`, `rerank` (e.g. `[chat]`). The proxy refuses a `/v1/embeddings` or `/v1/rerank` request for an entry that doesn't list that mode with a `400 unsupported_endpoint`, before starting it, and `/v1/models` lists the entry under its first mode. Default: all modes, so the engine answers or refuses each request itself. |
| `arch`, `params`, `quant`, `ctx` | no | Row info for an entry without `model`, for the Arch, Params, Quant and Ctx columns of `list`, `show` and the TUI (`params: 80B`, `quant: Q4_K_M`, `ctx: 262144`). Strings show as written; the TUI shortens Ctx (`256k`). `arch` also ranks proxy fallback candidates like a GGUF's arch, but picks no `arch_defaults`. `ctx` defaults to the `ctx: true` knob's `default`; Size comes from `memory_gib` and Mode from `modes`. Refused with `model`, since those rows read their GGUF. A `ctx` below a `--ctx` you pass prints the "exceeds native context" warning. |
| `vision`, `reasoning_effort`, `reasoning_effort_default` | no | What the server accepts, for an entry without `model`, since there is no GGUF to read it from. `vision: true` marks it as taking images, and `reasoning_effort` lists the effort levels it accepts (e.g. `[low, medium, xhigh]`), which also marks it as a reasoning model. Levels are lowercase letters, digits, `-` and `_`, sent as written. `none` in the list means thinking can be turned off; `[none]` alone marks a reasoning model with no levels to pick, so Zed sends earlier thinking back and no tool gets an effort control. `reasoning_effort_default` names the level the server uses when a request sends none; it must be in the list and not `none`. Unset, Codex and Zed, which write one level, write the highest. `integrations` writes all of this into tool configs the way it does for a GGUF with a vision projector and a chat-template effort list (see [`integrations`](#llamastash-integrations-tools)), and the TUI shows the vision glyph. LlamaStash does not check any of it against the server. Refused with `model`. |
| `rewrite_model` | no | `true` makes the proxy replace `body.model` with the launch's `{name}` value. Set it for a server that refuses any other model name (gufo's `--served-model-name`), so a plain id sent to a named launch, or `<model>@<other>` sent to the unnamed one, still reaches it. Default `false`: the body is forwarded unchanged. With it on, the proxy rebuilds each JSON body's top level (keys in their original order, nested values copied byte for byte), so a large body is held twice while it is rewritten. |

**Placeholders** in `args` and `env`: `{port}`, `{host}` (always `127.0.0.1`), `{name}` (the id `/v1/models` publishes for the model, repo-qualified when another model shares its name; `id@launch` for a named launch), `{model}`, and `{<knob id>}`. An unknown placeholder is refused at config load. Braces that don't hold a plain identifier (`{"a": 1}`) stay literal.

**Knobs** are strings passed as `<flag> <value>`, in declaration order, after `args` and before `-- <extras>`. A bare string (`- --seed`) is shorthand for `{flag: --seed}`. Fields: `flag`, `id` (default: the flag without dashes), `default`, `ctx: true` (at most one; makes the knob the context window, so `--ctx`, the TUI Context row and `status` ctx use it), `switch: true` (an on/off knob that sends the bare flag when on and nothing when off; its `default` is `true` or `false`), `label`, `help`. A knob with no value and no default sends nothing. A knob referenced by a placeholder is substituted there instead of emitted; if it has no value the launch is refused. Knob ids may not reuse a built-in knob id or alias (set `id:`). Knobs appear in the TUI editor (an unset knob shows its `default`), presets, `last_params`, `status --json` and `llamastash knobs`, and are set from the CLI with presets or the TUI; `start <model> -- --flag v` passes `--flag v` straight to the engine as an extra; it does not set the knob.

```bash
llamastash list                                            # entries and matching GGUFs
llamastash knobs --backend generic                         # each entry's knobs
llamastash start Qwen3.8-Flash-Next-UD-Q4_K_XL --server generic-gufo --ctx 32768
llamastash start flash-next-halogen --ctx 65536
```

### Rules the backend does not enforce

These are documented, not checked. Break one and the launch fails or misbehaves at your own risk:

- **Bind loopback.** Use `{host}` or bind `127.0.0.1` yourself. llamastash can't see what a foreign binary binds.
- **Stay in the foreground.** The supervised PID must live as long as the server. A wrapper that exits early reads as a crashed launch.
- **One clean stop.** SIGTERM goes to the whole process group once. Turn it into the engine's own clean stop, and finish inside `stop_grace_secs`. A GPU server killed mid-kernel can hang the device.
- **Clean up your own leftovers** at start, and name external resources (containers) after `{port}` so two launches don't collide. There is no orphan adoption after a daemon crash.
- **Multiple launches are your call.** Two launches of one entry get two ports and nothing else: internal ports, GPU memory and disk the engine uses are not checked.
- **Answer to `{name}`** if the engine checks the request's `model` field (gufo does). A client sending a partial name the proxy accepts will get the engine's 404.
- **`--server generic-<name>` is not checked against `model`.** Only the TUI Server row filters by it; the CLI and presets run whatever you pick.

### Examples: gufo, Halogen, CIRU

Three Qwen3.8 Flash-Next engines, tested on a Strix Halo box with gufo `d9a84f1`, Halogen `0.14.0` and CIRU `3cf984c`. Replace the `/path/to/...` parts with your own paths. Each takes 80-100 GB, so run one at a time.

**gufo**: a native binary that runs a catalog GGUF directly, so it uses `model` and needs no wrapper. It checks the request's `model` field, hence `--served-model-name "{name}"`. Loads in about 22 s; `/ready` returns 503 until then.

```yaml
backend:
  generic:
    servers:
      - name: gufo                                  # server id: generic-gufo
        model: Qwen3.8-Flash-Next-UD-Q4_K_XL        # a model id; globs work too
        binary: /path/to/gufo/build/release/gufo
        args: [serve, --host, "{host}", --port, "{port}", --sessions, "1", llm,
               --served-model-name, "{name}", --model, "{model}",
               --mtp-model, /path/to/MTP/mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf,
               --top-p, "0.95", --top-k, "20"]
        knobs:
          - {flag: --context, ctx: true, default: "131072"}
          - {flag: --speculative, default: mtp}
          - {flag: --temperature, default: "1.0"}
          - --seed
        ready: /ready
        stop_grace_secs: 60

presets:
  Qwen3.8-Flash-Next-*:
    entries:
      gufo-128k:
        server: generic-gufo       # the preset picks the engine
        knobs:
          context: 131072
          speculative: mtp
          temperature: '1.0'
```

```bash
llamastash start Qwen3.8-Flash-Next-UD-Q4_K_XL --preset gufo-128k
llamastash start Qwen3.8-Flash-Next-UD-Q4_K_XL --server generic-gufo -- --seed 7
```

**Halogen**: a Docker image configured by `HALOGEN_*` env vars, with its own `.hgn` weights (not a catalog GGUF), so it is its own row. Its knobs feed the container through `env`. `reasoning_effort` lists the levels its chat template accepts, so `integrations` gives the row effort levels in pi, OpenCode, Zed and Codex. Halogen also takes `minimal`, `high` and (from 0.15.1) `max` as aliases of those levels, and `none` turns thinking off. A preset that sets `halogen-effort` changes what the server uses when a request sends no effort, but not `reasoning_effort_default`, so Codex and Zed still write `xhigh` for it. Leave `vision` off unless the wrapper sets `HALOGEN_VISION_TOWER`; without it Halogen refuses images with a `400`.

```yaml
      - name: flash-next-halogen
        binary: ~/bin/halogen-serve.sh
        args: ["{port}"]
        reasoning_effort: [none, low, medium, xhigh]
        reasoning_effort_default: xhigh      # same as halogen-effort's default
        knobs:
          - {flag: --ctx-window, id: halogen-ctx, ctx: true, default: "131072"}
          - {flag: --halogen-temperature, id: halogen-temp, default: "1.0"}
          - {flag: --halogen-mtp-depth, id: halogen-mtp-depth, default: "3"}
          - {flag: --halogen-reasoning-effort, id: halogen-effort, default: xhigh}
        env:
          HALOGEN_MODEL_ID: "{name}"
          HALOGEN_CTX: "{halogen-ctx}"
          HALOGEN_KV_POOL_POSITIONS: "{halogen-ctx}"
          HALOGEN_TEMPERATURE: "{halogen-temp}"
          HALOGEN_MTP_DEPTH: "{halogen-mtp-depth}"
          HALOGEN_REASONING_EFFORT: "{halogen-effort}"
        ready: /v1/models
        stop_grace_secs: 90
        ready_timeout_secs: 600

presets:
  flash-next-halogen:
    entries:
      halogen-medium:
        knobs:
          halogen-effort: medium   # shorter thinking, faster turns
```

```bash
llamastash start flash-next-halogen --preset halogen-medium
```

`~/bin/halogen-serve.sh`:

```sh
#!/bin/sh
# llamastash generic wrapper for Halogen 0.14.0. Bridge network published on
# loopback only: the image's API binds 0.0.0.0 inside the container.
# $1 = port; HALOGEN_* come from the entry env.
port="$1"
name="llamastash-halogen-$port"          # per port, so two launches don't collide
docker rm -f "$name" >/dev/null 2>&1      # leftover from a crashed daemon
hub=/path/to/huggingface/hub
hg=/hub/models--peonist-ai--halogen-qwen3.8-flash-next/snapshots/<revision>
docker run -d --rm --name "$name" -p "127.0.0.1:$port:8080" \
  --device /dev/kfd --device /dev/dri \
  --group-add "$(getent group video | cut -d: -f3)" --group-add "$(getent group render | cut -d: -f3)" \
  --ipc=host --ulimit memlock=-1:-1 -v "$hub":/hub:ro \
  -e HALOGEN_API_PORT=8080 -e HALOGEN_MODEL_ID \
  -e HALOGEN_CHECKPOINT="$hg/qwen38-flash-next-w4b.hgn" \
  -e HALOGEN_MTP_HEAD="$hg/qwen38-flash-next-mtp.hgn" -e HALOGEN_TOKENIZER="$hg/tokenizer" \
  -e HALOGEN_CTX -e HALOGEN_KV_POOL_POSITIONS -e HALOGEN_MTP_DEPTH -e HALOGEN_REASONING_EFFORT \
  -e HALOGEN_MAX_TOKENS_DEFAULT=16384 -e HALOGEN_TEMPERATURE -e HALOGEN_TOP_P=0.95 -e HALOGEN_TOP_K=20 \
  ghcr.io/peonist-ai/halogen-flash-server:0.14.0 >/dev/null || exit 1
# One clean stop: SIGTERM from llamastash becomes `docker stop`, which the
# image turns into an engine shutdown (its own SIGKILL comes 30 s later).
trap 'docker stop -t 60 "$name" >/dev/null 2>&1' TERM INT
docker logs -f "$name" 2>&1 &
docker wait "$name" >/dev/null &
wait $!
```

- `docker run -d` plus `docker wait` keeps the wrapper in the foreground while the container never sees the process-group SIGTERM directly, so the engine gets exactly one signal. Keep `docker stop -t` above the image's 30 s and `stop_grace_secs` above `-t`.
- Halogen 0.14.0's `all` mode binds its API on `0.0.0.0` whatever `HALOGEN_BIND` says (that variable covers only the internal engine port). Hence the bridge port published on `127.0.0.1` instead of `--network host`, which also keeps two launches' internal engine ports apart.
- It does not check the request's `model` field. Cold load took 93-105 s here, about 6 s when the weights are still in page cache.
- `HALOGEN_REASONING_EFFORT` sets the default thinking effort: `minimal`, `low`, `medium`, `high` or `xhigh`, mapped to the template's `low`, `medium` and `xhigh`. Unset, the template's own default applies, which is `xhigh`. A request that sends `reasoning_effort` still wins. The wrapper has to pass the variable through (`-e HALOGEN_REASONING_EFFORT`), or the preset has no effect.

**CIRU**: a `run-server.sh` launcher configured by env vars that `exec`s its own llama-server build and passes extra args through. The wrapper exports the fixed variables; per-launch values go in the entry's `env`.

```yaml
      - name: flash-next-ciru
        binary: ~/bin/ciru-serve.sh
        knobs:
          - {flag: --ciru-ctx, id: ciru-ctx, ctx: true, default: "65536"}
          - {flag: --mtp-depth, id: mtp-depth, default: "3"}
        env:
          PORT: "{port}"
          CONTEXT_SIZE: "{ciru-ctx}"
          MTP_DEPTH: "{mtp-depth}"
        ready: /health
        stop_grace_secs: 60
        ready_timeout_secs: 600
```

`~/bin/ciru-serve.sh`:

```sh
#!/bin/sh
# llamastash generic wrapper for CIRU (3cf984c). Env comes from the entry;
# extra args pass through to llama-server.
export ROCM_ROOT=/path/to/ciru/.venv-rocm/lib/python3.12/site-packages/_rocm_sdk_devel
export CIRU_RUNTIME_ROOT=/path/to/ciru/runtime
export MODEL_DIR=/path/to/huggingface/hub/models--jcbtc--Qwen3.8-Flash-CIRU-STRIX-IU4/snapshots/<revision>
export ENABLE_UI=0 HOST=127.0.0.1
exec /path/to/ciru/scripts/ciru/run-server.sh "$@"
```

- `exec` hands the PID to llama-server, so llamastash's one SIGTERM reaches the engine directly.
- CIRU answers under its own alias (`Qwen3.8-Flash-CIRU-STRIX-IU4`) and does not check the request's `model` field. Loads in about 115 s.

### Running ds4 as a generic server

[ds4](https://github.com/antirez/ds4) (antirez's DwarfStar) runs the DeepSeek-V4 Flash/PRO GGUFs from [huggingface.co/antirez/deepseek-v4-gguf](https://huggingface.co/antirez/deepseek-v4-gguf) through its `ds4-server` binary. It was a dedicated backend up to 0.4.0. It's now a generic server entry, which gives the same argv, readiness and speed (tested on 2026-09-28 with ds4 `0aaea5a` and the Flash IQ2_XXS `0731` GGUF: 13.2 t/s either way). You build `ds4-server` yourself (`git clone https://github.com/antirez/ds4 && cd ds4 && make`).

```yaml
backend:
  generic:
    servers:
      - name: ds4                                   # server id: generic-ds4
        model: "DeepSeek-V4-*"
        binary: /path/to/ds4/ds4-server
        args: [-m, "{model}", --host, "{host}", --port, "{port}"]
        ready: /v1/models            # ds4-server binds its port only after the load
        knobs:
          - {flag: --ctx, id: ds4-ctx, ctx: true}
          - --power
          - --tokens
          - {flag: --threads, id: ds4-threads}
          - --kv-disk-dir
          - --kv-disk-space-mb
          - {flag: --ssd-streaming, switch: true}
          - --ssd-streaming-cache-experts
          - --ssd-streaming-preload-experts
          - {flag: --ssd-streaming-cold, switch: true}
          - {flag: --warm-weights, switch: true}
          - {flag: --quality, switch: true}
          - {flag: --mtp, id: mtp-model}
          - {flag: --mtp-draft, id: ds4-mtp-draft}
          - --mtp-margin
          - {flag: --dspark, switch: true}
          - --dspark-confidence
          - {flag: --dspark-strict, switch: true}

presets:
  DeepSeek-V4-Flash-*-0731.gguf:
    default: ds4-mtp
    entries:
      ds4:
        server: generic-ds4
      ds4-mtp:
        server: generic-ds4
        knobs:
          mtp-model: /path/to/DeepSeek-V4-Flash-MTP-Q4K-Q8_0-F32.gguf
      ds4-dspark:
        server: generic-ds4
        knobs:
          dspark: true
          mtp-model: /path/to/DeepSeek-V4-Flash-DSpark-support-0731.gguf
```

```bash
llamastash start DeepSeek-V4-Flash-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix-0731 --ctx 32768
llamastash start DeepSeek-V4-Flash-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix-0731 --preset ds4-dspark
```

The knob ids match the old backend's, so presets keep working. The exceptions are `ds4-ctx`, `ds4-threads` and `ds4-mtp-draft`, because `ctx`, `threads` and `mtp-draft-n` are built-in llama.cpp ids. `--ctx` still sets the context window. Any other `ds4-server` flag goes in the extras tail after `--`.

**Migrating from the ds4 backend.** A config that still has `backend.ds4:` is rejected at load, and the error points here.

1. Stop every running ds4 model (`llamastash stop <model>`) **before** you upgrade. The new daemon can't re-adopt a `ds4-server` the old backend started: it keeps running and holds its port and memory, but `status` doesn't list it and `stop` can't reach it. If one is left over, find it with `pgrep -a ds4-server` and `kill` its PID.
2. Delete the `backend.ds4:` block, and drop `--ds4` / `LLAMASTASH_DS4` from your scripts.
3. Add the `generic` entry above, with `binary` set to your `ds4-server`.
4. In your ds4 presets, replace `backend: ds4` with `server: generic-ds4`. llama.cpp is now the default server for these GGUFs, so a preset or `--server generic-ds4` picks ds4. A leftover `backend: ds4` with no `server:` doesn't fail: it runs the model on llama.cpp. The same goes for the remembered last launch of a ds4 model, until you launch it once with `--server generic-ds4`.
5. Set `mtp-model` in a preset to pair a draft head. The old backend found the MTP or DSpark file next to the model on its own; a generic entry doesn't.
6. Run `llamastash daemon restart`.

What the dedicated backend did that the generic entry doesn't:

- **No automatic routing or fallback.** A DeepSeek-V4 GGUF defaults to llama.cpp. llama.cpp runs these files from **b9840** on ([ggml-org/llama.cpp#24162](https://github.com/ggml-org/llama.cpp/pull/24162)); an older `llama-server` fails with `unknown model architecture: 'deepseek4'`.
- **No automatic SSD streaming.** Set `ssd-streaming: true` yourself on a box below the memory floor (about 128 GB on CUDA/ROCm, 96 GB on Metal). The admission check still sizes the launch from the GGUF, so a streaming launch that doesn't fit needs `start --force`.
- **No MTP and streaming reconciliation.** `ds4-server` refuses `--ssd-streaming` together with `--mtp` after the full model load, so don't set both.
- **No split-half guard.** The PRO `*-Layers00-30.gguf` / `*-Layers-31-output.gguf` halves are for ds4's distributed mode; launch a single-file GGUF instead.

Two `ds4-server` behaviors to know about. Its `/v1/models` always lists both `deepseek-v4-flash` and `deepseek-v4-pro`, whatever is loaded; chat requests echo back the `model` you send, so no `rewrite_model` is needed. And `--kv-disk-dir` is ds4's own persistent cache: it holds conversation data under ds4's permissions at exactly the path you give, so point it at a private directory. See [DSpark speculative decoding](#dspark-speculative-decoding) for the DSpark caveats.

## Proxy (OpenAI-compatible listener)

The daemon binds a single OpenAI-compatible HTTP proxy on `127.0.0.1:11435` (default mode) so any agent that speaks the OpenAI REST shape — OpenCode, Pi (pi.dev), the OpenAI SDKs, Cline, llm-cli — can talk to every discovered model through one stable URL. The default port is `11435` (one above Ollama's `11434`) so llamastash co-exists with an installed Ollama daemon without a collision. If the base port is taken the listener walks up to `11440` and binds the first free slot — the actual address is reported via `llamastash status` / the TUI Daemon pane under `proxy.listen`.

The installable Agent Skills bundle for this flow lives under [`skills/llamastash/`](https://github.com/llamastash/llamastash/tree/main/skills/llamastash). Claude Code, OpenClaw, OpenCode, and similar harnesses can install it by copying that directory into their configured skills path.

The proxy resolves `body.model` against the same fuzzy matcher `llamastash start <ref>` uses, forwards the request byte-for-byte to the matching `llama-server` child, and streams the response back. If the named model isn't running, the proxy auto-starts it (replaying `last_params`, else `arch_defaults`). A model that is already *loading* is waited on rather than started again, no matter which surface launched it, so a request arriving mid-load never yields a second copy of the same model. The launch mode follows the endpoint that triggered it (`/v1/embeddings` starts the model in embedding mode, `/v1/rerank` in rerank mode), then the recorded `last_params` mode, then the GGUF's own hint; a model whose hint says `chat` is never started in embedding or rerank mode, since that would lock it out of chat completions for its whole lifetime. If the launch fails and another model is already Ready, the proxy falls back to it and stamps `x-llamastash-served-by` + `x-llamastash-fallback-reason: launch_failed` headers on the response. Substitution is observable; no extra round-trip is needed to discover what served the request. The full mechanism — coalesced launches, family-MRU fallback selection, scope boundaries — is documented in [`docs/plans/2026-05-21-001-feat-proxy-router-plan.md`](https://github.com/llamastash/llamastash/blob/main/docs/plans/2026-05-21-001-feat-proxy-router-plan.md).

Routes served: `/v1/models`, `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/rerank`, the OpenAI `/v1/responses` (+ `/v1/responses/input_tokens`), and the Anthropic `/v1/messages` + `/v1/messages/count_tokens`.

### Model ids on the proxy

Every model `/v1/models` (and `/api/tags`) lists carries an id that resolves back to exactly that model. Normally that is the plain name: a GGUF's file stem (`Qwen3.8-27B-Q8_0`), a safetensors repo's id (`Qwen/Qwen3-0.6B`), an Ollama model's `<name>:<tag>`.

When two models would publish the same plain id, each takes the shortest longer form that nothing else in the catalog claims. Models whose plain id is already unique are untouched, so ids in existing tool configs keep working. The escalation, in order:

1. **Repo-qualified** — the `REPO` label `llamastash list` shows, a `/`, then the plain id: `unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M`. Enough whenever the two copies live in different repos.
2. **Source-qualified** — the discovery source in front of that: `huggingface/lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M` vs `lm-studio/lmstudio-community/…`. This is the rung the ordinary duplicate needs — one repo cached by two different tools derives the *same* `owner/repo` from both roots, so step 1 cannot separate them.
3. **The full canonical path**, when even that collides — the same file name in two subdirectories of one repo, reached through one source.

A **named launch** publishes one more id: the model's published id, an `@`, and the launch name (`Qwen3.8-27B-Q4_K_M@coder`). These come from the live launch registry rather than the disk catalog, so they appear while the launch runs and drop when it stops, and the model half is the same disambiguated id the catalog row publishes. Sending a named id that has no live launch goes to the model's only launch when that launch is unnamed (running or still loading) and the name is not one of the model's presets, instead of loading a second copy with the same settings. Otherwise it auto-starts one carrying that name, so a client holding a cached id recovers instead of erroring. That auto-start also reads the name as a preset: if the model has a preset called `coder`, `qwen3@coder` launches under it, otherwise under the model's `default:` preset as before. A request body carries nothing but `model`, so this is the only way a client picks a preset — it applies to proxy auto-starts only, never to `start --name` or the TUI, where `--preset` already chooses one. A model file whose own name contains an `@` still resolves whole, and the split is taken at the last `@`.

A request for the plain model id while it runs more than once goes to an unnamed launch before a named one, and among those to the newest (highest `L#`).

The resolver accepts every form for every model, collision or not, and each qualified form in both the published spelling and the `.gguf` filename spelling. It also accepts a partial repo reference (`unsloth/Qwen3.8`), which the raw cache path (`models--unsloth--Qwen3.8-…`) never matched. Sending any form two models share — the bare name, or a repo-qualified form that does not separate them — returns `400 ambiguous_model`, and its `matches` array lists the published id of each candidate, every one of which routes, so resend one verbatim.

### Anthropic-shape clients (Claude Code)

llama-server speaks the Anthropic Messages API natively, so the proxy forwards `/v1/messages` and `/v1/messages/count_tokens` on the same path as the OpenAI routes — no body translation, apart from the one effort field below. Point Claude Code (or anything that drives the Anthropic shape) at the proxy with `ANTHROPIC_BASE_URL` (no `/v1` suffix — the SDK appends `/v1/messages` itself):

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
- **`/effort` reaches a llama.cpp model.** Claude Code sends effort as `output_config.effort`, which llama.cpp's own `/v1/messages` translation drops, so the proxy copies that one value to `chat_template_kwargs.reasoning_effort` before forwarding and leaves every other byte alone. A `chat_template_kwargs.reasoning_effort` the client sends itself wins over it. `count_tokens` gets the same mapping, so its count matches the request that follows.
- **The mapping overrides a launch-time effort.** A per-request kwarg beats what the server was launched with, so a `-- --reasoning-effort low` (or a chat-template-kwargs extra) no longer applies to a client that sends an effort on every request, which Claude Code does (its own model default when you never pick one). Set `backend.llamacpp.map_anthropic_effort: false` to let the launch and the engine defaults stand.
- **An effort the model's template rejects fails the request.** The value passes through unclamped and the template decides, so the error names that model's own set: `Unexpected reasoning effort max. Supported types are xhigh (default), medium, and low.` on Qwen3.8 (which also maps `high` onto `xhigh`). llama.cpp answers that with HTTP 500 and the template's own text, so a client can retry it a few times before showing it. With no mapping the field was simply dropped and the request went through on the engine default.
- Other backends forward the body untouched. Halogen reads `output_config.effort` itself. gufo's `/v1/messages` refuses unknown fields with `400 request field '<x>' is not supported on this endpoint`, so Claude Code can't drive that engine today.
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
OpenAI-standard (`id` / `object` / `created` / `owned_by`) plus a `mode`
field (`chat`, `embedding`, `rerank`) that standard clients ignore. `list --json`
carries the same as `mode_hint` (under the nested `metadata` block) along with
the context size, so generate the block from it and filter to just the chat
models:

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

**Named launches are not discovered.** Neither generator sees them: `list --json` is a catalog listing, and the discovery plugin reads `/v1/models` once at startup, while a named id exists only while its launch runs. Until [the patchers learn to emit them](../TODO.md), add one by hand: duplicate the model's block and append `@<name>` to the key (and to its `name`), so `Qwen3.8-27B-Q4_K_M` gains a sibling `Qwen3.8-27B-Q4_K_M@coder`. The proxy auto-starts the named launch on first use, so the entry works even when nothing is running yet. The same applies to pi's `~/.pi/agent/models.json`.

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
| `GET`  | `/v1/models`           | OpenAI-shape `{"object":"list","data":[…]}` listing every discovered model. Each row carries `id` (the published model id — see [Model ids on the proxy](#model-ids-on-the-proxy)), `object: "model"`, `created: 0` (no stable epoch — the catalog has no creation timestamp; documented choice), `owned_by: "llamastash"`, and `mode` (`chat`, `embedding` or `rerank`, from the GGUF header or a generic entry's `modes`; absent when unknown). Sorted by `id` so the output is byte-stable across calls.                                                                                                                   |
| `POST` | `/v1/chat/completions` | OpenAI chat completions. Streaming (`stream: true`) is byte-piped end-to-end — SSE chunks reach the agent in the same order with the same framing the upstream `llama-server` emitted.                                                                                                                                                                                                                                                               |
| `POST` | `/v1/completions`      | OpenAI text completions. Same forwarding semantics.                                                                                                                                                                                                                                                                                                                                                                                                  |
| `POST` | `/v1/embeddings`       | OpenAI embeddings. JSON pass-through.                                                                                                                                                                                                                                                                                                                                                                                                                |
| `POST` | `/v1/rerank`           | llama.cpp's rerank endpoint (also exposed under the `/v1/` prefix for client uniformity). JSON pass-through.                                                                                                                                                                                                                                                                                                                                         |
| `GET`  | `/api/tags`            | **Ollama compat — discovery.** Ollama-shape `{"models":[{name, model, modified_at, size, digest, details:{format,family,parameter_size,quantization_level,…}}]}` projection of the discovered catalog. Sorted alphabetically by `name`. Empty catalog → `{"models":[]}`. See [Ollama-compat surface](#ollama-compat-surface).                                                                                                                        |
| `GET`  | `/api/version`         | **Ollama compat.** `{"version":"<crate-version>"}` — same value `status.daemon.build` surfaces.                                                                                                                                                                                                                                                                                                                                                      |
| `GET`  | `/api/ps`              | **Ollama compat.** Currently-Ready supervisors in Ollama's running-list shape (`{models:[…{expires_at, size_vram, …}]}`). `expires_at` is a far-future placeholder until idle-TTL eviction lands (R34 deferred); `size_vram` is `0` until per-PID VRAM attribution lands.                                                                                                                                                                            |
| `POST` | `/api/show`            | **Ollama compat.** `{"model":"<name>"}` or `{"name":"<name>"}` body → per-model metadata in Ollama shape (`{modelfile, parameters, template, details, model_info, capabilities}`). Same fuzzy resolver as `/v1/chat/completions`.                                                                                                                                                                                                                    |

Request body cap: **`proxy.max_body_size` bytes, default 16 MiB**, enforced via `http-body-util::Limited` before forwarding. Anything larger returns HTTP 413 naming the configured limit. Text-only chat completions are typically well under 1 MiB even with long histories; 16 MiB covers vision payloads (a base64 image is ~33% larger than the source file — one phone photo fits with room to spare) while still bounding worst-case per-request memory. The cap is **per request body, not a global pool** — N concurrent max-size requests buffer up to N × the cap, so a LAN-exposed proxy (`proxy.host`) with many large in-flight bodies can use more RAM than the cap alone suggests. `0` disables the check: one request can buffer arbitrary RAM (we buffer in memory — unlike nginx's `client_max_body_size 0`, which spools to disk, so its `0` carries a safety property ours does not). To stop serving bodies altogether, `proxy.enabled: false` is the honest switch.

### Ollama-compat surface

The four `/api/*` endpoints above let Ollama-shape discovery libraries — `ollama-python`'s default code path, IDE plugins that probe `GET /api/tags` to detect Ollama, `OLLAMA_HOST`-based env discovery in agent frameworks — recognise llamastash as Ollama-compatible. Once recognised, clients fall through to the OpenAI-compat surface (`/v1/chat/completions` etc.) for actual inference, which already works against llamastash without further changes. This unlocks OOB compatibility with anything that "speaks Ollama" for discovery but uses OpenAI shape for completions — the most common pattern in the agent ecosystem.

The Ollama **inference** endpoints (`POST /api/chat`, `POST /api/generate`, `POST /api/embed`) are **not** implemented in v1. They emit a different request/response shape than OpenAI compat (newline-delimited JSON streaming, different field names) and would require request/response body translation — incompatible with the proxy's current byte-pure forward path. Tracked in TODO §Low priority as a brainstorm/plan item. For now, point Ollama-shape _inference_ clients at `OLLAMA_HOST=http://127.0.0.1:11434` and they will discover models via `/api/tags`, then fall through to the OpenAI-compat completion endpoints on those same client libraries that support both shapes (most do).

A few field-level details where llamastash's projection diverges from Ollama's:

- **`digest`** — Ollama uses `sha256:<hex>`; llamastash uses `blake3:<hex>` derived from the canonical path string of the discovered file. The value is stable across `/api/tags` and `/api/ps` for the same model — both endpoints hash the same path — so clients can join the two endpoints by digest. It is **not** the GGUF header BLAKE3 that `ModelId` carries internally; re-reading the header on every `/api/tags` row would brick discovery, and the catalog doesn't cache the header hash today. Lifting the digest to the truthful header BLAKE3 is tracked in [TODO §Low priority](https://github.com/llamastash/llamastash/blob/main/TODO.md) ("Ollama-compat digest from cached header BLAKE3"). Clients that round-trip the digest opaquely keep working; clients that _validate_ the algorithm see the truthful `blake3:` tag rather than a misleading `sha256:` prefix on a non-SHA-256 hash.
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
| 400  | `ambiguous_model`                                            | Fuzzy match returned >1 candidate. `matches` lists each candidate under the id `/v1/models` publishes it as, so every entry can be sent straight back as `body.model`; the client retries with one of those, a full path, or a tighter substring.                                                                                                                                                                                                          |
| 400  | `invalid_request`                                            | Request body wasn't valid JSON, or the HTTP method couldn't be translated for forwarding.                                                                                                                                                                                                                                                                                                                                                                  |
| 404  | `model_not_found`                                            | Fuzzy match returned zero candidates. `matches` is omitted from the body when empty (the field is `Option`-shaped and serialised with `skip_serializing_if`).                                                                                                                                                                                                                                                                                              |
| 404  | `not_found`                                                  | No such route (unknown path _or_ wrong HTTP method on a known path — e.g. `GET /v1/chat/completions`).                                                                                                                                                                                                                                                                                                                                                     |
| 413  | `payload_too_large`                                          | Request body exceeded `proxy.max_body_size` (default 16 MiB).                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 502  | `upstream_unreachable`                                       | The model was Ready a moment ago but the connect to `llama-server` failed (process exited between snapshot and forward, kernel-level refusal, …). The agent sees this rather than a hanging socket.                                                                                                                                                                                                                                                        |
| 503  | `launch_failed`                                              | Auto-start failed and no Ready models exist for fallback. `running: []` is always present on this arm. The list reflects models that were **in `Ready` state at the moment the proxy snapshotted the supervisor registry for fallback** — models in `Launching` / `Loading` are not included, so an empty list does not mean "the daemon has nothing alive," only "no candidate was available for instant fallback." Retry once the slow launch completes. |

Upstream non-2xx responses (e.g. `llama-server` returns 500 for a malformed completion request) are passed through verbatim — same status code, same body bytes; the OpenAI-shape envelope above only covers errors the proxy itself emits. Mid-stream upstream death: once headers are sent the routing decision is committed; if the upstream stream errors after that point, the proxy closes its connection to the agent (the agent sees a truncated SSE / chunked body) — no retry, no fallback.

### Keeping the prompt cache across an unload (opt-in)

When the idle sweep or make-room stops a model mid-conversation, the next request reloads it and processes the whole prompt again. With `backend.llamacpp.slot_save.enabled: true` the daemon saves the model's prompt cache to disk before it stops the launch and reads it back when the model starts again, before the first request reaches it.

Measured on Llama-3.2-1B with a 100,000-token prompt (llama.cpp b11457): processing the prompt again takes 73 to 82 s, a save takes 0.3 to 2.7 s and a restore 0.3 to 1.1 s. Numbers and method are in [`docs/spikes/2026-10-06-slot-save-restore.md`](spikes/2026-10-06-slot-save-restore.md).

What to expect:

- **llama.cpp chat launches only.** Embedding and rerank launches and the other backends are not affected.
- **Full-attention models only, for now.** llama-server reuses a restored cache for those. For hybrid models (`qwen35`, which covers Qwen3.5 and Qwen3.8) and sliding-window models (`gemma4`) it restores the file and then processes the whole prompt anyway ([llama.cpp#28194](https://github.com/ggml-org/llama.cpp/issues/28194)). The daemon checks this with two short requests when a proxy-started model first loads and saves nothing where it would not help. The check runs against the server itself, so these models start saving once a llama.cpp build reuses their restored cache.
- **Saved before an eviction only.** `stop`, `daemon stop` and `daemon restart` do not save.
- **Same model, same settings.** A file is read back only by a launch of the same model file with the same server build and launch flags. Start the model with a different preset or `--ctx` and it loads without the file.
- **The most recent conversation.** llama-server keeps one conversation per slot and, with the default slot settings, moves the others into memory that is lost with the process. The slots that still hold a prompt are what gets saved.
- **Disk.** Files live in `<cache dir>/slots/` (`~/.cache/llamastash/slots/` on Linux). A slot file holds the conversation's tokens, so the directory is created owner-only (`0700`). A file is deleted when it is read back, after `max_age_secs`, or oldest first when the total passes `max_gib`. A slot under `min_tokens`, or one that would not fit the cap or the free disk space, is not saved. Turning the option off leaves existing files in place; delete the directory to reclaim the space.
- **Eviction takes longer by the save time**, and a request that arrives during the save keeps the model loaded.

With this on, every llama.cpp chat launch gets `--slot-save-path <cache dir>/slots`, which also enables llama-server's `POST /slots/{id}?action=save|restore|erase` on the launch's loopback port and through `/ui/`. Pass your own `--slot-save-path` after `--` to manage slot files yourself; the daemon then leaves that launch alone.

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
  # host: 0.0.0.0        # LAN bind (requires api_key unless insecure_no_auth).
  # api_key: "..."       # Bearer token enforced whenever set.
  # fallback_enabled: true   # Family-MRU fallback on auto-start failure.
  # header_read_timeout_secs: 30
  # idle_ttl_secs: 1800      # 0 disables the global deadline; a preset can still pin its own.
  # max_body_size: 16777216  # Bytes; cap on every request body (default 16 MiB; 0 disables the check).
```

Unknown keys inside `[proxy]` are **rejected loudly** (`#[serde(deny_unknown_fields)]`) — a typo never silently falls back to defaults. The top-level config still tolerates unknown keys for forward-compat. No `tls_*` — TLS for a LAN-exposed proxy is still deferred per the plan's Scope Boundaries. The full key set with per-key sources is in `config.example.yaml` under `[proxy]`.

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

Examples: `llamastash init server --install gh-releases`, `llamastash init models --json`, `llamastash init config --config-step write`. Bare `llamastash init` (no subcommand) still runs the full wizard and honors the `--only` / `--skip` flags. Two steps also have top-level shortcuts that skip the wizard entirely: [`llamastash recommend`](#llamastash-recommend) and [`llamastash integrations`](#llamastash-integrations-tools).

| Flag                     | Effect                                                                                                                                                                  |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--recommended`          | Accept the hardware-aware default for every prompt; no prompts fire. Canonical form.                                                                                    |
| `--yes`                  | Hidden alias for `--recommended`. Preserved for script and agent compatibility.                                                                                         |
| `--json`                 | Emit a structured summary (schema: `schema_version`, `steps_ran`, `steps_skipped`, `install`, `model`, `config`, `smoke`, `hardware`) and skip all human prose.         |
| `--offline`              | Refuse outbound network. Useful for `--only config` / `--only server` reruns where the model and snapshot are already cached. `LLAMASTASH_OFFLINE=1` is equivalent.     |
| `--only <STEPS>`         | Comma-separated list of `server,models,config,integrations` (other names rejected). Only the listed steps run. Or run one step as a subcommand: `init server`.            |
| `--skip <STEPS>`         | Inverse of `--only`. Mutually exclusive with it (clap refuses both).                                                                                                    |
| `--install <CHOICE>`     | Pre-answer the install-method prompt. Values: `brew`, `gh-releases`, `gh-releases:vulkan`, `existing`, `custom:<PATH>`. Override beats `--recommended`. See [Linux + NVIDIA](#linux--nvidia-cuda-or-vulkan) for `gh-releases:vulkan`. |
| `--model <CHOICE>`       | Pre-answer the model-pick prompt. Values: `recommended`, `none`, `<owner>/<repo>[:<filename>.gguf]`.                                                                    |
| `--config-step <CHOICE>` | Pre-answer the config-write confirm. Values: `write`, `skip`. (Named `--config-step` rather than `--config` because the top-level `--config <PATH>` is already global.) |

#### Linux + NVIDIA: CUDA or Vulkan

On Linux with an NVIDIA card, the GitHub Releases install picks llama.cpp's CUDA build when the driver can run it, and the Vulkan build otherwise. A host with both an NVIDIA and an AMD card keeps the Vulkan build, since a CUDA build drives only the NVIDIA card. The CUDA build ships with a `cudart-` bundle (`libcudart`, `libcublas`, `libcublasLt`) that `init` downloads and places next to `llama-server`, so no CUDA toolkit is needed.

| Driver | Card (compute capability) | x86_64 | arm64 |
| --- | --- | --- | --- |
| 580 or newer | 7.5 or newer (Turing and later) | CUDA 13 | CUDA 13 |
| 580 or newer | older than 7.5 (Maxwell, Pascal, Volta) | CUDA 12 | Vulkan |
| 525 to 579 | any | CUDA 12 | Vulkan |
| older, or unknown | any | Vulkan | Vulkan |

The driver version and each card's compute capability come from `nvidia-smi` (the driver alone from `/proc/driver/nvidia/version` when `nvidia-smi` fails). Upstream's CUDA 13 build has no kernels for cards older than 7.5, and such a card still shows up in `--list-devices`, so the check below would not catch it. With several cards the oldest one decides. When the compute capability is unknown, x86_64 takes CUDA 12, which covers every card a 525+ driver supports.

The download is larger: about 560 to 730 MiB for build plus runtime (CUDA 13 x86_64 565 MiB, CUDA 13 arm64 667 MiB, CUDA 12 730 MiB at `b11316`), against about 30 MiB for Vulkan. `--recommended`, `--json` and non-interactive runs take the CUDA build too, and `--install gh-releases:vulkan` picks Vulkan instead. The progress line shows the size; under `--json` there is no progress line, so the size is only in the log file (`llamastash.log`), or on stderr with `--verbose`. The interactive picker offers both builds (`GitHub Releases · CUDA 13` and `GitHub Releases · Vulkan`; `· CUDA` on Windows). Downloads stream to disk under the install root and are hashed on the way; the step checks for about three times the download size in free space first. A CUDA build installs to its own directory (`llama-cpp/<tag>-cuda-<ver>-<arch>/`). A re-run reuses a build already in its directory without downloading it again. Temp files an interrupted run left behind (`.download.*`, `*.tmp.*`) are removed by the next run once they are 10 minutes old.

After the install, `init` runs `llama-server --list-devices`. llama.cpp loads its CUDA backend as a plugin and skips it when it cannot load, so a broken CUDA install still starts and passes `--version`, on the CPU. When the list shows no CUDA device, `init` says so, removes the CUDA build, and installs the Vulkan build instead. It records the failed build in `llama-cpp/.cuda-failed` under the state dir. Later runs default to the Vulkan build: the picker still offers CUDA, with a `listed no CUDA device last time` hint, and `--recommended` or a non-interactive run takes Vulkan and says so. Picking CUDA or passing `--install gh-releases` tries it again, and a CUDA build that then lists a device clears the record. When the check itself fails (timeout after 120 s, non-zero exit), it keeps the CUDA build and says how to check by hand.

On Windows with an NVIDIA card the install picks the `win-cuda` zip, which carries no CUDA runtime DLLs, so it needs a CUDA toolkit on `PATH`. The same check runs there: without a toolkit the zip lists no CUDA device and `init` installs the `win-vulkan` build instead. `--install gh-releases:vulkan` picks the Vulkan build directly.

The three per-step flags are **advisory, not authoritative**: supplying `--install brew` for a step that `--skip server` already excludes emits one stderr warning and proceeds. Conflicting axes don't abort.

Non-interactive contract: when stdout isn't a terminal and `--recommended` is not set, the wizard emits one consolidated stderr warning, then the install + model steps use recommended defaults silently. The config-write step refuses to proceed without explicit consent — pass `--recommended`, `--config-step write`, or `--config-step skip`. Without that consent the wizard aborts with exit `72` after persisting whatever durable state earlier steps already wrote (so `doctor` sees the partial baseline).

### `llamastash doctor`

Read-only diagnostic (its one write is the memory-drift baseline refresh). Re-runs hardware detection, diffs against `_init_snapshot.json`, and emits findings with stable ids agents can branch on: `binary_missing`, `binary_digest_drift` (skipped on brew installs — routine `brew upgrade` legitimately rotates the digest), `hardware_drift`, `memory_drift`, `gtt_hint`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`, plus two configured-server advisories — `server_binary_missing` (Warning: a `backend.<id>.servers[].binary` path no longer resolves) and `servers_configured` (Info: a summary of the resolvable servers and their device counts; silent when no `servers:` are configured). All of these ids are additive, so `schema_version` stays `2`; readers refuse only versions above their max. When the local benchmark snapshot looks stale, `doctor` probes the latest remote (the same one the recommender prefers) before judging `snapshot_stale`, so it only fires when no fresher snapshot is actually reachable; `LLAMASTASH_OFFLINE` skips that probe.

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

### `llamastash integrations [tools...]`

Shortcut for `init --only integrations` that points your AI dev tools at the local proxy without walking the wizard. Patches each selected tool's config with the proxy URL and every model you have **favorited**, and writes the sourceable env snippets.

```
llamastash integrations [TOOLS] [--integrations <TOOLS>] [--json]
```

| Flag / arg              | Effect                                                                                                                                             |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `[TOOLS]`               | Tool ids to patch, space- or comma-separated: `opencode`, `aider`, `continue`, `zed`, `pi`, `codex`, `env-sh`, `claude-code`. Omit for the interactive multiselect; `none` runs the step and patches nothing. |
| `--integrations <TOOLS>` | Same list in flag form, for parity with `init --integrations`.                                                                                     |
| `--json`                | Same `{"steps_ran": ["detect","integrations"], "integrations": {"applied": [...], "failed": [...]}}` shape as `init --only integrations --json`.    |

Examples: `llamastash integrations pi`, `llamastash integrations opencode,zed`, `llamastash integrations` (pick from the list).

**Which proxy URL gets written.** The address the running daemon's proxy is listening on, read from the daemon (the run starts one if none is up). That covers a proxy that moved past a busy port (`11435` taken, so `11436`) and a daemon started with `--proxy-port` or `--host`, neither of which is saved to `config.yaml`. A wildcard bind (`0.0.0.0`, `::`) is written as loopback. When the daemon can't be reached or its proxy is not listening, the run uses `proxy.host` / `proxy.port` from `config.yaml`, else `127.0.0.1:11435` (`11434` in Ollama-compat mode), and says so on stderr.

**Which models get registered.** The run reads your favorites from the daemon and registers each one, named exactly as `/v1/models` publishes it — a GGUF by its file stem (`Qwen3-Coder-30B-Q4_K_M`), a safetensors repo by its repo id (`Qwen/Qwen3-0.6B`), an Ollama model by `<name>:<tag>`, and a GGUF whose file name is shared by another model under its repo-qualified form ([Model ids on the proxy](#model-ids-on-the-proxy)). So whatever a tool sends back as `body.model` is a name the proxy already answers to. During a full `llamastash init` the model the download step just fetched is registered first, then the favorites. No favorites and nothing downloaded means a provider block with no models: the run says so on stderr, and `llamastash favorites add <model>` then a re-run fills it in. A re-run replaces the model list in pi.dev's and OpenCode's `llamastash` provider, so a model you unfavorite, or a preset you rename, drops out. The rest of each file keeps its keys in the order you wrote them.

**Presets and context size.** A favorite with presets registers one model per preset, as `<id>@<preset>` (`Qwen3-Coder-30B-Q4_K_M@coder`), the default preset first; the plain id is left out. A favorite without presets registers its plain id. Tools that take a context size (pi.dev's `contextWindow`, Zed's `max_tokens`) get the size each one launches with, in this order:

1. the preset's context,
2. the server entry's configured default (a generic entry's `ctx: true` knob `default:`), for the server the preset picks,
3. the model's trained context,
4. 32768.

A context left to `--fit` (`auto`) is not known ahead of time, so the trained context is used. Launches named with `--name` alone have no config record and are not registered; add those by hand.

Per-tool shape: tools whose schema holds a model list (OpenCode, Continue.dev, Zed, pi.dev) register all of them; tools with a single model slot (Aider's `model:`, Codex's `model`, Claude Code's `ANTHROPIC_MODEL`) take the first non-embedding model. OpenCode also gets each model's `limit` (`context` as above, `output` 32000 or half the context, whichever is smaller); without it OpenCode reads the context as `0` and never compacts. pi.dev's `maxTokens` takes the same output figure. pi sends it as `max_completion_tokens` and llama.cpp stops there, and thinking and the answer share it, so a low cap can end a long think before any answer.

**Vision and reasoning flags.** A model with a vision projector is declared as taking images: pi.dev `input: ["text", "image"]`, OpenCode `modalities.input: ["text", "image"]` (without it OpenCode replaces an attached image with an error message), Zed `capabilities.images: true`. Codex already assumes image input. Continue.dev gets nothing, because listing `image_input` in its `capabilities` also turns off its own tool-use detection. A reasoning model gets Zed's `capabilities.interleaved_reasoning: true`, so earlier thinking goes back to the server as `reasoning_content` instead of as plain answer text. Embedders are routed by kind — Continue.dev gets `roles: [embed]`; Zed and pi.dev leave them out, since both drive chat only and pi has no embeddings API at all.

**Reasoning effort.** For a reasoning model whose chat template lists the effort levels it accepts, the patchers wire each tool's effort control to those levels. The list comes from the template, not the model family: Qwen3.8's template accepts `low`, `medium` and `xhigh` (its default), plus `high`, which it turns into `xhigh`, and raises an error on any other value. An alias like `high` is accepted but not offered as a level of its own. `none` turns thinking off where the template reads `enable_thinking`, since llama.cpp maps `reasoning_effort: "none"` to that. A generic entry without `model` has no template, so it declares its levels, default and vision with `reasoning_effort`, `reasoning_effort_default` and `vision` (see [Generic backend](#generic-backend)). Models without such a list (including safetensors repos, whose template is not read yet) get no effort fields.

| Tool | What is written | How to change the level |
| --- | --- | --- |
| pi.dev | `reasoning: true` and a `thinkingLevelMap`; levels the template rejects, and aliases like `high`, map to `null`, which hides them | `/thinking <level>` |
| OpenCode | `reasoning: true` and one `variants` entry per level (plus `none`) | the variant picker |
| Zed | `reasoning_effort` set to the template's default | the effort picker; Zed's list is fixed to minimal ... max, so for Qwen3.8 `minimal` and `max` get an HTTP 500 whose message ends `Unexpected reasoning effort minimal. Supported types are xhigh (default), medium, and low.` |
| Codex | `model_reasoning_effort` set to the template's default, with the accepted levels in a comment | edit it in the profile (kept on re-run when the model accepts it), or `-c model_reasoning_effort=<level>` per run |
| Aider | nothing | `/reasoning-effort <level>` in the chat already works; the `--reasoning-effort` flag would need a model-settings entry that replaces Aider's own defaults for that model |
| Continue.dev | nothing | Continue sends an effort only for OpenAI `o*` / `gpt-5+` models |

**Codex writes a profile, not `config.toml`.** `codex` writes `$CODEX_HOME/llamastash.config.toml` (`~/.codex/` by default), which Codex loads as a layer over `config.toml` when started with `codex --profile llamastash`. Plain `codex` keeps your own settings. The profile points a `llamastash` provider at the proxy with `wire_api = "responses"` (Codex speaks only the Responses API; llama-server serves `/v1/responses` natively and the proxy forwards it), sets `model` and `model_context_window` for the first chat model, and gets the key by running `llamastash api-key`. For a model with effort levels it also sets `model_reasoning_effort`: without it, the value in your `config.toml` (set for OpenAI models) would be sent to the local model, and Qwen3.8 rejects levels like `minimal`. The file is rewritten on each run, keeping only a `model_reasoning_effort` the model accepts.

**pi.dev patches two files.** `~/.pi/agent/models.json` gets the provider block, and `~/.pi/agent/settings.json` gets `llamastash/**` appended to `enabledModels` — pi's model switcher is bounded by that list, so without the pattern the models are configured but out of scope until you widen it by hand. The pattern is only appended when `enabledModels` is already set: pi reads an absent or empty list as "no scoping", and writing ours there would hide every other provider. Any config that is a symlink (a dotfiles repo, typically) is written *through* the link, not over it.

**Where the key ends up**, per tool — it is only a real secret when you have turned proxy auth on; the loopback default ignores the value and every writer uses the `llamastash` stub.

| Tool | Form | Secret at rest? |
| --- | --- | --- |
| pi.dev | `!llamastash api-key` (pi runs it, reads stdout) | No — resolved per pi process |
| Codex | `auth.command = "llamastash"`, `args = ["api-key"]` (Codex runs it) | No |
| OpenCode | `{env:LLAMASTASH_API_KEY}` | No — needs the var exported |
| Zed | nothing written (Zed reads `LLAMASTASH_API_KEY` from env by its own convention) | No |
| Aider, Continue.dev | literal, file mode `0600` | Yes |
| `env-sh`, `claude-code` | literal in the `.sh` they write, mode `0600` | Yes |

The tools in the last two rows have no reference syntax to use, so the value goes in directly and the file is written user-only. If you keep these configs in a dotfiles repo, that is the row to check before committing.

When the run patches a tool that reads the variable **and** the proxy has auth on, the summary says so and gives the line to add to your shell rc — pointing at the `env.sh` it just wrote when you picked that integration, and at `export LLAMASTASH_API_KEY="$(llamastash api-key)"` when you did not. `--json` carries the same under `integrations.env_requirement` (`{var, tools, source_file}`); the field is absent when nothing needs it. Nothing is said on the keyless loopback default, where the value is ignored.

### `llamastash api-key`

```
llamastash api-key [--json]
```

Prints the proxy's bearer key on stdout, alone on one line, for client configs that resolve a credential by shelling out and for `$(...)` in scripts. Reads the local config only — no daemon contact, so it stays inside a client's shell-out timeout. On the keyless loopback default it prints the `llamastash` stub, since the proxy ignores the value but clients that demand a non-empty key still need one. `--json` emits `{"api_key", "auth", "base_url"}`; `base_url` is the address a running daemon's proxy listens on (asked with a 2 s limit, never starting a daemon), else the configured one.

### `llamastash pull <repo>`

HuggingFace pull primitive. Built on the `hf-hub` crate. Accepts `<owner>/<repo>` (downloads every GGUF file in the repo) or `<owner>/<repo>:<filename>.gguf` (single file). Honors `HF_TOKEN` for gated repos.

```
llamastash pull <repo> [--json] [--offline]
```

`--json` emits `{"repo", "revision", "files": [...], "total_bytes"}`. Exit `69` on any failure (network, disk, integrity).

`pull` performs a disk-space precheck by HEADing each file before download, so an out-of-space failure surfaces before any bytes hit disk. It refuses to write the HF token to disk in cache-file modes that would persist it insecurely.

On a terminal, `pull` paints one progress line on **stderr**, in the shape `⬇ <file> (2/4)  42%  1.2G / 4.1G · 85M/s`. The percent, bytes and rate cover the whole pull, not just the current file. The rate counts bytes off the wire, so files served from the HF cache advance the percent without inflating it. The line is trimmed to the terminal width — a long filename loses its middle, keeping the directory and the shard suffix — and it repaints in place and clears itself before the summary. Redirect stderr, or pipe it, and nothing is written: stdout (including `--json`) is identical either way.

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
| `70` | `BINARY_NOT_FOUND`     | The engine the model needs is unavailable: neither `llama-server` nor `llama` on PATH, with no `--llama-server` flag and `LLAMASTASH_LLAMA_SERVER` unset, or the model's backend is disabled / its launcher missing |
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
| `Ctrl+D`                                      | Delete the focused model from disk (idle rows only: `NotLaunched` / `Stopped` — opens a confirmation popup naming every file that goes). See [Deleting a model](#deleting-a-model).                       |
| `Ctrl+X`                                      | Cancel the currently-active HF download (any focus; opens a confirmation popup; queued pulls stay in line — press again on the next promoted pull)                                                       |

### Deleting a model

`Ctrl+D` on an idle Models row removes the model *and everything on disk that belongs only to it*. Unlinking just the launch path would leave shards and companions behind, so the confirmation popup names the full set before you commit:

| Also removed                            | When                                                                                                                                                     |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Shards 2..N of a split GGUF             | Always — a `*-00001-of-000NN.gguf` row owns its whole set.                                                                                                |
| The `mmproj-*.gguf` projector           | Only when no other model in the folder pairs with it. A shared projector stays.                                                                            |
| The separate `mtp-*.gguf` draft head    | Same rule as the projector.                                                                                                                               |
| The HuggingFace cache blob behind a file | When the file is a snapshot symlink into its own repo's `blobs/`. Without this the bytes stay and the delete frees nothing.                               |
| The whole `models--<owner>--<repo>` dir | Only when the row is the **last** model in that repo — then every revision, ref and blob goes. A repo holding a second quant takes the per-file path instead, so the survivor keeps its bytes. |

An HF-shaped tree that is *not* under the configured cache root (an rsynced backup, a restored archive) never gets the recursive removal — it falls back to per-file unlinking.

Refusals: a running, loading or errored launch (stop it first), and Lemonade registry models (delete those through Lemonade — there is no local GGUF).

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

Three-stage modal: **Search → File picker → Confirm**. Search runs live against the public `/api/models` endpoint (300 ms debounce); paste an `owner/repo[:filename]` slug + Enter to bypass search. Each search row carries a `fmt` column and two size columns — `params` (model parameter count, e.g. `35B`) and `size` (approximate download size, the representative GGUF file HF parsed, e.g. `5.3G`); the exact per-quant size lands in the File picker.

`fmt` is the repo's weight format: `GGUF` for llama.cpp, `SFTN` for a safetensors repo (vLLM, SGLang), `-` when the repo publishes both or neither. Both formats are searched — the browser used to be GGUF-only, which left safetensors repos unfindable and so unpullable. The `init` wizard still searches GGUF only, since it is bootstrapping a first model for the default backend.

Drilling into a GGUF repo lists its quants to pick from. A safetensors repo has nothing to pick — one model spread over `*.safetensors` plus `config.json` and the tokenizer files, all of which an engine needs — so the picker offers a single whole-repo row and the pull takes the full set.

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
| `Alt+Enter` (`⌥⏎` on macOS) | Name this launch, then dispatch it. Accepts on `Enter`, cancels on `Esc`; an empty name launches unnamed |
| `Esc`     | Cancel an open inline edit, or return focus to the Models list |

Knob set, grouped into labelled clusters in display order:

| Group                                        | Knobs                                              |
| -------------------------------------------- | -------------------------------------------------- |
| Context                                      | `ctx`, `reasoning`                                 |
| GPU / CPU offload                            | `n_gpu_layers`, `n_cpu_moe`                        |
| Device _(servers offering more than one selector)_ | `device`                                     |
| Multi-GPU placement _(multi-GPU servers only)_ | `tensor_split`, `main_gpu`, `split_mode`         |
| Attention & KV cache                         | `flash_attn`, `cache_type_k`, `cache_type_v`       |
| Throughput                                   | `threads`, `parallel`, `batch_size`, `ubatch_size` |
| Memory loading                               | `load_mode`                                        |
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
the same comma-separated list, and `start --device none` offloads nothing
(CPU only), as llama-server's own `--device none` does.

Two gates decide whether any of this is shown, both scoped to the server
the launch is on (the selected one while editing, the one serving the
model in the read-only view). The **Device** group appears when that
server offers **more than one `--device` selector**. The **Multi-GPU
placement** group (`tensor_split`, `main_gpu`, `split_mode`) — and the
matching `Device` column in the model list and in `list` — appear only
when it sees **more than one physical GPU**.

The two differ on one host shape: a build compiled with two compute APIs
reports the same card once per API (`ROCm0` and `Vulkan0` for one
Radeon). That is a real choice — the compute path changes throughput —
so the `device` row stays, while the placement rows do not, because
there is no second GPU to split a model across. Selectors are matched to
cards by adapter name across compute families, so the same card named
`AMD Radeon 8060S Graphics` by ROCm and
`AMD Radeon 8060S Graphics (RADV STRIX_HALO)` by Vulkan counts once;
two cards under one API always count separately, even with identical
names. `doctor` spells the difference out (`llamacpp-fp4 (1 GPU, 2
selectors)`). Single-GPU and CPU-only hosts see neither group, so the
launcher stays uncluttered when there's no choice to make. In the model list the
`Device` column reads `all` for a running launch that targets every GPU
(no `--device`), so it never blanks out inconsistently next to launches
that pinned a selector. Once a model is running, the read-only Settings
view shows a `server` row naming the build that served it (when the
model has more than one compatible server). The bottom `extras` row holds the free-form argv tail for
flags the typed editor doesn't model; forbidden flags
(`--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`, `--port`) surface a
red inline warning with secret values redacted.

### Precedence chain

When the daemon composes the argv for `start_model`, it walks the
following layers top-down per knob; the first `Some` wins:

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
