"""Partial vendoring of Andyyyy64/whichllm (MIT).

Per the v2-GA plan (docs/plans/2026-05-19-001-feat-vendor-benchmark-
scrapers-plan.md) this module holds only the symbols the two CI-side
adapters (open_llm_leaderboard.py, aider.py) genuinely share.

Vendoring is intentionally minimal: each adapter re-implements its fetch
inline with stdlib + httpx, so this module stays a thin attribution shim
rather than a copy of whichllm's full benchmark-source layer. Re-syncs
move on demand (R57); when the upstream surface drifts, refresh the
adapters individually and bump the constants below in lockstep with
NOTICE.

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
