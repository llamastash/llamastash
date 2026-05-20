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

# Preferred quants the snapshot ships, ordered loosely from "smallest
# / fastest" to "largest / most faithful". Every quant in this list
# that a trusted GGUF publisher ships becomes a separate snapshot row,
# so the recommender can pick across the quant axis (e.g. Q8_0 on a
# 64 GB box, Q3_K_M on a 12 GB box). Spans the same range as
# `whichllm --json --top 10` for direct comparison.
PREFERRED_QUANTS: Tuple[str, ...] = (
    "Q3_K_M",
    "Q4_K_S",
    "Q4_K_M",
    "Q5_K_M",
    "Q6_K",
    "Q8_0",
)

# Per-quant rough density (GB per billion params) for sanity checks
# when HF doesn't expose the GGUF file size in metadata. Aligned with
# bartowski's published numbers as of 2026-05.
_QUANT_GB_PER_BPARAM: Dict[str, float] = {
    "Q3_K_M": 0.46,
    "Q4_K_S": 0.56,
    "Q4_K_M": 0.60,
    "Q5_K_M": 0.71,
    "Q6_K": 0.82,
    "Q8_0": 1.06,
}

# Per-quant speed multiplier on the params-based tok/s baseline.
# Smaller quants move fewer bytes per token, so they're faster on
# memory-bandwidth-bound LLM inference. Kept gentle (±15%) so the
# speed term doesn't drown out the quality discount below — without
# this restraint Q3_K_M would always outrank Q4_K_M of the same
# model since the composite score's `tok_per_second` weight (0.25)
# is large enough that a 30% raw speed gain wipes out a 4% quality
# drop. Real-world bandwidth ratios are larger, but for ranking the
# recommender values "good fit + good quality" over raw throughput.
_QUANT_SPEED_MULT: Dict[str, float] = {
    "Q3_K_M": 1.05,
    "Q4_K_S": 1.00,
    "Q4_K_M": 1.00,
    "Q5_K_M": 0.96,
    "Q6_K": 0.93,
    "Q8_0": 0.87,
}

# Per-quant quality discount applied to the family's benchmark score.
# Q8 is essentially loss-free; Q4_K_M loses a few points on hard
# evals; Q3_K_M loses ~5-7%. Combined with the gentle speed mults
# above, this leaves Q4_K_M as the default winner within a family
# (best quality/speed/size tradeoff) and Q8_0 surfacing only when
# Q4_K_M doesn't fit — matching the de facto consumer default.
_QUANT_QUALITY_MULT: Dict[str, float] = {
    "Q3_K_M": 0.94,
    "Q4_K_S": 0.97,
    "Q4_K_M": 0.98,
    "Q5_K_M": 0.99,
    "Q6_K": 0.995,
    "Q8_0": 1.0,
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
            for row in _project_candidate(candidate, allowlist, prefixes, defaults):
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
) -> List[DiscoveredModel]:
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
        return []

    if not _publisher_trusted(publisher, source_hf_id, allowlist):
        return []

    chosen = _collect_variants(variants)
    if not chosen:
        return []

    architecture = (
        _attr(candidate, "architecture")
        or _attr(candidate, "model_type")
        or "unknown"
    )
    task_hints = list(attach_task_hints(source_hf_id, prefixes, list(defaults)))
    downloads = int(_attr(candidate, "downloads") or 0)
    last_modified = str(
        _attr(candidate, "published_at") or _attr(candidate, "last_modified") or ""
    )

    rows: List[DiscoveredModel] = []
    for quant, file, weights_bytes in chosen:
        if weights_bytes <= 0:
            weights_bytes = _estimate_weights_bytes(params, quant)
        rows.append(
            DiscoveredModel(
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
                downloads=downloads,
                last_modified=last_modified,
                task_hints=list(task_hints),
            )
        )
    return rows


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


def _collect_variants(variants: Iterable[Any]) -> List[Tuple[str, str, int]]:
    """Return one ``(quant, filename, file_size_bytes)`` tuple per
    preferred quant the publisher ships. Order matches
    :data:`PREFERRED_QUANTS`.

    Sharded models surface as multiple variants with the same quant
    but different files (e.g., ``...-00001-of-00003.gguf``); take the
    first such filename per quant — the regen script doesn't need to
    know about the rest because the recommender's weights_bytes
    estimate is the summed footprint anyway.

    Multi-quant emission lets the recommender pick across the quant
    axis (Q8_0 on a fat box, Q3_K_M when VRAM is tight). The Rust-side
    dedup keeps one row per ``source_hf_id`` in the user-facing top-N
    so the picker still shows a variety of models, not five quants of
    the same one.
    """
    by_quant: Dict[str, List[Any]] = {}
    for v in variants:
        q = (_attr(v, "quant_type") or _attr(v, "quant") or "").upper()
        if q in PREFERRED_QUANTS:
            by_quant.setdefault(q, []).append(v)
    out: List[Tuple[str, str, int]] = []
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
            out.append((quant, filename, size))
    return out


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
    publisher, then cap so we keep all preferred quants of the top
    ``limit`` *unique* source models.

    Multiple GGUF publishers (bartowski, mradermacher, lmstudio,
    unsloth, the source org's own GGUF release) often re-host the
    same upstream model at the same quant. Without dedup the
    snapshot ends up with three identical rows for the same
    ``(source_hf_id, quant)`` slug — the recommender then surfaces
    them as separate picks and the wizard's recommendation list
    repeats itself. Sort-then-keep-first by downloads picks the
    most-trusted host (since publisher reputation correlates
    strongly with download count) without needing a curated table.

    Capping on *unique source models* (not row count) is important
    once multi-quant emission is in play: a flat ``[:limit]`` would
    truncate mid-quant-set for the last few models, producing
    asymmetric coverage. Counting source ids keeps the budget
    interpretable as "how many distinct models the snapshot ships".
    """
    rows.sort(
        key=lambda r: (r.get("downloads", 0), r.get("last_modified", "")), reverse=True
    )
    deduped: List[Dict[str, Any]] = []
    seen_pair: set[Tuple[str, str]] = set()
    seen_source: set[str] = set()
    for row in rows:
        source = row.get("source_hf_id") or ""
        quant = row.get("quant") or ""
        pair = (source, quant)
        if pair in seen_pair:
            continue
        if source not in seen_source and len(seen_source) >= limit:
            # Past the unique-model budget; drop everything from new
            # source ids but keep emitting more quants of sources
            # already counted in.
            continue
        seen_pair.add(pair)
        seen_source.add(source)
        deduped.append(row)
    return deduped
