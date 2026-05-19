"""Aider polyglot benchmark adapter — sync entry point ``fetch()``.

Upstream reference: ``Andyyyy64/whichllm`` (MIT), file
``src/whichllm/models/benchmark_sources/aider.py``.
URL: see ``whichllm.WHICHLLM_UPSTREAM_URL``.
Pinned commit: see ``whichllm.WHICHLLM_VENDORED_COMMIT``
(vendored on ``whichllm.WHICHLLM_VENDORED_DATE``).

Purpose: fetch Aider's ``polyglot_leaderboard.yml`` from raw.githubusercontent,
parse it with a regex-based mini-YAML extractor (avoids the PyYAML dep),
take the best ``pass_rate_2`` per Aider model name, map names to HuggingFace
ids via the curated ``AIDER_NAME_TO_HF_IDS`` table, normalize raw pass rates
(0-90) to a 0-100 scale, and emit ``SourceResult`` rows keyed by HF id.

The ``AIDER_NAME_TO_HF_IDS`` mapping is curated maintainer data — that's the
entire point of vendoring this source rather than re-deriving the join. When
upstream adds entries we resync verbatim.

R45 single-binary invariant: this module runs in CI only — it produces the
JSON artefact the Rust binary reads via ``include_str!``. Nothing here ships
in the compiled binary.
"""

from __future__ import annotations

import re
import sys
import traceback
from pathlib import Path
from typing import Dict, List, Tuple

import httpx

# Support both package import (regen script) and direct script invocation
# (smoke harness: ``python scripts/benchmark_sources/aider.py``).
if __package__:
    from . import whichllm
    from .whichllm import SourceResult
else:  # pragma: no cover — only hit when run as a bare script
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from benchmark_sources import whichllm  # type: ignore[no-redef]
    from benchmark_sources.whichllm import SourceResult  # type: ignore[no-redef]

# --- Constants (verbatim from upstream where noted) ----------------------

AIDER_POLYGLOT_YML_URL = (
    "https://raw.githubusercontent.com/Aider-AI/aider/main/"
    "aider/website/_data/polyglot_leaderboard.yml"
)

# Verbatim from upstream ``_PG_MIN`` / ``_PG_MAX``. Polyglot pass-rate is a
# percent of exercises passing; treat 0..90 as the practical floor/ceiling
# since the cap of strong models historically tops out near 88%.
_PG_MIN = 0.0
_PG_MAX = 90.0

# Per-request HTTP timeout (seconds). One GET, small file.
_REQUEST_TIMEOUT_SECS = 30.0

# Response body cap. polyglot_leaderboard.yml is ~few-KB to low-tens-of-KB
# in practice; 2 MB absorbs growth with substantial headroom and refuses
# anything that would OOM the runner.
_MAX_RESPONSE_BYTES = 2 * 1024 * 1024

SOURCE_NAME = "aider"
ROW_SOURCE_TAG = "aider-polyglot"

# Curated Aider-name → HuggingFace-id mapping. Vendored from upstream
# whichllm with two local additions (qwen2.5-coder-7b/14b-instruct) so
# every bundled snapshot row tagged ``"aider"`` has at least an entry
# here — without that, upstream additions for those models would not
# get picked up. Mapping is hardcoded by design: it's part of the
# curation surface, not data, so corpus changes get explicit review.
AIDER_NAME_TO_HF_IDS: dict[str, list[str]] = {
    "deepseek-r1": ["deepseek-ai/DeepSeek-R1"],
    "deepseek-r1-0528": ["deepseek-ai/DeepSeek-R1-0528"],
    "deepseek-v3": ["deepseek-ai/DeepSeek-V3"],
    "deepseek-v3-0324": ["deepseek-ai/DeepSeek-V3-0324"],
    "deepseek-v3.1": ["deepseek-ai/DeepSeek-V3.1"],
    "deepseek-v3.2": ["deepseek-ai/DeepSeek-V3.2"],
    "deepseek-v4-pro": ["deepseek-ai/DeepSeek-V4-Pro"],
    "deepseek-v4-flash": ["deepseek-ai/DeepSeek-V4-Flash"],
    "qwen3-coder-30b-a3b-instruct": ["Qwen/Qwen3-Coder-30B-A3B-Instruct"],
    "qwen3-coder-next": ["Qwen/Qwen3-Coder-Next"],
    "qwen2.5-coder-7b-instruct": ["Qwen/Qwen2.5-Coder-7B-Instruct"],
    "qwen2.5-coder-14b-instruct": ["Qwen/Qwen2.5-Coder-14B-Instruct"],
    "qwen2.5-coder-32b-instruct": ["Qwen/Qwen2.5-Coder-32B-Instruct"],
    "qwen3-32b": ["Qwen/Qwen3-32B"],
    "qwen3.6-27b": ["Qwen/Qwen3.6-27B"],
    "llama-3.3-70b-instruct": ["meta-llama/Llama-3.3-70B-Instruct"],
    "llama-4-maverick": ["meta-llama/Llama-4-Maverick-17B-128E-Instruct"],
    "gemma-3-27b-it": ["google/gemma-3-27b-it"],
    "gemma-4-31b": ["google/gemma-4-31b-it"],
    "mistral-large-2411": ["mistralai/Mistral-Large-Instruct-2411"],
    "devstral-small": ["mistralai/Devstral-Small-2505"],
    "gpt-oss-120b": ["openai/gpt-oss-120b"],
    "gpt-oss-20b": ["openai/gpt-oss-20b"],
    "glm-4.5": ["zai-org/GLM-4.5"],
    "glm-4.6": ["zai-org/GLM-4.6"],
    "glm-5": ["zai-org/GLM-5"],
    "glm-5.1": ["zai-org/GLM-5.1"],
    "kimi-k2-instruct": ["moonshotai/Kimi-K2-Instruct"],
    "phi-4": ["microsoft/phi-4"],
    "qwq-32b": ["Qwen/QwQ-32B"],
}


# --- Helpers (verbatim from upstream) -----------------------------------


def _normalize(pass_rate: float) -> float:
    """Normalize raw Aider pass_rate (0-90 practical range) to 0-100."""
    # bool is a subclass of int; exclude it so True/False can't slip
    # through as 1.0/0.0 numeric scores.
    if isinstance(pass_rate, bool) or not isinstance(pass_rate, (int, float)):
        return 0.0
    span = _PG_MAX - _PG_MIN
    normalized = (pass_rate - _PG_MIN) / span * 100.0
    return max(0.0, min(100.0, round(normalized, 1)))


def _parse_yaml_lite(text: str) -> List[Tuple[str, float]]:
    """Tiny YAML extractor for the polyglot leaderboard format.

    We avoid pulling in PyYAML; the file shape is stable enough that two
    regexes scanning each record block suffice. Each record looks like::

        - dirname: 2024-12-22-blah
          model: deepseek/deepseek-chat
          edit_format: diff
          pass_rate_2: 80.7
          ...
    """
    out: List[Tuple[str, float]] = []
    # Split into records starting with "- "
    records = re.split(r"\n(?=-\s+\w)", text)
    for rec in records:
        m_model = re.search(r"^\s*model[:\s]+(.+?)$", rec, re.MULTILINE | re.IGNORECASE)
        m_rate = re.search(r"pass_rate_2[:\s]+(\d+(?:\.\d+)?)", rec, re.IGNORECASE)
        if not m_model or not m_rate:
            continue
        # Strip any trailing inline YAML comment before normalising the name.
        # Upstream doesn't use them today, but `foo # bar` would otherwise
        # become the lookup key and silently miss the curated map.
        raw_name = m_model.group(1).split("#", 1)[0]
        name = raw_name.strip().strip("\"'")
        # Strip any provider prefix like "deepseek/" or "openrouter/"
        name = name.split("/", 1)[-1].strip().lower()
        try:
            rate = float(m_rate.group(1))
        except ValueError:
            continue
        if rate <= 0:
            continue
        out.append((name, rate))
    return out


# --- Fetch --------------------------------------------------------------


def _fetch_scores(client: httpx.Client) -> Dict[str, float]:
    """Fetch and parse the polyglot leaderboard YAML.

    Streams the response with a body-size cap so a malicious / runaway
    upstream can't OOM the runner. Returns a mapping
    ``hf_id -> normalized score``. Raises on HTTP non-2xx
    (``raise_for_status``), oversized body (``ExtractionFailed``), or
    when the parser returns 0 records (``ExtractionFailed``).
    """
    chunks: list[bytes] = []
    total = 0
    with client.stream("GET", AIDER_POLYGLOT_YML_URL) as resp:
        resp.raise_for_status()
        for chunk in resp.iter_bytes():
            total += len(chunk)
            if total > _MAX_RESPONSE_BYTES:
                raise whichllm.ExtractionFailed(
                    f"polyglot_leaderboard.yml exceeded "
                    f"{_MAX_RESPONSE_BYTES} bytes (read {total}); "
                    f"refusing to buffer"
                )
            chunks.append(chunk)
    text = b"".join(chunks).decode("utf-8", errors="replace")
    pairs = _parse_yaml_lite(text)
    if not pairs:
        raise whichllm.ExtractionFailed(
            "polyglot_leaderboard.yml parsed to 0 records "
            "(format drift or empty upstream)"
        )

    # Take the best pass_rate_2 per Aider model name (multiple edit_format
    # records can share a model name; we keep the strongest).
    best_by_name: Dict[str, float] = {}
    for name, rate in pairs:
        cur = best_by_name.get(name)
        if cur is None or rate > cur:
            best_by_name[name] = rate

    scores: Dict[str, float] = {}
    for name, rate in best_by_name.items():
        ids = AIDER_NAME_TO_HF_IDS.get(name)
        if not ids:
            continue
        normalized = _normalize(rate)
        if normalized <= 0:
            continue
        for hf_id in ids:
            if scores.get(hf_id, 0.0) < normalized:
                scores[hf_id] = normalized
    return scores


def fetch() -> SourceResult:
    """Synchronous entry point. Returns a ``SourceResult``.

    Hard-fails (``ok=False``) on any of: network timeout, HTTP non-2xx,
    empty parse result (``ExtractionFailed``), or zero mapped scores after
    the HF id join. Never raises — the regen script's ``collect_sources()``
    treats each adapter as independent and routes failures through the
    ``ok=False`` channel.
    """
    try:
        # follow_redirects=True so a future GitHub repo rename of
        # Aider-AI/aider doesn't silently fail the daily cron with an
        # unhelpful 301; raw.githubusercontent.com remains the only
        # hostname we'll ever land on by following.
        with httpx.Client(
            timeout=_REQUEST_TIMEOUT_SECS, follow_redirects=True
        ) as client:
            scores = _fetch_scores(client)
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
    except whichllm.ExtractionFailed as e:
        return SourceResult(
            name=SOURCE_NAME, ok=False, rows=[], message=f"parse error: {e}"
        )
    except ValueError as e:
        return SourceResult(
            name=SOURCE_NAME, ok=False, rows=[], message=f"parse error: {e}"
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
            message="empty result set (no Aider names mapped to HF ids)",
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
