---
title: "feat: Vendor real benchmark snapshot scrapers (Unit 7 GA follow-up)"
type: feat
status: completed
date: 2026-05-19
origin: docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md
---

# Vendor real benchmark snapshot scrapers (Unit 7 GA follow-up)

## Overview

The v2 init plan shipped Unit 7's CI framework (regen script, GitHub Actions
workflow, corpus gate, snapshot publish) with **placeholder** source
adapters — they return empty rows so the framework is exercisable but the
candidate snapshot still relies entirely on the hand-curated
`data/benchmark-snapshot.json` checked into the repo. This plan replaces
the two placeholders with real scrapers (whichllm-derived, MIT) so the
daily CI loop produces genuinely refreshed benchmark data without a
maintainer touching the bundled JSON.

In scope:

- Vendor a **minimal subset** of `Andyyyy64/whichllm` (MIT) — only the
  scoring helpers and curated tables the two adapters actually call.
- Vendor `open_llm_leaderboard.py` adapter for the general/reasoning lane.
- Vendor `aider.py` adapter for the code lane (Aider polyglot benchmark).
- Wire both adapters into `scripts/regenerate-benchmark-snapshot.py`,
  drop the `TODO(unit7-v2-ga)` placeholders, and verify the existing
  16/20 corpus gate still passes against the refreshed snapshot.
- Record upstream commit hashes in `NOTICE`; pin Python deps in a new
  `scripts/requirements.txt`.

Out of scope (deferred to its own plan):

- VRAM overhead-band remeasurement on real CUDA/HIP/Vulkan/Metal
  hardware — that work is independent and gated on hardware access, not
  on this plan's CI changes.
- Any change to the Rust binary, the recommender, the corpus, or the
  bundled JSON schema. The Rust side is already happy reading whatever
  shape `build_snapshot()` produces; this plan only changes the *source*
  of the rows being merged.

## Problem Frame

`scripts/regenerate-benchmark-snapshot.py:128` and `:144` carry the only
two `TODO(unit7-v2-ga)` markers left in the tree, and both surface in
`scripts/benchmark_sources/README.md` as "Planned vendoring (v2-GA)". As
long as they sit empty, the daily CI workflow's publication is a no-op:
`build_snapshot()` preserves the committed bundled `models[]` array and
republishes it with a fresh `bundle_date`. That means `doctor`'s
`SnapshotStale` finding (R74) is the only signal a user gets that the
data hasn't actually refreshed — and the rollback-DoS detection path
(`remote_fetch_failures`) is meaningless when the remote copy is
identical to the bundled one by design.

The v2 brainstorm (see origin) explicitly calls vendoring out as
on-demand maintenance work: *"Upstream sync is on-demand (triggered by a
maintainer when the post-launch corpus check regresses or a known-
relevant upstream change lands)"* — this plan executes the **initial**
on-demand sync that takes the CI loop from "framework runs green" to
"framework actually refreshes data."

## Requirements Trace

Originates from `docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md` Unit 7 (R57).
This plan satisfies the deferred portion of that unit.

- **R1.** Replace the two `TODO(unit7-v2-ga)` placeholder source
  adapters in `scripts/regenerate-benchmark-snapshot.py` with real
  fetches so a daily CI run produces a genuinely refreshed snapshot
  (origin: R57).
- **R2.** Vendor only the whichllm code the adapters actually call;
  record the upstream commit hash in `NOTICE` so the sync trail is
  auditable (origin: R57, NOTICE).
- **R3.** Preserve the partial-source-failure policy: if any source
  returns no data the script exits non-zero and the last-known-good
  Release asset stays live (origin: regen script docstring §"Partial-
  source-failure policy").
- **R4.** Preserve the release-blocking 16/20 corpus gate. If the
  refreshed data regresses the corpus, publication is blocked and a
  recalibration issue is filed (origin: R57, CI workflow).
- **R5.** Keep the binary pure-Rust (R45 single-binary invariant) — all
  vendored code runs in CI only and never enters the compiled artefact.
- **R6.** Honour MIT attribution: NOTICE records each upstream source
  with license, usage, and pinned commit hash (origin: NOTICE).

## Scope Boundaries

- No change to the Rust binary, the recommender algorithm, the corpus
  test, or `data/benchmark-snapshot.json`'s schema. Schema is owned by
  Unit 5 of the v2 plan; we only feed it.
- No automation of "upstream-sync-on-every-commit" — the brainstorm
  explicitly defers that; vendored revisions move on demand.
- No VRAM overhead remeasurement (separate plan).
- No new sources beyond the two named by R57 (Open LLM Leaderboard,
  Aider). Adding a third source is a separate decision.
- No change to the Rust `load_remote` URL, the rolling `snapshot-latest`
  tag, or per-day audit tag policy — those are owned by the existing
  CI workflow.

## Context & Research

### Relevant Code and Patterns

- `scripts/regenerate-benchmark-snapshot.py` — the regen script's
  framework: `SourceResult` dataclass, `collect_sources()`,
  `build_snapshot()`, `run_corpus_gate()`. The two placeholders to
  replace live at `load_open_llm_leaderboard()` (line 125) and
  `load_aider_leaderboard()` (line 141). The partial-failure contract
  (`ok=False` blocks publication, exit code 2) is already enforced in
  `main()` lines 86-96.
- `scripts/benchmark_sources/__init__.py` — empty marker; vendored
  modules land here as siblings.
- `scripts/benchmark_sources/README.md` — documents the planned
  vendoring and the R45 single-binary boundary (CI-only).
- `data/benchmark-snapshot.json` — the bundled snapshot the script
  reads and rewrites; gives the canonical row shape (`repo`, `file`,
  `architecture`, `quant`, `params`, `weights_bytes`, `task_hints`,
  `benchmark_score: {value, source}`, `tok_s_factor`, `recency`).
- `NOTICE` — already enumerates the three planned vendored sources
  (whichllm, Open LLM Leaderboard, Aider) with placeholder
  `TODO(unit7-v2-ga)` commit-hash lines to fill in.
- `.github/workflows/regenerate-benchmark-snapshot.yml` — already
  installs `scripts/requirements.txt` *if it exists* (line 56). This
  plan creates that file.
- `tests/recommender_corpus.rs` (the file `cargo test --test
  recommender_corpus` resolves to) — the release-blocking gate. The
  script invokes it via `run_corpus_gate()`.

### Institutional Learnings

- The v2 brainstorm's "MIT-license alignment for vendored whichllm
  data" decision (origin: R57 key decisions) frames the *partial*
  vendoring posture: re-implement algorithmic logic in Rust, vendor
  only curated tables and the scraping plumbing the script genuinely
  needs. This plan honours that posture — no algorithmic logic from
  whichllm enters the Rust binary; only its CI-side scraping helpers
  land in `scripts/benchmark_sources/`.
- The Unit 7 retro decided **last-known-good is the right CI failure
  mode** (script exits non-zero on any source failure → CI workflow
  skips publication → previous Release asset stays live → doctor's
  `RemoteSnapshotUnreachable` surfaces prolonged outages via
  `_init_snapshot.remote_fetch_failures`). This plan must not weaken
  that contract.

### External References

Implementation-time research (not pre-resolved in this plan, listed for
the implementer):

- `https://github.com/Andyyyy64/whichllm` — the upstream MIT repo named
  in NOTICE. Implementer pins a specific commit hash on first vendoring
  PR and records it in NOTICE; that hash is the sync anchor for future
  refreshes.
- `https://huggingface.co/spaces/open-llm-leaderboard/open_llm_leaderboard`
  — the Open LLM Leaderboard. Public dataset; access pattern depends
  on what whichllm already does (likely the `datasets` library or a
  direct HF Hub dataset fetch).
- `https://aider.chat/docs/leaderboards/` — the Aider polyglot
  benchmark page. Whichllm's adapter is the reference for whatever
  scrape shape works (HTML table parse vs. published CSV vs. JSON).

## Key Technical Decisions

- **Minimal-subset vendoring (per user direction).** Vendor only the
  whichllm code the two adapters actually call — scoring helpers,
  quant tables, lineage/source mappings — not the full
  `benchmark_sources/` tree. **Why:** brainstorm explicitly says
  "partial"; less code = smaller license/audit surface, smaller
  maintenance footprint, simpler upstream-sync diffs. **Trade-off:**
  upstream updates won't be drop-in; each refresh requires re-pulling
  the same minimal subset. Acceptable given on-demand sync cadence.
- **One Python file per source under `scripts/benchmark_sources/`.**
  `open_llm_leaderboard.py`, `aider.py`, plus `whichllm.py` for the
  shared scoring/table code. Each adapter exposes a single
  `fetch() -> SourceResult` (or equivalent) entry point so the regen
  script's `collect_sources()` stays a one-line-per-source list.
  **Why:** mirrors the structure already documented in
  `scripts/benchmark_sources/README.md`; matches the `SourceResult`-
  per-source contract already encoded in the regen script.
- **`scripts/requirements.txt` pins all Python deps.** The CI workflow
  already conditionally installs it (line 56). Pinning prevents the
  daily cron from breaking on an unrelated upstream Python release.
  **Why:** the CI corpus gate is release-blocking; we don't want a
  silent `requests` major bump to look like a recommender regression.
- **Partial-failure remains hard-fail.** Each adapter returns
  `SourceResult(ok=False, message=…)` on any unrecoverable error
  (timeout, parse failure, upstream HTTP 4xx/5xx, empty rows). The
  regen script's existing `main()` already exits 2 in that case and
  the CI workflow auto-files a `snapshot-regression` issue. **Why:**
  this is the rollback-DoS detection contract spelled out in R57 and
  doctor R74's silent-fallback-freshness check; weakening it would
  break user-facing diagnostics. We do not introduce "best-effort
  partial publication" here.
- **`SourceResult.rows` shape stays internal to the regen script.**
  The regen script's `build_snapshot()` is the only consumer; it
  decides how rows map into the snapshot's `models[]` array. Adapters
  do **not** emit the snapshot JSON shape directly — they emit a flat
  list of dicts with whatever fields whichllm exposes, and
  `build_snapshot()` translates. **Why:** decouples future schema
  evolution from adapter code; mirrors how Unit 5's snapshot schema
  is owned separately from Unit 7's fetch layer.
- **No new sources beyond Open LLM Leaderboard + Aider in this plan.**
  R57 specifies these two; adding a third is a separate decision
  with corpus-impact analysis. **Why:** keeps the corpus-gate blast
  radius small for this PR.
- **Vendored commit hash lives in NOTICE, not in code comments.**
  Single source of truth. **Why:** NOTICE is already the documented
  attribution surface (per AGENTS.md "Protected artefacts" implicitly
  via `LICENSE`/`NOTICE` posture); a code comment that drifts from
  NOTICE has no rot-resistance.

## Open Questions

### Resolved During Planning

- **Should VRAM remeasurement land in the same plan?** No — deferred
  to its own plan (user decision). Hardware-access logistics are
  decoupled from CI Python work.
- **Vendor whichllm fully or partially?** Partial / minimal subset
  (user decision; matches brainstorm wording).
- **Do we add a third source?** No — R57 names two sources; expanding
  the source set is a separate decision with separate corpus impact.
- **Should the snapshot's `recommender_weights` be re-tuned in this
  plan?** No — re-tuning is reactive. The corpus gate will tell us if
  it's needed; if so it lands in this plan's final unit as a
  contingent step rather than upfront.

### Deferred to Implementation

- **Exact upstream whichllm commit hash to pin.** Resolved by the
  implementer on first vendoring PR. Recorded in NOTICE.
- **Whether `open_llm_leaderboard.py` consumes the HF `datasets`
  library or a direct HTTPS pull.** Depends on what whichllm itself
  does at the pinned commit — adopt whichever the upstream code
  uses so the vendored slice stays minimal.
- **Aider leaderboard scrape shape (HTML / CSV / JSON).** Same
  rationale — defer to upstream's actual implementation.
- **Whether `params` / `weights_bytes` need backfilling from a
  secondary source.** The Open LLM Leaderboard rows may not include
  these — if not, `build_snapshot()` either preserves the bundled
  values (lookup by `repo+file`) or surfaces a row-level
  `ok=False`. Decide on real data.
- **If real data regresses the corpus from 20/20 (current curated)
  toward 16/20, do we re-tune `recommender_weights` or curate the
  data?** Decide on real corpus output. Both paths are valid; the
  16/20 threshold is the release gate and either approach can clear
  it.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance
> for review, not implementation specification. The implementing agent
> should treat it as context, not code to reproduce.*

```text
scripts/benchmark_sources/
├── __init__.py                  (existing, marker)
├── README.md                    (existing — update Status section)
├── whichllm.py                  NEW: minimal subset, scoring helpers,
│                                     curated tables, shared utilities
│                                     (vendored from Andyyyy64/whichllm)
├── open_llm_leaderboard.py      NEW: fetch() -> SourceResult
│                                     uses whichllm.{tables,helpers}
└── aider.py                     NEW: fetch() -> SourceResult
                                      uses whichllm.{tables,helpers}

scripts/
├── regenerate-benchmark-snapshot.py  MODIFIED:
│   ├── load_open_llm_leaderboard()       drop TODO, call adapter
│   └── load_aider_leaderboard()          drop TODO, call adapter
└── requirements.txt              NEW: pin Python deps

NOTICE                            MODIFIED: record upstream commit hash
                                            on each of the three blocks

.github/workflows/regenerate-benchmark-snapshot.yml
                                  unchanged (already installs
                                  requirements.txt conditionally)
```

Adapter contract sketch:

```python
# scripts/benchmark_sources/open_llm_leaderboard.py
def fetch() -> SourceResult:
    """Return current Open LLM Leaderboard rows for the general /
    reasoning lane. ok=False on any failure (timeout, parse error,
    upstream removal, empty rows)."""
    ...
```

Daily CI loop, end-to-end:

```text
cron 03:00 UTC
  └─ checkout + setup python + setup rust + cache
     └─ pip install -r scripts/requirements.txt
        └─ python scripts/regenerate-benchmark-snapshot.py
           ├─ collect_sources():
           │   ├─ load_open_llm_leaderboard() → adapter → SourceResult
           │   └─ load_aider_leaderboard()    → adapter → SourceResult
           ├─ any ok=False? → exit 2 → keep last-known-good live
           ├─ build_snapshot(): merge rows + bundled fallback fields
           ├─ write_atomic(data/benchmark-snapshot.json)
           └─ run_corpus_gate(): cargo test --test recommender_corpus
              ├─ exit 0 → workflow publishes to snapshot-latest +
              │           per-day audit tag
              └─ non-zero → workflow files snapshot-regression issue
```

## Implementation Units

- [x] **Unit 1: Vendor `whichllm.py` minimal subset + pin deps + update NOTICE**

**Goal:** Land the shared whichllm helpers under
`scripts/benchmark_sources/whichllm.py` (scoring functions, lineage /
quant / source-mapping tables that the two adapters will call), pin
the Python dep set in `scripts/requirements.txt`, and record the
upstream commit hash in `NOTICE`.

**Requirements:** R2, R5, R6

**Dependencies:** None.

**Files:**
- Create: `scripts/benchmark_sources/whichllm.py`
- Create: `scripts/requirements.txt`
- Modify: `NOTICE` — replace `TODO(unit7-v2-ga)` on the whichllm block
  with the pinned upstream commit hash, license confirmation, and
  date of vendoring.
- Modify: `scripts/benchmark_sources/README.md` — Status section
  transitions from "v2 launch ships empty" to "vendored at
  `<short-sha>` — see NOTICE"; "Planned vendoring (v2-GA)" section
  trimmed to reflect what actually landed.
- Modify: `TODO.md` — strike the `whichllm.py` line and (if applicable)
  the README "Planned vendoring (v2-GA)" line.

**Approach:**
- Read whichllm at the chosen commit; copy only the symbols
  `open_llm_leaderboard.py` and `aider.py` will reach for. Strip out
  CLI surface, anything tied to a published-crate UX, and anything
  that drags in optional deps not on the minimal path.
- Add a one-paragraph header comment in `whichllm.py` recording the
  upstream URL, commit hash, license (MIT), and the note that this
  is a partial vendoring — see NOTICE for sync trail. Do not paste
  the upstream LICENSE inline; NOTICE references it.
- `scripts/requirements.txt` pins exact versions. Start narrow (only
  what whichllm's minimal subset imports) and let later units widen
  it if Unit 2 or 3 need more.

**Patterns to follow:**
- `NOTICE` already has the three vendored-source blocks with
  placeholder `TODO(unit7-v2-ga)` lines — fill in, do not restructure.
- AGENTS.md §"Protected artifacts" — NOTICE is part of the
  engineering record; don't condense it.
- `scripts/benchmark_sources/__init__.py` stays empty; vendored
  modules are siblings (existing convention).

**Test scenarios:**
- *Happy path:* `python -c "from benchmark_sources import whichllm"`
  succeeds with `scripts/` on `PYTHONPATH`.
- *Happy path:* The CI workflow's `Install Python dependencies` step
  succeeds against the new `scripts/requirements.txt` (verified by
  triggering the workflow via `workflow_dispatch` on the PR branch).
- *Edge case:* `NOTICE` parses as plain UTF-8 text; no Markdown lint
  introduced.
- *Integration:* `python scripts/regenerate-benchmark-snapshot.py
  --dry-run --skip-corpus-gate` continues to succeed (placeholders
  still in place; unit only adds plumbing, doesn't wire it yet).

**Verification:**
- `scripts/requirements.txt` exists and the CI workflow installs it
  without warnings.
- NOTICE no longer contains `TODO(unit7-v2-ga)` for the whichllm
  block; commit hash and date are present.
- `TODO.md` reflects the strike-through(s).
- The regen script still passes its existing smoke (`--dry-run
  --skip-corpus-gate`).

- [x] **Unit 2: Vendor `open_llm_leaderboard.py` adapter**

**Goal:** Replace the `load_open_llm_leaderboard()` placeholder's data
source with a real fetch from the Open LLM Leaderboard, returning
populated `SourceResult` rows. Adapter lives at
`scripts/benchmark_sources/open_llm_leaderboard.py` and is invoked
*through* the regen script in Unit 4 (this unit only lands the
adapter file).

**Requirements:** R1, R3, R5

**Dependencies:** Unit 1 (uses `whichllm` helpers + relies on
`requirements.txt`).

**Files:**
- Create: `scripts/benchmark_sources/open_llm_leaderboard.py`
- Modify (potential): `scripts/requirements.txt` — widen pins if the
  adapter needs anything beyond what Unit 1 added.

**Approach:**
- Mirror the upstream whichllm adapter's surface (HF `datasets` API
  call, direct HTTPS, whichever is canonical at the pinned commit).
  Return a flat list of dicts containing whatever fields whichllm
  exposes; do **not** mint the snapshot row shape inside the adapter
  (see Key Decision: `SourceResult.rows` shape stays internal to the
  regen script).
- Hard-fail on any non-success: HTTP non-2xx, parse failure, empty
  rows, schema drift in expected columns. Wrap the request layer in
  a short timeout (target ≤30s end-to-end; the CI step has a 30-min
  budget but the adapter should not be the long pole).
- Adapter exposes a single top-level entry point
  (`fetch() -> SourceResult`) so the regen script's
  `collect_sources()` stays a one-line list.
- No global mutable state in the module; nothing module-import-time
  network. Importing the module from the regen script must not
  trigger I/O.

**Patterns to follow:**
- `SourceResult` dataclass shape lives in
  `scripts/regenerate-benchmark-snapshot.py:51` — adapter constructs
  and returns it (or returns the inputs and the regen script wraps;
  pick whichever is cleanest at implementation time and document in
  the adapter docstring).
- whichllm's own adapter style at the pinned commit; preserve it
  where reasonable so future upstream syncs are mechanical.

**Test scenarios:**
- *Happy path:* `python -c "from benchmark_sources.open_llm_leaderboard
  import fetch; r = fetch(); assert r.ok; assert r.rows"` succeeds
  end-to-end against the live source. (Manual; not run in CI's PR job
  because it would couple the unit test to external availability.)
- *Error path:* upstream returns HTTP 503 → adapter returns
  `SourceResult(ok=False, message=…)`. Verifiable by pointing the
  adapter at a deliberate bad URL via env override or by monkey-patch
  in an inline `if __name__ == "__main__":` smoke harness.
- *Error path:* upstream returns 200 with an empty row set → adapter
  returns `ok=False, message="empty rows"`. Last-known-good policy
  must hold.
- *Error path:* upstream returns 200 with a renamed expected column
  → adapter returns `ok=False` with a message naming the missing
  column.
- *Edge case:* importing the module does not perform network I/O.
  Verifiable by `python -c "import benchmark_sources.open_llm_leaderboard"`
  under `requests` monkey-patched to assert no calls.
- *Edge case:* the adapter is timezone-safe — any `bundle_date` /
  `recency` derived from upstream `updated_at` uses UTC, not local
  time (the regen script's `bundle_date` is already
  `datetime.date.today().isoformat()` UTC-equivalent; adapter must
  not introduce naive local-time fields).

**Verification:**
- A manual `python -c "..."` invocation against the live source
  returns `ok=True` with a non-empty `rows`.
- Adapter never makes a network call at import time.
- Error-path return shape conforms to `SourceResult`.

- [x] **Unit 3: Vendor `aider.py` adapter**

**Goal:** Same as Unit 2, but for the Aider polyglot benchmark.
Replaces the `load_aider_leaderboard()` placeholder's data source.
Adapter lives at `scripts/benchmark_sources/aider.py` and is invoked
through the regen script in Unit 4.

**Requirements:** R1, R3, R5

**Dependencies:** Unit 1. Independent of Unit 2 — can land in
parallel.

**Files:**
- Create: `scripts/benchmark_sources/aider.py`
- Modify (potential): `scripts/requirements.txt` — widen pins if
  needed.

**Approach:**
- Same shape as Unit 2's adapter: `fetch() -> SourceResult`, no
  import-time I/O, hard-fail on any non-success, short timeout.
- Aider publishes its leaderboard at `https://aider.chat/docs/
  leaderboards/`. Whichever shape whichllm reaches for at the pinned
  commit (HTML scrape, JSON, or published CSV) is what we mirror.

**Patterns to follow:**
- Same as Unit 2 — symmetry between the two adapters keeps Unit 4's
  wiring uniform.

**Test scenarios:**
- *Happy path:* manual `fetch()` against the live source returns
  `ok=True` with non-empty `rows`.
- *Error path:* upstream returns HTTP 4xx/5xx → `ok=False`.
- *Error path:* upstream HTML schema drift (renamed column, missing
  table) → `ok=False` with descriptive message.
- *Error path:* empty row set → `ok=False`.
- *Edge case:* no network I/O at import time.
- *Edge case:* the adapter parses Aider's `pass_rate_2`-style fields
  (or whichever field whichllm uses) without locale-dependent number
  parsing — a comma vs. period decimal separator must not flip
  meaning.

**Verification:**
- Manual `fetch()` against live source returns populated `ok=True`.
- Adapter never makes a network call at import time.
- Error paths return well-shaped `SourceResult`.

- [x] **Unit 4: Wire adapters into regen script, run corpus gate, recalibrate if needed**

**Goal:** Drop both `TODO(unit7-v2-ga)` placeholders in
`scripts/regenerate-benchmark-snapshot.py`, call the vendored
adapters, verify the existing 16/20 corpus gate still passes, and
recalibrate the bundled snapshot's `recommender_weights` *only if*
the gate regresses.

**Requirements:** R1, R3, R4

**Dependencies:** Units 1, 2, 3.

**Files:**
- Modify: `scripts/regenerate-benchmark-snapshot.py` — `load_open_
  llm_leaderboard()` (line 125) and `load_aider_leaderboard()` (line
  141) call the vendored adapters; both `TODO(unit7-v2-ga)` comments
  removed. May also adjust `build_snapshot()` if the upstream row
  shape doesn't already provide every field the snapshot needs
  (`params`, `weights_bytes`, etc.) — backfill from the bundled
  snapshot's existing entries by `repo+file` lookup when missing.
- Modify (conditional): `data/benchmark-snapshot.json` — only if the
  refreshed data regresses the corpus gate. Re-tune
  `recommender_weights` or curate the `models[]` array (drop /
  re-rank entries) until the corpus passes ≥16/20.
- Modify: `TODO.md` — strike the two `regenerate-benchmark-snapshot.py`
  line items.
- Modify: `scripts/benchmark_sources/README.md` — Status section
  reflects "vendored" rather than "ships empty".

**Approach:**
- Replace each placeholder function body with a single delegation to
  the adapter:
  - `load_open_llm_leaderboard()` returns whatever
    `benchmark_sources.open_llm_leaderboard.fetch()` returns.
  - `load_aider_leaderboard()` returns whatever
    `benchmark_sources.aider.fetch()` returns.
- The partial-failure exit-2 path in `main()` already exists; if
  either adapter returns `ok=False`, the script bails before the
  corpus gate runs. Do not weaken or branch around this.
- `build_snapshot()` currently preserves the bundled `models[]` array
  verbatim when sources return empty rows. Now that sources return
  real rows, decide the merge policy:
  - **Default:** the live row's `benchmark_score.value` wins; fields
    the adapter doesn't supply (`params`, `weights_bytes`,
    `task_hints`, `tok_s_factor`, `recency`) fall back to the bundled
    snapshot's existing row keyed by `(repo, file)`. New repos that
    aren't in the bundled snapshot at all are skipped (they need a
    maintainer-curated `task_hints` to slot into the recommender's
    task lanes; surfacing them automatically would couple the corpus
    to upstream taxonomy drift).
  - This keeps the schema stable and the corpus gate predictable
    across daily runs.
- Run `python scripts/regenerate-benchmark-snapshot.py` locally with
  the corpus gate enabled. Three outcomes:
  1. **Gate passes ≥16/20**: ship. No `recommender_weights` change.
  2. **Gate regresses but the failure mode is "a borderline model
     dropped one rank because its benchmark score moved"**: re-tune
     `recommender_weights` (the three score weights —
     `benchmark`/`tok_per_second`/`param_quality`/`recency` — sum to
     1.0; adjust to restore corpus picks). Document the change in
     the commit message with the corpus diff.
  3. **Gate regresses with structural failures (multiple categories
     flip, picks move beyond top-3)**: curate the `models[]` array —
     drop entries whose upstream score swung the ranker, or accept
     that the upstream taxonomy has drifted and revisit the corpus
     itself (the corpus is part of the recommender's spec; we don't
     re-fit it lightly).
- The maintainer-PR path is automatically covered: the CI workflow's
  `paths:` filter already runs the same gate when
  `data/benchmark-snapshot.json` is touched, so a recalibration PR
  re-validates itself.

**Patterns to follow:**
- Existing `main()` partial-failure flow (lines 86-96) — don't
  duplicate or re-implement the exit-2 path.
- Atomic-write pattern in `write_atomic()` (line 180) — unchanged.

**Test scenarios:**
- *Happy path:* `python scripts/regenerate-benchmark-snapshot.py
  --dry-run` succeeds locally with both adapters returning real
  rows; output JSON has non-empty `models[]` and `bundle_date` is
  today.
- *Happy path:* `python scripts/regenerate-benchmark-snapshot.py`
  (no flags) succeeds, writes `data/benchmark-snapshot.json`, runs
  the corpus gate, and passes ≥16/20.
- *Integration:* `cargo test --features test-fixtures --test
  recommender_corpus` runs (via the regen script) and exits 0
  against the refreshed snapshot.
- *Error path:* simulate one adapter returning `ok=False` (e.g.
  monkey-patch via env var in an inline test, or run with adapter
  pointed at a bad URL) → regen script exits 2 *before* the corpus
  gate runs, last-known-good remains. CI workflow's failure branch
  files a `snapshot-regression` issue.
- *Error path:* refreshed snapshot is well-formed but the corpus
  gate regresses to <16/20 → `cargo test` exit code propagates →
  workflow does not publish; the on-call procedure is: open an
  issue, decide whether to recalibrate weights or curate the
  catalog, land it as a follow-up PR (caught by the `paths:` filter,
  same gate).
- *Edge case:* a new repo appears in upstream that's not in the
  bundled snapshot. `build_snapshot()` skips it silently — verify
  the row count doesn't grow uncontrolled. (Add a deliberate test
  during implementation: known upstream row absent from bundled JSON
  → output `models[]` size equals bundled size.)
- *Edge case:* a bundled repo is *missing* from the latest upstream
  fetch (delisted or renamed). `build_snapshot()` preserves the
  bundled row (which is the safe choice: don't drop catalog
  silently). Add this as a covered case.

**Verification:**
- Local `python scripts/regenerate-benchmark-snapshot.py` succeeds
  end-to-end (no source failures, corpus gate passes).
- Output `data/benchmark-snapshot.json` differs from the previous
  bundled snapshot only in `bundle_date` and the `benchmark_score`
  fields of rows where upstream actually moved. (Diff is small and
  reviewable.)
- The two `TODO(unit7-v2-ga)` markers no longer exist in the tree.
- `TODO.md` reflects the strikes.
- The CI workflow on the PR branch (triggered via the `paths:`
  filter) runs the gate green.

## System-Wide Impact

- **Interaction graph:** the regen script is the only consumer of the
  adapters; nothing in the Rust binary imports them. The bundled
  snapshot's *contents* affect the recommender (R55, R56), but its
  *shape* is unchanged.
- **Error propagation:** any adapter `ok=False` propagates through
  `main()` → exit 2 → CI workflow's `failure()` branch → auto-filed
  `snapshot-regression` issue. The last-known-good Release asset
  stays live so users on the bundled snapshot are unaffected, and
  doctor's `RemoteSnapshotUnreachable` finding surfaces prolonged
  outages via `_init_snapshot.remote_fetch_failures`. This contract
  is documented in the regen script's docstring and the CI workflow's
  comment block; this plan preserves it unchanged.
- **State lifecycle risks:** the regen script already uses
  `write_atomic()` with `tmp.<pid>` + rename, so a partial write
  can't corrupt the snapshot. No new state surface introduced.
- **API surface parity:** none — vendoring is CI-internal. The Rust
  binary's `load_remote` contract is unchanged.
- **Integration coverage:** the `cargo test --test recommender_corpus`
  gate covers the integration boundary that matters (refreshed data
  must still satisfy the corpus). It's already wired into the regen
  script's `run_corpus_gate()`.
- **Unchanged invariants:**
  - **R45 single-binary.** No vendored Python ever enters the Rust
    artefact — the new modules live under `scripts/` and are not
    referenced from any `include_str!` or `include_bytes!` path.
  - **Snapshot JSON schema** (Unit 5 of the v2 plan). This plan does
    not change `schema_version`, the field set, or row identity.
  - **Daemon contract.** Nothing in the daemon, IPC, or CLI surface
    changes.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Refreshed data regresses the 16/20 corpus gate | The gate is release-blocking by design — publication is blocked, recalibration lands as a follow-up PR via the `paths:` filter using the same gate. Unit 4's approach section enumerates the response playbook. |
| Upstream whichllm structure has drifted enough that the "minimal subset" is harder than expected | Implementer can fall back to full `benchmark_sources/` directory vendoring without re-architecting. The license posture and CI-only boundary are unchanged. Document the change in the unit's commit message. |
| Open LLM Leaderboard or Aider changes hosting / URL / schema between vendoring and daily runs | Each adapter's hard-fail on schema drift surfaces the issue immediately (next daily run files a `snapshot-regression` issue). The `paths:` filter on `scripts/benchmark_sources/**` means a fix-up PR re-validates itself. Pinned commit in NOTICE gives the sync-trail to retest after upstream lands a fix. |
| Pinned Python deps introduce a CVE | Daily cron exposes the surface but isolation is good: vendored code runs in a GitHub Actions runner only, doesn't touch user data, and the output is a JSON file the Rust binary parses with size + redirect caps. Dependabot or manual review on dep bumps is sufficient. |
| Adapter network calls slow the daily run past timeout | CI workflow has 30-min budget; adapter contract enforces short timeouts. Worst case the run fails open (last-known-good stays). Monitor on first few runs. |
| `params` / `weights_bytes` missing from upstream rows force a backfill from the bundled snapshot, masking real upstream changes | Document the backfill policy in `build_snapshot()` (and in the script docstring) so future maintainers know that absence of `params` ≠ change in `params`. Add a CI-time warning if a row falls back for >50% of its fields. |

## Documentation / Operational Notes

- `scripts/benchmark_sources/README.md` Status section needs an
  update in Unit 1 ("ships empty" → "vendored at `<sha>`") and Unit
  4 (Planned-vendoring list trimmed to whatever genuinely remains).
- `NOTICE` filled in across all three vendored-source blocks during
  Unit 1.
- `TODO.md` updated in Units 1 and 4 to strike the four checked
  items (which the TODO file already flags as overlapping pairs).
- `CHANGELOG.md` — under `[Unreleased]`, add a line under an
  appropriate section: `Internal: vendor whichllm-derived benchmark
  scrapers (CI only; no binary impact).` Per AGENTS.md, internal-only
  refactors can be omitted, but this one changes observable CI
  behaviour (daily Release asset is now actually fresh), so it's
  worth a line.
- No `README.md`, `docs/architecture.md`, `docs/usage.md`, or
  `config.example.yaml` change — surfaces are unaffected.
- No `AGENTS.md` change — vendoring posture is already documented
  there implicitly via the "Protected artifacts" + Unit 7 references.

## Sources & References

- **Origin plan (Unit 7):** [docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md](2026-05-18-001-feat-init-wizard-doctor-pull-plan.md)
- **Origin requirements (R57):** [docs/brainstorms/2026-05-18-init-wizard-requirements.md](../brainstorms/2026-05-18-init-wizard-requirements.md)
- **Spike (deferred):** [docs/spikes/2026-05-19-vram-overhead-band.md](../spikes/2026-05-19-vram-overhead-band.md) — VRAM remeasurement is split into its own plan, not covered here.
- **Existing regen script:** `scripts/regenerate-benchmark-snapshot.py`
- **Existing CI workflow:** `.github/workflows/regenerate-benchmark-snapshot.yml`
- **NOTICE:** `NOTICE` — vendored-source attribution.
- **TODO index:** `TODO.md`
- **Upstream whichllm (vendoring target):** https://github.com/Andyyyy64/whichllm (pinned commit `73cd92f` per NOTICE)
- **Open LLM Leaderboard:** https://huggingface.co/spaces/open-llm-leaderboard/open_llm_leaderboard
- **Aider polyglot benchmark:** https://aider.chat/docs/leaderboards/
