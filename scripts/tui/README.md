# LlamaStash TUI drivers

Two ways to drive the full-screen TUI non-interactively. LlamaStash is a
`ratatui`/`crossterm` app, so you can't assert on its output by piping stdout —
both tools give it a real PTY, render the live screen with a terminal emulator
([pyte]), and hand you back plain text.

| Tool | Use it when |
|------|-------------|
| **`tui_drive.py`** | Quick, throwaway inspection. Zero deps beyond `pyte`, JSON-on-argv (easy for an agent to generate inline), prints each screen to stdout. No assertions, no exit code. Reach for this to *look* at a flow. |
| **`harness.py`** | Repeatable UAT / regression checks. Adds `expect`/`refute` assertions, PASS/FAIL accounting, a non-zero exit code for CI, persisted `snap:` screenshots, and mid-run re-`spawn:`. Reach for this to *gate* on a flow. Needs `pexpect` on top of `pyte`. |
| **`drive_init_search.py`** | Drive the `init` **cliclack** wizard (not the ratatui TUI), specifically the model picker's "Search HuggingFace by name…" flow. Reach for this to inspect the init model-search path. Needs `pexpect` + `pyte`. |

Both inherit this process's env, so pair either with an isolated state dir
(`LLAMASTASH_STATE_DIR` + friends, see `../../AGENTS.md`) to drive a clean
daemon. Build first: `cargo build --bin llamastash`.

`harness.py` also answers crossterm's cursor-position query (`ESC[6n`) so the
app doesn't abort with "cursor position could not be read"; `tui_drive.py`
does not, so it can be more fragile depending on TUI init.

## Requirements

Python 3.9+. A throwaway venv keeps it off the system Python:

```bash
python3 -m venv /tmp/ls-tui-venv
/tmp/ls-tui-venv/bin/pip install pyte pexpect   # tui_drive.py only needs pyte
```

## tui_drive.py

```bash
python3 scripts/tui/tui_drive.py '[["", 4, "boot"], ["/gemma|<enter>", 2, "staged"]]'
```

A JSON array of `[keys, wait_seconds, label]` steps; `|` separates tokens in a
step; `<down> <up> <left> <right> <enter> <esc> <tab>` map to escape sequences.
See the script's docstring for the full contract.

## harness.py

```bash
# program file, outdir for snapshots, optional binary + extra args
/tmp/ls-tui-venv/bin/python scripts/tui/harness.py \
    scripts/tui/example.prog /tmp/ls-tui-out
```

- `program` — a step file (see below).
- `outdir` — where `snap:` writes `<label>.txt` screenshots.
- `binary` — defaults to `target/debug/llamastash`.
- `args...` — extra CLI args (default: none; the bare binary opens the TUI).

Exit code is non-zero if any `expect`/`refute` failed.

### Recording an asciinema cast

Add `--cast <path>` anywhere in the args to also record the whole driven
session as an [asciinema] v2 cast. It tees the raw PTY bytes the harness already
reads, so the recording is exactly what the app painted, driven by your scripted
keystrokes (deterministic, not a hand-recorded session). The header embeds the
Catppuccin Macchiato palette, so shell / wizard ANSI colors render on-brand.

```bash
/tmp/ls-tui-venv/bin/python scripts/tui/harness.py \
    scripts/tui/example.prog /tmp/ls-tui-out target/debug/llamastash \
    --cast /tmp/ls-tui-out/demo.cast

asciinema play /tmp/ls-tui-out/demo.cast        # replay in the terminal
agg --font-size 16 /tmp/ls-tui-out/demo.cast out.gif   # render a GIF
```

`--cast` works alongside `expect`/`refute`/`snap` — one run both asserts and
records. Drive a smaller terminal with `--cols/--rows` (the canonical demo uses
`--cols 131 --rows 34`, which `agg --font-size 16` renders at 1281×784 to match
`assets/tui.gif`). Bracket the interesting part with `startcast`/`stopcast` to
skip load and quit:

- `startcast` drops everything captured so far and re-bases the clock to now. It
  nudges the window size to force a full repaint, because ratatui only redraws
  changed cells — without that the clip would open on a blank grid.
- `stopcast` finalizes the recording, so a trailing quit is excluded.

`scripts/tui/demo.prog` is the ready-made tour behind `assets/demo.cast` /
`assets/tui.gif`: it drives a real shell through `llamastash init`, then the TUI
(launch → chat → Requests tab → HuggingFace pull → theme cycle). See its header comments for
the capture command.

`scripts/tui/presets.prog` gates the config-presets TUI surface: the always-shown
Settings preset cycle row (which opens on `last used` and cycles `last used →
auto → [default] → named presets`) and the `Ctrl+P` save dialog (name entry,
save, overwrite confirm). It only needs an isolated daemon with at least one
discovered model — point `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG` at a temp
dir and run the harness against it. (Seed a `presets:` block for the focused
model if you also want to exercise the named-preset stops.)

`scripts/tui/seed_demo_models.py <dir>` provides that discovered model without
downloading real weights: it writes a curated `qwen2` / `llama` / `gemma2` trio
(or custom specs) as header-only GGUFs, sparse-sized so the Size column reads in
GB. Point `LLAMASTASH_MODEL_PATHS` at the seeded dir with `LLAMASTASH_NO_SCAN=1`
so the catalog lists only those, never the user's real library. See the script's
docstring for the spec format.

### Reworking a recorded cast

Two helpers consume a recorded cast (both handle v2 and v3):

- **`trim_cast.py`** — even out timing: clamp idle gaps, rescale speed, hold a
  payoff frame, drop a ragged tail. Used to turn the raw real-time capture into
  the steady-cadence `assets/demo.cast`. See its `--help`.
- **`cast_frames.py`** — pull the README image assets out of one cast:

  ```bash
  # find the cast time of a moment (dashboard, chat reply, a theme, the finale)
  python3 scripts/tui/cast_frames.py timeline assets/demo.cast --step 1.0

  # extract that frame to a PNG, exactly as painted at that time
  python3 scripts/tui/cast_frames.py frame assets/demo.cast 15.11 assets/tui_2.png

  # carve a shorter sub-cast (e.g. just the init wizard) and render it
  python3 scripts/tui/cast_frames.py clip assets/demo.cast /tmp/init.cast --end 5.66
  agg --font-size 16 /tmp/init.cast assets/init.gif
  ```

  `timeline` needs `pyte`; `frame` also shells out to `agg` + ImageMagick
  (`magick`). It renders at `--font-size 16` by default so stills line up with
  `assets/tui.gif`. `frame` is deterministic — it clips `0..T`, renders, and
  coalesces the GIF's last frame, so there is no GIF-time-vs-cast-time seek
  guessing.

[asciinema]: https://asciinema.org/

## drive_init_search.py

```bash
/tmp/ls-tui-venv/bin/python scripts/tui/drive_init_search.py target/debug/llamastash qwen3
```

Args: `[binary] [query]` (defaults `target/debug/llamastash` / `qwen3`). Drives
`init --only models` to the "Search HuggingFace by name…" item, types the query,
prints the live results, walks back to the model list, and ends on **Skip** — so
it never downloads a model (the search step does hit the live HF API). Pair with
an isolated `LLAMASTASH_STATE_DIR` + friends. This is the only driver here for
the `cliclack` init wizard; the others drive the full-screen ratatui app.

### Program steps

One step per line; blank lines and `#` comments are ignored.

| Step | Effect |
|------|--------|
| `spawn:<args>` | (Re)spawn llamastash with extra CLI args |
| `key:<name>` | Send named key(s), space-separated (see below) |
| `type:<text>` | Type literal characters |
| `wait:<seconds>` | Sleep while pumping PTY output into the screen |
| `settle` | Wait the default settle interval |
| `snap:<label>` | Save the current screen to `<outdir>/<label>.txt` |
| `waitfor:<substr>[\|<s>]` | Wait until the screen contains `substr` (default 90 s), FAIL on timeout |
| `expect:<substr>` | Assert the screen contains `substr` (PASS/FAIL) |
| `refute:<substr>` | Assert the screen does not contain `substr` |
| `iexpect:<substr>` | Case-insensitive `expect` |
| `comment:<text>` | Print a comment line |
| `startcast` | (Re)start the `--cast` clip here, dropping earlier frames |
| `stopcast` | Finalize the `--cast` clip here, excluding later frames |

### Key names

`enter esc backspace tab backtab space up down left right home end pageup pagedown`
`ctrl-c ctrl-d ctrl-h ctrl-r`

Plain characters (letters, digits, `?`, `/`, `-`) are sent with `type:`.
Shift+letter is just the uppercase letter, e.g. `type:P` for `Shift+p`.

The screen is rendered at `160x45` to match the canonical `make render` size,
so `snap:` output lines up with the golden fixtures under `tests/golden/`.

[pyte]: https://github.com/selectel/pyte
