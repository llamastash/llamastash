"""Attribution shim for Andyyyy64/whichllm (MIT).

Holds the symbols our regen adapters share: vendoring metadata and the
``SourceResult`` dataclass used to communicate partial-failure state.

The original v2-GA plan (docs/plans/2026-05-19-001-feat-vendor-benchmark-
scrapers-plan.md) vendored two per-source adapters here (Open LLM
Leaderboard + Aider). That covered only 2 of whichllm's 6 sources and
dropped the layered current-over-frozen precedence plus the lineage
recency demotion — exactly the gap that left the wizard surfacing
two-generation-old picks. The follow-up collapses those adapters into
``whichllm_combined.py``, which delegates the full pipeline to
``whichllm.models.benchmark.fetch_benchmark_scores()``.

R45 single-binary invariant: none of this runs in the Rust artefact.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List

WHICHLLM_UPSTREAM_URL = "https://github.com/Andyyyy64/whichllm"
WHICHLLM_VENDORED_COMMIT = "73cd92f9a35a1c3f02e01ec3bbf09fb135a1df26"
WHICHLLM_VENDORED_DATE = "2026-05-19"
# Version of the upstream `whichllm` pip package this shim is meant to
# track. Unit 7's CI lint asserts this matches the installed package's
# `whichllm.__version__` before publishing the snapshot — drift means
# either the pin in scripts/requirements.txt or this constant was
# bumped without the other.
WHICHLLM_PINNED_VERSION = "0.5.7"


class ExtractionFailed(Exception):
    """Raised by adapters when upstream returned data we couldn't parse."""


@dataclass
class SourceResult:
    """One source's contribution to the snapshot regen. ``ok=False``
    blocks publication; see scripts/regenerate-benchmark-snapshot.py
    docstring for the partial-failure contract."""

    name: str
    ok: bool
    rows: List[Dict[str, Any]] = field(default_factory=list)
    message: str = ""
