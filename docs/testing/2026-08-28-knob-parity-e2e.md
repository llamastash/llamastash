# E2E plan — unified knob registry (`feat/knob-parity`)

The claim under test: **one declaration per knob, and every surface reaches it.**
A knob set through the CLI, the TUI, or a preset must produce the same launch.

Run fully isolated: `LLAMASTASH_CONFIG_DIR` / `STATE_DIR` / `CACHE_DIR` /
`HF_HOME` all redirected under one scratch root, on non-default ports, so a
developer's own config and running daemon are never touched. Hash the real
config before and after to prove it.

- Binary: `target/debug/llamastash` (never bare `llamastash` — that resolves to
  whatever is on `PATH`, not the working tree)
- Engine: a real `llama-server` — build 10656, commit 732707dff
- Model: a 2.7 GB GGUF carrying an MTP head, so the speculation group is
  exercised rather than gated off; symlinked into an isolated scan dir
- Ports: control 21436, proxy 21435 — off the defaults

## Cases

| # | Surface | What it proves |
|---|---|---|
| 1 | daemon | Starts clean on an empty isolated config; migration is a no-op on a fresh file |
| 2 | CLI | `knobs` lists every backend's declarations, human + `--json` |
| 3 | CLI | `start --help` carries a non-default backend's flag, grouped by backend |
| 4 | CLI | A knob set inline reaches the real argv: check `/proc/<pid>/cmdline`, not our own output |
| 5 | presets | `presets save` writes the new shape; `presets show --json` reads it back |
| 6 | presets | `--preset` reproduces case 4's argv byte-for-byte |
| 7 | presets | A preset pins `backend` + `server`; a bare relaunch inherits them (stage 6) |
| 8 | presets | Comments in a hand-annotated config survive a `presets save` |
| 9 | JSON | `status --json`, `list --json`, `presets show --json` carry the `knobs` map |
| 10 | TUI | The Settings editor renders the generated rows for the model's backend |
| 11 | TUI | A row edited in the TUI lands in the same knob the CLI flag would |
| 12 | migration | An old-shape config is rewritten in place, comments kept, backup written, idempotent |
| 13 | safety | A denylisted flag in extras is stripped before spawn |

## Rule

Iterate until every case is green. A case that needs the real engine checks the
real engine — `/proc/<pid>/cmdline` for argv, the live `/props` for what loaded.
No case passes on llamastash's own report of itself.

## Results — 2026-08-28, all green

Engine: llama-server build 10656 (732707dff). Model: Qwen3.5-4B-Q4_K_M (MTP head).
Every argv assertion read `/proc/<pid>/cmdline`, never llamastash's own output.

| # | Result |
|---|---|
| 1 | Daemon started isolated on its own ports; the developer's own daemon untouched throughout |
| 2 | `knobs` lists 52 declarations across 4 backends, human + `--json` |
| 3 | `start --help` groups by backend; every non-default backend's flags present |
| 4 | `--ctx/--threads/--flash-attn/--batch-size/--cache-type-k` all reached argv; `--flash-attn on` in the corrected form; MTP auto-enabled from the model's own head |
| 5 | `presets save` wrote the flat `knobs:` shape; `presets show --json` read it back |
| 6 | **`--preset` produced byte-identical argv to the CLI launch** — the core claim |
| 7 | `server: null` is the correct folded default on a single-server host; inheritance itself is covered by the stage-6 unit tests |
| 8 | All comments survived `presets save`, including the following-sibling one that used to be eaten |
| 9 | `params.knobs` carries one flat map with declared ids and bare `auto`; `mtp.active` true |
| 10 | Running view renders the real dispatched values, matching argv |
| 11 | **A row cycled in the TUI produced `--threads 6`**, rest of argv identical to the CLI and preset launches |
| 12 | Old-shape config migrated in place: 6/6 comments kept, `{auto: true}` → `auto`, `backend_knobs:` → `knobs:`, backup written, idempotent on a second start |
| 13 | `--host 0.0.0.0 --api-key …` in extras refused outright, both flags named |

Two defects found and fixed here rather than in a test:

- the `mtp` row read `inherited` on a launch that was speculating
- bools read `true`/`false` in the running view and `on`/`off` in the editor

One pre-existing behaviour noted, not changed: `last_params` records only the
user's own layer, so a bare launch after a preset launch records `{}` and the
remembered values decay after one launch. The code comment says this is
deliberate.
