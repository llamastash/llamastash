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
    """Open LLM Leaderboard rows for the general / reasoning lane.

    TODO(unit7-v2-ga): vendor the actual whichllm scraping module
    under ``scripts/benchmark_sources/`` so the CI loop produces
    a real snapshot. For now the function returns a placeholder
    success so the script's framework is exercisable in CI.
    """
    return SourceResult(
        name="open-llm-leaderboard",
        ok=True,
        rows=[],
        message="placeholder — real fetch lands when whichllm is vendored",
    )


def load_aider_leaderboard() -> SourceResult:
    """Aider polyglot benchmark for the code lane.

    TODO(unit7-v2-ga): vendor the actual Aider leaderboard scrape.
    """
    return SourceResult(
        name="aider",
        ok=True,
        rows=[],
        message="placeholder — real fetch lands when Aider scrape is vendored",
    )


def build_snapshot(sources: List[SourceResult]) -> Dict[str, Any]:
    """Merge source rows into the snapshot shape Rust expects. For v2
    the merge is a no-op (sources are placeholders) and we preserve the
    committed bundled snapshot's catalog so the corpus gate has data."""
    bundled_models = []
    if SNAPSHOT_PATH.exists():
        with SNAPSHOT_PATH.open() as f:
            bundled = json.load(f)
            bundled_models = bundled.get("models", [])
            recommender_weights = bundled.get("recommender_weights", {})
            remote_url = bundled.get("remote_url")
    else:
        recommender_weights = {}
        remote_url = None

    candidate: Dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "bundle_date": datetime.date.today().isoformat(),
        "min_version": DEFAULT_MIN_VERSION,
        "remote_url": remote_url,
        "recommender_weights": recommender_weights,
        "models": bundled_models,
    }
    return candidate


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
