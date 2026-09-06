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

## Review — PR #76

Findings from the review passes on
[#76](https://github.com/llamastash/llamastash/pull/76) against `ca672e5`,
consolidated from the inline comments and the review bodies. Each item carries
its fix plan. Tick as landed.

### Parse and name matching (the DRY cluster)

- [x] **RV1 — `model@name` is parsed twice.** `route::decide` (`src/proxy/route.rs:269`)
      and `resolve_running` (`src/cli/resolve.rs:502`) each split their own copy,
      and the copies have drifted on three axes: the empty-name guard, the case
      rule, and the D2 whole-string-first fail-safe.
      *Fix:* one `parse_named_reference(&str) -> Option<(&str, &str)>` in
      `src/launch/resolve.rs`, which both the proxy and the CLI already import.
      Each caller keeps its own model-half matching (catalog resolve vs the
      resolver's substring walk); only the split is shared.
- [x] **RV2 — both split on the first `@`; D2 says the last.** `foo@bar.gguf@coder`
      misparses in both copies, and `mo@del@coder` yields the name `del@coder`.
      *Fix:* `rsplit_once('@')` inside the shared helper from RV1, so D2 is a
      one-line change in one place.
- [x] **RV3 — "does this row carry this name" is written four times with three
      comparison semantics.** `route::decide`, `proxy::launch::attach_target`, the
      `compose_and_spawn` gate, and `cli::resolve::resolve_running`.
      *Fix:* one `launch_carries_name(&RunningSnapshot, launch_id, name)` predicate
      beside `RunningSnapshot` in `src/daemon/state_store.rs`, called from all four.
- [x] **RV4 — the case rule is split-brain, and the split is unrecoverable.** The
      proxy and the daemon gate compare exactly; the CLI compares lowercased. A
      request for `@CODER` against a live `coder` auto-starts a second full model
      load, and once that variant exists the CLI's case-insensitive match makes
      `stop <model>@coder` return "matches 2 launches", so neither launch is
      stoppable by name.
      *Fix:* `eq_ignore_ascii_case` in the shared predicate, which also makes the
      gate reject `CODER` before the second load. Regression test for both halves.
- [x] **RV5 — the port-to-name join is copy-pasted and keys on the wrong field.**
      `decide` and `attach_target` duplicate it, comments included, and match on
      `port`, which `src/ipc/status.rs` documents as reused the moment its launch
      stops.
      *Fix:* the shared predicate from RV3 keys on `launch_id` (both walks already
      destructure it); `port` stays only as the fallback for adopted rows whose
      `launch_id` is `None`.
- [x] **RV6 — the gate matches rows in any state, so a launch stuck in `error`
      keeps its name locked** until it is stopped by hand.
      *Fix:* the predicate takes a holds-a-name state test; `error` releases the
      name, `launching`/`loading`/`ready` hold it.
- [x] **RV7 — the duplicate-name gate is not atomic.** `ctx.state.snapshot()`
      (`src/daemon/context.rs:150`) takes the mutex, clones, and releases it; the
      row is not pushed until `spawn_supervised`. Two concurrent `start --name coder`
      both pass, and because the port allocator *is* serialized the only symptom is
      two rows with the same name and no error.
      *Fix:* reserve the name in the same critical section as the port
      (`src/daemon/registry.rs:48` already serializes ports), or hold the state lock
      across check and insert.
- [x] **RV8 — `--name ""` is accepted and produces a listed-but-unaddressable id.**
      `/v1/models` publishes `qwen3@` because the emission loop does not guard the
      suffix, while `decide` rejects the split on its `!n.is_empty()` guard. The
      TUI normalizes empty to `None`; the CLI and the daemon do not.
      *Fix:* trim in `build_payload`, map empty to `None`, and reject a
      whitespace-only value as a CLI usage error.
- [x] **RV9 — an empty model half matches every row.** `stop @coder` hits
      `"".contains("")`, which is true for all rows, so it silently becomes a
      cross-model name lookup and fails with an ambiguous error naming unrelated
      models.
      *Fix:* guard both halves non-empty in the shared parse; an empty model half
      falls through to the plain reference path instead of widening the match set.

### Named id emission

- [ ] **RV10 — the named-row block is copy-pasted** between `list_models` and
      `ollama_tags` (`src/proxy/router.rs:375`, `:454`): same snapshot, same
      `model_public_id(path, None)`, same `format!("{base_id}@{name}")`, same
      linear dedup scan.
      *Fix:* one `named_launch_ids()` helper both handlers dress up in their own
      row type.
- [ ] **RV11 — emission bypasses `published_id`, and drops `display_label`.**
      Two same-named `qwen3.gguf` in different roots publish bare `qwen3@coder`
      instead of the disambiguated stem, and a request for it then 400s ambiguous on
      the split. D6 says `published_id_index` stays the single rule for the model
      half.
      *Fix:* look the running row's path up in the same `published_ids` index the
      catalog rows use, and pass the row's display label instead of `None`.
- [ ] **RV12 — `decide_umbrella_route` never sees the parsed name.** A delegated
      lemonade launch gets its name stamped and publishes `X@coder`, but a request
      for that id goes down the umbrella path with the name dropped, so the
      published address is cosmetic and D4 cannot happen for managed-multiplexer
      models.
      *Fix:* thread the name into `decide_umbrella_route` and honor it, or stop
      emitting named ids for umbrella-sourced rows. Either is fine; publishing an
      address that does not route is not.

### CLI surface

- [ ] **RV13 — `running_index` keeps one row per path and drops the rest**, so
      `list` can only ever show one `@name` and `show` matches with `.find()`. The
      feature's own motivating case, `qwen3@coder` and `qwen3@writer` both running,
      renders as a single catalog row carrying whichever name won.
      *Fix:* index becomes `HashMap<String, Vec<RunningRow>>`; `list` and `show`
      emit one line per live launch of the path.
- [ ] **RV14 — `list` appends `@name` to the STATUS cell** (`src/cli/output.rs:184`)
      instead of showing `<model-id>@<name>` whole in the id column as Step 3
      specifies. The joined string is what a user pastes into a client.
      *Fix:* join in the id cell; STATUS goes back to what it was.
- [ ] **RV15 — `status` replaces the model display name with the launch name**
      (`src/cli/output.rs:529`) and has no MODEL column, so two different models
      both named `coder` are indistinguishable in the command you reach for to work
      out what to stop. `status_json` is unaffected.
      *Fix:* NAME renders `<model>@<name>`, or add a MODEL column and let NAME hold
      the launch name alone.
- [ ] **RV16 — the PR body claims commands that do not work.** `stop coder`,
      `show coder`, and `show <model>@coder` all exit 66: there is no bare-name
      branch in `resolve_running`, and `show` resolves against the catalog only.
      *Fix:* accept `model@name` in `show`'s resolve and add D3's unique-bare-name
      branch to `resolve_running`; if bare-name is deferred, narrow the body to
      `stop <model>@<name>` / `logs <model>@<name>` / name-aware output.
- [x] **RV17 — `start` never reports the name it set** (`src/cli/start.rs:775`);
      the headline uses `row.name()`, the model name. Step 3 asks for the name, and
      it is the only confirmation the daemon accepted rather than dropped it.
      *Fix:* append ` name=<name>` when set.
- [x] **RV18 — the duplicate-name refusal does not name the launch that holds the
      name**, so the user has to run `status` to find what to stop. D3's own wording
      is `name 'coder' is already running as L3`.
      *Fix:* include the conflicting `launch_id` in the error text.

### TUI

- [ ] **RV19 — move `LaunchNamed` off the model list onto the launch picker.**
      Model list: `⏎` opens the picker and that is it, `Alt+⏎` does nothing.
      Picker: `⏎` launches unnamed as today, `Alt+⏎` asks for the name and launches
      `<model-id>@<name>`.
      *Fix:* `LaunchNamed` moves from `FocusSet::LIST` to `FocusSet::RIGHT_PANE`,
      the list-side handler in `events.rs` goes away, and the dialog's accept path
      submits against the picker that is already open.
- [ ] **RV20 — `commit_launch_name` sets `focus` but not `right_tab = Settings` or
      the scroll reset** that `open_launch_picker` does, so with the right pane on
      Logs/Chat/Embed/Rerank the staged picker is unreachable and `Action::Submit`
      no-ops. *Fix:* removed structurally by RV19; if any staging path survives,
      mirror `open_launch_picker` field for field.
- [ ] **RV21 — the dialog hint resolves the wrong key, under the wrong focus.** It
      renders `LaunchNamed` under `Focus::ConfirmPopup` while the action is scoped
      `FocusSet::LIST`, so the lookup always misses and a user's `keybindings:`
      override never shows; it also advertises `Alt+⏎` when the field submits on a
      bare `⏎`.
      *Fix:* hint text is the Enter label, resolved under the focus the action is
      actually scoped to.
- [ ] **RV22 — `ALT_ENTER_LABEL` is a hardcoded `⌥⏎` on every platform**
      (`src/tui/keybindings.rs:1413`), while `ALT_PREFIX` right above it and
      `TAB_LABEL`/`SHIFT_TAB_LABEL` all use a `#[cfg(target_os = "macos")]` split.
      D5 asks for `⌥⏎` on macOS and `Alt+⏎` elsewhere.
      *Fix:* the same cfg split, or compose it from `ALT_PREFIX` + `ENTER_LABEL` so
      there is one source of truth.
- [ ] **RV23 — `LaunchNameDialog::error` is never set to `Some`**, so the field, the
      spacer row, and the render branch at `launch_name_dialog.rs:114` are dead.
      *Fix:* wire it to the empty/whitespace validation RV8 needs anyway, or delete
      it.
- [ ] **RV24 — `commit_launch_name` takes a `writer` it discards** with
      `let _ = writer;` (`src/tui/events.rs:1358`). *Fix:* drop the parameter.
- [ ] **RV25 — the dialog is a structural clone of `save_preset_dialog`** minus one
      stage. No change now; record the extraction trigger (a third single-field
      modal) as a `TODO.md` line so the shared frame gets pulled out then.

### Nits

- [ ] **RV26 — `FlightKey` is declared but unused**: `Leader::key` and `acquire`
      (`src/proxy/coalesce.rs:67`) spell the tuple out. *Fix:* use the alias so the
      key shape has one name.
- [ ] **RV27 — adding `name` meant hand-editing ~15 `name: None` literals across 8
      test modules**; only the `launch_service` tests got a builder.
      *Fix:* one `RunningSnapshot` test builder used repo-wide, which absorbs the
      next field addition too.

### Pre-existing, newly likely

- [ ] **RV28 — `build_log_path` collides for two launches of one model**
      (`src/daemon/launch_service.rs:1793`). The filename is
      `{stem}-{blake3[0..8]}-{unix_secs}.log` with no launch id, so two launches
      started in the same wall-clock second open the same file and
      `logs <model>@<name>` returns both processes interleaved. Observed twice in
      about five attempts, so timing-dependent rather than always-on.
      *Fix:* put the launch id in the filename.

### Tests still missing

- [ ] **RV29 — `state.json` byte-identical round-trip for an unnamed row** (Step 5,
      marked not optional). `RunningSnapshot` gained a field at
      `src/daemon/state_store.rs:153` and nothing pins that an unnamed row still
      serializes to the pre-feature bytes.
- [ ] **RV30 — no test that a name survives a daemon restart through orphan
      re-adoption.** `orphans.rs:191` clones the whole snapshot so `name` rides
      along, and that is D1's entire justification, but it is the one load-bearing
      path with no coverage.
- [x] **RV31 — no case-variant regression test**: `@CODER` against a live `coder`
      must not start a second launch, and `<model>@coder` must stay unambiguous.
- [ ] **RV32 — no TUI golden snapshots** for the hint, the list pane's `@name`
      suffix, or the Settings name row.

### Docs

- [ ] **RV33 — `docs/usage.md`**: `start --name`, the `model@name` reference form,
      the new `status.name` JSON field, the keybinding table entry, and the D7
      manual pi / opencode step.
- [ ] **RV34 — `docs/architecture.md`**: naming in the routing section, and that
      named rows come from the registry while catalog rows come from
      `published_id_index`.
- [ ] **RV35 — `CHANGELOG.md`**: one line under `[Unreleased]`.
- [ ] **RV36 — `TODO.md`**: close the R9 proxy-ambiguity entry, add the high-priority
      patcher entry from D7, and the RV25 shared-frame note.
