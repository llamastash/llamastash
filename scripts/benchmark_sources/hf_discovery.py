"""Live HuggingFace Hub discovery, sourced from whichllm.

Per Unit 3 of docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-
plan.md this module is the catalog *owner* for the daily regen flow.
It replaces the hand-curated ``BUNDLED_ID_TO_SOURCE_HF_ID`` table by
asking whichllm to enumerate every popular / trending / frontier model
on the Hub, filters to "has a usable GGUF from an allowlisted
publisher", and yields the rows the regen script merges with adapter
scores.

The whichllm import lives inside :func:`_fetch_via_whichllm` so this
module parses without whichllm being pip-installed locally — the CI
workflow runs ``pip install -r scripts/requirements.txt`` before
invoking the regen script.

R45 single-binary invariant: none of this runs in the Rust artefact.

## whichllm 0.5.7 contract (the shape this module assumes)

``whichllm.models.fetcher.fetch_models(limit, include_vision) -> coroutine``
returns ``list[ModelInfo]`` where each ``ModelInfo`` carries:

- ``id``: the HF repo. For published-GGUF rows this is the GGUF
  publisher's repo (e.g. ``bartowski/Qwen3-Coder-30B-GGUF``); for
  source-only rows it's the upstream weights repo.
- ``base_model``: the source HF id (e.g. ``Qwen/Qwen3-Coder-30B-A3B-
  Instruct``) when ``id`` is a GGUF mirror. Often ``None`` on
  publisher-org rows.
- ``parameter_count`` / ``parameter_count_active``: total / active
  params.
- ``is_moe``, ``architecture``, ``downloads``, ``published_at``.
- ``gguf_variants``: list of ``GGUFVariant(filename, quant_type,
  file_size_bytes)``. Files live under the same repo as ``id``.
"""

from __future__ import annotations

import asyncio
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple

try:
    import yaml  # type: ignore[import-not-found]
except ImportError:  # pragma: no cover — PyYAML is a regen-time CI dep
    yaml = None  # type: ignore[assignment]

from benchmark_sources.whichllm import SourceResult

# Preferred quants in priority order. The first available wins per
# candidate. Matches the recommender's assumption that Q4_K_M is the
# default footprint for un-downloaded models.
PREFERRED_QUANTS: Tuple[str, ...] = ("Q4_K_M", "Q4_K_S", "Q5_K_M")

# Per-quant rough density (GB per billion params) for sanity checks
# when HF doesn't expose the GGUF file size in metadata. Aligned with
# bartowski's published numbers as of 2026-05.
_QUANT_GB_PER_BPARAM: Dict[str, float] = {
    "Q4_K_M": 0.60,
    "Q4_K_S": 0.56,
    "Q5_K_M": 0.71,
}


@dataclass
class DiscoveredModel:
    """One catalog row, schema-aligned with ``ModelEntry`` in
    ``src/init/benchmark.rs``. The regen script consumes this and
    composes the final JSON, attaching benchmark scores and recency
    multipliers on top."""

    source_hf_id: str
    repo: str
    file: str
    architecture: str
    quant: str
    params: int
    weights_bytes: int
    is_moe: bool
    params_active: Optional[int]
    gguf_publisher: str
    downloads: int = 0
    last_modified: str = ""
    task_hints: List[str] = field(default_factory=list)


def load_task_hints(repo_root: Path) -> Tuple[Dict[str, List[str]], List[str]]:
    """Parse ``data/task-hints.yaml``. Returns ``(prefixes, defaults)``.

    ``prefixes`` is the longest-match-wins map; ``defaults`` applies
    when no prefix matches.
    """
    if yaml is None:
        raise RuntimeError(
            "PyYAML must be installed before calling load_task_hints; "
            "see scripts/requirements.txt"
        )
    path = repo_root / "data" / "task-hints.yaml"
    with path.open() as f:
        body = yaml.safe_load(f)
    prefixes = dict(body.get("prefixes") or {})
    defaults = list(body.get("defaults") or ["general"])
    return prefixes, defaults


def load_publisher_allowlist(repo_root: Path) -> List[str]:
    """Parse ``data/gguf-publisher-allowlist.yaml``."""
    if yaml is None:
        raise RuntimeError(
            "PyYAML must be installed before calling load_publisher_allowlist; "
            "see scripts/requirements.txt"
        )
    path = repo_root / "data" / "gguf-publisher-allowlist.yaml"
    with path.open() as f:
        body = yaml.safe_load(f)
    return list(body.get("allowlist") or [])


def attach_task_hints(
    source_hf_id: str,
    prefixes: Dict[str, List[str]],
    defaults: List[str],
) -> List[str]:
    """Longest-prefix-wins task hint lookup."""
    best_match: Optional[Tuple[int, List[str]]] = None
    for prefix, tags in prefixes.items():
        if source_hf_id.startswith(prefix):
            if best_match is None or len(prefix) > best_match[0]:
                best_match = (len(prefix), list(tags))
    if best_match is None:
        return list(defaults)
    return best_match[1]


def discover(
    repo_root: Path,
    limit: int = 100,
    include_vision: bool = False,
    whichllm_limit: int = 300,
) -> SourceResult:
    """Run whichllm's discovery, filter to GGUF-bearing candidates from
    allowlisted publishers, and return the rows as a SourceResult.

    ``include_vision=False`` per Key Decisions: v2 does not ship vision
    / multimodal models. ``whichllm_limit`` controls how many models
    whichllm tries to enumerate from HF before this module filters
    down to ``limit`` rows.

    Failure modes follow the partial-failure policy: any exception
    inside whichllm bubbles up as ``ok=False`` so the regen script
    keeps last-known-good live.
    """
    rows: List[Dict[str, Any]] = []
    try:
        candidates = _fetch_via_whichllm(
            whichllm_limit=whichllm_limit, include_vision=include_vision
        )
        prefixes, defaults = load_task_hints(repo_root)
        allowlist = load_publisher_allowlist(repo_root)
        for candidate in candidates:
            row = _project_candidate(candidate, allowlist, prefixes, defaults)
            if row is None:
                continue
            rows.append(_serialise(row))
        rows = _rank_and_cap(rows, limit=limit)
    except Exception as exc:  # noqa: BLE001 — surface every failure mode
        return SourceResult(
            name="hf-discovery",
            ok=False,
            rows=[],
            message=f"{type(exc).__name__}: {exc}",
        )
    return SourceResult(name="hf-discovery", ok=True, rows=rows)


def _fetch_via_whichllm(
    *, whichllm_limit: int, include_vision: bool
) -> Sequence[Any]:
    """Import whichllm lazily and run its async catalog fetcher.

    The exact import path here tracks whichllm 0.5.x; bump
    ``scripts/requirements.txt`` and re-test if the upstream surface
    moves. The CI lockstep assertion in Unit 7 catches version drift
    before publication.
    """
    from whichllm.models.fetcher import fetch_models  # type: ignore[import-not-found]

    coro = fetch_models(limit=whichllm_limit, include_vision=include_vision)
    return asyncio.run(coro)


def _project_candidate(
    candidate: Any,
    allowlist: Sequence[str],
    prefixes: Dict[str, List[str]],
    defaults: Sequence[str],
) -> Optional[DiscoveredModel]:
    """Map a whichllm ModelInfo to a DiscoveredModel, returning None
    when no acceptable GGUF exists from a trusted publisher.

    Strategy:
      * The candidate's ``id`` is the GGUF repo (whichllm rows are
        GGUF-publisher repos, not source weight repos). The publisher
        is therefore ``id.split('/')[0]``.
      * Accept the candidate iff that publisher is in the allowlist
        OR matches the model family's official org (derived from
        ``base_model``).
      * Pick the first preferred quant the publisher ships.
      * ``source_hf_id`` comes from ``base_model`` when whichllm has
        it, otherwise falls back to the candidate id (so the regen
        joins against adapter scores under the same key).
    """
    repo = _attr(candidate, "id")
    if not repo or "/" not in repo:
        return None
    publisher = repo.split("/", 1)[0]

    params = _attr(candidate, "parameter_count") or _attr(candidate, "params")
    if not params:
        return None
    params = int(params)

    params_active_raw = _attr(candidate, "parameter_count_active") or _attr(
        candidate, "params_active"
    )
    params_active = int(params_active_raw) if params_active_raw else None
    is_moe = bool(_attr(candidate, "is_moe"))

    base_model = _attr(candidate, "base_model")
    source_hf_id = base_model if isinstance(base_model, str) and base_model else repo

    variants = _attr(candidate, "gguf_variants") or _attr(candidate, "ggufs") or []
    if not variants:
        return None

    if not _publisher_trusted(publisher, source_hf_id, allowlist):
        return None

    chosen = _pick_variant(variants)
    if chosen is None:
        return None
    quant, file, weights_bytes = chosen

    architecture = (
        _attr(candidate, "architecture")
        or _attr(candidate, "model_type")
        or "unknown"
    )
    if weights_bytes <= 0:
        weights_bytes = _estimate_weights_bytes(params, quant)

    task_hints = list(attach_task_hints(source_hf_id, prefixes, list(defaults)))

    return DiscoveredModel(
        source_hf_id=source_hf_id,
        repo=repo,
        file=file,
        architecture=architecture,
        quant=quant,
        params=params,
        weights_bytes=weights_bytes,
        is_moe=is_moe,
        params_active=params_active,
        gguf_publisher=publisher,
        downloads=int(_attr(candidate, "downloads") or 0),
        last_modified=str(
            _attr(candidate, "published_at") or _attr(candidate, "last_modified") or ""
        ),
        task_hints=task_hints,
    )


def _publisher_trusted(
    publisher: str,
    source_hf_id: str,
    allowlist: Sequence[str],
) -> bool:
    """A publisher is trusted iff it appears in the allowlist OR it
    matches the model family's own org (so first-party GGUFs from
    e.g. ``Qwen/Qwen3-Next-GGUF`` always qualify even when the org
    isn't on the allowlist explicitly)."""
    if publisher in allowlist:
        return True
    if "/" in source_hf_id:
        source_org = source_hf_id.split("/", 1)[0]
        if publisher == source_org:
            return True
    return False


def _pick_variant(variants: Iterable[Any]) -> Optional[Tuple[str, str, int]]:
    """Pick the first variant matching ``PREFERRED_QUANTS`` (in order).
    Returns ``(quant, filename, file_size_bytes)`` or None.

    Sharded models surface as multiple variants with the same quant
    but different files (e.g., ``...-00001-of-00003.gguf``); take the
    first such filename — the regen script doesn't need to know about
    the rest because the recommender's weights_bytes estimate is the
    summed footprint anyway.
    """
    by_quant: Dict[str, List[Any]] = {}
    for v in variants:
        q = (_attr(v, "quant_type") or _attr(v, "quant") or "").upper()
        if q in PREFERRED_QUANTS:
            by_quant.setdefault(q, []).append(v)
    for quant in PREFERRED_QUANTS:
        bucket = by_quant.get(quant)
        if not bucket:
            continue
        first = bucket[0]
        filename = _attr(first, "filename") or _attr(first, "file") or ""
        size = int(
            _attr(first, "file_size_bytes")
            or _attr(first, "size")
            or _attr(first, "weights_bytes")
            or 0
        )
        if filename:
            return quant, filename, size
    return None


def _estimate_weights_bytes(params: int, quant: str) -> int:
    """Fallback when HF metadata lacks file size. ``params`` × density."""
    density = _QUANT_GB_PER_BPARAM.get(quant.upper(), 0.65)
    return int(params * density)


def _attr(obj: Any, name: str) -> Any:
    """Read either an attribute or a dict key — whichllm has used both
    representations across releases, and our unit tests stub with
    dicts."""
    if obj is None:
        return None
    if hasattr(obj, name):
        return getattr(obj, name)
    if isinstance(obj, dict):
        return obj.get(name)
    return None


def _serialise(model: DiscoveredModel) -> Dict[str, Any]:
    """Turn a DiscoveredModel into the dict the regen script merges
    into ``models[]``. Mirrors the Rust ``ModelEntry`` field names so
    the JSON round-trips through serde with `#[serde(default)]` on the
    new fields."""
    return {
        "source_hf_id": model.source_hf_id,
        "repo": model.repo,
        "file": model.file,
        "architecture": model.architecture,
        "quant": model.quant,
        "params": model.params,
        "weights_bytes": model.weights_bytes,
        "task_hints": list(model.task_hints),
        "is_moe": model.is_moe,
        "params_active": model.params_active,
        "gguf_publisher": model.gguf_publisher,
        "downloads": model.downloads,
        "last_modified": model.last_modified,
    }


# Quant-filename matcher kept for backwards-compat — unused now that
# whichllm exposes ``quant_type`` directly, but the unit tests still
# pin the regex shape so a future re-introduction has a known-good
# reference.
def _quant_matches_filename(name: str, quant: str) -> bool:
    """Match common GGUF filename shapes like ``model-Q4_K_M.gguf`` or
    ``model.Q4_K_M.gguf``. Case-insensitive."""
    pattern = re.compile(rf"[-._]{re.escape(quant)}\.gguf$", re.IGNORECASE)
    return bool(pattern.search(name))


def _rank_and_cap(rows: List[Dict[str, Any]], *, limit: int) -> List[Dict[str, Any]]:
    """Rank by downloads × recency proxy, dedupe per
    ``(source_hf_id, quant)`` keeping the highest-download GGUF
    publisher, then cap to ``limit``.

    Multiple GGUF publishers (bartowski, mradermacher, lmstudio,
    unsloth, the source org's own GGUF release) often re-host the
    same upstream model at the same quant. Without this dedup the
    snapshot ends up with three identical rows for the same
    ``(source_hf_id, quant)`` slug — the recommender then surfaces
    them as separate picks and the wizard's recommendation list
    repeats itself. Sort-then-keep-first by downloads picks the
    most-trusted host (since publisher reputation correlates
    strongly with download count) without needing a curated table.
    """
    rows.sort(
        key=lambda r: (r.get("downloads", 0), r.get("last_modified", "")), reverse=True
    )
    deduped: List[Dict[str, Any]] = []
    seen: set[Tuple[str, str]] = set()
    for row in rows:
        key = (row.get("source_hf_id") or "", row.get("quant") or "")
        if key in seen:
            continue
        seen.add(key)
        deduped.append(row)
    return deduped[:limit]
