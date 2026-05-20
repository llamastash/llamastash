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
   **one row per preferred quant the publisher ships** — see
   `PREFERRED_QUANTS` in `hf_discovery.py` (Q3_K_M, Q4_K_S, Q4_K_M,
   Q5_K_M, Q6_K, Q8_0). Each row carries `source_hf_id`, `params`,
   `params_active`, `is_moe`, `weights_bytes` (per-quant),
   `gguf_publisher`, `downloads`, `last_modified`.
3. Task hints come from `data/task-hints.yaml` via longest-prefix
   match; unmatched models default to `["general"]`.
4. Rows are deduped on `(source_hf_id, quant)` (highest-download GGUF
   publisher wins per pair), then capped at `SNAPSHOT_MODEL_LIMIT`
   unique *source models* — every preferred quant of the top 100 model
   ids ships. A snapshot therefore typically holds 300–600 rows.
5. `whichllm_combined.fetch()` returns one merged score per `hf_id`.
   The regen joins it onto the catalog rows on lowercased
   `source_hf_id`. Each row gets a small per-quant quality discount
   (Q8_0 ≈ family score, Q3_K_M ≈ 0.94×) and a per-quant speed mult
   so the composite ranker can distinguish quants within a family.
   Rows whichllm doesn't cover ship with `score=0` and source
   `no-source`; the recommender still ranks them by params / speed /
   recency so they remain reachable when the user paginates.
6. The Rust recommender's output dedup keeps one row per
   `source_hf_id` (the best-scoring quant that fits), so the user-facing
   top-N looks like a list of distinct models, not five quants of the
   same model. The full snapshot stays multi-quant so `--quant` filters
   and JSON consumers can inspect every variant.

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
