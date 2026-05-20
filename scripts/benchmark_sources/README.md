# benchmark_sources/

Vendored scrapers for the snapshot regen flow (Unit 7).

## Status

Vendored at upstream commit
[`73cd92f`](https://github.com/Andyyyy64/whichllm/commit/73cd92f9a35a1c3f02e01ec3bbf09fb135a1df26)
on 2026-05-19. Re-syncs are on-demand (R57): refresh when the corpus
gate regresses or a known-relevant upstream change lands. The pinned
commit is recorded in `NOTICE` and in `whichllm.py`.

## Layout

- `whichllm.py` — partial vendoring of
  [`Andyyyy64/whichllm`](https://github.com/Andyyyy64/whichllm) (MIT).
  Thin attribution shim: vendoring metadata + shared
  `ExtractionFailed`. `WHICHLLM_PINNED_VERSION` here pairs with the
  `whichllm==` pin in `scripts/requirements.txt`; Unit 7's CI lint
  asserts they stay in lockstep before each daily regen.
- `hf_discovery.py` — catalog owner (Unit 3 of
  `docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md`).
  Wraps `whichllm.models.fetcher.fetch_models()`, filters to
  GGUF-bearing candidates from allowlisted publishers, attaches task
  hints from `data/task-hints.yaml`, and yields rows shaped like the
  Rust `ModelEntry` struct.
- `open_llm_leaderboard.py` — adapter for the HuggingFace open-llm-
  leaderboard dataset (`datasets-server.huggingface.co/rows`). Exposes
  `fetch() -> SourceResult` and supplies `benchmark_score.value` for
  general / reasoning rows.
- `aider.py` — adapter for the Aider polyglot benchmark
  (`polyglot_leaderboard.yml` in the Aider GitHub repo). Exposes
  `fetch() -> SourceResult` and supplies the code lane.

## Pipeline (regen flow)

1. `hf_discovery.discover()` queries whichllm for ~80 candidate
   `ModelInfo` records (downloads + lastModified + trending + the
   curated frontier list).
2. Each candidate is filtered by GGUF availability + publisher
   allowlist (`data/gguf-publisher-allowlist.yaml`), then projected to
   a row carrying `source_hf_id`, `params`, `params_active`, `is_moe`,
   `weights_bytes`, `gguf_publisher`, `downloads`, `last_modified`.
3. Task hints come from `data/task-hints.yaml` via longest-prefix
   match; unmatched models default to `["general"]`.
4. Rows are ranked by downloads × last_modified and capped at
   `SNAPSHOT_MODEL_LIMIT` (100 — Key Decision 3 of plan
   2026-05-20-001).
5. `open_llm_leaderboard` and `aider` adapters supply
   `benchmark_score.value`, joined on `source_hf_id`. Rows with no
   upstream score still ship with a neutral default.

Vendoring keeps the binary pure-Rust (R45) — these modules run only in
CI to produce the JSON artefact the Rust binary loads via `include_str!`.

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
