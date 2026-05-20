---
title: "feat: consistent colorful CLI output across non-wizard commands"
type: feat
status: active
date: 2026-05-20
---

# feat: consistent colorful CLI output across non-wizard commands

## Overview

The init wizard ships a polished, color-rich, cliclack-driven UX (intro/outro
panels, log-prefix glyphs, spinners, structured key/value blocks). The rest of
the non-interactive CLI surface (`daemon start|stop|status`, `list`, `status`,
`presets`, `favorites`, `last-params`, `start`, `stop`, `doctor`) only adopts
the bare `cli::colors` helpers — success/error/dim — and emits tab-separated
rows or, in the worst case (`daemon status`), a raw `serde_json::to_string_pretty`
dump. This plan brings the rest of the human-readable CLI surface up to the
same visual quality as `init` while keeping every `--json` byte-stable and
every non-TTY pipe still parseable.

The work is bounded: one new render module (`src/cli/format.rs`), an extension
of `src/cli/colors.rs` with semantic helpers, edits to each existing CLI
handler's human-output branch, and matching test/snapshot updates. No new
crates — `console` + `cliclack` (both already top-level) cover everything. No
daemon, IPC, supervisor, JSON contract, or exit-code changes.

## Problem Frame

`init` set the visual bar for llamastash CLI output: a Clack-style stepped
panel with intro + outro, log-prefix glyphs for info/success/warn/error,
hardware/diff blocks rendered as colored YAML, spinners for long steps. Every
*other* command predates that work and still emits plain TSV rows with at
most a bold header line, a colored leading glyph on success messages, and
otherwise undecorated values. Two specific gaps are user-visible:

1. **`llamastash daemon status` dumps raw pretty-JSON** for the daemon's
   `version` response — there is no parsing, no labelling, and no formatting,
   so the output looks like a debug print, not an end-user surface.
2. **`status`, `list`, `presets list`, `favorites list`, `last-params`**
   use raw `\t`-separated rows. The columns don't line up, the values
   (state, port, path, ID) carry no color even when colors are enabled, and
   on a wide terminal the row drift is hard to scan.

`doctor` already groups findings with severity glyphs; it benefits from a
small polish pass but does not need a redesign. `start` / `stop` are
single-line success/failure messages and only need value-level color
(launch_id / port / pid stand out from prose).

The user-facing ask is "make the rest of the CLI look as polished as `init`,
using the libs we already pulled in, without inventing a new visual identity."

## Requirements Trace

- **C1.** Every human-readable CLI output uses the same color policy as
  `init` (the `console::colors_enabled()` global initialised in `cli::dispatch`),
  honoring `--no-colors`, `NO_COLOR`, and non-TTY auto-disable.
- **C2.** Report-style commands (`list`, `status`, `presets list`,
  `favorites list`, `last-params`, `daemon status`) render padded + colored
  tables when stdout is a TTY with colors enabled. When stdout is *not* a
  TTY (or colors are disabled), they continue to emit tab-separated rows
  byte-equivalent to today's TSV — preserves `awk -F\t` / `cut -f` pipelines
  and existing snapshot-style tests.
- **C3.** Action-style commands (`daemon start|stop|status`, `start`,
  `stop`, `presets save|delete`, `favorites add|remove`) keep their current
  one-line success/error/dim shape but gain color highlights on the values
  that matter (launch_id, port, pid, path, model name, state) using a small
  set of semantic helpers.
- **C4.** `daemon status` MUST render the parsed `version` response as a
  labelled key/value block (build, pid, uptime, socket path, server path,
  active connections) — never as raw pretty-JSON.
- **C5.** `doctor` keeps its severity-grouped output but gains visual
  consistency with the rest of the surface: section header line, count
  summary, severity-colored line prefix, value-colored fix hint. Findings
  shape and JSON output are unchanged.
- **C6.** `--json` output for every touched command is byte-for-byte
  unchanged. JSON paths never call the new formatter helpers.
- **C7.** No new top-level crates. `console` and `cliclack` (both already
  in `Cargo.toml`) cover every renderer the plan needs.
- **C8.** Visual identity matches `init`: same glyphs (`✓`/`✗`/`!`/`›`),
  same color semantics (success-green, error-red, warning-yellow,
  dim-secondary, bold-header), same key-value separator style as the
  init diff/intro blocks. No new color palette and no new fonts.

## Scope Boundaries

- **In scope:**
  - A new `src/cli/format.rs` module with shared helpers: padded-table
    builder, key/value block, section header, status badge (state →
    color), value highlights (port, path, id, count).
  - An extension to `src/cli/colors.rs` for semantic value helpers
    (`state(s)`, `port(n)`, `path(p)`, `launch_id(id)`, `count(n)`,
    `kv(k, v)`).
  - Human-output rewrites for the report-style commands listed in C2.
  - Value-color polish for the action-style commands listed in C3.
  - `daemon status` rendering rewrite (C4).
  - `doctor` section-header + count-summary polish (C5).
  - Matching unit tests and updated snapshots.
  - Doc updates: `README.md` screenshots refresh note, `AGENTS.md` CLI
    color policy paragraph addendum (TTY-aware padded tables vs piped
    TSV), `docs/usage.md` mention, `CHANGELOG.md` `[Unreleased]` entry.

- **Out of scope:**
  - TUI (ratatui) styling — it already has its own theme system.
  - Daemon log coloring — `simplelog` handles its own ANSI.
  - Init wizard changes — `init`'s look is the reference; we don't touch
    `src/init/prompts.rs` or `src/init/wizard.rs` rendering.
  - JSON output shape changes for any command (C6 hard line).
  - New color flags, `--color always/never/auto`, per-subcommand color
    overrides, or config-file color customisation — `--no-colors` +
    `NO_COLOR` + TTY detection already cover every real case.
  - New crates (`comfy-table`, `tabled`, `prettytable-rs`, etc.). The
    in-tree padded-table builder is ~60 lines and avoids a dep that
    has to be re-audited for license / advisory drift.
  - Windows-specific ANSI handling — out of scope per v1 plan.
  - Re-running tests on TUI/daemon/IPC paths beyond what the CLI tests
    already cover.

- **Explicit non-features:**
  - No animated transitions on report commands. `init` uses spinners only
    because its steps do real work; `list` and friends complete instantly.
  - No cliclack `intro`/`outro` panel wrapper around report commands —
    these are data reports, not stepped workflows. Forcing a panel would
    feel out of character for `list | head` / `status | grep` usage.

## Context & Research

### Relevant Code and Patterns

- `src/cli/colors.rs` — the single source of truth for the color policy and
  the existing `success` / `error` / `warning` / `dim` / `bold` / `header`
  helpers. Extended in Unit 2 with semantic value helpers; the `init`
  function and OR composition of off-conditions stay untouched.
- `src/cli/mod.rs` — `dispatch` calls `colors::init` once; the new
  `format::table_renderer()` reads `console::colors_enabled()` to decide
  between padded+colored and TSV output, so per-call sites don't branch.
- `src/cli/output.rs` — current `list_human`, `status_human`, `favorites_json`,
  `status_json`, `pretty_json`. `list_human` and `status_human` are the
  two TSV report renderers that need to fork on TTY-status. The JSON
  formatters stay verbatim.
- `src/cli/daemon.rs::handle_status` (line 195) — prints raw
  `serde_json::to_string_pretty(&result)` of the daemon's `version` reply.
  This is the single worst gap in current CLI polish; Unit 3 rewrites it
  into a parsed key/value block. The connect-failure branch already emits
  `colors::dim("daemon: not running")` and stays as-is.
- `src/cli/daemon.rs::handle_start` / `handle_stop` — single-line
  success/dim/error messages. Unit 3 keeps them at one line but adds
  value-color emphasis on pid / socket path.
- `src/cli/list.rs::handle` — calls `list_human`. Unit 4 routes it
  through the new `format::table` helper for the TTY/colored path.
- `src/cli/status.rs::handle` — calls `status_human`. Unit 5 splits
  the daemon preamble into a key/value block and the launches into a
  padded table, with the GPU/host inline summary at the bottom.
- `src/cli/presets.rs` — `List` action prints a tab-separated table with
  a bold header. Unit 6 routes this through `format::table`. `Save` /
  `Show` / `Delete` keep single-line action messages with value color.
- `src/cli/favorites.rs` — `List` prints one path per line, no header,
  no padding. Unit 6 keeps the simple list shape but adds dim path
  segments and an explicit `(n favorites)` footer in the TTY path.
- `src/cli/last_params.rs` — TSV-with-bold-header table. Unit 6
  re-routes through `format::table`.
- `src/cli/start.rs::emit_response` and `src/cli/stop.rs` — single-line
  `colors::success(...)` messages. Unit 3 picks up value color (launch_id,
  port, pid) using the new `colors::launch_id` / `colors::port` helpers.
- `src/init/doctor.rs::render_human` — already severity-grouped with
  colored glyphs. Unit 7 adds a section header line plus per-finding
  bold finding-id and keeps everything else.
- `src/init/prompts.rs` — the reference for visual identity. The new
  `format` module mirrors `render_diff_preview`'s pattern (bold key,
  colored marker, dim value) for its key/value block.
- `Cargo.toml` — `console = "0.15"` already in tree as a top-level dep;
  no changes needed. `cliclack = "0.3"` also already in tree; not used
  by the new module (cliclack is wizard-flavored; report commands stay
  out of its log/panel idiom per Scope Boundaries).

### Institutional Learnings

- The 2026-05-19 `feat-interactive-init-wizard-and-cli-colors-plan.md` plan
  established the visual identity, the off-conditions (flag / `NO_COLOR` /
  non-TTY), and the helper-not-direct-escapes rule. This plan is its
  direct extension to the non-wizard commands and inherits every
  conclusion (single init site, three OR-ed off-conditions, no
  `--color always/never/auto` ternary, no per-subcommand override).
- `cli::colors::init` is process-global and called *before* any output
  site. The new `format::table` reads `console::colors_enabled()` —
  the same predicate, so a single off-condition silences both color and
  padding without per-site branching.
- AGENTS.md "CLI color policy" already documents the TTY/`NO_COLOR`/`--no-colors`
  rule. This plan adds one paragraph clarifying that padded tables are
  also TTY-gated and that `--json` remains the agent contract.
- `docs/solutions/` is empty for this repo (greenfield), and the v1 +
  v2 plans don't cover table rendering. No prior conflict.

### External References

- `console` crate docs (already top-level dep, 0.15) — `term_size()`,
  `Term::stdout().size()` for terminal width detection; `measure_text_width`
  for cell-width-correct padding under unicode-width 0.2 (already pulled
  in for `daemon-info` truncation); `set_colors_enabled` already used by
  `colors::init`. The `style(s).cyan()`, `.green()`, `.yellow()`, etc.
  chain works under the global enabled/disabled flag with no per-site
  branching.
- No external references needed for the report-table design. The
  TTY-vs-pipe shape decision is a project convention, not an industry
  standard, and the existing `cli::colors` infrastructure already
  resolved the orthogonal questions (env var, flag, fd check).

## Key Technical Decisions

- **One new module, `src/cli/format.rs`, owns padded-table + key/value
  block rendering**. Centralising the layout in one place lets a future
  add (e.g. wrap-instead-of-truncate, column alignment hints, a
  "key=value" key/value form) land in one file rather than across nine
  CLI handlers. The module is `pub(crate)` since nothing outside the
  binary needs to consume it. Rationale: matches the `cli::colors` and
  `cli::output` pattern — one module per concern.
- **Padded vs TSV is gated on `console::colors_enabled()`, not a
  separate predicate**. The three off-conditions for color
  (`--no-colors` / `NO_COLOR` / non-TTY) are exactly the three
  off-conditions we want for padded output: piping into `awk` or
  `column -t` should see clean TSV, and a user who explicitly disabled
  color almost certainly wants plain unpadded output too. Rationale:
  a single source-of-truth predicate keeps users from hitting
  "I asked for no color but I got padded columns" surprises.
- **Padding is computed per render call from row content, not a fixed
  width matrix**. The table helper walks rows once to find max-per-col
  cell width (in display cells via `unicode-width`, not bytes), then
  emits header + separator + rows with `format!("{:<width$}")`. Cost is
  trivial (a few hundred rows max for `list`/`status`). Rationale:
  fixed widths would either truncate paths on a 200-col terminal or
  blow up `list` rows on a 60-col one; per-call sizing is right.
- **Long-cell truncation uses `unicode-width::display_width` measurement
  and the same `…` ellipsis already used in `src/cli/output.rs`**.
  When stdout's terminal width is known (via `console::term_size()`)
  and the natural row would exceed it, only the last column (typically
  `PATH`) truncates with a trailing `…`. Other columns never truncate.
  Rationale: paths are the only column that benefits from truncation;
  arch / quant / ctx / port are fixed-shape. Falls back to
  no-truncation when terminal width is unknown (e.g. detached daemon
  log redirected to a file — but C2 already routes that path to TSV,
  so this branch is rarely reached).
- **Header separator is one `─` rune per column-width**, not a
  per-column `+--+--+` form. The chosen preview style is
  "padded + colored, no borders", so the rule is a single horizontal
  line under the bold-colored header row.
- **Semantic value helpers live in `cli::colors`, not `cli::format`**.
  `colors::state("ready")` returns green, `colors::state("loading")`
  returns yellow, `colors::state("error")` returns red, etc.
  `colors::port(n)`, `colors::launch_id(id)`, `colors::path(p)`,
  `colors::count(n)` are all thin wrappers around `console::style(...)`
  with a fixed semantic color. Rationale: the formatter never decides
  what color a value gets; the helpers do. Keeps `cli::format` shape-only
  (lengths, padding, separators) and `cli::colors` semantics-only
  (which color for which kind of value). When a future helper needs to
  render coloured paths, only `cli::colors` changes.
- **`daemon status` renders a key/value block, not a table**. The
  daemon's `version` response has a fixed set of fields (build, pid,
  uptime_secs, socket_path, server_path, active_connections), so a
  table shape would be one-row-N-cols and waste space. A vertical
  key/value block (right-aligned key, value column) matches the
  init `intro` panel's shape and reads as a status report. Rationale:
  the same call site has historically dumped pretty-JSON, so any
  structured form is an improvement; the kv block is the form that
  best matches `init`'s identity.
- **`status` (model launches) uses a hybrid layout**: daemon kv block
  on top, padded launches table in the middle, GPU/host summary
  line at the bottom. The three sections are the same as today's
  `status_human` text shape; only the rendering of each upgrades.
  External rows render under the same table with `LAUNCH_ID` set to
  `ext-<pid>` and a dim color (matches today's TUI identifier shape).
- **`favorites list` keeps the simple "one path per line" form** even
  in the TTY path, because it has no other columns. Padding a single
  column would just left-align the same paths the unpadded form
  produces. Color picks up via `colors::path(...)` and a dim
  `(N favorites)` footer.
- **`doctor` keeps its current per-finding shape** but adds (1) a
  consistent section header line via `format::section_header(...)` and
  (2) bold finding-id glyphs in front of each finding. The
  zero-findings success line moves to `format::section_header` too so
  the visual feels consistent with the rest of the surface. Rationale:
  `doctor`'s shape is already 90% there; a minor polish wins more than
  a redesign and risks less.
- **No `comfy-table` / `tabled` / `prettytable-rs` dep**. The padded
  table we need is ~60 lines and has only three knobs (header, rows,
  optional terminal-width truncation). Adding a 3000-line table crate
  to a binary that already ships a TUI is a net negative on binary
  size, audit surface, and dep graph. Rationale: aligns with the
  prior plan's "console crate already covers it" decision.
- **The padded shape MUST round-trip across `--no-colors` to the same
  byte content** the TSV path produces, except for ANSI escapes. A
  user who runs `LLAMASTASH_NO_COLOR=1 llamastash list` and pipes the
  output should see exactly today's TSV bytes (no padding, no rule
  line). Rationale: existing snapshot tests pin TSV exactly; padded
  output is a TTY-only affordance.

## Open Questions

### Resolved During Planning

- **Q: Should the new module be `cli::format` or `cli::render`?** —
  Resolved: `cli::format`. `render` is already used in `tui::render`
  for ratatui draw logic; reusing the name in `cli::` would invite
  cross-module confusion. `format` matches `cli::output`'s shape (both
  shape outputs — `output` does JSON / TSV, `format` does padded /
  kv).
- **Q: Should padded output include row colors (zebra striping)?** —
  Resolved: no. The init wizard does not stripe; users have asked for
  consistency with `init`, which means no striping. Status-state and
  value-level colors carry the visual interest.
- **Q: Should `daemon status` show a daemon-status header glyph
  (`◆`/`›`)?** — Resolved: no. The init `intro` panel does not use a
  per-line header glyph beyond cliclack's vertical pipe, and the
  report commands aren't using cliclack panels at all. The
  `format::section_header` helper emits a bold colored title line
  with an underline rule, which is the visual identity we want.
- **Q: Should we wrap report commands inside a cliclack `intro`/`outro`
  panel?** — Resolved: no, per Scope Boundaries. The user-facing
  preview confirmed the "padded + colored, no borders" shape. cliclack
  panels are wizard-flavored; data reports use the plainer form.

### Deferred to Implementation

- **Per-launch RSS/CPU columns in `status`** — today's `RunningRow`
  carries `latest_rss_bytes` and `latest_cpu_pct`, but the current
  `status_human` doesn't render them. Adding them to the padded table
  would make the rows wide enough to push `PATH` truncation more
  aggressively. The decision (include vs omit, and if include, do we
  promote them ahead of `PATH`) is best made while wiring up Unit 5
  and seeing the natural row width. Default direction: include `RSS`
  and `CPU%` as optional trailing columns surfaced only when at least
  one row has primed data; omit when every row's RSS/CPU is `None`.
  Documented here so the implementer doesn't invent the rule at
  review time.
- **Terminal width source for truncation** — the `console::term_size()`
  fallback path needs decisions about (a) what to do when stdout is
  a TTY but the terminal width can't be read, and (b) whether the
  `COLUMNS` env var should override. Default direction: trust
  `console::term_size()`, fall back to "no truncation" when None;
  respect `COLUMNS` if set. Confirm at implementation when wiring
  the helper.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance
> for review, not implementation specification. The implementing agent
> should treat it as context, not code to reproduce.*

```
                       ┌────────────────────────────────────────┐
                       │ cli::dispatch                          │
                       │   colors::init(no_colors)              │
                       │     (sets console::colors_enabled)     │
                       └────────────────────┬───────────────────┘
                                            │
                ┌───────────────────────────┴───────────────────────────┐
                │                                                       │
        Action-style commands                                  Report-style commands
        (daemon start/stop/status,                              (list, status, presets list,
         start, stop, presets save/delete,                       favorites list, last-params,
         favorites add/remove, init, doctor)                     daemon status)
                │                                                       │
                ▼                                                       ▼
    cli::colors::success/error/warning           cli::format::table(header, rows)
    cli::colors::dim/bold/header                   ├── if !colors_enabled()
    cli::colors::state/port/launch_id/path         │   └── emit '\t'-joined rows (today's TSV)
    cli::colors::count/kv                          └── else
                │                                      └── pad cols by max display-width
                │                                          + colored header + rule line
                │                                          + colored value cells
                │
                ▼
       cli::format::section_header(title)    ──>   bold + underline + count "(n items)"
       cli::format::kv_block(items)          ──>   right-aligned keys, dim "=" / ":", colored vals
                │
                ▼
        Stdout / stderr (with policy applied via console::style)
```

Per-command sketch:

```
daemon status  (TTY/color)               daemon status  (--json | piped)
───────────────────────                  ─────────────────────────────────
daemon                                   {"build":"0.0.1","pid":4242,...}
  build               0.0.1              ← unchanged byte-stable JSON
  pid                 4242                  via current pretty_json path
  uptime              90s
  socket              /run/user/1000/...
  server              /usr/bin/llama-server
  connections         3

list  (TTY/color)
─────────────────
NAME            ARCH    QUANT   CTX     PATH
────────────────────────────────────────────────────────────
qwen2.5-7b      qwen2   Q4_K    8192    ~/.cache/huggingface/...
phi-3.5-mini    phi3    Q5_K    4096    ~/.cache/huggingface/...

(2 models · 1 source)

status  (TTY/color)
───────────────────
daemon · pid 4242 · uptime 90s · connections 3 · build 0.0.1

LAUNCH_ID  STATE   MODE    PORT   PID    RSS      CPU%    PATH
─────────────────────────────────────────────────────────────────────
L1         ready   chat    41100  12345  4.5 GiB  312%    ~/m/qwen.gguf
ext-9999   ext     -       -      9999   -        -       /m/external.gguf

GPU: NVIDIA RTX 4090 · 24 GiB
```

## Implementation Units

- [ ] **Unit 1: Add `src/cli/format.rs` with padded-table + kv-block helpers**

**Goal:** Centralise the layout logic so each command's render branch is a
one-line call. The module owns: (1) `table(header, rows)`, (2)
`kv_block(items)`, (3) `section_header(title, count: Option<usize>)`, (4)
the TTY/colors-enabled detection that decides padded-vs-TSV.

**Requirements:** C1, C2, C7, C8

**Dependencies:** None — Unit 1 is the foundation every later unit consumes.

**Files:**
- Create: `src/cli/format.rs`
- Modify: `src/cli/mod.rs` (register `pub(crate) mod format;`)
- Test: `src/cli/format.rs` (`#[cfg(test)] mod tests`)

**Approach:**
- `table(header: &[&str], rows: &[Vec<String>])` returns a `String`
  ready to print. Branches on `console::colors_enabled()`:
  enabled → padded+colored layout, disabled → today's
  `header.join("\t")` + per-row `row.join("\t")`.
- Padding uses `console::measure_text_width` (display cells, not
  byte length) on each cell, computes per-column max, then formats
  rows with `{:<width$}`.
- Header row goes through `colors::bold` (preserving the bold+underline
  pattern from `colors::header` but without the underline — the
  rule line below the header acts as the visual underline already).
- Separator rule is `─` (U+2500) repeated for the total padded width
  (sum of column widths + per-col separator spacing). Rendered
  through `colors::dim` so it doesn't compete with the header.
- `kv_block(items: &[(&str, String)])` builds a two-column form: keys
  right-aligned via `{:>maxk$}`, then two-space gap, then value. Keys
  go through `colors::bold`; values go through caller-supplied color
  (helpers in Unit 2). A leading two-space indent on each line matches
  the cliclack `intro` panel style.
- `section_header(title, count)` renders a single bold + underlined
  line like `list (3 models)`, dimming the count suffix.
- Terminal width via `console::term_size()` is *only* consulted by
  the table helper for last-column truncation; the helper exposes a
  small `with_truncate_last(true|false)` knob (default true). Caller
  ergonomics: report commands pass `true`; future commands that
  need verbatim values pass `false`.

**Patterns to follow:**
- `src/cli/colors.rs` — module shape, `#[cfg(test)] mod tests` style,
  `pub(crate)` visibility.
- `src/init/prompts.rs::render_diff_preview` — pattern for combining
  bold keys + colored markers + dim trailing text.

**Test scenarios:**
- Happy path: `table(["NAME","CTX"], &[vec!["a".into(),"4096".into()],
  vec!["bb".into(),"8192".into()]])` with colors enabled produces a
  padded layout where the `NAME` column is at least 2 cells wide and
  every row aligns. With colors disabled, output is exactly
  `"NAME\tCTX\na\t4096\nbb\t8192\n"`.
- Edge case: empty rows. `table(header, &[])` with colors enabled
  emits the header row + rule line only; with colors disabled emits
  the header `\t`-joined plus newline. Caller is responsible for the
  "(no items)" empty-state message.
- Edge case: unicode-wide cell (`日本語`) is measured by display
  width, not byte length — column widths line up visually.
- Edge case: cell containing `\n` is rejected (would corrupt the
  row); the helper either panics in debug or replaces with a space
  in release. Document and test the chosen behavior.
- Edge case: very long PATH cell with terminal width 80 — truncates
  to `…` per the rule.
- `kv_block` happy path: two items render as
  `"  build:  0.0.1\n  pid:    4242\n"` (keys right-aligned, two-space
  indent) with colors disabled.
- `section_header(title, Some(3))` includes the dim count `(3 items)`
  with colors enabled; with colors disabled it emits the plain
  `"title (3 items)\n"`.

**Verification:**
- `cargo test -p llamastash --lib cli::format` passes both the
  colors-enabled and colors-disabled variants of every scenario.
- `cargo clippy --all-targets --features test-fixtures -- -D warnings`
  is clean.
- `cargo fmt --all -- --check` is clean (two-space indent).

---

- [ ] **Unit 2: Extend `src/cli/colors.rs` with semantic value helpers**

**Goal:** Provide one place for "this kind of value gets this color." Every
value-color decision in the rest of the plan resolves to one of these
helpers, so the formatter never picks colors and the call sites never
embed `console::style(...)` directly.

**Requirements:** C1, C3, C8

**Dependencies:** None. Unit 2 can land alongside or before Unit 1.

**Files:**
- Modify: `src/cli/colors.rs` (add `state`, `port`, `launch_id`,
  `path`, `count`, `kv` helpers + tests)

**Approach:**
- `state(s: &str) -> String` — maps `"ready"`/`"loading"`/`"launching"`/
  `"stopping"`/`"stopped"`/`"error"` to bold green / yellow / yellow /
  yellow / dim / red respectively, with an explicit fallback that
  returns the plain string. Used by `status` (model launches) and
  by `stop` action messages.
- `port(n: u16) -> String` — cyan (the same cyan as the init diff's
  key path). Used wherever a port number is rendered as part of a
  value cell or success line.
- `launch_id(id: &str) -> String` — bold magenta (high-contrast against
  green state cells and dim paths).
- `path(p: &str) -> String` — plain text with a dim home-prefix
  collapse (`/home/$USER/foo` → `~/foo`) when stdout is a TTY. The
  collapse mirrors the rendering pattern used by the TUI Models
  pane; isolating it in this helper keeps the substitution
  consistent across surfaces.
- `count(n: usize, noun: &str) -> String` — dim "`(N <noun>)`"
  rendering used by `section_header` and various empty-state lines.
- `kv(k: &str, v: &str) -> String` — convenience for inline single-pair
  rendering when a full `kv_block` would be overkill. Bold key,
  two-space gap, plain value.

**Patterns to follow:**
- The existing `success` / `error` / `warning` / `dim` / `bold` /
  `header` helpers — same signature shape (`fn(&str) -> String`),
  same use of `console::style(...)`, same `pub(crate)` visibility.

**Test scenarios:**
- Happy path: every helper renders its glyph + text in both colored
  and uncolored modes (mirrors the existing
  `success_helper_carries_glyph_and_text_in_both_modes` test).
- Happy path: `state("ready")` is green; `state("error")` is red;
  `state("unknown")` is plain (the fallback). With colors disabled,
  all three return the input string verbatim.
- Edge case: `path("/home/<user>/foo")` collapses to `"~/foo"` when
  `$HOME=/home/<user>` and colors are enabled. With colors disabled
  the path is returned verbatim — preserves test snapshots.
- Edge case: `path("/")` and `path("")` are returned verbatim (no
  collapse).
- Edge case: `port(0)` renders as cyan `"0"`; never errors.
- `kv("build", "0.0.1")` returns `"build  0.0.1"` (two-space gap) with
  colors disabled.

**Verification:**
- `cargo test -p llamastash --lib cli::colors` passes every new
  test alongside the existing ones.

---

- [ ] **Unit 3: Reformat action-style daemon and start/stop output**

**Goal:** Apply value-color polish to single-line success/error/dim
messages in the action-style commands, and rewrite `daemon status`
to render the parsed `version` response as a `kv_block` instead of
raw pretty-JSON.

**Requirements:** C1, C3, C4, C6, C8

**Dependencies:** Units 1, 2.

**Files:**
- Modify: `src/cli/daemon.rs` (`handle_start`, `handle_stop`,
  `handle_status`)
- Modify: `src/cli/start.rs::emit_response`
- Modify: `src/cli/stop.rs` (both the `--all` branch and the
  single-target branch + the external-pid branch)
- Test: `src/cli/daemon.rs` (`#[cfg(test)] mod tests`),
  `src/cli/start.rs` (`#[cfg(test)] mod tests`),
  `src/cli/stop.rs` (`#[cfg(test)] mod tests`)

**Approach:**
- `daemon start` (foreground/detached): same one-line success/dim
  messages, but the pid in `"already running (pid 4242)"` flows
  through `colors::bold` (or a dedicated `colors::pid` if we end up
  adding one — decision deferred to implementation). The "started
  (detached)" line is unchanged byte-shape — the success glyph is
  already provided by `colors::success`.
- `daemon stop`: `"daemon: shutdown requested"` and `"daemon: not
  running"` unchanged in shape; no value-color opportunities exist
  on these two messages (no pid, no socket — `colors::success` /
  `colors::dim` already carry the right semantics).
- `daemon status` (the gap in C4): if `Client::connect` succeeds and
  the `version` call returns a JSON object, parse the documented
  fields (`build`, `pid`, `uptime_seconds`, `socket_path`,
  `server_path`, `active_connections`) into a `kv_block`. Render
  through `format::section_header("daemon")` + `format::kv_block(...)`.
  Unknown / missing fields render as dim `-`. JSON-shaped fallback
  for unrecognised responses: print a dim warning and fall through
  to today's pretty-JSON path so we never lose info.
- `start`: `emit_response` (the success branch) gains value color
  via `colors::launch_id(lid)` / `colors::port(port)` / bold pid in
  the success line. The shape stays one line; only the values get
  emphasis. JSON branch (caller passes `args.json`) unchanged.
- `stop`: each of the three sub-flows (single target managed,
  external pid, `--all`) emits a one-line `colors::success` today.
  Add value color: launch_id via `colors::launch_id`, pid via bold,
  count via `colors::count`. JSON branches unchanged.

**Patterns to follow:**
- `src/cli/colors.rs` (the helpers added in Unit 2).
- `src/init/wizard.rs::print_handoff` — the `kv_block`-style output
  in the init outro is the closest existing reference.

**Test scenarios:**
- `daemon status` (happy path, daemon up): the rendered string
  contains `build`, `pid`, `uptime`, `socket`, `server`,
  `connections` labels, and the colored-stripped output ends with
  the parsed values (no `{` / `}` chars). With colors disabled, the
  output is the plain key-value lines with two-space indent.
- `daemon status` (daemon down): output unchanged (today's
  `colors::dim("daemon: not running")` already renders correctly).
- `daemon status` (unexpected schema): output falls back to today's
  `pretty_json` of the body and emits a dim warning line above it.
  Test by injecting an empty `{}` response and asserting the
  fallback message appears.
- `daemon start --detach` (already running): "already running (pid
  4242)" includes the pid wrapped by `colors::bold` when colors are
  enabled.
- `start` (json=false): success line includes `launch_id=L1
  port=41100 pid=12345` with each value wrapped through the matching
  helper. Test by stripping ANSI and verifying token positions are
  unchanged from today's output.
- `start` (json=true): output unchanged. Verify with a
  string-equality check against today's pretty-printed JSON snapshot.
- `stop ext-1234`: success line wraps pid via bold and the SIGTERM /
  SIGKILL label via `colors::dim`.
- `stop --all` (count=3): success line wraps `3` via `colors::count`.

**Verification:**
- `cargo test -p llamastash --lib cli::daemon` /
  `cli::start` / `cli::stop` passes with both color modes covered.
- `cargo test -p llamastash --test cli_smoke` (or whichever existing
  CLI smoke test exists — confirm at implementation) still
  asserts the same TSV bytes when run non-interactively.

---

- [ ] **Unit 4: Rewrite `list` human output through `format::table`**

**Goal:** Padded + colored table on a TTY; today's TSV when piped.

**Requirements:** C1, C2, C6

**Dependencies:** Units 1, 2.

**Files:**
- Modify: `src/cli/output.rs::list_human` (route through
  `format::table`)
- Modify: `src/cli/list.rs` (no logic change — the dispatcher remains
  identical; the human-format branch already routes through
  `list_human`)
- Test: `src/cli/output.rs` (`#[cfg(test)] mod tests` — add
  colors-enabled assertions alongside the existing TSV ones)

**Approach:**
- `list_human` builds the existing rows (name, arch, quant, ctx, path)
  but hands them to `format::table` instead of joining with `\t`.
  Names, arches, and quants render plain; CTX gets a dim color (it's
  metadata, not user-actionable); PATH goes through `colors::path` so
  the home-prefix collapse applies.
- Empty catalog: stay with today's `(no models discovered)` dim line.
  Add a `(N models)` footer under the table when N > 0 via
  `format::section_header` or a trailing dim line — pick the
  cleaner shape at implementation review.
- The JSON branch in `list.rs` is unchanged.

**Patterns to follow:**
- `src/cli/output.rs::list_human` — preserve its current empty-state
  handling and its current bold-header rule, just route through the
  new helper.

**Test scenarios:**
- `list_human` happy path (colors enabled): output contains the
  header row, the rule line, two data rows, and the `(2 models)`
  footer. Stripping ANSI yields a layout whose columns align (each
  row column-start at the same column index).
- `list_human` non-TTY / colors-disabled: output is exactly today's
  TSV — `NAME\tARCH\tQUANT\tCTX\tPATH\n...` — byte-for-byte. Verify
  by string equality against the current snapshot to avoid silent
  drift.
- `list_human` filter applied: unchanged. The filter happens before
  this function; coverage stays at the existing call sites.
- `list_json` byte-stable (regression guard for C6).

**Verification:**
- `cargo test -p llamastash --lib cli::output` covers both branches.
- Manual: `cargo run -- list` shows the padded table;
  `cargo run -- list | cat` shows TSV.

---

- [ ] **Unit 5: Rewrite `status` human output (daemon kv + launches table)**

**Goal:** Daemon health rendered as `kv_block`, launches as a padded
table with `LAUNCH_ID`/`STATE`/`MODE`/`PORT`/`PID`/`PATH` (and
optional `RSS`/`CPU%`), GPU summary as a one-line dim footer.

**Requirements:** C1, C2, C6

**Dependencies:** Units 1, 2.

**Files:**
- Modify: `src/cli/output.rs::status_human` (split into kv-block +
  table + footer)
- Test: `src/cli/output.rs` (add tests for the three sections)

**Approach:**
- The daemon preamble (`pid`, `uptime`, `active_connections`,
  `build`, `socket_path`, `server_path`) becomes a single
  `format::kv_block` rendered under a `format::section_header("daemon")`.
- The managed/external launches become a single padded table. State
  cells go through `colors::state(...)`; port through `colors::port`;
  launch_id through `colors::launch_id`; path through `colors::path`.
  External rows synthesize `launch_id = "ext-<pid>"` and dim the whole
  row.
- RSS / CPU% columns are included when at least one row carries primed
  data (deferred Implementation Note in §"Open Questions"). When every
  row is `None`, those two columns are omitted to keep the table
  narrower.
- The GPU summary uses today's `gpu_label` helper, wrapped by
  `colors::dim` for the entire footer line.
- Non-TTY path: identical TSV bytes as today (regression guard via
  the existing tests in `src/cli/output.rs::tests`).

**Patterns to follow:**
- `src/cli/output.rs::status_human` shape — keep the same three
  sections, only swap the rendering.
- `src/cli/output.rs::gpu_label` — reuse verbatim; only the colour
  wrapper changes.

**Test scenarios:**
- Happy path (TTY/color): one managed + one external row, GPU
  CpuOnly, daemon `Some(...)`. Output contains a "daemon" section
  header, six kv rows, a launches table with two rows (the second
  dim), and a GPU footer.
- Empty launches: output shows the daemon block + the existing
  `"(no managed launches)"` dim line, no table. GPU footer stays
  when present.
- RSS/CPU omitted when all rows are `None` — verify the table header
  does not include those columns.
- RSS/CPU shown when one row has primed data — verify both columns
  appear, with the `None` rows rendering as dim `"-"`.
- `--json` (regression): identical byte output to today.
  Test by asserting equality against the current `status_json`
  snapshot.
- Non-TTY: identical TSV byte output (regression guard).

**Verification:**
- `cargo test -p llamastash --lib cli::output` covers each scenario.
- Manual: `cargo run -- status` and `cargo run -- status | cat`
  produce the right shapes.

---

- [ ] **Unit 6: Reformat `presets list`, `favorites list`, `last-params`**

**Goal:** Padded + colored tables for the three remaining report
commands; preserve TSV in non-TTY; preserve `--json`.

**Requirements:** C1, C2, C6

**Dependencies:** Units 1, 2.

**Files:**
- Modify: `src/cli/presets.rs` (the `List` action's non-JSON branch)
- Modify: `src/cli/favorites.rs` (the `List` action's non-JSON branch)
- Modify: `src/cli/last_params.rs` (the non-JSON branch)
- Test: each of the three files (add colors-enabled + colors-disabled
  assertions; ensure JSON byte equality).

**Approach:**
- `presets list`: rows are `NAME`/`CTX`/`REASONING`/`EXTRA`. Route
  through `format::table`. Reasoning cell goes through a small
  helper that renders `on` green and `off` dim (folded into
  `colors::state` if the variant set fits, or a local helper if it
  doesn't — pick at implementation review).
- `favorites list`: keep the simple one-path-per-line shape; just
  apply `colors::path` to each line and add a dim `(N favorites)`
  footer.
- `last-params`: rows are `MODEL`/`CTX`/`REASONING`/`ADVANCED`. Route
  through `format::table`. Path column gets the same dim + home
  collapse treatment.
- Empty-state messages stay as-is.

**Patterns to follow:**
- Unit 4's `list_human` rewrite — same shape, same TSV-fallback
  contract.

**Test scenarios:**
- `presets list` empty: `(no presets for <name>)` dim line, no
  table. With colors disabled, identical bytes to today.
- `presets list` with 3 entries: padded table on TTY, TSV on pipe.
  Verify `reasoning` cell color flips between `on` / `off`.
- `favorites list` empty: today's `(no favorites)` dim line.
- `favorites list` with 2 entries: each path through `colors::path`,
  dim `(2 favorites)` footer. Non-TTY emits today's "one path per
  line" without color, no footer.
- `last-params` empty: today's dim message; identical bytes.
- `last-params` with 1+ rows: padded table on TTY, TSV on pipe.
- JSON branches: byte-stable for all three (regression guard for C6).

**Verification:**
- `cargo test -p llamastash --lib cli::presets cli::favorites
  cli::last_params` covers each scenario.
- Manual: `cargo run -- favorites list`, `... presets <m> list`,
  `... last-params` produce the expected shapes.

---

- [ ] **Unit 7: Polish `doctor` rendering**

**Goal:** Bring `doctor`'s output in line with the rest of the surface
— section header line, count summary, bold finding-id, dim fix-hint
arrow — without redesigning the severity-grouped output.

**Requirements:** C1, C5, C6

**Dependencies:** Units 1, 2.

**Files:**
- Modify: `src/init/doctor.rs::render_human`
- Test: `src/init/doctor.rs` (the existing `render_human_handles_empty_report`
  test, plus a new test for the rendered shape)

**Approach:**
- The zero-findings success line routes through `format::section_header`
  so the visual identity matches `list`/`status`.
- The non-empty path emits a section header with the count, then one
  block per finding. Block format: severity glyph + `[finding_id]`
  (bold) + dim message; second line: dim `→ fix with: <hint>` with
  the hint itself styled bold (so an agent reading the output can
  see the actionable token at a glance).
- The `info` severity branch keeps its leading `•` glyph; `warning`
  keeps `!`; `error` keeps `✗`. Color semantics unchanged.
- JSON branch unchanged (C6).

**Patterns to follow:**
- `src/init/doctor.rs::render_human` current shape — preserve every
  branch; only swap each `colors::*` call site for the polished
  variant.

**Test scenarios:**
- Zero findings: output starts with `llamastash doctor:` and
  includes the section-header rule line. Last-init date renders
  dim when present.
- One error, one warning, one info: output starts with the section
  header (count=3), then three blocks in the order returned by
  `build_report`. Each block's first line contains the matching
  glyph; each block's second line contains `→ fix with:`.
- Non-TTY / colors-disabled: same finding-count summary as today;
  no ANSI escapes; line shape preserved so existing terminal
  recordings stay readable.

**Verification:**
- `cargo test -p llamastash --lib init::doctor` covers the existing
  + new tests.
- Manual: `cargo run -- doctor` against a daemon with seeded
  findings shows the new shape.

---

- [ ] **Unit 8: Doc + changelog updates**

**Goal:** Keep docs in lock-step with the new behavior so AGENTS.md,
README, usage.md, and the CHANGELOG reflect what shipped.

**Requirements:** C1, C2, C6, plus AGENTS.md's "Docs stay in sync
with code" policy.

**Dependencies:** Units 3–7 (so the snapshots in screenshots can match
the actual output).

**Files:**
- Modify: `README.md` (refresh CLI output snippets if any inline; add
  a one-paragraph note that TTY output now shows padded tables)
- Modify: `AGENTS.md` (add a paragraph under "CLI agent surface"
  clarifying that the TTY-side rendering is decorative and agents
  pin against `--json`; mention that the non-TTY surface emits the
  same TSV as before so existing pipelines keep working)
- Modify: `docs/usage.md` (similar one-paragraph note in the section
  that documents per-subcommand output)
- Modify: `CHANGELOG.md` (`[Unreleased]` entry summarising the
  rendering polish, calling out the two contracts that are
  preserved: TSV when piped, JSON byte-stable)
- Modify: `TODO.md` (strike any related TODO entries when this lands)
- Modify: each touched plan checkbox in this file when its Unit
  lands

**Approach:**
- One-paragraph additions in each doc, not new sections. The CLI
  color policy already lives in `AGENTS.md`; the addendum is
  literally "padded tables are also TTY-gated, JSON unchanged."
- CHANGELOG entry follows the format already used in the file (one
  line per item under `[Unreleased]`).
- README screenshot refresh is best-effort: if there are no actual
  PNG/ASCII screenshots inline today, the note is enough; if there
  are, regenerate them locally and commit alongside.

**Patterns to follow:**
- `AGENTS.md` "CLI color policy" paragraph — the new sentence sits
  in the same scope.
- `CHANGELOG.md` `[Unreleased]` format — match existing entries.

**Test scenarios:**

- Test expectation: none -- documentation-only change. Verification
  is human review.

**Verification:**
- `git grep "CLI color policy"` finds the AGENTS.md addendum.
- CHANGELOG `[Unreleased]` has the matching entry.
- The plan checkboxes in this file are ticked for each completed
  Unit.

## System-Wide Impact

- **Interaction graph:** Touches every CLI handler that emits
  human-readable output. No daemon, IPC, supervisor, or TUI code is
  touched. The init wizard's `prompts.rs` is also untouched — it is
  the visual reference, not a target.
- **Error propagation:** Unchanged. Every handler keeps its existing
  `CliResult`/`anyhow::Result` shape; rendering changes are purely
  in the success-branch print paths.
- **State lifecycle risks:** None — no persistent state is touched.
- **API surface parity:** `--json` for every command stays
  byte-for-byte stable (C6). The IPC wire shape is untouched.
- **Integration coverage:** The TSV-byte regression for piped output
  is the only cross-layer scenario unit tests alone won't fully
  cover; the existing CLI integration tests (`tests/cli_*.rs`,
  confirm at implementation) already drive piped paths and act as
  the integration regression.
- **Unchanged invariants:**
  - `cli::colors::init` still runs exactly once at process start
    from `cli::dispatch`; no per-call re-derivation.
  - Three off-conditions (`--no-colors` / `NO_COLOR` / non-TTY)
    still silence color (and now also silence padding).
  - `--json` is the agent contract; every JSON path bypasses the
    new formatter helpers.
  - `cliclack` panels remain wizard-only (`init`). Report commands
    are not wrapped in cliclack `intro`/`outro` (Scope Boundaries).

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| TSV byte-drift breaks `awk -F\t` pipelines for users who upgrade. | Hard regression guard: each table-rendering test asserts byte-equality against today's TSV in the colors-disabled branch. The TTY/colors-enabled branch is the only divergence. CI catches any drift. |
| Unicode-width measurement bugs cause misaligned columns under CJK or emoji file names. | `console::measure_text_width` uses the same `unicode-width` crate already pinned in `Cargo.toml`; coverage includes a CJK row in the `format::table` test set. |
| Terminal width detection returns `None` under `tmux`/`screen` and the helper falls back to no-truncation, producing very wide rows. | The fallback is the safe direction (truncation is the rarer affordance). Document the env override (`COLUMNS`) in `docs/usage.md`. |
| Snapshot tests for `status_human` / `list_human` get noisier when the rendering forks on color state. | Tests explicitly toggle `console::set_colors_enabled` via the existing `EnvGuard` pattern from `src/cli/colors.rs::tests`; each test scenario covers one branch only. |
| Help text + banner rendering through clap (`BANNER` in `src/banner.rs`) is unaffected — flagging here just in case a reviewer assumes otherwise. | Not touched; clap renders `BANNER` via `before_help`; the policy initialisation runs after clap, so no interaction. |
| A consumer of `daemon status` (a script) parses today's pretty-JSON output. | Low likelihood: the daemon also exposes the same fields through `status --json` which is the stable agent contract. The TTY path was never a contract surface. Mention in CHANGELOG that `daemon status` shape changed for human output; JSON path is unchanged. |

## Documentation / Operational Notes

- README: add a one-paragraph note that `llamastash <command>` shows
  padded tables on a TTY and TSV when piped. Add `--json` callout as
  the agent contract.
- AGENTS.md "CLI color policy" paragraph adds: "Padded tables are
  also TTY-gated using the same three off-conditions. When piped or
  `--no-colors`, every command emits the same TSV bytes as before so
  `awk -F\t` and `column -t` pipelines keep working unchanged.
  `--json` remains the agent contract."
- `docs/usage.md`: mention `COLUMNS` env var honored for last-column
  truncation when set.
- CHANGELOG `[Unreleased]`: one entry — "feat(cli): padded + colored
  tables on TTY for `list`, `status`, `presets`, `favorites`,
  `last-params`, `daemon status`. TSV preserved when piped; JSON
  byte-stable."
- No runbook / rollout / monitoring changes — the binary's shipped
  output formatting is a presentational change.

## Sources & References

- Reference plan: `docs/plans/2026-05-19-001-feat-interactive-init-wizard-and-cli-colors-plan.md`
  — established `cli::colors`, the three off-conditions, the helper-
  not-direct-escapes rule, and the `cliclack` adoption.
- Reference module: `src/init/prompts.rs` — visual identity source for
  the new `format::kv_block` and `format::section_header` helpers.
- Touched files:
  - `src/cli/format.rs` (new)
  - `src/cli/colors.rs`
  - `src/cli/mod.rs`
  - `src/cli/output.rs`
  - `src/cli/daemon.rs`
  - `src/cli/start.rs`
  - `src/cli/stop.rs`
  - `src/cli/list.rs` (no logic; verifies dispatcher remains unchanged)
  - `src/cli/presets.rs`
  - `src/cli/favorites.rs`
  - `src/cli/last_params.rs`
  - `src/init/doctor.rs`
  - `README.md`
  - `AGENTS.md`
  - `docs/usage.md`
  - `CHANGELOG.md`
  - `TODO.md`
- Related issues/PRs: none open at plan time; this work was
  user-requested directly.
- External docs: `console` crate 0.15 docs
  (https://docs.rs/console/0.15/console/) — `style`, `colors_enabled`,
  `measure_text_width`, `term_size`. Already a top-level dep.
