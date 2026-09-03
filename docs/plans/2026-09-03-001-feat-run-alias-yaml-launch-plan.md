# Plan: `run` alias + YAML launch files for `start`

**Status:** planned (2026-09-03). Unit 8 (CLI). Commit subjects: `feat(unit8):`.

## Requirement

1. Add `run` as an alias for `start`.
2. Let `start`/`run` take a `.yaml`/`.yml` file holding presets for a **single**
   model. `llamastash run qwen3.8.yml` starts that model with its default preset.
   - Multiple models in the file → error.
   - Multiple presets with neither `default:` nor `--preset` → error.
   - A single preset → runs.

Reuse the existing "start a model with a preset" plumbing. DRY and YAGNI: the
launch file is a new *source* of a preset, not a new launch path.

## Problem

`start --preset <name>` only reaches presets already saved in the daemon's
`config.yaml`. There is no way to hand someone a file that says "this model,
these presets, launch it".

A launch file reuses `config.yaml`'s `presets:` schema verbatim — no new preset
types. It writes nothing, touches no `state.json`, restarts nothing. It is
**not** self-contained: the model key resolves against the running daemon's
catalog, exactly like a `start` argument.

## Scope

- `src/cli/cli_args.rs` — `run` alias on `Command::Start`.
- `src/cli/launch_file.rs` — **new**: load + validate + select. No IPC.
- `src/cli/mod.rs` — `pub mod launch_file;`.
- `src/cli/start.rs` — detection, model-resolution branch, preset name in output.
- `src/launch/params.rs` — `launch_params_row` moves here as `LaunchParams::to_wire`.
- `src/ipc/methods.rs` — two call sites + three tests follow that move.
- `docs/usage.md`, `README.md`, `CHANGELOG.md`.

Not in scope, as **decisions** (no `TODO.md` entries — closed, not deferred):
writing launch files (`presets save` still targets `config.yaml`), multi-model
files, TUI integration, a `--file` flag.

## What gets reused

The point of the design. Nothing below is re-implemented:

| Need | Existing thing used |
|---|---|
| Model key → catalog row | `select_start_row` (`src/cli/start.rs:282`), which wraps `resolve_model_with_candidates` |
| Preset schema | `ConfigPresetBlock` / `PresetBody` (`src/config/loader.rs:603`/`:620`) |
| `PresetBody` → `LaunchParams` | `materialize_preset` (`src/launch/presets.rs:132`) |
| `LaunchParams` → `PartialParams` | `LaunchParams::to_wire` + `partial_params_from_preset` (D4) |
| Flag layering, mode resolution, payload, IPC | `handle` from `resolve_mode` down — **untouched** |
| Preset isolation semantics | `selection = "explicit"` (D6) — no daemon change |

Genuinely new: one module of validation, one 3-line adapter, one `if` in `handle`.

## Key decisions

Settled here. Do not re-derive during implementation.

### D1 — the model key uses `start`'s resolver, not `classify_preset_key`

`classify_preset_key` (`src/launch/presets.rs:218`) matches **exactly**:
`row.path == key`, or `row.name().eq_ignore_ascii_case(key)`. `CatalogRow::name()`
(`src/launch/resolve.rs:283`) keeps the file extension, so a key like `qwen3.8`
matches nothing and classifies as `KeyClass::Arch`.

That is right for `config.yaml`, where a key may deliberately scope a whole
architecture. It is wrong here: a launch-file key names **one model to launch**,
and an arch-wide key in a single-model file is meaningless.

So the key goes through `select_start_row` — the same function `start <ref>`
uses. A key is therefore a name substring, an exact name, an exact catalog path,
or an absolute path outside the catalog (which, as on the CLI, then also needs
`--mode`, via `direct_path_candidate`).

`classify_preset_key` is **not** called anywhere in this feature.

### D2 — `auto` is rejected in a launch file

`auto` is the reserved pure-fit sentinel: `--preset auto` (`src/cli/start.rs:46`)
and a block's `default: auto` (`AUTO_DEFAULT`, `src/launch/presets.rs:238`) both
mean "apply no preset at all". A launch file exists to apply one, so both
spellings are a usage error inside one. Neither silently falls through.

### D3 — an unknown knob id is fatal

`KnobSetVisitor::visit_map` (`src/launch/knobs/serde_impl.rs:147`) drops a knob
whose key does not resolve, with a `log::warn!` and nothing else — `KnobSet` has
nowhere to put an orphan, since a `KnobId` borrows a `&'static str` from a
declaration. That warning goes to the log file; it reaches the terminal only
under `--verbose` (`src/util/logging.rs:20`).

Fine for `config.yaml`: the daemon parses it once at start, the log is the
surface you check, and the preset persists where `presets show` can display it.
Not fine for a launch file. It is hand-authored by definition (nothing writes
one), parsed in a one-shot CLI process, and has no inspection surface at all —
you never see its parsed form. `n_gpu_layerz: 99` would launch without the knob,
and the file would read as one configuration while producing another. That
defeats the single thing a launch file is for.

Scoped as tightly as possible: after selection, check the **selected entry's**
`knobs` keys only, against `crate::launch::knobs::resolve_id`. One raw-document
lookup down a path already resolved. Dash and underscore are equivalent
(`registry::normalise`), so `n_gpu_layers` and `n-gpu-layers` both resolve; an
undeclared name does not.

### D4 — reuse the existing round-trip, do not write a parallel converter

`materialize_preset` yields `LaunchParams`; the start path needs `PartialParams`.
The pair that already does exactly this conversion is `launch_params_row`
(`src/ipc/methods.rs:848`) → `partial_params_from_preset` (`src/cli/start.rs:485`),
and `launch_params_row`'s own doc comment says `start --preset` reads it back to
rebuild a preset's launch params. Reuse it rather than restating the mapping.

A hand-written field-for-field converter was the alternative. Rejected: it is a
second definition of the same mapping, and it drifts silently the first time a
field is added to one and not the other — the exact bug `launch_params_row`'s
comments already record twice (dropped `backend:`, disarmed `mtp:`).

`launch_params_row` moves to `LaunchParams::to_wire` in `src/launch/params.rs`
as part of this (Part 3c). It is the wire projection of `LaunchParams`, so it
belongs beside the type — `CatalogRow::to_wire_value` (`src/launch/resolve.rs:102`)
is the same pattern already. Two non-test call sites in `src/ipc/methods.rs`
(`:839`, `:953`) and three tests move with it. This is what keeps the CLI from
reaching into `ipc::methods` for a projection that was never IPC-specific.

### D5 — exit codes

- Model key does not resolve → `MODEL_NOT_FOUND` (66), matching `select_start_row`.
- Everything else (unreadable file, bad YAML, wrong model count, unknown knob,
  bad preset selection) → `USAGE` (64).

### D6 — `selection = "explicit"`

A launch file's preset is a self-contained baseline, identical to
`--preset <name>`. `is_pure_fit` (`src/daemon/launch_service.rs:223`) treats
`Explicit` as pure-fit, so the daemon skips both the `PresetDefault` and
`LastUsed` layers — nothing leaks in from a previous run. No daemon-side change.

## Schema

```yaml
presets:
  Qwen3-8B-Q4_K_M.gguf:      # model key: substring / exact name / path (D1)
    default: fast            # optional; required when >1 entry and no --preset
    entries:
      fast:
        knobs:
          n_gpu_layers: 99
          flash_attn: true
          ctx_size: 32768
        backend: llamacpp
        server: llamacpp-vulkan
        extras: ["--rope-freq-base", "1000000"]
      slow:
        knobs:
          n_gpu_layers: 40
```

- `presets:` is `BTreeMap<String, ConfigPresetBlock>` — the type `Config.presets`
  holds (`src/config/loader.rs:134`).
- `ConfigPresetBlock` = `{ default: Option<String>, entries: BTreeMap<String, PresetBody> }`.
- `PresetBody` = `{ knobs: KnobSet, extras: Option<Vec<String>>, backend: Option<String>,
  server: Option<String> }`.
- `knobs:` keys are registry ids (`n-gpu-layers`, `flash-attn`, `ctx-size`, …);
  underscores accepted. `llamastash knobs` lists them.
- `extras:` is for flags **no backend declares**. A declared knob belongs in
  `knobs:` — in `extras` it bypasses knob layering and can emit twice.
- Top-level keys other than `presets:` are ignored, so a `config.yaml` holding
  exactly one preset key is a valid launch file. The one-model rule is what makes
  a file a launch file; no `deny_unknown_fields` guard is added.

## Detection

Inside `start`'s existing positional `model` arg — no new flag. It is a launch
file when **both** hold:

1. the lowercased extension is `yaml` or `yml`, and
2. `Path::new(value).is_file()`.

Otherwise it is a model reference. A model literally named `foo.yml` that is not
a file on disk still resolves as a model ref.

`start file.yml` and `run file.yml` are the same command. `run` is a clap alias,
so both dispatch to `Command::Start` and the same `handle`; detection reads the
positional value, never which name was typed. Do **not** gate the launch file on
the alias — `run` is shorthand, not a mode.

## Changes

### Part 1: `run` alias

`src/cli/cli_args.rs:187`:

```rust
  /// Start a model.
  #[command(visible_alias = "run")]
  Start(StartArgs),
```

`visible_alias`, not `alias` — a hidden alias never appears in
`llamastash --help`, which defeats a discoverable shorthand.

### Part 2: `src/cli/launch_file.rs` (new)

```rust
//! `llamastash run <file>.yml` — a single-model launch file.
//!
//! The file is a `presets:` map in `config.yaml`'s own shape, narrowed to
//! exactly one model. Validation is stricter than the config loader's on
//! purpose: a hand-authored launch file that silently drops a misspelled knob
//! launches a different configuration than it reads as, with nothing on screen.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::cli::exit_codes::{CliExit, USAGE};
use crate::config::{ConfigPresetBlock, PresetBody};
use crate::launch::presets::AUTO_DEFAULT;

#[derive(Debug, Deserialize)]
struct LaunchFileDoc {
  #[serde(default)]
  presets: BTreeMap<String, ConfigPresetBlock>,
}

/// What a launch file resolved to: the key to hand the catalog resolver, and
/// the one preset to materialize over the row it finds.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchFileSelection {
  pub model_key: String,
  pub preset_name: String,
  pub body: PresetBody,
}

/// `true` when `value` names a launch file: a `.yaml`/`.yml` extension **and**
/// an existing file. A model named `foo.yml` that is not on disk is still a
/// model reference.
pub fn is_launch_file(value: &str) -> bool {
  let path = Path::new(value);
  path
    .extension()
    .and_then(|e| e.to_str())
    .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    && path.is_file()
}

/// Read, validate, and pick the one preset this launch file runs.
///
/// `preset_flag` is `--preset` as typed, so the reserved `auto` is rejected
/// here rather than falling through to pure-fit (D2).
pub fn load(path: &Path, preset_flag: Option<&str>) -> Result<LaunchFileSelection, CliExit> {
  let bad = |e: String| CliExit::new(USAGE, e);
  let text = std::fs::read_to_string(path)
    .map_err(|e| bad(format!("cannot read launch file `{}`: {e}", path.display())))?;
  let raw: yaml_serde::Value = yaml_serde::from_str(&text)
    .map_err(|e| bad(format!("invalid YAML in `{}`: {e}", path.display())))?;
  let doc: LaunchFileDoc = yaml_serde::from_str(&text)
    .map_err(|e| bad(format!("invalid launch file `{}`: {e}", path.display())))?;
  let sel = select_launch(doc.presets, preset_flag)?;
  reject_unknown_knobs(&raw, &sel)?;
  Ok(sel)
}

/// The validation matrix. Split out of [`load`] so it unit-tests without a
/// file on disk.
pub fn select_launch(
  models: BTreeMap<String, ConfigPresetBlock>,
  preset_flag: Option<&str>,
) -> Result<LaunchFileSelection, CliExit> {
  // ... matrix below
}

/// Error on any knob key in the *selected* entry that no backend declares.
///
/// The shared `KnobSet` deserializer drops these with a log line the CLI never
/// shows, so the raw document is consulted for the one entry that launches.
fn reject_unknown_knobs(raw: &yaml_serde::Value, sel: &LaunchFileSelection) -> Result<(), CliExit> {
  let Some(knobs) = raw
    .get("presets")
    .and_then(|v| v.get(&sel.model_key))
    .and_then(|v| v.get("entries"))
    .and_then(|v| v.get(&sel.preset_name))
    .and_then(|v| v.get("knobs"))
    .and_then(|v| v.as_mapping())
  else {
    return Ok(());
  };
  let mut unknown: Vec<String> = knobs
    .iter()
    .filter_map(|(k, _)| k.as_str())
    .filter(|k| crate::launch::knobs::resolve_id(k).is_none())
    .map(str::to_string)
    .collect();
  if unknown.is_empty() {
    return Ok(());
  }
  unknown.sort_unstable();
  Err(CliExit::new(
    USAGE,
    format!(
      "preset `{}` sets knobs no backend declares: {}\nrun `llamastash knobs` for the declared list",
      sel.preset_name,
      unknown.join(", ")
    ),
  ))
}
```

`select_launch` implements exactly this matrix, in this order. The messages are
part of the deliverable — the E2E asserts on them.

| # | Condition | Code | Message |
|---|---|---|---|
| 1 | `models.is_empty()` | 64 | ``launch file has no `presets:` entries; it must name exactly one model`` |
| 2 | `models.len() > 1` | 64 | ``launch file names {n} models ({keys}); it must name exactly one`` |
| 3 | `block.entries.is_empty()` | 64 | ``model `{key}` has no `entries:`; a launch file must define at least one preset`` |
| 4 | `preset_flag == Some("auto")` | 64 | ``a launch file always applies a preset; `--preset auto` means "no preset". Drop the flag or drop the file.`` |
| 5 | `block.default.as_deref() == Some(AUTO_DEFAULT)` | 64 | ``model `{key}` sets `default: auto`; a launch file's `default:` must name a preset ({names})`` |
| 6 | `preset_flag = Some(n)`, `n` not in `entries` | 64 | ``preset `{n}` is not in the launch file; it defines: {names}`` |
| 7 | `block.default = Some(d)`, `d` not in `entries` | 64 | ``model `{key}` sets `default: {d}`, which is not one of its presets: {names}`` |
| 8 | `entries.len() > 1`, no flag, no `default:` | 64 | ``launch file defines {n} presets ({names}) and no `default:`; pass `--preset <name>``` |

`{names}` / `{keys}` are the sorted map keys joined with `, `. Row **7**
deliberately differs from `config.yaml`, where an absent `default:` name is
silently ignored (`EffectivePresets`); in a launch file it is an error.

Selection once the matrix passes: `--preset` > `default:` > the single entry.

### Part 3: wire into `src/cli/start.rs`

**3a. Let `select_start_row` take the reference.** It currently reads
`args.model` and `.expect()`s it (`src/cli/start.rs:282-286`). The launch file
needs the same resolution for a different string:

```rust
fn select_start_row(
  rows: &[CatalogRow],
  args: &StartArgs,
  model: &str,
) -> Result<CatalogRow, CliExit> {
  match resolve_model_with_candidates(rows, model) {
    // ... body unchanged
```

Delete the `let model = args.model...expect(...)` lines. Update the two existing
tests (`select_start_row_falls_back_to_direct_path_when_catalog_misses`,
`select_start_row_prefers_catalog_match_over_direct_path_fallback`) to pass the
path as the third argument.

**3b. Branch at the top of `handle`.** Replace lines 33-60:

```rust
pub async fn handle(args: StartArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let rows = fetch_catalog(&mut client).await?;

  // A launch file supplies both the model reference and the preset, so it
  // replaces the `--preset` → `presets_show` round trip rather than adding a
  // path beside it.
  let from_file = match args.model.as_deref() {
    Some(m) if crate::cli::launch_file::is_launch_file(m) => Some(
      crate::cli::launch_file::load(std::path::Path::new(m), args.preset.as_deref())?,
    ),
    _ => None,
  };

  let row = match (&from_file, args.model.as_deref()) {
    (Some(sel), _) => select_start_row(&rows, &args, &sel.model_key)?,
    (None, Some(m)) => select_start_row(&rows, &args, m)?,
    (None, None) => crate::cli::picker::pick_catalog_row(&rows, args.json).await?,
  };

  // The preset that actually applied. A launch file's `default:` picks one
  // without `--preset` ever being typed, and the success line must say so.
  let applied_preset: Option<String> = match &from_file {
    Some(sel) => Some(sel.preset_name.clone()),
    None => args.preset.clone(),
  };

  let preset_is_auto = args.preset.as_deref() == Some(crate::launch::presets::AUTO_DEFAULT);
  let selection = match (&from_file, args.preset.as_deref()) {
    // A launch file's preset is a self-contained baseline (D6).
    (Some(_), _) => "explicit",
    (None, Some(p)) if p == crate::launch::presets::AUTO_DEFAULT => "auto",
    (None, Some(_)) => "explicit",
    (None, None) => "default",
  };

  let mut params = match (&from_file, args.preset.as_ref()) {
    (Some(sel), _) => partial_params_from_launch(
      &crate::launch::presets::materialize_preset(
        &sel.preset_name,
        &sel.body,
        std::path::PathBuf::from(&row.path),
      )
      .params,
    ),
    (None, Some(name)) if !preset_is_auto => {
      fetch_preset_params(&mut client, &row.path, name).await?
    }
    _ => PartialParams::default(),
  };
```

Everything from `resolve_mode` (line 65) down is **unchanged** — `--ctx`,
`--port`, `--reasoning`, `--mtp*`, the knob overlay, `build_payload`,
`start_model`. CLI flags layer over the file's preset exactly as they layer over
`--preset` today.

**3c. Move the wire projection, then adapt** (D4).

First move `launch_params_row` (`src/ipc/methods.rs:848`) to `src/launch/params.rs`
as an inherent method, body unchanged:

```rust
impl LaunchParams {
  /// This launch's params in the wire shape `presets_show` and
  /// `last_params_list` publish — and that `start` reads back to rebuild a
  /// preset's params. The one definition of that projection, kept beside the
  /// type it projects (as `CatalogRow::to_wire_value` is).
  pub(crate) fn to_wire(&self) -> Value { /* body of launch_params_row */ }
}
```

Update the two call sites in `src/ipc/methods.rs` (`:839`, `:953`) to
`p.params.to_wire()` / `entry.params.to_wire()`, and move the three
`launch_params_row_*` tests across with it.

Then the adapter, beside `partial_params_from_preset`:

```rust
/// A materialized preset's params, taken through the same projection a
/// `presets_show` baseline takes.
///
/// Routed through the wire shape on purpose: it is the one definition of
/// `LaunchParams` → `PartialParams`, so a launch file and `--preset <name>`
/// cannot resolve the same preset differently.
fn partial_params_from_launch(p: &crate::launch::params::LaunchParams) -> PartialParams {
  partial_params_from_preset(&json!({ "params": p.to_wire() }))
}
```

`PartialParams.mode` of `Some("chat")` is not a pin — `resolve_mode` filters it
(`src/cli/start.rs:437`) — so a preset with no `mode:` still lets the catalog
hint decide. Nothing to change there.

**3d. Report the preset that applied.** At the two call sites, swap
`args.preset.as_deref()` for `applied_preset.as_deref()`:

```rust
  if args.wait {
    return wait_and_emit(&mut client, applied_preset.as_deref(), &row, &resp, args.json, cli.quiet).await;
  }
  emit_response(applied_preset.as_deref(), &row, &resp, args.json, cli.quiet);
```

Without this, `run file.yml` prints `started <model>` with no preset and emits
`"preset": null` for a launch that used one. `--json`'s shape is otherwise
unchanged — no new keys.

### Part 4: tests

**Unit, `src/cli/launch_file.rs`** — one per matrix row, plus:

- `is_launch_file_needs_both_extension_and_an_existing_file` — `.yml` on disk yes;
  `.yml` absent no; `.gguf` on disk no; `.YAML` on disk yes.
- `a_single_entry_is_selected_without_a_default`
- `the_preset_flag_outranks_the_files_default`
- `an_undeclared_knob_id_is_fatal` — via `load` on a tempfile with
  `n_gpu_layerz: 99`; the message names the key. **The D3 regression test.**
- `an_undeclared_knob_in_an_unselected_entry_is_ignored` — pins the scope of D3.
- `underscore_and_dash_knob_spellings_both_resolve`

**Unit, `src/cli/start.rs`**:

- `partial_params_from_launch_carries_every_preset_field` — a `PresetBody` setting
  knobs, extras, backend, server, ctx, mode and mtp; `materialize_preset` it;
  assert every `PartialParams` field survives. The field-drop guard, mirroring the
  existing `partial_params_from_preset` sibling test.

**E2E, `tests/`** (`--features test-fixtures`; one `unique_temp_dir` per test,
never a shared `state_dir`):

- `run_alias_starts_a_model` — `run` and `start` produce the same launch.
- `start_accepts_a_launch_file_like_run` — `start file.yml` and `run file.yml`
  emit the same `--json` body. Pins that detection is not alias-gated.
- `run_launch_file_uses_the_files_default_preset` — `--json` reports `"preset": "fast"`.
- `run_launch_file_preset_flag_selects_from_the_file` — `--preset slow` works
  though no such preset exists daemon-side.
- `run_launch_file_cli_flags_layer_over_the_files_preset` — `--ctx 4096` wins
  while the file's other knobs survive (`KnobSet::overlay` is per-key). Also
  assert the existing extras asymmetry holds for a launch file: an undeclared
  flag after `--` **replaces** the file's whole `extras:` list rather than
  merging into it, exactly as it does for `--preset <name>`.
- `run_launch_file_with_an_unresolvable_model_key_exits_66` (D5).
- `run_launch_file_with_two_models_exits_64`.

### Part 5: docs

- **`docs/usage.md`**, `### llamastash start <model-ref>` (line 328): retitle to
  `### llamastash start <model-ref> | llamastash run <model-ref>`, note the alias
  in the opening line, and add `#### Launch files` after
  `#### Auto launch mode (default)` — schema block, detection rule, the `auto`
  rejection (D2), the strict-knob rule (D3), the exit codes (D5).
- **`README.md`** Quickstart (~line 89): one `llamastash run qwen.yml` line.
- **`CHANGELOG.md`**, `[Unreleased]` → `### Added`, one bullet:
  ``- `llamastash run` — alias for `start`, and a launch file: `run model.yml` starts the file's one model with its own preset, without saving anything to `config.yaml`.``

## Rejected alternatives

Both are the obvious first ideas. Recorded so they are not re-proposed.

### Inject the file's presets into the daemon's in-memory preset store

"Load the file's `presets:` into the live `ConfigPresetStore` (no write-back),
then run the normal `--preset <name>` path." It would delete the adapter in
Part 3c. It is **more** work and **less** DRY, on four counts:

1. **New wire surface.** `ConfigPresetStore::save` (`src/daemon/preset_store.rs:59`)
   always writes through when `config_path` is `Some`, which the real daemon
   always has. Injection needs a memory-only store method *and* a new
   `presets_*` IPC verb to reach it — more new code than the ~15 lines it
   removes, on the contract surface CLAUDE.md pins as stable.
2. **Global mutation with no defined lifetime.** The store is one
   `Arc<Mutex<..>>` for the whole daemon. After injection, `presets list` in
   another terminal shows a preset that exists in no file, the TUI's `Ctrl+P`
   cycle offers it, and `effective_presets` hands the file's `default:` to any
   no-selection launch — including **proxy auto-start**
   (`src/daemon/launch_service.rs:354`). Nothing removes it; a scope or TTL
   would have to be built.
3. **It breaks a documented scope boundary.** "Presets live in `config.yaml`,
   not `state.json` — that is the writable source of truth." An in-memory
   shadow layer backed by no file is the second source of truth that boundary
   exists to prevent.
4. **It makes a read into a write.** `run file.yml` mutates shared daemon state
   so it can read it straight back.

The reuse it promises is already had: D4 routes through the same
`LaunchParams` → `PartialParams` pair the `--preset` path uses. The only thing
injection additionally avoids is the `presets_show` round trip — a network call
fetching something the CLI is already holding. Skipping that is the point.

### Hand-write a `LaunchParams` → `PartialParams` converter

See D4. A second definition of one mapping, which drifts the first time a field
lands on one side only.

## Verification

Isolated from the real daemon per `CLAUDE.md` — never point this at the default
state dir.

```bash
make lint
make test                                   # cargo test --features test-fixtures

export LLAMASTASH_STATE_DIR=$(mktemp -d)
export LLAMASTASH_CONFIG_DIR=$(mktemp -d)
cargo build --bin llamastash
target/debug/llamastash daemon start --proxy-port 21435

target/debug/llamastash --help | grep -i run        # `run` visible as an alias
target/debug/llamastash run --help                  # identical to `start --help`

# the alias is shorthand, not a mode: both must launch identically
target/debug/llamastash run   /tmp/ls-launch.yml --json | jq .preset
target/debug/llamastash start /tmp/ls-launch.yml --json | jq .preset

# /tmp/ls-launch.yml naming a real discovered model, presets fast + slow, default: fast
target/debug/llamastash run /tmp/ls-launch.yml --json | jq .preset      # "fast"
target/debug/llamastash run /tmp/ls-launch.yml --preset slow --json | jq .preset
target/debug/llamastash run /tmp/ls-launch.yml --ctx 4096 --json | jq .

# each must print its matrix message and exit with the listed code:
target/debug/llamastash run /tmp/two-models.yml       ; echo "want 64, got $?"
target/debug/llamastash run /tmp/multi-no-default.yml ; echo "want 64, got $?"
target/debug/llamastash run /tmp/ls-launch.yml --preset absent ; echo "want 64, got $?"
target/debug/llamastash run /tmp/ls-launch.yml --preset auto   ; echo "want 64, got $?"
target/debug/llamastash run /tmp/bad-knob.yml         ; echo "want 64, got $?"
target/debug/llamastash run /tmp/no-such-model.yml    ; echo "want 66, got $?"

target/debug/llamastash daemon stop
```
