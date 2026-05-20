"""Combined benchmark-score adapter — delegates to whichllm.

Calls ``whichllm.models.benchmark.fetch_benchmark_scores()`` to obtain
the merged dict of ``hf_id -> 0..100 score``. That function internally
fetches six upstream sources (Open LLM Leaderboard v2, Chatbot Arena
ELO, LiveBench, Artificial Analysis Index, Aider polyglot, Vision)
with bundled curated fallbacks, layers ``current`` (LiveBench / AA /
Aider / Vision) over ``frozen`` (OLLB / Arena) per-model, then applies
lineage-based recency demotion to scores that remain frozen-only.

Replaces the per-source ``open_llm_leaderboard.py`` + ``aider.py``
adapters this script used to ship. Two reasons for collapsing:

* whichllm already merges all six sources with the correct precedence
  and demotion. Re-implementing a subset of that pipeline in our regen
  drifts behind whichllm's own ranking.
* AA Index + LiveBench cover modern frontier MoE releases (Qwen3.6,
  gpt-oss, GLM, DeepSeek V4, …) that OpenLLM Leaderboard v2 archived
  before they shipped. Without them every modern row collapsed to the
  recommender's score floor and the wizard surfaced two-generation-old
  Qwen 2.5 picks on hosts that should see Qwen3-30B-A3B class.

R45 single-binary invariant: this module runs in CI only.
"""

from __future__ import annotations

import asyncio
import sys
import traceback
from typing import Any, Dict, List

try:
    from . import whichllm as _whichllm  # noqa: F401  (attribution-shim import)
    from .whichllm import SourceResult
except ImportError:  # script-style import path
    from benchmark_sources import whichllm as _whichllm  # type: ignore[no-redef]  # noqa: F401
    from benchmark_sources.whichllm import SourceResult  # type: ignore[no-redef]


SOURCE_NAME = "whichllm-combined"

# Sanity floor on the merged dict. whichllm's bundled curated fallbacks
# for AA + LiveBench keep ``fetch_benchmark_scores`` returning hundreds
# of entries even with no network; a result below this threshold means
# the upstream surface drifted (function renamed, dict shape changed)
# and we'd be publishing a snapshot with no modern coverage. Refuse.
MIN_EXPECTED_SCORES = 100


def fetch() -> SourceResult:
    """Run whichllm's full multi-source merge and emit one row per
    scored model.

    Treats the merged dict as a single source: if whichllm itself
    can't produce a populated dict (network out *and* its bundled
    fallbacks empty, or the import surface drifted), we mark the
    source ``ok=False`` so the regen script's partial-failure policy
    refuses to publish and last-known-good stays live.
    """
    try:
        scores = asyncio.run(_invoke_whichllm())
    except Exception as exc:  # noqa: BLE001 — surface every failure mode
        traceback.print_exc(file=sys.stderr)
        return SourceResult(
            name=SOURCE_NAME,
            ok=False,
            rows=[],
            message=f"{type(exc).__name__}: {exc}",
        )
    if len(scores) < MIN_EXPECTED_SCORES:
        return SourceResult(
            name=SOURCE_NAME,
            ok=False,
            rows=[],
            message=(
                f"merged dict had only {len(scores)} entries "
                f"(< {MIN_EXPECTED_SCORES}); refusing to publish a snapshot "
                "without modern benchmark coverage."
            ),
        )
    rows: List[Dict[str, Any]] = [
        {"hf_id": hf_id, "score": float(score)} for hf_id, score in scores.items()
    ]
    return SourceResult(name=SOURCE_NAME, ok=True, rows=rows)


async def _invoke_whichllm() -> Dict[str, float]:
    """Call whichllm and return its lowercased CI index of scores.

    Using ``build_score_index`` rather than the raw dict means our
    downstream join is case-insensitive — different upstream sources
    case their model ids inconsistently and we don't want a
    ``Qwen/Qwen3-30B-A3B`` vs ``qwen/qwen3-30b-a3b`` mismatch to drop
    a real score on the floor.
    """
    from whichllm.models.benchmark import (  # type: ignore[import-not-found]
        build_score_index,
        fetch_benchmark_scores,
    )

    scores = await fetch_benchmark_scores()
    ci_index, _line_index = build_score_index(scores)
    return ci_index
