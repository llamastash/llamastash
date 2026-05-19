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

import json
import sys
import time
import traceback
from pathlib import Path
from typing import Dict

import httpx

# Support both package import (regen script) and direct script invocation
# (smoke harness: ``python scripts/benchmark_sources/open_llm_leaderboard.py``).
if __package__:
    from . import whichllm
    from .whichllm import SourceResult
else:  # pragma: no cover — only hit when run as a bare script
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from benchmark_sources import whichllm  # type: ignore[no-redef]
    from benchmark_sources.whichllm import SourceResult  # type: ignore[no-redef]

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

# Per-page response body cap (bytes). The rows API serves ~100 rows per
# page at ~5-15 KB each; a 5 MB cap absorbs metadata-fat pages with
# room to spare. A multi-GB response (compromised upstream, accidental
# binary blob, or attacker-controlled redirect target) is rejected
# before httpx buffers it into memory.
_MAX_RESPONSE_BYTES = 5 * 1024 * 1024

# Whole-adapter wall-clock budget. With ~45 pages today plus retries,
# a healthy run takes ~30s; this gives 20x headroom and still
# fast-fails inside the CI workflow's 30-min job timeout, so the
# documented ok=False path executes instead of GitHub silently killing
# the job.
_ADAPTER_BUDGET_SECS = 600.0

# Page-count safety net independent of the wall clock. A faulty
# ``num_rows_total`` that says "always more" would otherwise loop until
# either the wall-clock guard fires or the API rate-limits us into
# retries.
_MAX_PAGES = 500

# Required columns we extract from each row. Schema-drift guard.
_REQUIRED_COLUMNS = ("fullname", "Average ⬆️")

SOURCE_NAME = "open-llm-leaderboard"
ROW_SOURCE_TAG = "openllm-leaderboard"


# --- Helpers (verbatim from upstream) -----------------------------------


def _normalize_leaderboard_avg(avg: float) -> float:
    """Normalize Open LLM Leaderboard average to 0-_OLLB_MAX_NORMALIZED scale."""
    score = avg / _LB_AVG_MAX * _OLLB_MAX_NORMALIZED
    return max(0.0, min(_OLLB_MAX_NORMALIZED, round(score, 1)))


def _fetch_page_with_retry(
    client: httpx.Client, params: Dict[str, str]
) -> bytes:
    """Fetch one rows-API page with bounded retry + streaming size cap.

    Streams the response and counts bytes so an oversized body raises
    ``ExtractionFailed`` *before* httpx buffers it into memory. Retries
    transient HTTP statuses (5xx / 429) and ``httpx.TransportError``
    subclasses (ConnectError, ReadError, RemoteProtocolError) with
    linear backoff. Raises on retry exhaustion.
    """
    for attempt in range(_MAX_RETRIES):
        try:
            with client.stream(
                "GET", LEADERBOARD_ROWS_URL, params=params
            ) as resp:
                if (
                    resp.status_code in _RETRY_STATUSES
                    and attempt < _MAX_RETRIES - 1
                ):
                    time.sleep(_RETRY_BACKOFF_SECS * (attempt + 1))
                    continue
                resp.raise_for_status()
                return _read_capped(resp)
        except httpx.TransportError:
            if attempt == _MAX_RETRIES - 1:
                raise
            time.sleep(_RETRY_BACKOFF_SECS * (attempt + 1))
            continue
    raise RuntimeError("unreachable: _fetch_page_with_retry loop exhausted")


def _read_capped(resp: httpx.Response) -> bytes:
    """Read a streamed response, raising ``ExtractionFailed`` if the
    body exceeds :data:`_MAX_RESPONSE_BYTES`. Stops reading at the cap
    so a malicious / runaway upstream can't OOM the runner."""
    chunks = []
    total = 0
    for chunk in resp.iter_bytes():
        total += len(chunk)
        if total > _MAX_RESPONSE_BYTES:
            raise whichllm.ExtractionFailed(
                f"response body exceeded {_MAX_RESPONSE_BYTES} bytes "
                f"(read {total}); refusing to buffer"
            )
        chunks.append(chunk)
    return b"".join(chunks)


# --- Fetch --------------------------------------------------------------


def _fetch_rows(client: httpx.Client) -> Dict[str, float]:
    """Paginate the rows API. Returns a mapping ``hf_id -> normalized score``.

    Raises on HTTP non-2xx, JSON decode error, schema drift (missing
    required columns), oversized response body, or budget exhaustion
    (wall clock or page count).
    """
    scores: Dict[str, float] = {}
    offset = 0
    page_count = 0
    saw_required_columns = False
    start = time.monotonic()

    while True:
        elapsed = time.monotonic() - start
        if elapsed > _ADAPTER_BUDGET_SECS:
            raise whichllm.ExtractionFailed(
                f"adapter exceeded {_ADAPTER_BUDGET_SECS}s budget "
                f"({elapsed:.1f}s used, {page_count} pages)"
            )
        page_count += 1
        if page_count > _MAX_PAGES:
            raise whichllm.ExtractionFailed(
                f"adapter exceeded {_MAX_PAGES}-page cap; "
                f"upstream num_rows_total may be wrong"
            )
        params = {
            "dataset": LEADERBOARD_DATASET,
            "config": "default",
            "split": "train",
            "offset": str(offset),
            "length": str(_PAGE_SIZE),
        }
        body = _fetch_page_with_retry(client, params)
        data = json.loads(body)

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
            # avg is not None separates missing values from a legitimate
            # 0.0. isinstance excludes bool (which is an int subclass)
            # and rejects unexpected types like a stringly-typed score
            # without crashing the loop with TypeError.
            if (
                not isinstance(name, str)
                or not name
                or avg is None
                or isinstance(avg, bool)
                or not isinstance(avg, (int, float))
                or avg <= 0
            ):
                continue
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
