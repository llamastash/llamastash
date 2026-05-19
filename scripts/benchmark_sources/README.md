# benchmark_sources/

Vendored scrapers for the snapshot regen flow (Unit 7).

## Status

For v2 launch this directory ships **empty** — the regen script
(`scripts/regenerate-benchmark-snapshot.py`) preserves the maintainer-
curated `data/benchmark-snapshot.json` and runs the corpus gate so the
CI workflow's framework is exercisable on day one.

## Planned vendoring (v2-GA)

- `open_llm_leaderboard.py` — adapter for the HuggingFace
  open-llm-leaderboard dataset. Source:
  <https://huggingface.co/spaces/open-llm-leaderboard/open_llm_leaderboard>.
- `aider.py` — adapter for the Aider polyglot benchmark CSV.
- `whichllm.py` — partial vendoring of the whichllm scoring code under
  MIT license. Track the upstream commit hash in `NOTICE`.

Vendoring keeps the binary pure-Rust (R45) — these modules run only in
CI to produce the JSON artefact the Rust binary loads via `include_str!`.
