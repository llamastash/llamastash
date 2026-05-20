---
title: "feat: interactive init wizard + global colors + per-step skip flags"
type: feat
status: completed
date: 2026-05-19
origin: docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md
---

# feat: interactive init wizard + global colors + per-step skip flags

## Overview

The v2 plan (origin) called for an interactive `dialoguer` wizard with persistent header, per-step prompts, and `--yes` for non-interactive defaults. The shipped implementation skipped the prompts entirely: `dialoguer` is declared as a dep in `Cargo.toml` but never imported, and the wizard runs as fully non-interactive on every invocation. This plan closes the gap and extends it with three small CLI-wide additions the original spec did not call out: a `--recommended` alias for "use all derived defaults", per-step typed value flags (`--install`, `--model`, `--config`) that let scripts pre-answer individual prompts, and a global `--no-colors` switch with NO_COLOR + TTY-aware defaults that all human-readable CLI outputs honour. Machine outputs (`--json`) are untouched.

The work is bounded: one new dependency (`cliclack`), one new module (`src/cli/colors.rs`), one new module (`src/init/prompts.rs`), targeted edits to the wizard step functions, a couple of new fields on `InitArgs` and `Cli`, and a color-polish pass over the existing non-JSON CLI outputs. No daemon, IPC, supervisor, recommender, fetch contract, or security contract changes.

## Problem Frame

The R48–R80 plan committed to an interactive wizard (origin §R48–R51, §"`init` step lifecycle", §"`init`/`doctor` mode/flag decision matrix" — the no-flag row says "prompt" for steps 2, 3, 4) but Unit 10 shipped without the `dialoguer` calls. Today `llamastash init` is functionally equivalent to `llamastash init --yes`: it derives defaults from hardware detection and executes them without ever asking the user, which (a) violates the documented contract in the origin plan, (b) gives users no way to override the install method when the auto-pick is wrong (e.g. a Linux box where GH Releases is preferable but brew is available), and (c) provides no progressive disclosure for the recommender's per-pick justification (R58).

Separately, no part of llamastash currently uses colors in its human outputs. Modern CLIs are expected to ship colored output by default with the standard `NO_COLOR` escape hatch, and the user has called this out as a baseline expectation. Bringing in colors for the wizard alone would be jarring against the plain-text rest of the CLI, so the global policy lands together.

## Requirements Trace

- **W1.** Default `init` invocation MUST present an interactive wizard with per-step prompts: install method, model pick, config-diff confirm, smoke handoff. (Closes origin R48, R49, R51, R79.)
- **W2.** A `--recommended` flag MUST short-circuit every prompt to the hardware-aware default. `--yes` is preserved as a hidden alias for backward compatibility with scripts and agents that already pass it. (Origin R76; this plan's "merge `--yes` into `--recommended`" choice.)
- **W3.** Per-step value flags MUST allow callers to pre-answer individual prompts without skipping the rest of the wizard: `--install <choice>`, `--model <choice>`, `--config <choice>`. When supplied, the matching step's prompt is suppressed and the value is used directly.
- **W4.** When `--install` / `--model` / `--config` is supplied for a step that `--skip` already excludes, the wizard MUST emit a one-line stderr warning and proceed; conflicting flags don't abort.
- **W5.** Integrity-check failures under any non-interactive mode (`--recommended`, `--yes`, per-step flags) MUST still abort with the documented exit codes — no silent downgrade. (Carried from origin Key Decision "`--yes` is `--no-confirm` semantics".)
- **W6.** A global `--no-colors` flag MUST disable ANSI styling for every llamastash subcommand. Color output MUST also be auto-disabled when (a) `NO_COLOR` is set in the environment (per https://no-color.org), or (b) stdout is not a terminal. The three conditions are OR-ed; any one of them silences color.
- **W7.** Human-readable outputs of every existing CLI command (`list`, `status`, `start`, `stop`, `logs`, `presets`, `favorites`, `last_params`, `pull`, `init`, `doctor`) SHOULD use colors by default: success-green, error-red, header-bold, dim secondary text. Status indicators (`✓`/`✗`/`›`) replace plain dashes where they aid scanning. `--json` outputs are byte-for-byte unchanged.
- **W8.** Color policy MUST be initialised exactly once at process start (in `cli::dispatch` before any output). Modules consuming the policy MUST NOT each re-derive it.

## Scope Boundaries

- **In scope:** the interactive wizard, the prompt library wiring, `--recommended` / `--yes` merging, the three per-step value flags, the global `--no-colors` flag, NO_COLOR + TTY detection, and a color-polish pass on the existing CLI surfaces.
- **Out of scope:** TUI color theming (ratatui already has its own theme system), daemon log coloring (`simplelog` already handles its own ANSI), test-fixture coloring, doctor's findings re-design (only the rendering picks up colors — the findings list and JSON shape are unchanged), R20 / R55 / R58 recommender behavior changes (only the *display* of the existing justification picks up colors).
- **Explicit non-features:** no `--color always/never/auto` ternary flag (the simpler `--no-colors` + env + TTY detection covers every realistic case); no per-subcommand color overrides; no color customisation via config (would invite an unbounded request stream); no Windows-specific ANSI handling (origin §"Out of scope" already excludes Windows).
- **Deferred:** an "expanded justification" prompt for the recommender (`?` to expand R58's block) — current implementation only stores the one-line justification on `Recommendation`, so an expanded view would need a recommender-side change that's not in this plan's scope. The plan wires the prompt with the one-line variant only; the expand-on-`?` UX becomes a follow-up.

## Context & Research

### Relevant Code and Patterns

- `src/cli/cli_args.rs` — clap derive surface. `Cli` already exposes globals `verbose`, `quiet`, `render`, `render_size`, `no_scan`, `no_spawn`. The new global `--no-colors` plugs in next to `quiet` with `global = true`. `InitArgs` (line 338) adds `recommended`, `install`, `model`, `config_choice` fields next to the existing `yes`, `json`, `offline`, `only`, `skip`.
- `src/cli/mod.rs::dispatch` — central async dispatcher; the single place every subcommand routes through. This is where the color policy gets initialised once (W8). After parsing, before any subcommand handler runs, call `colors::init(cli.no_colors)`.
- `src/init/wizard.rs::run` (line 189) — entry point. `run_install_step` (line 407) and `run_models_step` (line 470) are where the missing prompts go. `run_config_step` (line 536) already builds a diff; just needs a Confirm before the write. `print_persistent_header` (line 390) and `print_handoff` (line 771) are the colored-output sites.
- `src/init/install/mod.rs::InstallChoice` (line 30) + `default_install_method` (line 80) — already enumerate `Brew`, `GhReleases`, `CustomPath(PathBuf)`. The interactive prompt reads this list and pre-selects `default_install_method(hardware)`. The new `--install` flag value-parses into this same enum.
- `src/init/recommender.rs` — `Recommendation` (line 60) + `RecommendationKind` (line 75) with `Curated { entry }`, `OnDisk`, `Escape`. The model-pick prompt renders these rows with `r.justification` next to each. The `Escape` row is the "paste HF repo id" entry; selecting it triggers an `Input` prompt for the repo string.
- `src/cli/output.rs` — the JSON formatters that must remain untouched. Color helpers live in a separate `colors.rs` module to keep the JSON path drift-free.
- `src/cli/list.rs`, `src/cli/status.rs`, `src/cli/start.rs`, `src/cli/stop.rs`, `src/cli/logs.rs`, `src/cli/presets.rs`, `src/cli/favorites.rs`, `src/cli/last_params.rs`, `src/cli/daemon.rs`, `src/init/doctor.rs`, `src/init/download.rs`, `src/init/config_writer.rs` — every existing direct `println!/eprintln!` site that emits human-readable text. Color polish replaces direct prints with helpers from `cli::colors`.
- `src/cli/stop.rs:197` — existing TTY check via `io::stdin().is_terminal()`. The same pattern (via `std::io::IsTerminal`) is used by `colors::init` for the auto-disable-when-piped branch.
- `Cargo.toml:88` — `dialoguer = { version = "0.11", default-features = false }` currently unused. It is replaced by `cliclack` (Key Decision below). The `console` crate (already transitively present via `dialoguer` + `indicatif`) is kept and used directly by `cli::colors` so the color policy module doesn't need to add a dep of its own.

### Institutional Learnings

No `docs/solutions/` entries directly cover prompt or color work. The closest adjacency: `docs/solutions/` is empty for this repo (greenfield), and the v1 review artefacts under `docs/review*.md` do not discuss CLI rendering.

### External References

- `cliclack` crate — Clack-style (https://github.com/natemoo-re/clack) stepped wizard for Rust. API: `intro(title)`, `outro(message)`, `select(prompt).items(...).initial_value(...).interact()`, `confirm(prompt).initial_value(true).interact()`, `input(prompt).validate(...).interact()`, `spinner()`, `log::info(...)`, `log::error(...)`, `log::success(...)`. Built on `console` (already in tree). Uses `console::user_attended()` for non-TTY auto-skip semantics. Pinned by patch version in `Cargo.toml`.
- `console::set_colors_enabled(false)` — process-global color kill switch. `colors::init` calls this when any of the three off-conditions holds. Subsequent `console::style(s).red().bold()` style chains evaluate to no-op strings, so existing call sites don't need conditional branching.
- NO_COLOR specification — https://no-color.org/ — "any non-empty value" disables. `colors::init` checks `std::env::var_os("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false)`.

## Key Technical Decisions

- **Prompt library: `cliclack`** (not `dialoguer`, which is currently declared-but-unused). Rationale: the Clack-style stepped layout (vertical pipe, colored bullets, intro/outro panels) gives the wizard a coherent visual identity with zero per-prompt styling work, exactly the "modern Rust library" the user asked for. `dialoguer` works fine functionally but every prompt looks like a one-shot question; `cliclack` reads as one continuous wizard, which is the desired UX. Cost: swap one dep for another; both are MIT-licensed, both built on `console`. The unused `dialoguer` dep is removed in the same commit.
- **`--yes` is preserved as a hidden alias for `--recommended`** (User Choice §2). The wizard reads a single canonical `is_recommended()` predicate that returns `args.recommended || args.yes`. `--yes` is `#[arg(long, hide = true)]` so it doesn't appear in `--help` for new users but stays parseable for existing scripts. No deprecation warning at runtime — the alias is permanent; the docstring on the `yes` field documents the equivalence.
- **Per-step value flags use small typed enums, not free-form strings** (User Choice §3). `--install <brew|gh-releases|custom:<PATH>|existing>` parses into a new `InstallOverride` enum next to `InstallChoice`. `--model <hf-repo|recommended|none>` parses into `ModelOverride`. `--config <write|skip>` is a two-state enum. clap's `ValueEnum` derive handles the simple variants; `custom:<PATH>` uses a value-parser that splits on the first `:` and validates the suffix exists. Rationale: invalid values are caught at clap parse time, not deep inside the wizard, and `--help` lists every valid choice.
- **`--install` / `--model` / `--config` are advisory, not authoritative**. They suppress the prompt for that step only. They do not implicitly add the step to `--only` or remove it from `--skip`. If `--skip server --install brew` is supplied, the wizard emits one stderr line `warning: --install ignored because the server step is skipped` and proceeds. This matches W4 and keeps the four flags' axes independent: `--only`/`--skip` control *which steps run*; `--install`/`--model`/`--config` control *how each running step decides*.
- **Color policy lives in `src/cli/colors.rs`, initialised once in `cli::dispatch`** (W8). The module exports: `init(no_colors_flag: bool)` (sets `console::set_colors_enabled`), plus small render helpers `success(msg)`, `error(msg)`, `warning(msg)`, `dim(msg)`, `header(msg)`, `key_value(k, v)` returning `console::StyledObject<...>` or pre-rendered `String` as the call site prefers. Callers never re-derive whether colors are enabled. Rationale: a single source of truth for the policy means a future fourth condition (e.g., a `LLAMASTASH_COLOR=never` env var) lands in one place rather than every output site.
- **Wizard prompts wrap cliclack via `src/init/prompts.rs`**. The wrapper handles three things the bare cliclack calls don't: (1) honoring the recommended/yes flag without each step re-checking; (2) honoring per-step value flags (when `--install brew` is set, the install-method prompt is bypassed and the value is returned directly); (3) honoring `LLAMASTASH_INIT_NONINTERACTIVE=1` (auto-set by the TTY detector when stdout isn't a terminal — so piped invocations still pick defaults rather than hang on `interact()`). Rationale: each wizard step calls a single helper (`prompts::pick_install_method(args, hardware, default)`), keeping the step function bodies focused on what they do, not on which interaction mode they're in.
- **`InitArgs::recommended` is the canonical flag; `yes` is read-only fallback**. Inside the wizard, all decisions branch on a single computed `is_recommended` boolean, not on the raw flags. Rationale: a future flag like `--all-defaults` could be added by extending the predicate without revisiting every step.
- **Color polish on existing CLI commands uses helpers, not direct ANSI escapes**. Each touched site (list / status / start / stop / etc.) calls `colors::success("started ...")` rather than embedding `\x1b[32m`. Rationale: the helpers are tested for the `--no-colors` path once; the call sites stay clean.
- **The colored persistent header (line 399's `eprintln!("llamastash init — detected ... ")`) becomes a cliclack `intro` block**. Rationale: cliclack already renders an intro panel with the wizard's identity colour; we shouldn't double-print a custom header above it. The hardware information moves into the intro block's body.

## Open Questions

### Resolved During Planning

- **Prompt library** → `cliclack` (User Choice §1; see Key Decisions).
- **`--yes` vs `--recommended` semantics** → merged; `--yes` is a hidden alias (User Choice §2; see Key Decisions).
- **Per-step skip-with-choice flag shape** → three typed flags `--install`, `--model`, `--config` (User Choice §3; see Key Decisions).
- **`--no-colors` triggers** → flag + NO_COLOR env + non-TTY stdout (User Choice §4; see Key Decisions).
- **Where the colored-output policy initialises** → once in `cli::dispatch` before any handler runs (W8 + Key Decisions).
- **Whether `--install custom` accepts a path** → yes, syntax `--install custom:<PATH>` parsed at the clap layer; invalid path errors at parse time, not at step execution (see Unit 3).
- **Whether the JSON output picks up colors** → no, only human outputs. JSON is a machine contract (see Unit 5).
- **What happens when a per-step flag is supplied for a skipped step** → one-line stderr warning, no abort (W4 + Key Decisions).
- **`dialoguer` dependency disposition** → removed in the same commit that adds `cliclack`; it was declared but unused (verified via `grep -rn 'dialoguer::' src/` returning zero matches).

### Deferred to Implementation

- The exact `cliclack` patch version to pin (latest stable at implementation time; `cargo add cliclack --vers '~X.Y'` and let the lockfile freeze the patch).
- The exact glyphs/colors for `success` / `error` / `warning` helpers in `cli::colors` — picked once at Unit 1 with reference to `cliclack`'s own palette so the wizard and the rest of the CLI feel consistent. (Initial palette: success=`✓` green, error=`✗` red, warning=`!` yellow, dim=`›` gray.)
- Whether `pull --json` (already structured) needs a colored human-mode equivalent or stays at its existing line-based progress. Decide at Unit 5.
- The wording of the install-method prompt's "use existing" pre-select label — finalised against the real `BinaryPresence` shape at Unit 2 review.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

### Decision matrix for prompt-vs-skip behaviour

For each step, three independent axes decide the runtime behaviour. The interactive prompt only fires when **all** three of (a) step in plan, (b) no per-step value flag, (c) `is_recommended()` is false — and only when running attached to a TTY.

| `--only`/`--skip` says step runs | Per-step flag supplied | `--recommended` / `--yes` set | TTY | Behaviour |
|---|---|---|---|---|
| yes | no | no | yes | prompt |
| yes | no | no | no | use derived default (auto-non-interactive) |
| yes | no | yes | any | use derived default |
| yes | yes | any | any | use flag value (prompt suppressed) |
| no | no | any | any | step skipped |
| no | yes | any | any | step skipped + stderr warning "flag ignored, step skipped" |

### Wizard flow shape (cliclack rendering)

```text
intro:  ◆ llamastash init
        │
        │  detected: NVIDIA RTX 4090 · 64 GB RAM · 24 GB VRAM · Linux/x86_64
        │
step 2: ▲ Install method
        │  ● GitHub Releases (recommended for Linux/AMD)
        │  ○ Homebrew (not detected on PATH)
        │  ○ Use existing binary at /usr/local/bin/llama-server (v9111)
        │  ○ Custom path…
        │
step 3: ▲ Pick a model
        │  ● Qwen2.5-Coder 7B Q4_K_M — fits 14.2 GB / 21 GB headroom · 38 tok/s
        │  ○ Llama-3.2 3B Q4_K_M — fits 2.1 GB / 21 GB headroom · 92 tok/s
        │  ○ Mistral 7B Q5_K_M — fits 5.5 GB / 21 GB headroom · 35 tok/s
        │  ○ Paste an HF repo ID…
        │
step 4: ▲ Write config?
        │  diff preview:
        │  + llama_server_path: /opt/llamastash/llama-cpp/b9222/llama-server
        │  + arch_defaults.qwen2.n_gpu_layers: 99
        │  + arch_defaults.qwen2.flash_attn: true
        │  ● Yes
        │  ○ Skip
        │
step 5: spinner → "smoke launching…" → ✓ ok (240 ms)
outro:  ◆ ready — `llamastash` to launch the TUI
```

### `cli::colors` module shape (pseudo)

```text
init(no_colors_flag: bool):
  off  =  no_colors_flag
       || NO_COLOR-env-set-and-nonempty
       || !stdout-is-terminal
  console::set_colors_enabled(!off)

success(msg)  →  "✓ " + green-bold(msg)
error(msg)    →  "✗ " + red-bold(msg)
warning(msg)  →  "! " + yellow(msg)
dim(msg)      →  gray(msg)
header(msg)   →  bold-underline(msg)
key_value(k,v)→  bold(k) + "  " + plain(v)
```

When colors are disabled, every helper short-circuits to the plain glyph + message; the call sites are unconditional and the `--no-colors`/NO_COLOR/non-TTY paths produce byte-identical output to today's plain text.

## Implementation Units

- [x] **Unit 1: `cli::colors` module + global `--no-colors` flag + policy initialisation**

**Goal:** Add the color-policy module, the global flag, and the one-time init in `cli::dispatch`. No other call sites change yet — they're covered in Unit 5.

**Requirements:** W6, W7 (foundation), W8.

**Dependencies:** None. Lands first; every later unit consumes it.

**Files:**
- Create: `src/cli/colors.rs` — module with `pub fn init(no_colors: bool)`, plus the render helpers (`success`, `error`, `warning`, `dim`, `header`, `key_value`). Helpers return `console::StyledObject<&str>` for inline composition and a `String` for end-of-line writes.
- Modify: `src/cli/mod.rs` — declare `pub mod colors;`. In `dispatch` (or its caller), call `colors::init(cli.no_colors)` exactly once before routing to any subcommand handler.
- Modify: `src/cli/cli_args.rs` — add `#[arg(long, global = true)] pub no_colors: bool` on `Cli` next to `quiet`.
- Test: inline `#[cfg(test)] mod tests` in `src/cli/colors.rs`.

**Approach:**
- `init(no_colors)` computes the three off-conditions OR-ed together. `is_terminal()` comes from `std::io::IsTerminal` on `std::io::stdout()`. Calls `console::set_colors_enabled(!off)` and stashes the resolved bool in a `OnceLock<bool>` for future helpers that need to branch (none today, but the slot is cheap).
- Helpers wrap `console::style(...)`. Because `console::set_colors_enabled(false)` already short-circuits the style chain at print time, the helpers don't need an extra `if colors_on` branch.
- The module is `pub(crate)`; nothing outside the binary uses it.

**Patterns to follow:**
- `src/cli/cli_args.rs` for the global-arg shape (mirror `quiet`'s declaration).
- `src/cli/stop.rs:197` for the `IsTerminal` import pattern.

**Test scenarios:**
- *Happy path*: `init(false)` with TTY-attached and no `NO_COLOR` env → `console::colors_enabled()` returns `true` afterward.
- *Edge case*: `init(true)` always disables, regardless of TTY/env.
- *Edge case*: `init(false)` with `NO_COLOR=1` exported → disabled.
- *Edge case*: `init(false)` with `NO_COLOR=""` (set but empty) → enabled per the spec (empty value does not trigger).
- *Edge case*: `init(false)` with stdout redirected to a pipe (non-TTY in test harness) → disabled.
- *Happy path*: `colors::success("ok")` returns a string starting with `✓` and ending with `ok` in both colored and uncolored modes; the colored mode contains an ANSI escape, the uncolored mode does not.
- *Happy path*: `colors::error("bad")` similarly. Same test asserts that the ANSI-stripped form is identical across both modes (regression guard against accidental glyph drift).

**Verification:**
- `cargo run -- --no-colors list` emits no ANSI escapes (after Unit 5 lands; for Unit 1 alone, verify that `colors::error("x")` produces an escape-free string when `init(true)` was called).
- `NO_COLOR=1 cargo run -- list` emits no ANSI escapes (same caveat).

---

- [x] **Unit 2: Replace `dialoguer` with `cliclack`; add `src/init/prompts.rs` wrapper**

**Goal:** Swap the unused `dialoguer` dependency for `cliclack`, and add the wizard-facing prompt wrapper module that every wizard step calls into.

**Requirements:** W1 (foundation — bare prompt primitives, no per-step wiring yet), W2 (the `is_recommended()` predicate lives here).

**Dependencies:** Unit 1 (the prompt wrapper logs via `cli::colors` for non-cliclack messages outside the wizard's intro/outro block).

**Files:**
- Modify: `Cargo.toml` — remove the `dialoguer` line at 88; add `cliclack = "<latest stable>"` pinned by patch (the resolved version freezes in `Cargo.lock`).
- Create: `src/init/prompts.rs` — wrapper module. Public surface:
  - `pub fn is_recommended(args: &InitArgs) -> bool` — returns `args.recommended || args.yes`.
  - `pub fn intro(hardware: &HardwareSnapshot)` — renders the cliclack intro panel with the hardware line baked into the body.
  - `pub fn outro(summary: &InitSummary)` — renders the cliclack outro panel; the existing `print_handoff` content (sans header) becomes the outro body.
  - `pub async fn pick_install_method(args: &InitArgs, default: InstallChoice, existing: Option<&BinaryPresence>) -> Result<InstallChoice, CliExit>` — handles `--install` override + `--recommended` short-circuit + interactive Select. Returns the chosen `InstallChoice`.
  - `pub async fn pick_model(args: &InitArgs, recs: &[Recommendation]) -> Result<ModelChoice, CliExit>` — handles `--model` override + `--recommended` short-circuit + interactive Select with a "paste HF repo" Input branch. Returns a `ModelChoice` enum (`Curated(ModelEntry)`, `Paste(String)`, `Skip`).
  - `pub async fn confirm_config_write(args: &InitArgs, diff_render: &str) -> Result<bool, CliExit>` — handles `--config` override + `--recommended` (auto-yes) + interactive Confirm.
- Test: inline `#[cfg(test)] mod tests` for `is_recommended` and the override-honouring branches of the three pickers (with a stubbed cliclack interaction).

**Approach:**
- `prompts::is_recommended` is the single canonical predicate. Every wizard step calls it; raw `args.recommended` / `args.yes` are not read elsewhere.
- Each picker checks, in order: (1) is the corresponding override flag set? → return it; (2) is `is_recommended()` true? → return the derived default; (3) is stdout non-TTY? → return the derived default and log one warning line via `cli::colors::warning`; (4) otherwise → cliclack prompt.
- The pickers return their domain enum (`InstallChoice`, `ModelChoice`, `bool`), not a raw cliclack response. This lets the wizard step bodies stay free of UI logic.
- Tests stub the cliclack call by checking each picker only runs the prompt when none of the three short-circuit conditions held. (Mocking cliclack itself is out of scope; the prompt path runs against a real terminal in CI only when manually exercised. The unit tests cover the override + recommended + non-TTY branches.)

**Patterns to follow:**
- `src/init/wizard.rs::run_install_step` (line 407) for the current branch shape the picker replaces.
- cliclack's documented stepped-wizard pattern (intro → select/confirm/input → outro).

**Test scenarios:**
- *Happy path*: `is_recommended` returns true when `args.recommended` is set and false otherwise.
- *Happy path*: `is_recommended` returns true when `args.yes` is set (backward-compat alias).
- *Happy path*: `pick_install_method` with `args.install = Some(InstallOverride::Brew)` returns `InstallChoice::Brew` without touching the prompt path.
- *Edge case*: `pick_install_method` with `args.recommended = true` returns the supplied `default` argument unchanged.
- *Edge case*: `pick_install_method` under non-TTY stdout (test simulates with `console::set_colors_enabled` + a TTY override hook) returns the default and emits one warning line.
- *Error path*: `pick_install_method` with `args.install = Some(InstallOverride::Custom(p))` where `p` is not executable → returns `Err(CliExit::new(INIT_ABORTED, ...))` synchronously, no prompt.
- *Happy path*: `pick_model` with `args.model = Some(ModelOverride::Paste("owner/repo"))` returns `ModelChoice::Paste("owner/repo".into())`.
- *Edge case*: `pick_model` with `args.model = Some(ModelOverride::None)` returns `ModelChoice::Skip` and the wizard's caller falls through to "no model downloaded this run".
- *Happy path*: `confirm_config_write` with `args.config_choice = Some(ConfigOverride::Skip)` returns `Ok(false)` without prompting.
- *Edge case*: `confirm_config_write` with `is_recommended()` true returns `Ok(true)` without prompting.

**Verification:**
- `cargo build` succeeds with `cliclack` in deps and `dialoguer` removed.
- `grep -rn 'dialoguer::' src/` returns zero matches (the dep is genuinely unused).

---

- [x] **Unit 3: Add `--recommended`, `--install`, `--model`, `--config` to `InitArgs`**

**Goal:** Extend the clap surface so the per-step value flags and the canonical `--recommended` parse cleanly, with `--yes` preserved as a hidden alias.

**Requirements:** W2, W3, W4 (clap parsing half — the wizard-side behaviour is Unit 4).

**Dependencies:** None. Can land in parallel with Unit 1/2 if needed; Unit 4 consumes it.

**Files:**
- Modify: `src/cli/cli_args.rs` — extend `InitArgs`:
  - Rename the existing `yes: bool` to keep its name but add `#[arg(long, hide = true)]` so it stays parseable without appearing in `--help`.
  - Add `#[arg(long)] pub recommended: bool`.
  - Add `#[arg(long, value_name = "CHOICE")] pub install: Option<InstallOverride>`.
  - Add `#[arg(long, value_name = "CHOICE")] pub model: Option<ModelOverride>`.
  - Add `#[arg(long = "config", value_name = "CHOICE")] pub config_choice: Option<ConfigOverride>` (field name avoids collision with the existing `--config` global on `Cli`; clap routes the long-flag to this field under the `init` subcommand only via clap's per-subcommand scoping — verify in tests).
  - Add the three new enums:
    - `InstallOverride { Brew, GhReleases, Existing, Custom(PathBuf) }` — `Custom` uses a custom value-parser splitting on the first `:` (input form: `custom:/abs/path/to/llama-server`).
    - `ModelOverride { Recommended, None, Paste(String) }` — `Paste` uses a value-parser that validates the string contains exactly one `/` (HF repo id shape: `owner/name`).
    - `ConfigOverride { Write, Skip }` — plain `ValueEnum`.
- Test: extend `tests/cli_init_parse.rs` for every new flag's parsing matrix, including `--install custom:/usr/local/bin/llama-server` and `--model bartowski/Llama-3.2-3B-GGUF`.

**Approach:**
- The existing `--config` is on the *top-level* `Cli` struct (for the YAML config file path). The new `--config` lives on `InitArgs` and only resolves under `llamastash init`. clap routes per-subcommand args independently; both can coexist as long as the field names don't collide on the same struct. The plan's `config_choice` field name keeps Rust-level distinct identifiers.
- The `Custom(PathBuf)` variant's value parser: accept `custom:<PATH>`, error on missing colon, error on empty path, do **not** stat the path at parse time (the wizard's existing `is_safe_to_adopt` runs the integrity check at step execution time, which is the documented contract). Parse-time validation is shape-only.
- The `Paste(String)` value parser: accept any string of the form `<owner>/<repo>` where both parts are non-empty and contain no whitespace. Further repo-existence verification happens at HF download time (already covered by the existing fetch path).
- The full conflict matrix between `--only` / `--skip` and `--install` / `--model` / `--config` is **not** enforced at parse time. The wizard emits the warning at runtime per W4 because clap's `conflicts_with` doesn't model "warn but allow" semantics.

**Patterns to follow:**
- `src/cli/cli_args.rs::InitStep` for the `ValueEnum` derive shape.
- `src/cli/cli_args.rs::parse_render_size` for the custom value-parser pattern.

**Test scenarios:**
- *Happy path*: `llamastash init --recommended` parses; `args.recommended` is `true`, `args.yes` is `false`.
- *Happy path*: `llamastash init --yes` parses (hidden alias); `args.recommended` is `false`, `args.yes` is `true`.
- *Happy path*: `llamastash init --recommended --yes` parses with both flags `true` (no mutual exclusion).
- *Happy path*: `llamastash init --install brew` parses; `args.install` is `Some(InstallOverride::Brew)`.
- *Happy path*: `llamastash init --install custom:/usr/local/bin/llama-server` parses; `args.install` is `Some(InstallOverride::Custom("/usr/local/bin/llama-server".into()))`.
- *Error path*: `llamastash init --install custom:` fails clap parse with an actionable error (empty path).
- *Error path*: `llamastash init --install custom:relative/path` either parses (relative paths permitted; stat happens at exec time) or errors — pick one in implementation; the test pins the chosen behavior.
- *Error path*: `llamastash init --install unknown` fails clap parse with "possible values: brew, gh-releases, existing, custom:<PATH>".
- *Happy path*: `llamastash init --model bartowski/Llama-3.2-3B-GGUF` parses; `args.model` is `Some(ModelOverride::Paste("bartowski/Llama-3.2-3B-GGUF".into()))`.
- *Happy path*: `llamastash init --model recommended` parses; `args.model` is `Some(ModelOverride::Recommended)`.
- *Happy path*: `llamastash init --model none` parses; `args.model` is `Some(ModelOverride::None)`.
- *Error path*: `llamastash init --model invalid-no-slash` fails clap parse.
- *Happy path*: `llamastash init --config skip` parses; `args.config_choice` is `Some(ConfigOverride::Skip)`.
- *Edge case*: `llamastash init --recommended --install brew --model bartowski/Llama-3.2-3B-GGUF --config write` parses cleanly (all flags coexist).
- *Edge case*: existing `--only`/`--skip` mutual exclusion still holds (no regression).

**Verification:**
- `cargo test --features test-fixtures cli_init_parse` passes including the new matrix.
- `cargo run -- init --help` shows the new flags with their value enumerations; `--yes` is absent (hidden).

---

- [x] **Unit 4: Wire the wizard to actually prompt — `run_install_step`, `run_models_step`, `run_config_step`, intro/outro**

**Goal:** Replace the silent non-interactive paths in the wizard step functions with calls into `init::prompts`. The wizard now defaults to interactive; `--recommended` / `--yes` / per-step flags suppress prompts deterministically.

**Requirements:** W1, W2, W3, W4 (runtime half), W5.

**Dependencies:** Units 1, 2, 3.

**Files:**
- Modify: `src/init/wizard.rs`:
  - `run` (line 189): at the top of the function, call `prompts::intro(&hardware)` instead of `print_persistent_header`. Remove the now-redundant `print_persistent_header` function (its content moves into the intro).
  - `run_install_step` (line 407): replace the `if args.yes` adopt branch + `default_install_method` direct call with `let choice = prompts::pick_install_method(args, default_install_method(hardware), binary.as_existing()).await?`. The downstream `match choice` block stays unchanged.
  - `run_models_step` (line 470): remove the `let _ = args.yes;` discard line. After the `recommend(...)` call, call `prompts::pick_model(args, &recs).await?` and branch on the returned `ModelChoice` (Curated → use that ModelEntry, Paste → wrap in a new HF spec, Skip → return empty `ModelSummary`).
  - `run_config_step` (line 536): after `write_with_diff` returns and the diff is rendered, call `prompts::confirm_config_write(args, &result.diff_render).await?`. If the user declines, *roll back* the write — since `merge_and_write` is the actual write step, the confirm needs to move *before* the write call, taking the rendered diff from a dry-run pass. **Note:** `WriteOptions::show_diff_preview` already controls whether the diff is rendered to stderr; the confirm path replaces that flag's positive branch.
  - `print_handoff` (line 771): replace the direct `println!` block with a call to `prompts::outro(summary)`. The JSON branch (`args.json`) stays untouched — JSON output bypasses the outro and prints raw JSON to stdout.
- Modify: `src/init/wizard.rs` — emit the W4 warning when any of `args.install` / `args.model` / `args.config_choice` is `Some(_)` but the corresponding step is not in the resolved `StepPlan`. One stderr line via `cli::colors::warning`.
- Modify: `src/init/config_writer.rs` — expose a `dry_run_diff(...)` helper that returns the rendered diff without writing, so the confirm step can render before the user agrees. The existing `write_with_diff` becomes a thin wrapper around `dry_run_diff` + the actual write.
- Test: extend `tests/init_orchestration.rs` to cover:
  - `--recommended` end-to-end runs without prompting (existing `--yes` test renames).
  - `--install brew --recommended` honours `--install` even though `--recommended` would have picked GH Releases.
  - `--skip server --install brew` emits the W4 warning line.
  - `--config skip` causes `run_config_step` to return early without writing.

**Approach:**
- The wizard step functions stay async because cliclack's interaction functions are sync-blocking; cliclack runs them on the calling thread inside `tokio::task::spawn_blocking` (wrapped inside the prompt-helper module to keep the wizard's async signature uniform).
- The `--config skip` branch: the wizard skips the entire config write step and records `config: None` in `InitSummary`. The wizard's existing config-step skip path (when `--skip config` is supplied) is reused; `--config skip` simply forces the same outcome.
- Integrity-check failures (W5): if an `InstallOverride::Custom(path)` fails `is_safe_to_adopt`, the wizard aborts with `INIT_ABORTED = 72`. If a download fails, `INIT_DOWNLOAD_FAILED = 73`. The non-interactive abort is loud, not silent.
- The W4 warning is emitted exactly once per ignored flag, near the top of `run`, before any step executes.

**Patterns to follow:**
- Existing `run_install_step` / `run_models_step` / `run_config_step` shapes — preserve their public signatures and return types.
- Existing `--yes` short-circuit for "use existing binary" — the new flow runs this through `prompts::pick_install_method`, which surfaces the existing binary as the pre-selected option when present.

**Test scenarios:**
- *Happy path*: `init --recommended` runs every step without prompting; final exit 0; `InitSummary.steps_ran` lists detect/server/models/config/smoke/handoff.
- *Happy path*: `init --recommended --install brew` runs the brew installer regardless of the hardware-default pick. (Validates Unit 3's "per-step flag overrides --recommended").
- *Happy path*: `init --recommended --model none` skips the model download; `InitSummary.model` is `None` or has `repo: ""`.
- *Happy path*: `init --recommended --config skip` skips the config write; `InitSummary.config` is `None`; `_init_snapshot.json` is still updated (snapshot persistence is independent of config write).
- *Edge case*: `init --skip server --install brew` emits the W4 warning, then runs steps 3+5 + 4 normally without invoking the brew installer.
- *Edge case*: `init` (no flags) under a non-TTY stdout (piped/redirected) auto-disables prompts: the wizard logs `warning: stdout is not a terminal; using recommended defaults for all steps` and continues as if `--recommended` were set. Final exit 0.
- *Error path*: `init --install custom:/nonexistent/path` aborts with exit 72 at `run_install_step` (Unit 3's parser doesn't stat; this unit's integrity check does).
- *Error path*: `init --recommended` against a host where `default_install_method` returns `Brew` but brew is not installed → aborts with exit 72 (matches existing semantics; new flag does not soft-fail).
- *Integration*: `init` (interactive) with the cliclack prompt streaming to a real TTY in a `bash -c "..." | head"` harness — out of scope for `cargo test`; manual smoke covered by Verification.

**Verification:**
- `cargo run -- init` on a fresh box drops the user into the cliclack wizard; selecting through every step lands a working install + downloaded model + config; final outro prints.
- `cargo run -- init --recommended` runs end-to-end with zero prompts and the same outcome.
- `cargo run -- init --install gh-releases --model recommended --config write --recommended` is identical to `--recommended` alone (every override matches the default).
- `cargo run -- init --recommended | cat` (piped, non-TTY): exits 0 with warning line on stderr.
- All `tests/init_orchestration.rs` cases pass.

---

- [x] **Unit 5: Color polish for the existing CLI surfaces**

**Goal:** Replace direct `println!` / `eprintln!` of human-readable text with calls into `cli::colors` helpers across every existing subcommand. JSON output paths are untouched.

**Requirements:** W7.

**Dependencies:** Unit 1.

**Files (modify only — no new modules):**
- `src/cli/list.rs` — header row bold; "(no models discovered)" dim; row separator unchanged. JSON path untouched.
- `src/cli/status.rs` — daemon-state line uses `success` (running) / `error` (stopped) / `warning` (degraded); model rows use `dim` for secondary fields.
- `src/cli/start.rs` — "started {name} …" → `colors::success(...)`; error rendering from `CliExit` already goes through the dispatcher; ensure the dispatcher uses `colors::error` for the leading `error:` prefix.
- `src/cli/stop.rs` — "stopped {name}" → `colors::success(...)`.
- `src/cli/logs.rs` — log-stream pass-through stays plain (children produce their own colors); the wizard's own prefixes (`--follow` startup notice) pick up `dim`.
- `src/cli/presets.rs`, `src/cli/favorites.rs`, `src/cli/last_params.rs`, `src/cli/daemon.rs` — success / error lines through the helpers.
- `src/init/doctor.rs` — finding severity → color: critical=`error`, warning=`warning`, info=`dim`. The `fix_hint` line uses `dim`. JSON output untouched.
- `src/init/download.rs` — progress lines use `dim`; final completion uses `success`. (Already structured per-step; minimal change.)
- `src/init/config_writer.rs` — diff render: `+` lines green via `colors::success`-style, `-` lines red, untouched dim. The redacted-secret marker `<redacted>` stays plain (visually distinct already).
- `src/cli/mod.rs` (dispatcher) — error prefix uses `colors::error`; the warning prefix (none today, but the wizard's W4 stderr line goes through `colors::warning`) is consistent.
- Test: extend existing per-command tests where they already assert on output strings — assertions now strip ANSI before comparing (via a small `strip_ansi(s: &str) -> String` test helper added to the existing test-utils path; or `console::strip_ansi_codes` if `console` exposes it on the `parse` feature).

**Approach:**
- Color is applied at the print site, not buried inside the row formatter. Formatters return plain `String` (so `--json` paths and tests stay stable); the print site wraps in `colors::success(...)` etc.
- Existing tests that pin output bytes get an ANSI-strip step. The fixture strings stay plain so the tests document the canonical content; the runtime adds color on top.
- The dispatcher's existing `error:` prefix is the standardised entry point for failure messages — concentrating the `colors::error` call there saves touching every handler. Errors already flow through `CliExit` → dispatcher; verify that's still the only render path.
- JSON paths are byte-stable: every site that produces JSON output writes `colors::pretty_json` (or its equivalent — a passthrough on the JSON branch) and never wraps the result in a colored helper.

**Patterns to follow:**
- `src/cli/output.rs::pretty_json` for the JSON-passthrough convention.
- The existing `eprintln!("error: …")` formatting in `src/cli/mod.rs` — that single site picks up `colors::error` and every handler benefits.

**Test scenarios:**
- *Happy path*: `list --no-colors` output is byte-identical to today's `list` output (regression guard).
- *Happy path*: `list` (with TTY-attached test harness or `colors::init(false)` + forced TTY) output contains ANSI escapes around the header row.
- *Happy path*: `list --json` is byte-identical to today's `list --json` output regardless of `--no-colors` (JSON is untouched).
- *Edge case*: `NO_COLOR=1 list` is byte-identical to `list --no-colors`.
- *Edge case*: `list 2>&1 | cat` is byte-identical to `list --no-colors` (stdout is not a TTY → auto-disable).
- *Happy path*: `start <model>` success message renders green when colors enabled.
- *Happy path*: `doctor` with one critical finding renders the finding line in red; the `→ fix with:` hint renders dim.
- *Edge case*: `doctor --json` is byte-identical to today.
- *Integration*: existing test fixtures across `src/cli/*::tests` that assert on output strings are updated to strip ANSI before comparing. No fixture text changes.

**Verification:**
- `cargo test --features test-fixtures cli` passes after the ANSI-strip helper lands.
- Visual diff: running `llamastash list`, `llamastash status`, `llamastash start <m>`, `llamastash doctor` in a real terminal shows colored output; running with `--no-colors`, `NO_COLOR=1`, or piping to `cat` shows plain text.

---

- [x] **Unit 6: Documentation refresh**

**Goal:** Update `README.md`, `AGENTS.md` if present, and the origin plan's status to reflect the now-shipped interactive wizard. This is a small unit that keeps the documentation aligned with behaviour.

**Requirements:** Documentation hygiene; no W-numbered requirement.

**Dependencies:** Units 1–5 must be merged.

**Files (modify only):**
- `README.md` — update the `init` section to describe interactive default + the `--recommended` flag + the three per-step flags + `--no-colors`. Replace any `--yes` references with `--recommended` in user-facing docs; `--yes` remains in a small "compatibility note" box.
- `docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md` — append a short post-mortem note at the bottom (under a new `## Post-shipping addendum` heading) referencing this plan as the closure for the missed interactive-prompts work in Unit 10.
- `AGENTS.md` (if present in repo root) — ensure agent guidance for `init` mentions `--recommended` instead of `--yes`; preserve the `--yes` alias mention for legacy scripts.

**Approach:**
- Doc edits only — no code touched in this unit. Keeps the merge-time diff small for the doc-review reviewer.

**Patterns to follow:**
- Existing `README.md` / `AGENTS.md` voice and section structure.

**Test scenarios:**
- *Test expectation: none — doc-only unit, no behavioural change to verify.*

**Verification:**
- Reading the updated `README.md` end-to-end describes the current behaviour (interactive by default, `--recommended` for headless, three per-step flags, `--no-colors` global).
- The origin plan's `## Post-shipping addendum` notes the missed-and-now-closed item.

---

## System-Wide Impact

- **Interaction graph:** `cli::dispatch` adds one new call (`colors::init`) before subcommand routing; every subcommand handler downstream becomes color-aware passively (no per-handler changes beyond Unit 5's renderer-site edits). The wizard's step functions add one new call each into `init::prompts`, but their return types and error contracts are unchanged.
- **Error propagation:** the wizard's `INIT_ABORTED` / `INIT_DOWNLOAD_FAILED` / `INIT_SMOKE_FAILED` exit codes remain the contract; new per-step flag validation funnels into the same `CliExit`-returning paths. clap-level value-parser failures produce exit 64 (USAGE) per the existing convention.
- **State lifecycle risks:** `--config skip` causes the wizard to *not* write `config.yaml` but still persist `init_snapshot.json` with the new install + model state. If a future doctor finding compares snapshot ↔ config, it must tolerate the snapshot-without-config-keys case. This already holds today for `--skip config`; the new `--config skip` flag follows the same path.
- **API surface parity:** `init --json` output schema is unchanged. Adding fields to the schema is explicitly out of scope; the wizard's JSON consumers (`R77`) continue to see the same shape, with the same `safe_to_log` redaction policy.
- **Integration coverage:** the cliclack interactive path can't be exercised by `cargo test` (it requires a real PTY). Coverage is split: unit tests cover override + recommended + non-TTY branches; the interactive path is exercised by manual smoke (see Verification in Unit 4). This matches the original plan's posture for Unit 10's interactive surface.
- **Unchanged invariants:**
  - The fetch contract, security contract, recommender output shape, and `_init_snapshot.json` schema are all untouched.
  - The daemon's IPC surface, supervisor lifecycle, and orphan-readopt behavior are untouched.
  - `--json` outputs across every subcommand are byte-stable.
  - The clap mutual-exclusion between `--only` and `--skip` is unchanged.
  - The existing `cli_doctor_basic`, `cli_integration_test`, `init_orchestration`, `init_snapshot_persistence`, and `tests/cli_init_parse.rs` integration tests retain their current assertions (with ANSI-strip applied where they pin output bytes).

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `cliclack`'s panel rendering looks broken on unusual terminals (xterm-mono, minimal Linux consoles, some CI runners) | TTY auto-disable already catches non-interactive CI; for the interactive path, document `--no-colors` as the workaround. Add a manual smoke step in the PR checklist for at least one terminal class outside the developer's primary. |
| `cliclack` patch bump introduces breaking changes mid-release | Pin by minor version (`cliclack = "X.Y"` resolves to the highest patch of `X.Y`). Future minor bumps go through a deliberate PR with smoke-test confirmation. |
| Existing tests pin output bytes; ANSI escapes break assertions | Unit 5 adds an `strip_ansi` test helper. Tests are updated in the same PR as the renderer-site change. CI re-runs catch any miss. |
| `--config` field name on `InitArgs` shadows the global `--config` on `Cli` | Verified at clap-parse layer in Unit 3's tests; clap routes per-subcommand args independently. The Rust-level field name `config_choice` avoids any in-code ambiguity. |
| Users with `--yes` in scripts see no functional change but the doc says "deprecated alias" | `--yes` is hidden in `--help` but still parsed (Unit 3). No runtime warning. The doc note (Unit 6) explains the alias is permanent — not actually deprecated, just superseded as the recommended invocation. |
| The cliclack `confirm` returns `false` and the wizard rolls back — but the install + model steps already completed | The rollback for `--config skip` is just "don't write the file"; the install + downloaded model on disk are preserved (matches today's `--skip config` semantics). The summary line tells the user what landed and what was declined. |
| `console::set_colors_enabled(false)` is process-global; tests that run in parallel could race on it | `colors::init` is called once at process start. Tests that need a specific color state use `console::set_colors_enabled` directly within a test-local scope and restore the previous value on drop (RAII pattern). |
| Non-TTY auto-disable hits unexpected environments (e.g. an interactive subshell where stdout is captured by a wrapper) | Document `LLAMASTASH_FORCE_COLOR=1` as an opt-back-in env var. **Deferred to Unit 1** if the simpler three-condition policy proves insufficient in early testing — the OR-condition layout makes adding a fourth (positive) condition straightforward. |

## Documentation / Operational Notes

- `README.md` updated to reflect the interactive default and the new flags (Unit 6).
- Origin plan annotated with the closure note (Unit 6).
- No rollout / monitoring concern — purely CLI-surface changes; no runtime data or daemon-state migration.
- No `docs/solutions/` entry needed at landing; if interactive-vs-headless misuse surfaces post-launch (e.g. agents accidentally hanging on a prompt), a solution note covering the TTY-detection rule would be added then.

## Sources & References

- **Origin plan:** [docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md](docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md) — see Unit 10 (the unit that documented and then missed the interactive wizard).
- **Origin brainstorm (transitive):** docs/brainstorms/2026-05-18-init-wizard-requirements.md — R48, R49, R51, R76 specifically.
- **Related code:**
  - `src/init/wizard.rs` (entry point + step functions)
  - `src/cli/cli_args.rs` (flag surface)
  - `src/cli/mod.rs` (dispatcher + color init site)
  - `src/init/install/mod.rs::InstallChoice` (install enum the prompt reuses)
  - `src/init/recommender.rs` (recommendation rows the model prompt renders)
- **External docs:**
  - cliclack — https://crates.io/crates/cliclack (Clack-style stepped wizard, MIT)
  - NO_COLOR — https://no-color.org/ (env-variable spec)
  - `console` crate — https://docs.rs/console (already in tree via dialoguer/indicatif)
