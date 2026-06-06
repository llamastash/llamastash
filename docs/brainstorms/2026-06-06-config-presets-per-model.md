# Config presets per model — brainstorm

**Status:** Brainstorm — not yet scoped as an implementation plan.

**Origin:** User request — same GGUF file, different context lengths (and other knobs). Today the user saves a preset in `state.json` per model, but the UX is:
- Presets live in `state.json`, not the config file
- No default preset concept
- No config-file authoring of presets (only TUI/CLI creation)
- No agent-facing surface for presets (Pi can't see them)

## Problem statement

A user has `Qwen3.6-27B-Q4_K_M.gguf`. They want three pre-configs:

- `short-ctx` — `--ctx 8192`
- `long-ctx` — `--ctx 65536`
- `coding` — `--ctx 16384 --flash-attn --keep 0`

The current system lets them do this with **named presets** (saved per model in `state.json`), but the UX friction is:

- **No TUI UI** — named presets have zero TUI surface. You can only manage them via CLI (`llamastash presets list/save/delete/show`). The TUI only pre-fills a preset value if one exists in `state.json`; it doesn't let you create, edit, or delete them.
- **No default** — every launch needs manual selection. No way to say "always use `coding` for this model."
- **Not discoverable** — you don't know what presets exist without running `llamastash presets list`. The TUI doesn't show them.
- **Not in config file** — presets live in `state.json` (ephemeral, lost if deleted, can't be version-controlled, can't be shared across machines).
- **Not agent-facing** — Pi/Claude Code can't discover presets; `status` doesn't expose them.

## What exists today

The *capability* to save a group-of-settings already exists:

- **CLI named presets** — `llamastash presets save <model-path> --preset-name "coding" --ctx 16384 --flash-attn on`. Saved in `state.json` under `presets: HashMap<ModelId, Vec<NamedPreset>>`.
- **CLI preset usage** — `llamastash start <model-path> --preset coding`.
- **CLI preset CRUD** — `llamastash presets list/save/show/delete <model-path> <name>`.
- **IPC methods** — `presets_list`, `presets_save`, `presets_delete`, `presets_show` are all wired in the daemon.
- **TUI limited pre-fill** — the launch picker (Enter on a model) will pre-fill a saved preset's ctx value if one exists. But no TUI UI to create/edit/delete.
- **arch_defaults** — per-architecture defaults in `config.yaml` that apply to all models of that architecture. Not per-model, not named.
- **last_params** — auto-persisted after every successful launch. Not curated, not named.

**The brainstorm's TUI work is surfacing existing capability, not building new capability.** The named-preset concept, the `TypedKnobs` partial format, the `--preset` CLI flag, the IPC methods — all already exist. The brainstorm's TUI changes are purely UX: show the dropdown, add the CRUD modal, wire up the selectors.

**The brainstorm's NEW capability is:** moving presets from `state.json` → `config.yaml`, adding the default concept, and exposing presets to the agent surface. These don't exist today.

## Design goals

### Surfacing (existing capability → TUI)

1. **TUI selector.** When opening the launch picker, show a dropdown of presets for the selected model, with the default highlighted.
2. **TUI CRUD.** Allow creating / editing / deleting presets from the TUI, writing back to the config file.
3. **TUI discoverability.** Show preset count and names in the TUI (e.g., "3 presets saved" on the model card).

### New capability

4. **Config-file authoring.** Presets should live in the config file alongside `arch_defaults`, not in `state.json`. They're user-authored, not user-mutated — if the user wants to change them, they edit the config file.
5. **Default per model.** One preset per model is the "default" — used when launching without `--preset`.
6. **Agent-facing.** The `status` method exposes per-model presets so Pi/Claude Code can discover them. `llamastash start <model> --preset <name>` picks one.
7. **Backward compat.** Today's `state.json` presets survive — they're imported once on boot if `config.yaml` has no preset block for that model, then ignored.

## Data model

### Config file addition

```yaml
# ─── Presets (optional) ───────────────────────────────────────────
#
# Per-model launch presets. Keys are the canonical model id (same
# format as `arch_defaults` — the GGUF's `general.architecture`
# string, not a file path).
#
# `default` is the preset used when launching without `--preset`.
#
# Sources — CLI: (none) · Env: (none). Config-only.
presets:
  qwen2:
    default: long-ctx
    entries:
      - name: short-ctx
        ctx: 8192
      - name: long-ctx
        ctx: 65536
        flash_attn: true
      - name: coding
        ctx: 16384
        keep: 0
```

### Why model id, not file path?

- `state.json` already uses `ModelId` (canonical path + BLAKE3 header hash) as the key
- File paths can change (renames, moves, symlinks), but the model id is stable
- `arch_defaults` already uses the same key (GGUF `general.architecture` string)
- The config file is author-edited, not auto-generated — use the stable identifier

### TypedKnobs subset in presets

Presets don't carry the full `LaunchParams` — they carry a `TypedKnobs` partial. The user sets `name` + the knobs they want to override. At launch time the resolver chain is:

```
preset → last_params → arch_defaults → built-in table → llama-server default
```

This keeps presets small (user only writes what they want to change) and composable (a preset for `--ctx` + `--flash-attn` doesn't clobber `--threads` from `arch_defaults`).

## TUI UX

### Launch picker

When the user opens the launch picker (Enter on a model):

```
┌── Launch ────────────────────────────────────────────┐
│                                                        │
│  Preset: [ long-ctx ▼ ]     (default shown first)     │
│                                                        │
│  Context: 65536         ← updates when preset changes │
│  GPU layers: 99                                       │
│  Reasoning: Off                                       │
│                                                        │
│  [ Cancel ] [ Launch ]                                │
└────────────────────────────────────────────────────────┘
```

- The preset selector is a dropdown, sorted alphabetically.
- The default preset appears first (marked `*` in the list).
- Changing the preset updates the context/flash-attn/etc. display fields live.
- The "Custom" option is still available — it opens the advanced panel with the preset's knobs pre-filled and the user can add more.

### Preset editor (new TUI surface)

A new TUI modal accessible from the Settings tab or from the launch picker:

```
┌── Presets: qwen2 ────────────────────────────────────┐
│                                                        │
│  long-ctx  *  ctx=65536  flash-attn=true              │
│  short-ctx       ctx=8192                             │
│  coding        ctx=16384  keep=0                      │
│  ───────────────────────────────────────────────────  │
│  [ + Add ]  [ Edit ]  [ Delete ]  [ Set Default ]    │
│                                                        │
│  [ Close ]                                            │
└────────────────────────────────────────────────────────┘
```

- `*` marks the current default.
- Each row shows the preset name and a summary of its knobs.
- `+ Add` opens a new-preset form (name input + knobs).
- `Edit` opens the preset editor inline (same as the "Custom" panel but scoped to this preset).
- `Delete` removes the preset from the config file.
- `Set Default` promotes this preset to the default.

### Model card hint

On the Models list, show a small preset count badge on models that have presets:

```
Qwen3.6-27B-Q4_K_M (qwen2)  ● 3 presets
```

### Agent-facing surface

`status` method response adds a new top-level object:

```json
{
  "presets": {
    "qwen2": {
      "default": "long-ctx",
      "entries": [
        { "name": "short-ctx", "ctx": 8192 },
        { "name": "long-ctx", "ctx": 65536, "flash_attn": true },
        { "name": "coding", "ctx": 16384, "keep": 0 }
      ]
    }
  }
}
```

`llamastash start <model-ref> --preset <name>` — picks the preset from either the config file or `state.json` (config wins).

## Backward compatibility

### `state.json` presets

On first boot after a config file with `presets:` is read, check if `state.json` has a `presets` entry for that model. If yes and the config has none, **migrate once** — copy the `state.json` presets into the config file (as `entries`, no default set). After migration, `state.json` presets are ignored for that model.

If the config file already has presets for a model, `state.json` presets are ignored (config is the source of truth).

### CLI `--preset` flag

Already exists (`src/cli/start.rs`), no change needed. It already resolves from `state.json`. After this change it should resolve from config first, then fall back to `state.json`.

## Out of scope for v1

- Preset inheritance (a `base: short-ctx` field that extends another preset)
- Preset import/export / sync across machines
- Preset versioning
- Preset validation against a running llama-server (e.g. "ctx 65536 exceeds model's native 4096")
- Dynamic preset creation from a running model's current params (e.g. "save these as a new preset")

## Open questions

1. **Should presets be keyed by model id or by GGUF path?** Model id is stable but requires the user to know the GGUF's `general.architecture` string. Path is easier to discover but fragile. Compromise: use model id as the key but surface friendly names in the TUI.
2. **Should the config file support per-model presets AND a global `presets:` block?** (i.e., presets that apply to ALL models, not just one.) Probably yes — useful for things like `--flash-attn` or `--cache-type-k`.
3. **Should presets be editable in the TUI?** The user requested it, and it's a natural UX. But it also means the config file is no longer the only source of truth — the TUI becomes an authoring tool. This is fine as long as the TUI writes to the config file, not `state.json`.
4. **Agent auto-generation:** The user mentioned "auto-generating a name based on the config name so that from Pi we can go slash models and then bring up a new configuration." This likely means: when Pi discovers models via `status`, it should see the presets for each model and can suggest them to the user or auto-pick the default.
