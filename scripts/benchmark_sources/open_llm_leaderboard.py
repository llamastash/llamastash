"""Open LLM Leaderboard adapter — rows-API path only.

Upstream reference: ``Andyyyy64/whichllm`` (MIT), file
``src/whichllm/models/benchmark_sources/open_llm_leaderboard.py``.
URL: see ``whichllm.WHICHLLM_UPSTREAM_URL``.
Pinned commit: see ``whichllm.WHICHLLM_VENDORED_COMMIT``
(vendored on ``whichllm.WHICHLLM_VENDORED_DATE``).

Purpose: fetch HuggingFace ``open-llm-leaderboard/contents`` rows,
normalize each model's ``Average ⬆️`` to a 0-78 scale, and emit
``SourceResult`` rows keyed by HuggingFace ``fullname``. The regen
script joins these into the bundled snapshot's ``models[]`` via the
GGUF-repo → source-HF-id map owned by a later unit.

We deliberately vendor only the rows API. The upstream's parquet path
drags ``pyarrow`` into CI for a fallback the rows API already
satisfies, and ``pyarrow`` wheels are heavy.

R45 single-binary invariant: this module runs in CI only — it produces
the JSON artefact the Rust binary reads via ``include_str!``. Nothing
here ships in the compiled binary.
"""

from __future__ import annotations

import sys
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List

import httpx

# Support both package import (regen script) and direct script invocation
# (smoke harness: ``python scripts/benchmark_sources/open_llm_leaderboard.py``).
if __package__:
    from . import whichllm  # noqa: F401 — re-exported metadata
else:  # pragma: no cover — only hit when run as a bare script
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from benchmark_sources import whichllm  # type: ignore[no-redef]

# --- Constants (verbatim from upstream where noted) ----------------------

LEADERBOARD_ROWS_URL = "https://datasets-server.huggingface.co/rows"
LEADERBOARD_DATASET = "open-llm-leaderboard/contents"

# Verbatim from upstream `_LB_AVG_MAX` / `_OLLB_MAX_NORMALIZED`.
# OLLB v2 averages range ~5 to ~52. Leaderboard archived 2025-06 with the
# top slot held by Qwen2.5-32B (47.6 raw); capping at 78 prevents a
# strong-but-frozen score from dominating rankings that now have AA Index
# / LiveBench coverage too.
_LB_AVG_MAX = 52
_OLLB_MAX_NORMALIZED = 78.0

# Per-request HTTP timeout (seconds). Total adapter budget stays well
# under 5 minutes given OLLB has < ~3k rows at 100 rows/request.
_REQUEST_TIMEOUT_SECS = 30.0

# Pagination page size (matches upstream).
_PAGE_SIZE = 100

# Transient-failure retry policy. The datasets-server rows API has been
# observed to return 502 mid-pagination and 429 under back-to-back use.
# Retrying these once or twice with a short backoff turns "flaky daily
# run" into "consistently fresh daily run" without weakening the hard-
# fail contract — a true outage still surfaces as ok=False after the
# budget is exhausted.
_RETRY_STATUSES = frozenset({429, 500, 502, 503, 504})
_MAX_RETRIES = 3
_RETRY_BACKOFF_SECS = 2.0

# Required columns we extract from each row. Schema-drift guard.
_REQUIRED_COLUMNS = ("fullname", "Average ⬆️")

SOURCE_NAME = "open-llm-leaderboard"
ROW_SOURCE_TAG = "openllm-leaderboard"


# --- Local SourceResult shim --------------------------------------------
# Mirrors `SourceResult` in scripts/regenerate-benchmark-snapshot.py:51.
# Kept as a local redeclaration (5-line redundancy) because the regen
# script is not a package; restructuring it for one shared dataclass is
# more disruption than warranted. KEEP IN SYNC with that definition.
@dataclass
class SourceResult:
    name: str
    ok: bool
    rows: List[Dict[str, Any]] = field(default_factory=list)
    message: str = ""


# --- Helpers (verbatim from upstream) -----------------------------------


def _normalize_leaderboard_avg(avg: float) -> float:
    """Normalize Open LLM Leaderboard average to 0-_OLLB_MAX_NORMALIZED scale."""
    score = avg / _LB_AVG_MAX * _OLLB_MAX_NORMALIZED
    return max(0.0, min(_OLLB_MAX_NORMALIZED, round(score, 1)))


def _get_with_retry(
    client: httpx.Client, url: str, params: Dict[str, str]
) -> httpx.Response:
    """GET with bounded retry on transient HTTP statuses. Returns the
    final response (still calls ``raise_for_status`` upstream — this
    helper only converts transient flakes into a brief wait-and-retry."""
    last_exc: Exception | None = None
    for attempt in range(_MAX_RETRIES):
        try:
            resp = client.get(url, params=params)
        except httpx.TimeoutException as exc:
            last_exc = exc
            if attempt == _MAX_RETRIES - 1:
                raise
            time.sleep(_RETRY_BACKOFF_SECS * (attempt + 1))
            continue
        if resp.status_code in _RETRY_STATUSES and attempt < _MAX_RETRIES - 1:
            time.sleep(_RETRY_BACKOFF_SECS * (attempt + 1))
            continue
        return resp
    # Loop exits only via return / raise; this is a defensive guard so
    # the typechecker sees a definite return.
    raise last_exc if last_exc else RuntimeError("_get_with_retry exhausted")


# --- Fetch --------------------------------------------------------------


def _fetch_rows(client: httpx.Client) -> Dict[str, float]:
    """Paginate the rows API. Returns a mapping ``hf_id -> normalized score``.

    Raises on HTTP non-2xx (via ``raise_for_status``), JSON decode error,
    or schema drift (missing required columns).
    """
    scores: Dict[str, float] = {}
    offset = 0
    saw_required_columns = False

    while True:
        params = {
            "dataset": LEADERBOARD_DATASET,
            "config": "default",
            "split": "train",
            "offset": str(offset),
            "length": str(_PAGE_SIZE),
        }
        resp = _get_with_retry(client, LEADERBOARD_ROWS_URL, params)
        resp.raise_for_status()
        data = resp.json()

        # Schema-drift guard: validate column metadata on the first page
        # (the rows API echoes a ``features`` block listing column names).
        if not saw_required_columns:
            features = data.get("features") or []
            feature_names = {
                f.get("name") for f in features if isinstance(f, dict)
            }
            missing = [c for c in _REQUIRED_COLUMNS if c not in feature_names]
            if missing:
                raise whichllm.ExtractionFailed(
                    f"missing required columns: {missing!r} "
                    f"(saw: {sorted(feature_names)!r})"
                )
            saw_required_columns = True

        rows = data.get("rows", [])
        if not rows:
            break

        for r in rows:
            row = r.get("row", {})
            name = row.get("fullname")
            avg = row.get("Average ⬆️")
            if name and avg and avg > 0:
                scores[name] = _normalize_leaderboard_avg(avg)

        offset += len(rows)
        total = data.get("num_rows_total", 0)
        if total and offset >= total:
            break

    return scores


def fetch() -> SourceResult:
    """Synchronous entry point. Returns a ``SourceResult``.

    Hard-fails (``ok=False``) on any of: network timeout, HTTP non-2xx,
    JSON parse error, schema drift, or an empty result set. Never raises
    — the regen script's ``collect_sources()`` treats each adapter as
    independent and routes failures through the ``ok=False`` channel.
    """
    try:
        with httpx.Client(timeout=_REQUEST_TIMEOUT_SECS) as client:
            scores = _fetch_rows(client)
    except httpx.TimeoutException as e:
        return SourceResult(
            name=SOURCE_NAME, ok=False, rows=[], message=f"timeout: {e}"
        )
    except httpx.HTTPStatusError as e:
        return SourceResult(
            name=SOURCE_NAME,
            ok=False,
            rows=[],
            message=f"http {e.response.status_code}: {e.request.url}",
        )
    except httpx.HTTPError as e:
        return SourceResult(
            name=SOURCE_NAME, ok=False, rows=[], message=f"http error: {e}"
        )
    except ValueError as e:
        # json.JSONDecodeError is a subclass of ValueError.
        return SourceResult(
            name=SOURCE_NAME, ok=False, rows=[], message=f"parse error: {e}"
        )
    except whichllm.ExtractionFailed as e:
        return SourceResult(
            name=SOURCE_NAME, ok=False, rows=[], message=f"schema drift: {e}"
        )
    except Exception as e:  # pragma: no cover — last-resort guard
        return SourceResult(
            name=SOURCE_NAME,
            ok=False,
            rows=[],
            message=f"unexpected: {type(e).__name__}: {e}",
        )

    if not scores:
        return SourceResult(
            name=SOURCE_NAME,
            ok=False,
            rows=[],
            message="empty result set (upstream returned 0 usable rows)",
        )

    rows = [
        {"hf_id": hf_id, "score": score, "source": ROW_SOURCE_TAG}
        for hf_id, score in scores.items()
    ]
    return SourceResult(name=SOURCE_NAME, ok=True, rows=rows, message="")


# --- Smoke harness ------------------------------------------------------


if __name__ == "__main__":
    try:
        result = fetch()
    except Exception:
        traceback.print_exc()
        sys.exit(1)

    assert isinstance(result, SourceResult), "fetch() must return SourceResult"
    assert result.name == SOURCE_NAME, f"unexpected name: {result.name!r}"

    print(f"ok={result.ok}")
    print(f"rows_count={len(result.rows)}")
    if result.message:
        print(f"message={result.message}")
    print("first_3_rows=")
    for row in result.rows[:3]:
        print(f"  {row}")

    sys.exit(0 if result.ok else 1)
