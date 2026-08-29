# Unified knob registry — every backend declares its own knobs, every surface is generated

**Status:** ✅ done (2026-08-29) — all eight stages landed on `feat/knob-parity` (PR #72). 52 declarations across 4 backends; CLI flags, TUI rows and preset keys all generated from them. E2E-verified against a real `llama-server` (build 10656, commit 732707dff) in an isolated daemon: a CLI launch, a preset launch and a TUI-cycled row produce byte-identical engine argv, read from `/proc/<pid>/cmdline`. Independent review at [`docs/reviews/pr-72.md`](../reviews/pr-72.md); its findings are addressed below.

Supersedes the two-channel knob model (`TypedKnobs` IR + `native_knobs`). Origin: a
CLI/TUI/preset parity audit (2026-08-25) that found seven parity gaps, all of them on
settings that sit *outside* the typed-knob path, and none on settings inside it.

## Why

The audit result, in one table. "On the knob path" means a `KnobSpec` row in
`src/launch/flag_aliases.rs`.

| | On the knob path | Off it |
| --- | --- | --- |
| Settings | 19 typed knobs | mode, backend, server, port, mtp, mtp_draft_n, 26 native knobs |
| Parity gaps | **0** | **7 of 7** |

That correlation is structural, not accidental. A knob on the path gets a CLI flag
(`cli/knob_flags.rs` generates it), a TUI row (`knob_display_groups` renders it) and
preset persistence (`#[serde(flatten)]`) **without anyone wiring it**. Everything off the
path was hand-wired onto three surfaces independently, and every single one landed on
one or two.

The sharpest evidence is `mtp_draft_n`. Plan `2026-07-14-001` KD2 called it the easy
case — "a plain `u32 → --spec-draft-n-max`, fits the mechanical ~6-edit knob path". It
shipped as a bespoke `LaunchParams` sibling anyway. Result: CLI flag yes, TUI row no,
preset-save no. The plan's own trivial case still leaked.

Two more costs the audit surfaced:

- **The IR is 1/19 useful for 3 of 4 backends.** ds4, vLLM and Lemonade each honour
  exactly `Ctx` (`capabilities_honor_only_ctx`, `capabilities_cover_exactly_ctx`). Their
  real tuning surface — 17 and 9 knobs — is 100% outside the IR, in a channel with no
  layering, no arch defaults, no source chips and **no CLI surface at all**.
- **Shared concepts got modelled twice.** vLLM `max_num_seqs` ↔ llama.cpp `--parallel`;
  vLLM `kv_cache_dtype` ↔ `--cache-type-k/v`; ds4 `threads` ↔ `--threads`. That last one
  is already a live wart: ds4 declares a native `threads` knob *and* reports `Threads`
  unsupported, so `start --threads 8` silently vanishes on a ds4 launch.

The IR keyed itself to llama.cpp's *spelling* rather than the *concept*. This plan keeps
concrete flag spellings (users know them) but stops making one backend's vocabulary the
universal one.

## Goal

One registry. Each backend declares its own knobs, naming them with its own flags. The
CLI, TUI and preset surfaces are all **generated** from those declarations, so a knob
cannot exist without reaching all three. `backend_knobs` disappears as a concept —
ds4's `--ssd-streaming` and llama.cpp's `--threads` are the same kind of thing.

Non-goal: typing every flag every engine accepts. `extras` stays the free-form tail.

## Decisions

| # | Decision |
| --- | --- |
| D1 | **`id` is the flag spelling, dashless-prefix** (`n-gpu-layers`, `ssd-streaming`). `flag` is a separate field so a collision can be broken (ds4's `--mtp` sidecar *path* vs the neutral `mtp` *enable* → `id: "mtp-model", flag: "--mtp"`). |
| D2 | **`concept` tags the ~8 genuinely shared ideas** so values survive a backend switch and get a stable neutral CLI alias. Everything else is honestly backend-local. |
| D3 | **`capabilities()` is deleted.** "Doesn't declare it" *is* "doesn't support it". |
| D4 | **`auto` is per-knob policy**, not a hardcoded `--fit` meaning: `None` \| `Delegate` \| `Capability`. This is the fix for KD2 — MTP's tri-state was bespoke only because `KnobValue::Auto` was overloaded with a llama.cpp-ism. |
| D5 | **`mode`, `mtp`, `mtp_draft_n` become declared knobs.** Their three parity gaps close as a side effect. |
| D6 | **backend / server / port / extras stay launch *identity*** — they can't be backend-declared because they choose the backend. Fixed list of four, each declaring its surface set, covered by a drift test. |
| D7 | **Port stays exactly as today** — `start --port` (strict) only, no preset key, no TUI row. A deliberate scope exemption, not a parity gap; see below. |
| D8 | **Knob ids normalize `_` ↔ `-` on read**, and unknown keys warn. Hand-authored configs survive the rename; users gain a warning they don't get today. |
| D9 | **Byte-identical composed argv** is the invariant carried through every stage. `scripts/bench/` already depends on it. |
| D10 | **Config is migrated in place to the new shape**, once, with a backup. Not a permanent read-both path and not a manual chore; see below. |

### D10 rationale — migrate, don't shim

Users cannot hand-edit their way out of a shape change, and a compatibility
reader that gets deleted later is a delayed break rather than a migration. So
the daemon **rewrites `config.yaml` into the new shape on first load** and says
what it did.

What moves, per preset entry and per `arch_defaults` block:

- entry-level knob keys gain the `knobs:` wrapper;
- `backend_knobs:` contents move up into that same map;
- the `mode:` / `mtp:` / `mtp_draft_n:` siblings move in with them;
- `_` spellings become `-` (both keep loading regardless, per D8).

Safety, in order of importance:

1. **Comments survive.** The rewrite goes through `config::yaml_edit`, which
   exists precisely so app-driven writes preserve hand-authored comments. Real
   configs carry load-bearing prose — measured tuning results, "why this knob
   is pinned off" notes — and losing that would be worse than the break.
2. **The original is kept.** `config.yaml.pre-knobs.bak` is written first; the
   migration aborts if it cannot.
3. **It is announced.** A daemon-start log line names the file, the backup, and
   the entry count, so a surprised user can see what happened and revert.
4. **It is idempotent.** A config already in the new shape is left untouched,
   so a rollback to an older binary and back does not double-migrate.

The old-shape reader still exists — it is what the migration reads *from* — but
it feeds the rewrite rather than standing as a parallel path forever.

## Model

```rust
pub struct KnobDef {
  pub id:       &'static str,          // stable persistence / wire key
  pub flag:     &'static str,          // exact backend spelling to emit
  pub concept:  Option<Concept>,       // cross-backend carry + neutral CLI alias
  pub kind:     KnobKind,              // U32{range} | I64 | F32{range} | Bool | Enum{choices} | Str | Path
  pub auto:     Option<AutoKind>,      // Delegate | Capability; None = no Auto state
  pub group:    Group,                 // TUI grouping + CLI --help heading
  pub label:    &'static str,          // TUI row label
  pub help:     &'static str,          // CLI --help + TUI description
  pub aliases:  &'static [&'static str],
  pub emit:     Emit,                  // FlagValue | BareFlagWhenTrue | EnumFlags | Custom
}
```

Storage replaces `knobs: TypedKnobs` + `backend_knobs: BTreeMap<..>` with one map:

```rust
pub struct KnobSet(BTreeMap<KnobId, KnobValue>);
pub enum  KnobValue { Set(Scalar), Auto }     // absence = Inherited (unchanged tri-state)
pub enum  Scalar    { U32(u32), I64(i64), F32(f32), Bool(bool), Str(String) }
```

Declaration site — the only place a backend's tunables are named:

```rust
// src/backend/ds4/knobs.rs
pub const KNOBS: &[KnobDef] = &[
  KnobDef {
    id: "ctx", flag: "--ctx",
    concept: Some(Concept::ContextLength),
    kind: KnobKind::U32 { max: MAX_CTX_TOKENS },
    auto: Some(AutoKind::Delegate),
    group: Group::Context, label: "Context",
    help: "context window in tokens",
    aliases: &["-c"], emit: Emit::FlagValue,
  },
  KnobDef {
    id: "ssd-streaming", flag: "--ssd-streaming",
    concept: None,
    kind: KnobKind::Bool, auto: None,
    group: Group::Memory, label: "SSD streaming",
    help: "stream weights from disk (below-RAM-floor mode)",
    aliases: &[], emit: Emit::BareFlagWhenTrue,
  },
  // …
];
```

### Concepts (closed set, ~8)

`ContextLength`, `Threads`, `Device`, `KvCacheKType`, `KvCacheVType`, `MaxConcurrency`,
`FlashAttn`, `Mode`. A concept buys three things:

- **values survive a backend switch** — replaces today's blunt D-contamination gate,
  which throws away the *entire* `last_params` on a cross-backend launch. Only `extras`
  still needs that gate (free-form, genuinely unportable);
- **a stable CLI alias** — `--ctx` works on any backend, `--ctx-size` /
  `--max-model-len` are the exact per-backend spellings;
- **the ds4 `threads` wart resolves itself** — both tag `Concept::Threads`.

## Surfaces (all generated)

| Surface | Generated from | Notes |
| --- | --- | --- |
| `start` flags | registry union, grouped by declaring backend | ~46 flags; `--help` headings per backend |
| `presets save` flags | *the same* `KnobFlags` struct | parity by construction, not by discipline |
| TUI rows | resolved backend's `knobs()`, grouped by `Group` | `PickerField::Knob(KnobId)` — the `Knob`/`NativeKnob` duality disappears |
| `config.yaml` | `knobs:` sub-map | keys validated against the registry, unknown keys warn |
| `--json` | flat `knobs` map | `backend_knobs` gone |
| discovery | `llamastash knobs [--backend <id>] [--json]` | agent-readable descriptor list |

```
Launch params (llamacpp):
      --n-gpu-layers <N>        layers offloaded to the GPU
      --ctx-size <N|auto>       context window  [alias: --ctx]
Launch params (ds4):
      --ssd-streaming [<BOOL>]  stream weights from disk
      --power <1-100>           GPU duty-cycle target
```

## Target parity

| Setting | start CLI | TUI picker | presets save CLI | preset in config.yaml | inherited on plain relaunch |
| --- | --- | --- | --- | --- | --- |
| ctx, reasoning, extras | ✅ gen | ✅ gen | ✅ gen | ✅ `knobs:` | ✅ layered |
| 17 llama.cpp tuning knobs | ✅ gen | ✅ gen | ✅ gen | ✅ `knobs:` | ✅ layered |
| mode | ✅ gen | ✅ gen | ✅ gen | ✅ `knobs:` | ✅ default preset¹ |
| backend | ✅ `--backend` | ✅ Server row | ✅ `--backend` | ✅ `backend:` | ✅ preset→last_params |
| server | ✅ `--server` | ✅ Server row | ✅ `--server` | ✅ `server:` | ✅ preset→last_params |
| **26 backend-native knobs** | ✅ gen | ✅ gen | ✅ gen | ✅ `knobs:` | ✅ layered |
| mtp | ✅ gen | ✅ gen | ✅ gen | ✅ `knobs:` | ✅ layered |
| mtp_draft_n | ✅ gen | ✅ gen | ✅ gen | ✅ `knobs:` | ✅ layered |
| port | ✅ `--port` (strict) | — exempt (D7) | — exempt (D7) | — exempt (D7) | internal `prefer_port` |

¹ From the model's `default:` preset only, never from `last_params`. A one-off
`--mode embedding` must not be remembered: `--embeddings` makes llama-server refuse
`/v1/chat/completions` for the supervisor's whole life, so a remembered mode would let
one embedding request lock a chat model out of chat. The proxy's `resolve_auto_start_mode`
already guards this on its own path; the daemon rung matches it.

## Breaking changes

Pre-0.0.1, so `state.json` and the `config.yaml` shapes are still free (AGENTS.md: no
backwards-compat until first release). This is the moment; after 0.0.1 it is a migration.

| What | Severity as-is | Mitigation |
| --- | --- | --- |
| `config.yaml` preset + `arch_defaults` knob keys | **Silent value loss** — `PresetBody.knobs` is `#[serde(flatten)]`, which cannot carry `deny_unknown_fields`, so unknown keys drop with no error *today* | D8: `_`↔`-` normalization + registry-validated keys that **warn**. Net improvement over the status quo |
| `backend_knobs:` / `mode:` / `mtp:` move into `knobs:` | Would break every configured user | Carried by the same auto-migration; nothing for a user to do |
| **`--json` params shape** | Scripts pinning field paths break | **none — this is the one real break** |
| `state.json` | Whole-file parse failure → warn, quarantine, boot on defaults. Collateral: **favorites lost too** | Lenient `last_params` deserialize (drop bad rows, keep the rest). Worth doing regardless — latent bug today |
| CLI flags | Additive only; existing flags are already llama.cpp spellings | Registry rule: no knob may claim a flag colliding with a llamastash global or `FORBIDDEN_ADVANCED_PREFIXES` |
| TUI | Visible, not breaking; keybindings unchanged | — |

## Stages

Each compiles and keeps tests green. D9 (byte-identical argv) is asserted at every stage.

- [x] **0 — Registry + value model.** New `src/launch/knobs/`; `Backend::knobs()`; all
  four backends transcribe their existing tables. Nothing consumes it yet. Validation
  tests: no id collides with a conflicting kind; every concept maps to ≤1 knob per
  backend; no flag hits a llamastash global or the denylist.
- [x] **1 — Resolver on `KnobSet`.** Port `resolve_layered` / `seed_layerless`; add
  concept carry-over. `TypedKnobs` alive behind a bridge. Test: identical output to the
  old resolver over llama.cpp's set.
- [x] **2 — Argv emission** from `KnobDef.flag` + `Emit`. Deletes `argvify` and
  `native_knobs::translate`; one denylist guard instead of two. *Ordered before the
  persistence flip so byte-identical argv is proven while the old shape still loads.*
- [x] **3 — Persistence flip.** `LaunchParams.knobs: KnobSet` replaces `knobs` +
  `backend_knobs`; `PresetBody { knobs, extras, backend, server }` (no `port` — D7).
  Lenient `last_params` deserialize. The D10 config migration lands here. Bridge deleted.
- [x] **4 — CLI generated** from the registry union, grouped by backend. Concept
  aliases. `presets save` flattens the identical flag set. Adds `llamastash knobs`.
  Adds `presets save --from-last`.
- [x] **5 — TUI generated.** `PickerField::Knob(KnobId)`. Preset cycle and `Ctrl+P`
  carry the whole `KnobSet` + identity fields. No port row (D7).
- [x] **6 — Identity parity.** backend/server inheritance chain in the daemon
  (default preset → last_params, matching how extras/mtp already work); unknown
  preset-sourced server warns and falls back, typed `--server` stays a hard error.
- [x] **7 — Drift tests + docs.** Includes rewriting `config.example.yaml` to the
  new shape. The D10 migration **stays** — it is what upgrades a user, not a
  temporary scaffold.

### Enforcement (stage 7)

Four tests are the reason this can't rot:

1. `every_declared_knob_reaches_every_surface` — for each `KnobDef`: a CLI flag exists
   and round-trips, a TUI row is produced, it serialises into a preset and back.
2. `every_identity_field_reaches_its_declared_surfaces` — the D6 list of four, each
   with an explicit surface set. `backend` / `server` / `extras` declare
   CLI+TUI+preset; `port` declares **CLI only** (D7). The exemption lives in the test,
   so dropping a surface silently is still a failure and the one uneven row is a
   recorded decision rather than an oversight.
3. `registry_is_valid` — the stage-0 validation rules.
4. `composed_argv_matches_golden` — D9.

Docs to update in the same commits: `docs/architecture.md` (knob-registry section
replacing "Backend neutrality contract"'s IR half), `docs/usage.md`, `config.example.yaml`,
`AGENTS.md` (the backend no-leak rule gains "declare your knobs in `<id>/knobs.rs`"),
`src/launch/AGENTS.md`, `CHANGELOG.md`, `TODO.md`.

## Out of scope

- Migration code for `state.json` / `config.yaml` (pre-1.0; breaking changes ship clean).
- Typing every flag every engine accepts — `extras` stays the escape hatch.
- Per-knob arch defaults for native knobs beyond what the existing `arch_defaults` block
  already expresses.

## Review follow-ups (pr-72, 2026-08-29)

The independent review is [`docs/reviews/pr-72.md`](../reviews/pr-72.md). Its verdict held
— the byte-identical-argv claim was reproduced on distinct pids — and every finding is
resolved here.

- [x] **1 (must-fix) — a preset's pinned `mode` never reached a launch.** Storage was
  fixed by D5; the launch half was not, on all three surfaces. `start` resolved the mode
  from `--mode` or the catalog hint before it had even fetched the preset; the daemon
  took mode only off the wire, so a `default:` preset's mode was equally dead on proxy
  auto-start; the TUI submitted the model's hint and ignored its own Mode row.

  Fixing the three surfaces the review named was not enough, and the live E2E is what
  caught it: **every caller sent the catalog's mode hint on the wire unconditionally**,
  so `parsed.mode` was always `Some(..)` and the daemon's new preset rung was shadowed on
  every launch anyway. The hint is the *model's* default and sits below the user's config
  in the documented precedence; collapsing "the user chose" and "the model implies" into
  one wire field was the actual defect. So the resolution moved to one place:

  - `ResolvedIdentity` carries `mode_hint` (free — that header read already happened),
    and `launch_service` resolves `explicit > default-preset pin > hint > chat`. The
    preset rung sits beside the extras / mtp inheritance it mirrors, which meant hoisting
    `effective_default` above the mode decision (mode also picks the backend, so it has
    to settle first).
  - `start` and the proxy now send a mode only when something genuinely chose one. The
    CLI keeps its "unknown hint, pass `--mode`" refusal: the fix is a flag the user
    types, so the message belongs on the surface they typed at.
  - The TUI projects its own Mode row through `LaunchPickerState::mode_intent`, the same
    shape `mtp_intent` already had.

  The proxy's "an embeddings request must not lock a chat model out of chat" guard is
  unchanged in behaviour — a *request* still cannot raise a chat-hinted model; only the
  user's own config can, which is the point.

  Regression tests: seven in `cli/start.rs`, one on the picker, two on the proxy
  resolver, one daemon-level (`no_selection_start_applies_the_default_presets_pinned_mode`),
  and E2E case 14 (four sub-cases against the real engine).
- [x] **2 — the migration eats comments inside a preset entry body.** Documented as a
  boundary rather than fixed: `rewrite_entry` regenerates the body from the folded value,
  and the keys it renames leave an in-body comment nothing to anchor to. `upsert_block`
  already documents the analogous loss. Pinned by
  `a_comment_inside_a_migrated_entry_is_lost_but_the_backup_keeps_it`, which also asserts
  the `.pre-knobs.bak` still carries it, so the loss is recoverable rather than silent.
- [x] **3 — `parse_value` accepted `NaN` for float knobs.** `NaN` compares false against
  both bounds, so it walked the range check and shipped verbatim; an unbounded knob took
  `inf` the same way. Both kinds now require `is_finite`, `Ratio` included (`NaN,1` was a
  valid tensor split).
- [x] **Minor — no same-backend flag-collision check.** `registry::validate` now rejects
  two self-emitting knobs of one backend claiming one flag spelling. `Emit::Custom` is
  excluded: its backend consumes the value itself, so its derived flag is a name nothing
  writes — which is exactly the real ds4 `mtp` / `mtp-model` pair.
- [x] **Minor — a direct-hit `Auto` was not re-validated.** The layer loop stored `Auto`
  from a direct id hit without asking whether the destination declares an auto state.
  It now applies the same rule `carry_over` does and falls through to the next layer.
- [x] **Minor — a kind-mismatched carry-over vanished silently.** A value `parse_value`
  could not convert into the destination kind was neither set nor listed in `dropped`.
  `carried` now requires the destination concept slot to actually hold a value.
- [x] **Minor — legacy config reads before the first daemon start.** Kept as-is and
  documented in `usage.md`: it is transitional, self-heals on the first `daemon start`
  (which is what runs the migration), and a compatibility reader is the shim D10 exists
  to avoid.
- [ ] **Minor — CRLF configs go mixed-line-ending after one edit.** Cosmetic, not
  addressed. `yaml_edit` renders `\n` replacement lines into a file whose other lines end
  `\r\n`. Still valid YAML; the cost is diff noise on a Windows hand-edited config.
  → `TODO.md`.
