# Plan: named launches (`model@name`)

**Status:** planned (2026-09-03). Units 2 (daemon/IPC), 5 (launch/supervisor), 6
(TUI shell), 8 (CLI). Commit subjects: `feat(unit5):` for the daemon + proxy half,
`feat(unit8):` for the CLI, `feat(unit6):` for the TUI.

## Requirement

Launch one model several times and address a specific launch from a client, by
changing model in pi / opencode:

```
llamastash start qwen3.8-27b --name coder
llamastash start qwen3.8-27b --name reviewer     # same model, same preset

pi session A:   /model llamastash/qwen3.8-27b@coder
pi session B:   /model llamastash/qwen3.8-27b@reviewer
```

Each session pins one process: its own KV cache, no queueing behind the other
session's long request, and neither can evict the other.

## Problem

Investigated live on 2026-09-03 (isolated daemon, `Llama-3.2-1B-Instruct-Q4_K_M`
launched twice, `--ctx 4096` on `:41900` as `L1` and `--ctx 8192` on `:41901` as
`L2`) — the `TODO.md` R9 entry this plan closes:

1. Two launches of one GGUF are an allowed, deliberate state. Nothing refuses or
   warns, and `state.json` keys `(id, port)` so both persist.
2. `/v1/models` publishes **one** id, built from the disk catalog and not the
   launch registry, so the second launch is unaddressable by any client.
3. [`route::decide`](../../src/proxy/route.rs) returns the first Ready launch it
   meets walking `supervisors.snapshot()`, a `BTreeMap<LaunchId, _>`. Every
   request went to `L1`; `L2` served nothing. `LaunchId` is a `String`, so the
   order is lexicographic — past ten launches `L10` sorts ahead of `L2` and the
   winner changes for no reason the user can see.
4. The second launch's knobs are therefore dead.
5. Failover does work: stopping `L1` sent the next request to `L2` with no error
   and no restart.
6. `stop <name>` **refuses** the identical ambiguity (`matches 2 launches: L1,
   L2`), so the CLI and the proxy disagree about whether this is addressable.

## Scope

- `src/daemon/launch_service.rs` — `StartParams.name`, stamp, duplicate refusal.
- `src/daemon/state_store.rs` — `RunningSnapshot.name`.
- `src/ipc/status.rs` — surface `name` on the running row.
- `src/proxy/route.rs` — parse `model@name`, filter the supervisor walk.
- `src/proxy/router.rs` — named rows on `/v1/models` and `/api/tags`.
- `src/cli/cli_args.rs`, `src/cli/start.rs` — `--name`.
- `src/cli/resolve.rs` — accept `model@name`; better ambiguity message.
- `src/cli/output.rs` — `<model-id>@<name>` inline in the `list` table.
- `src/cli/show.rs` — name on each live launch.
- `src/tui/tabs/settings.rs` — name on the running launch in view.
- `src/tui/list_pane.rs` — `@name` inline on a named row.
- `src/tui/keybindings.rs` — `Action::LaunchNamed`, `Alt+⏎`, `ALT_ENTER_LABEL`.
- `src/tui/launch_name_dialog.rs` — **new**, modelled on `save_preset_dialog.rs`.
- `src/tui/app.rs`, `events.rs`, `render.rs`, `launch_picker.rs` — dialog wiring.
- `docs/usage.md`, `docs/architecture.md`, `CHANGELOG.md`, `TODO.md`.

Not in scope, as **decisions** (see D1, D7):

- Persisting names to `config.yaml`.
- Teaching the pi / opencode patchers to emit named rows — deferred with a
  high-priority `TODO.md` entry; the manual step is documented instead.
- A deterministic pick among *unnamed* duplicate launches. Named launches make
  the ambiguity avoidable; they do not remove it. Tracked separately.

## What gets reused

| Need | Existing thing used |
|---|---|
| Launch identity that already persists | `RunningSnapshot.launch_id` + the orphan re-adoption path (`src/daemon/orphans.rs:4`) |
| Model reference → catalog row | `resolve_model_with_candidates` (`src/launch/resolve.rs`) |
| Published id rule | `published_id_index` (`src/util/paths.rs:173`) — named rows are appended beside it, not folded into it (D6) |
| One decision point for every inference surface | `route::decide`, reached by OpenAI, Anthropic and Responses alike (`src/proxy/router.rs:103-122`) |
| Named-write modal | `save_preset_dialog.rs` `SaveStage::Name` + `input_field.rs` |
| Help-bar / overlay hint text | the `binds!` `hint:` / `description:` fields (D5) |
| Launch dispatch | `WriterCmd::StartModel` / `StartModelArgs` — one new field, no new path |

Genuinely new: one dialog module, one parse rule, one filter.

## Key decisions

Settled here. Do not re-derive during implementation.

### D1 — the name is a runtime label, never config

A name does **not** go in `config.yaml`. Presets already own *how* to launch a
model. If a name also carried params, `@coder` and `--preset long` would be two
overlapping answers to the same question — exactly the ambiguity the published-id
work removed. The name says *which instance*, nothing more, and it dies with the
launch.

It **does** go on `RunningSnapshot` (`state.json`), for one reason: the daemon
re-adopts entries from `state.json::running` whose PID is still alive
(`src/daemon/orphans.rs:4`), which is why `launch_id` is already stamped there.
Without the name on that row, restarting the daemon re-adopts a live
`llama-server` and drops its name, so `@coder` starts 404ing against a process
that is up and serving. That is runtime record-keeping, not configuration:
nothing accumulates, and the row is dropped when the launch stops.

### D2 — `@` is the separator, and the parse fails safe

`/` is taken by the repo qualifier (`unsloth/Qwen3-0.6B` already publishes and
routes). `:` is Ollama's tag separator on the compat surface. `@` is unclaimed in
both schemes, and opencode passes custom-provider model ids through to the API
unchanged, so `qwen3.8-27b@coder` arrives verbatim.

Split on the **last** `@`, and treat it as a name selector only when *both* hold:

- the left side resolves to exactly one catalog row, and
- the right side names a live launch of that row.

Otherwise the whole string resolves as a model reference, exactly as today. A
GGUF literally named `foo@bar.gguf` therefore keeps working. No existing
published id contains `@`, so nothing in the field changes meaning.

### D3 — names are per-model and unique among live launches

`coder` on two different models is fine; the addressable form is always
`model@name`. Two live launches of one model may not share a name — the second
`start --name coder` is refused with `name 'coder' is already running as L3`.
That refusal is what makes a name an identity rather than a label, and it stops
the accidental duplicate that started this whole investigation.

The resolver may also accept a bare name when it is unique across all live
launches (it already does fuzzy matching), so `stop coder` works. That is a
convenience, not the contract.

### D4 — a request for a name with no live launch auto-starts it

`qwen3.8-27b@coder` with no launch called `coder` starts one and stamps it
`coder`. Params come from the ordinary resolve chain (the model's `default:`
preset, then `last_params`), exactly as a bare `qwen3.8-27b` auto-start does
today. A named request behaves like an unnamed one; the name is what the result
gets called.

This is what keeps a pi config working across a reboot: the session pinned to
`@coder` reconnects and the launch comes back, with no CLI step in between.

Two consequences, both accepted:

- **The name namespace is unbounded.** Under D1 no name is known ahead of time,
  so any `@whatever` starts something — a typo starts a launch instead of
  erroring. The memory cost of that is bounded by the admission gate, which
  refuses a launch that will not fit exactly as it does for any other auto-start.
- **Two sessions cold-requesting two names start two copies.** That is the
  feature working, not a failure, but it is a real memory decision on a large
  model.

Only the *model* half can 404 now: `nosuchmodel@coder` is `model_not_found` on
the model reference, as today.

### D4a — single-flight keys on `(model, name)`

The auto-start path already single-flights concurrent requests for the same model
so two arriving requests do not spawn two servers. With D4 that key must widen to
`(model, name)`, or two sessions racing for `@coder` and `@reviewer` collapse into
one launch and one of them silently gets the wrong instance — the exact bug this
feature exists to remove. Unnamed requests keep their present key.

### D5 — the keybinding hint is derived, never written

`Alt+⏎` must appear in the help bar and the help overlay, and it must follow a
user's `keybindings:` override. So it is declared once in the `binds!` slice with
`hint:` and `description:`, and every surface reads it from the active `KeyMap`
(`Binding::label` / `description`) at render time. No literal `"Alt+Enter"`
anywhere in the UI — the project rule, and the reason the hint is a plan item and
not an afterthought.

`alt_label!` takes a `literal` and builds its string with `concat!`, so it cannot
wrap the `ENTER_LABEL` const. Add `ALT_ENTER_LABEL` beside `SHIFT_ENTER_LABEL`
(`src/tui/keybindings.rs:1399`) with the same `#[cfg(target_os = "macos")]` split
already used for `ALT_PREFIX` (`:1413`): `⌥⏎` on macOS, `Alt+⏎` elsewhere. Both
glyphs are already in use and are single-cell text-presentation BMP symbols.

### D6 — named rows are appended to the listings, not folded into the id index

`published_id_index` is built from the **disk catalog**; named launches live in
the **supervisor registry**. They are different sources with different lifetimes,
so `/v1/models` and `/api/tags` emit catalog rows as today *plus* one row per live
named launch. The named row reuses the launch's own model id as its stem, so
`published_id_index` keeps being the single rule for the model half of the string.

Consequence to accept: a named row appears and disappears as launches come and
go. That is correct — the name is only meaningful while the launch is live (D4).

### D7 — the tool-config patchers stay unchanged for now

pi's `/model` lists `providers.<id>.models[]` from `~/.pi/agent/models.json`;
opencode's `/models` lists the keys of `provider.<id>.models`. Neither discovers
ids from `/v1/models`, and both files are written only by the `init` wizard
(`src/init/wizard.rs:1306`).

So a named launch will not appear in either picker until the user adds it. That
is a documented manual step for this feature — duplicate the model block and
append `@name` to the id — plus a high-priority `TODO.md` entry to work out how
the patchers should emit named rows. Shipping the routing half first is what
makes the manual step worth anything.

## Implementation

### Step 1 — daemon carries the name (unit 5 + 2)

- [ ] `StartParams.name: Option<String>` (`src/daemon/launch_service.rs:43`),
      `#[serde(default)]` like every sibling.
- [ ] `RunningSnapshot.name: Option<String>` (`src/daemon/state_store.rs:131`),
      `#[serde(default, skip_serializing_if = "Option::is_none")]` — an unnamed
      row stays byte-identical in `state.json`.
- [ ] `compose_and_spawn` refuses a name already held by a live launch of the
      same model path (D3), and stamps it on the snapshot otherwise.
- [ ] `src/ipc/status.rs` surfaces `name` on the running row so the CLI and TUI
      can render it without a second call.
- [ ] The proxy's auto-start path builds `StartParams::default()` and now **does**
      set `name` (D4). `force` stays unreachable from there — a request off the
      network must still not be able to override the admission gate. Assert both
      halves, since they now diverge.

### Step 2 — proxy routes and publishes (unit 5)

- [ ] `route::decide` splits `model@name` per D2 before the catalog resolve, then
      filters the supervisor walk (`src/proxy/route.rs:281`) on the launch's
      stamped name.
- [ ] Miss → `NotRunning` carrying the name, so the existing auto-start flow
      launches it and stamps it (D4). Only the model half can 404.
- [ ] Widen the auto-start single-flight key to `(model, name)` (D4a).
- [ ] No name → today's walk, unchanged.
- [ ] `list_models` and `ollama_tags` (`src/proxy/router.rs:360`, `:422`) append
      one row per live named launch (D6).

### Step 3 — CLI (unit 8)

- [ ] `StartArgs.name: Option<String>` → `--name <NAME>`, threaded through
      `src/cli/start.rs` onto the wire.
- [ ] `list` renders the name **inline with the model id** as `<model-id>@<name>`,
      not as its own column — the joined string is the thing a user pastes into a
      client, so showing it whole is the point. Unnamed rows are unchanged.
      `--json` keeps `name` as its own field; only the table joins them.
- [ ] `show` reports the name for every live launch of the model it describes
      (`src/cli/show.rs`).
- [ ] `src/cli/resolve.rs` accepts `model@name`; `single_or_error`'s ambiguity
      message suggests the named form now that there is one.
- [ ] `start` success line reports the name when set.

### Step 4 — TUI (unit 6)

- [ ] `Action::LaunchNamed` on the `Action` enum, beside `OpenLaunchPicker`.
- [ ] `ALT_ENTER_LABEL` const (D5), then the binding:
      `scopes: FocusSet::LIST`, `hint: "launch as…"`,
      `description: Some("launch focused model under a name")`,
      `chords: [(KeyCode::Enter, KeyModifiers::ALT, ALT_ENTER_LABEL, CAT_MODELS)]`.
      `Alt+⏎` is currently bound to nothing — it appears only in a
      `parse_key_spec` unit test (`src/tui/keybindings.rs:1813`).
- [ ] `("launch_named", Action::LaunchNamed)` in the config-name table (`:1175`)
      so `keybindings:` can rebind it.
- [ ] `src/tui/launch_name_dialog.rs` — new. `save_preset_dialog.rs`'s `Name`
      stage without the `Confirm` stage; `Esc` cancels, `Enter` accepts.
- [ ] Wiring, mirroring the save-preset dialog exactly:
      `App.launch_name_dialog` field + init (`app.rs:401`, `:597`), render
      (`render.rs:171`), modal-active check (`events.rs:198`), input routing
      (`events.rs:325`), handler beside `handle_save_preset_input`
      (`events.rs:1220`).
- [ ] On accept, open the normal launch picker carrying the name:
      `LaunchPickerState.launch_name: Option<String>` → `StartModelArgs.name`
      (`app.rs:452`) → `StartParams.name`. Plain `⏎` is untouched and launches
      unnamed, as today.
- [ ] The list pane renders `<model-id>@<name>` inline on a named row, matching
      the `list` table (Step 3) so both read the same string.
- [ ] The Settings tab (`src/tui/tabs/settings.rs`) shows the name for the
      running launch in view, beside the launch identity it already renders.

### Step 5 — tests

- [ ] `state.json` round-trip: an unnamed row is byte-identical to a 0.2.0 row.
- [ ] A duplicate live name on one model is refused; the same name on two
      different models is not.
- [ ] Integration: two launches of one model with the **same** preset, addressed
      by name, land on different ports. This is the case that motivated the
      feature and the one `@preset` could not express.
- [ ] Integration: the name survives a daemon restart through orphan re-adoption
      (D1's whole justification).
- [ ] `foo@bar.gguf` still resolves as a model reference (D2's fail-safe).
- [ ] A named request with no live launch auto-starts one carrying that name
      (D4), and the model half still 404s when it resolves to nothing.
- [ ] Concurrent cold requests for two names on one model produce two launches,
      not one (D4a). This is the single-flight regression that would otherwise be
      invisible — it looks like the feature working while one session silently
      shares the other's instance.
- [ ] `/v1/models` lists the named row while live and drops it after `stop`.
- [ ] TUI: `Alt+⏎` on the list opens the dialog; `⏎` still opens the picker
      directly. Golden snapshot covers the help-bar hint (D5), the list pane's
      `@name` suffix, and the Settings row.
- [ ] The proxy auto-start path sets `name` but still cannot set `force`.

### Step 6 — docs

- [ ] `docs/usage.md` — `start --name`, the `model@name` reference form, the
      keybinding table entry, and the manual pi / opencode step from D7.
- [ ] `docs/architecture.md` — naming in the routing section; note that named
      rows come from the registry and catalog rows from `published_id_index`.
- [ ] `CHANGELOG.md` — one line under `[Unreleased]`.
- [ ] `TODO.md` — close the R9 proxy-ambiguity entry, and add the high-priority
      patcher entry from D7.

## Risks

- **D4a is the one that bites quietly.** Every other failure mode in this plan is
  loud. A single-flight key left on the model alone still serves both sessions,
  just from one process — which is indistinguishable from success until someone
  wonders why two sessions share a KV cache.
- **Named rows are live-only (D6).** A client that caches `/v1/models` will hold
  names that have since stopped. Auto-start (D4) makes that self-healing rather
  than an error, which is the main reason D4 earns its unbounded namespace.
- **`state.json` shape.** `RunningSnapshot` is read by the boot sweep and the
  orphan adopter before anything else runs. The `skip_serializing_if` on the new
  field is what keeps an existing row byte-identical; the round-trip test in
  Step 5 is not optional.
