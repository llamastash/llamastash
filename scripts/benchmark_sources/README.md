# benchmark_sources/

Snapshot regen sources (Unit 7). All upstream interaction goes through
the `whichllm` Python dependency rather than re-vendored adapters.

## Status

Tracks `whichllm` at the version pinned in `scripts/requirements.txt`
(currently `0.5.7`). The matching reference is recorded in `whichllm.py`
via `WHICHLLM_PINNED_VERSION` and in `NOTICE`. CI asserts the two pins
match before each daily regen.

We *used to* vendor per-source adapters (`open_llm_leaderboard.py`,
`aider.py`) under the same upstream — that path drifted: it covered
only 2 of whichllm's 6 sources, lost the layered current-over-frozen
precedence, and lost the lineage recency demotion. The wizard surfaced
two-generation-old picks (Qwen 2.5) on hosts that should have seen
Qwen3-30B-A3B class. The collapsed adapter below restores parity with
whichllm's own ranking.

## Layout

- `whichllm.py` — attribution shim. Vendoring metadata, the shared
  `SourceResult` dataclass, and `ExtractionFailed`. No upstream code.
- `hf_discovery.py` — catalog owner. Wraps
  `whichllm.models.fetcher.fetch_models()`, filters to GGUF-bearing
  candidates from allowlisted publishers, attaches task hints, dedupes
  on `(source_hf_id, quant)`, and yields rows shaped like the Rust
  `ModelEntry` struct.
- `whichllm_combined.py` — score adapter. Calls
  `whichllm.models.benchmark.fetch_benchmark_scores()` and returns a
  case-insensitive `hf_id -> score` index. Inherits Open LLM
  Leaderboard, Chatbot Arena, LiveBench, Artificial Analysis Index,
  Aider polyglot, and Vision — plus whichllm's layered merge and
  lineage demotion.

## Pipeline (regen flow)

1. `hf_discovery.discover()` queries whichllm for candidate `ModelInfo`
   records (downloads + lastModified + trending + the curated frontier
   list).
2. Each candidate is filtered by GGUF availability + publisher
   allowlist (`data/gguf-publisher-allowlist.yaml`), then projected to
   a row carrying `source_hf_id`, `params`, `params_active`, `is_moe`,
   `weights_bytes`, `gguf_publisher`, `downloads`, `last_modified`.
3. Task hints come from `data/task-hints.yaml` via longest-prefix
   match; unmatched models default to `["general"]`.
4. Rows are deduped on `(source_hf_id, quant)` (highest downloads
   wins), then ranked by downloads × last_modified and capped at
   `SNAPSHOT_MODEL_LIMIT` (100 — Key Decision 3 of plan 2026-05-20-001).
5. `whichllm_combined.fetch()` returns one merged score per `hf_id`.
   The regen joins it onto the catalog rows on lowercased
   `source_hf_id`. Rows whichllm doesn't cover ship with `score=0` and
   source `no-source`; the recommender still ranks them by params /
   speed / recency so they remain reachable when the user paginates.

Keeping the binary pure-Rust (R45) — these modules run only in CI to
produce the JSON artefact the Rust binary loads via `include_str!`.

## Local development

```bash
python3 -m pip install -r scripts/requirements.txt
python3 scripts/regenerate-benchmark-snapshot.py --dry-run --skip-corpus-gate
```

Without `HF_TOKEN`, whichllm may hit anonymous-tier HF rate limits on
the 5-7 query pattern. The CI workflow sets `HF_TOKEN` from the
repository secret (Unit 7 of plan 2026-05-20-001).

`scripts/benchmark_sources/hf_discovery_test.py` exercises the
projection / selection logic with stubbed candidates so changes to the
filter or task-hints lookup get a CI gate without requiring whichllm
itself.
