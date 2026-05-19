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
  Thin attribution shim: vendoring metadata + shared `ExtractionFailed`.
- `open_llm_leaderboard.py` — adapter for the HuggingFace open-llm-
  leaderboard dataset (`datasets-server.huggingface.co/rows`). Exposes
  `fetch() -> SourceResult`.
- `aider.py` — adapter for the Aider polyglot benchmark
  (`polyglot_leaderboard.yml` in the Aider GitHub repo). Exposes
  `fetch() -> SourceResult`.

Vendoring keeps the binary pure-Rust (R45) — these modules run only in
CI to produce the JSON artefact the Rust binary loads via `include_str!`.
