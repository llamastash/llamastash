#!/usr/bin/env python3
"""Regenerate ``data/benchmark-snapshot.json`` from external sources.

This is the CI-loop owner of v2's recommender snapshot (R57). On a
successful run it produces a candidate snapshot, validates it against
the Rust recommender's 16/20 corpus check, and (when invoked under CI)
uploads the artefact to the rolling ``snapshot-latest`` GitHub Release.

The script runs in CI only — never as part of the cargo build. The
bundled ``data/benchmark-snapshot.json`` is committed to the source
tree; CI updates the *release asset* daily without auto-PR'ing a new
bundled snapshot. A maintainer-triggered PR refreshes the bundled copy
when prudent.

Partial-source-failure policy:
- If any source returns no data (timeout, parse error, upstream
  removal), the script does **not** publish — last-known-good stays
  live. ``doctor``'s ``RemoteSnapshotUnreachable`` finding surfaces
  prolonged outages through ``_init_snapshot.remote_fetch_failures``.
- The corpus gate (``cargo test --test recommender_corpus``) is
  release-blocking. A regressed snapshot exits non-zero so the CI
  workflow skips publication and auto-files a recalibration issue.

Vendored Python sources (Open LLM Leaderboard, Aider, etc.) live under
``scripts/benchmark_sources/`` and are documented in ``NOTICE``. The
sources are intentionally absent from the binary: the script runs in CI
to produce a JSON artefact the Rust binary reads (R45 single-binary
invariant).
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional

REPO_ROOT = Path(__file__).resolve().parent.parent
SCHEMA_VERSION = 1
DEFAULT_MIN_VERSION = "0.2.0"
SNAPSHOT_PATH = REPO_ROOT / "data" / "benchmark-snapshot.json"
SOURCES_DIR = REPO_ROOT / "scripts" / "benchmark_sources"

# Make ``scripts/`` importable so the vendored adapters resolve under
# ``benchmark_sources.<name>``. The package itself ships under
# ``scripts/benchmark_sources/`` per R45 (CI-only, never in the binary).
sys.path.insert(0, str(REPO_ROOT / "scripts"))

from benchmark_sources import aider as _aider_adapter  # noqa: E402
from benchmark_sources import open_llm_leaderboard as _ollb_adapter  # noqa: E402

# Bundled GGUF rows are keyed by ``(repo, file)``; upstream adapters key
# their scores by source HuggingFace model id (e.g. the un-quantized
# instruct repo). This table is the join. Keep one entry per row in
# ``data/benchmark-snapshot.json::models[]``. Missing entries are
# tolerated — the bundled ``benchmark_score.value`` is preserved when
# upstream has no match.
BUNDLED_ID_TO_SOURCE_HF_ID: Dict[str, str] = {
    "qwen2.5-coder-1.5b-q4_k_m": "Qwen/Qwen2.5-Coder-1.5B-Instruct",
    "qwen2.5-3b-q4_k_m": "Qwen/Qwen2.5-3B-Instruct",
    "llama-3.2-3b-q4_k_m": "meta-llama/Llama-3.2-3B-Instruct",
    "qwen2.5-7b-q4_k_m": "Qwen/Qwen2.5-7B-Instruct",
    "qwen2.5-coder-7b-q4_k_m": "Qwen/Qwen2.5-Coder-7B-Instruct",
    "llama-3.1-8b-q4_k_m": "meta-llama/Meta-Llama-3.1-8B-Instruct",
    "mistral-nemo-12b-q4_k_m": "mistralai/Mistral-Nemo-Instruct-2407",
    "qwen2.5-14b-q4_k_m": "Qwen/Qwen2.5-14B-Instruct",
    "qwen2.5-coder-14b-q4_k_m": "Qwen/Qwen2.5-Coder-14B-Instruct",
    "qwen2.5-32b-q4_k_m": "Qwen/Qwen2.5-32B-Instruct",
    "qwen2.5-coder-32b-q4_k_m": "Qwen/Qwen2.5-Coder-32B-Instruct",
    "llama-3.3-70b-q4_k_m": "meta-llama/Llama-3.3-70B-Instruct",
}


@dataclass
class SourceResult:
    """One source's contribution. ``ok`` False blocks publication."""

    name: str
    ok: bool
    rows: List[Dict[str, Any]]
    message: str = ""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Build the snapshot but do not write it or publish. Used "
        "by PRs touching data/benchmark-snapshot.json to validate "
        "the corpus gate before merge.",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=SNAPSHOT_PATH,
        help="Where to write the candidate snapshot.",
    )
    parser.add_argument(
        "--skip-corpus-gate",
        action="store_true",
        help=(
            "Skip the cargo test corpus gate. Intended for local "
            "debugging only — CI must run the gate."
        ),
    )
    args = parser.parse_args()

    sources = collect_sources()
    failed = [s for s in sources if not s.ok]
    if failed:
        for s in failed:
            print(f"[ERR] source `{s.name}` failed: {s.message}", file=sys.stderr)
        print(
            "[FAIL] partial source failure — refusing to publish; "
            "last-known-good snapshot stays live.",
            file=sys.stderr,
        )
        return 2

    candidate = build_snapshot(sources)

    if args.dry_run:
        print(json.dumps(candidate, indent=2))
    else:
        write_atomic(args.out, candidate)
        print(f"[OK] wrote {args.out}")

    if args.skip_corpus_gate:
        print("[WARN] corpus gate skipped (--skip-corpus-gate)", file=sys.stderr)
        return 0

    return run_corpus_gate()


def collect_sources() -> List[SourceResult]:
    """Fetch every vendored source. Each source is independent so one
    upstream failure surfaces clearly rather than masquerading as a
    silent recommender regression."""
    results: List[SourceResult] = []
    results.append(load_open_llm_leaderboard())
    results.append(load_aider_leaderboard())
    # Future sources land here; each must return a SourceResult so the
    # partial-failure policy applies uniformly.
    return results


def load_open_llm_leaderboard() -> SourceResult:
    """Delegate to the vendored adapter under
    ``scripts/benchmark_sources/open_llm_leaderboard.py``. Returns rows
    keyed by source HuggingFace id (``hf_id``, ``score``, ``source``);
    ``build_snapshot()`` owns the join into the bundled
    ``(repo, file)`` rows via :data:`BUNDLED_ID_TO_SOURCE_HF_ID`.
    """
    src = _ollb_adapter.fetch()
    return SourceResult(name=src.name, ok=src.ok, rows=src.rows, message=src.message)


def load_aider_leaderboard() -> SourceResult:
    """Delegate to the vendored adapter under
    ``scripts/benchmark_sources/aider.py``. Same row shape as
    :func:`load_open_llm_leaderboard`.
    """
    src = _aider_adapter.fetch()
    return SourceResult(name=src.name, ok=src.ok, rows=src.rows, message=src.message)


def build_snapshot(sources: List[SourceResult]) -> Dict[str, Any]:
    """Merge live source rows into the bundled snapshot's ``models[]``.

    The bundled JSON is the catalog and the source of identity / shape:
    ``id``, ``repo``, ``file``, ``params``, ``weights_bytes``,
    ``task_hints``, ``tok_s_factor``, ``recency``, and the bundled
    ``benchmark_score.source`` tag. Live adapters supply a fresh
    ``benchmark_score.value`` keyed by source HuggingFace id; we join
    via :data:`BUNDLED_ID_TO_SOURCE_HF_ID`.

    Policy:

    * A bundled row whose HF id appears in the relevant adapter's rows
      gets its ``benchmark_score.value`` replaced with the live score.
    * A bundled row whose HF id is *absent* from upstream keeps the
      bundled value (don't drop the catalog on transient delistings).
    * New rows upstream introduces but the bundled snapshot does not
      have are skipped — they'd need maintainer-curated ``task_hints``
      and a slot in the corpus, which is out of CI scope.
    * ``recommender_weights`` (including ``overhead_band_bytes``) is
      preserved verbatim — it's owned by a separate plan.
    """
    bundled_models: List[Dict[str, Any]] = []
    recommender_weights: Dict[str, Any] = {}
    remote_url: Optional[str] = None
    if SNAPSHOT_PATH.exists():
        with SNAPSHOT_PATH.open() as f:
            bundled = json.load(f)
        bundled_models = bundled.get("models", [])
        recommender_weights = bundled.get("recommender_weights", {})
        remote_url = bundled.get("remote_url")

    scores_by_source = _index_adapter_scores(sources)
    refreshed = _refresh_bundled_models(bundled_models, scores_by_source)

    candidate: Dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "bundle_date": datetime.date.today().isoformat(),
        "min_version": DEFAULT_MIN_VERSION,
        "remote_url": remote_url,
        "recommender_weights": recommender_weights,
        "models": refreshed,
    }
    return candidate


# Bundled ``benchmark_score.source`` tag -> adapter ``name``. The bundled
# JSON uses ``"openllm-leaderboard"`` and ``"aider"`` as provenance tags;
# the corresponding adapter names are ``"open-llm-leaderboard"`` and
# ``"aider"``. Keep this table small — adding a new source means
# extending both ends.
_BUNDLED_SOURCE_TAG_TO_ADAPTER: Dict[str, str] = {
    "openllm-leaderboard": "open-llm-leaderboard",
    "aider": "aider",
}


def _index_adapter_scores(
    sources: List[SourceResult],
) -> Dict[str, Dict[str, float]]:
    """Index successful adapter results as ``adapter_name -> {hf_id: score}``."""
    by_source: Dict[str, Dict[str, float]] = {}
    for src in sources:
        if not src.ok:
            continue
        scores: Dict[str, float] = {}
        for row in src.rows:
            hf_id = row.get("hf_id")
            score = row.get("score")
            if not isinstance(hf_id, str) or not isinstance(score, (int, float)):
                continue
            scores[hf_id] = float(score)
        by_source[src.name] = scores
    return by_source


def _refresh_bundled_models(
    bundled_models: List[Dict[str, Any]],
    scores_by_source: Dict[str, Dict[str, float]],
) -> List[Dict[str, Any]]:
    """Return a fresh ``models[]`` list with refreshed
    ``benchmark_score.value`` fields where upstream had a match."""
    out: List[Dict[str, Any]] = []
    for model in bundled_models:
        refreshed = dict(model)
        bench = dict(model.get("benchmark_score", {}))
        bundled_tag = bench.get("source")
        adapter_name = _BUNDLED_SOURCE_TAG_TO_ADAPTER.get(bundled_tag or "")
        scores = scores_by_source.get(adapter_name or "")
        hf_id = BUNDLED_ID_TO_SOURCE_HF_ID.get(model.get("id", ""))
        if scores and hf_id and hf_id in scores:
            bench["value"] = scores[hf_id]
        refreshed["benchmark_score"] = bench
        out.append(refreshed)
    return out


def write_atomic(path: Path, body: Dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + f".tmp.{os.getpid()}")
    with tmp.open("w") as f:
        json.dump(body, f, indent=2)
        f.write("\n")
    os.replace(tmp, path)


def run_corpus_gate() -> int:
    """Invoke ``cargo test`` against the recommender corpus integration
    test. Non-zero exit blocks publication. CI's workflow auto-files a
    recalibration issue on regression."""
    cargo = shutil.which("cargo")
    if cargo is None:
        print("[WARN] cargo not on $PATH; skipping corpus gate", file=sys.stderr)
        return 0
    cmd = [
        cargo,
        "test",
        "--features",
        "test-fixtures",
        "--test",
        "recommender_corpus",
        "--",
        "--nocapture",
    ]
    print(f"[gate] {' '.join(cmd)}", flush=True)
    result = subprocess.run(cmd, cwd=REPO_ROOT)
    if result.returncode == 0:
        print("[gate] PASS")
        return 0
    print(
        "[gate] FAIL — corpus regressed; not publishing snapshot. "
        "CI workflow will open an issue with the recommender-regression label.",
        file=sys.stderr,
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
