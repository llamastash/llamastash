"""Unit tests for hf_discovery's projection / selection logic.

The tests stub out the whichllm import so they run on any Python 3.11+
without the pip dep. End-to-end discovery (whichllm + HF API) is
exercised by the daily CI regen run, not these tests.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import Any, Dict, List, Optional

# Make ``scripts/`` importable so ``benchmark_sources.*`` resolves.
SCRIPTS_DIR = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(SCRIPTS_DIR))

from benchmark_sources import hf_discovery  # noqa: E402


REPO_ROOT = SCRIPTS_DIR.parent


def _fake_candidate(
    repo_id: str,
    parameter_count: int,
    gguf_variants: List[Dict[str, Any]],
    *,
    architecture: str = "qwen3",
    is_moe: bool = False,
    parameter_count_active: Optional[int] = None,
    base_model: Optional[str] = None,
    downloads: int = 1000,
    published_at: str = "2026-04-01",
) -> Dict[str, Any]:
    """Build a dict-shaped stand-in for whichllm's ModelInfo.

    Field names match whichllm 0.5.7's ModelInfo dataclass exactly so
    the test exercises hf_discovery's real consumption path."""
    return {
        "id": repo_id,
        "parameter_count": parameter_count,
        "parameter_count_active": parameter_count_active,
        "architecture": architecture,
        "is_moe": is_moe,
        "base_model": base_model,
        "downloads": downloads,
        "published_at": published_at,
        "gguf_variants": gguf_variants,
    }


def _variant(filename: str, quant_type: str, size: int = 0) -> Dict[str, Any]:
    return {
        "filename": filename,
        "quant_type": quant_type,
        "file_size_bytes": size,
    }


class AttachTaskHintsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.prefixes = {
            "Qwen/Qwen3-Coder": ["code"],
            "Qwen/Qwen3": ["general", "reasoning"],
        }
        self.defaults = ["general"]

    def test_longest_prefix_wins(self) -> None:
        # Qwen3-Coder must beat the shorter Qwen3 prefix.
        result = hf_discovery.attach_task_hints(
            "Qwen/Qwen3-Coder-30B-A3B-Instruct", self.prefixes, self.defaults
        )
        self.assertEqual(result, ["code"])

    def test_fallback_to_defaults_on_no_match(self) -> None:
        result = hf_discovery.attach_task_hints(
            "nonexistent/Model-9B", self.prefixes, self.defaults
        )
        self.assertEqual(result, ["general"])


class PickVariantTest(unittest.TestCase):
    def test_picks_first_preferred_quant(self) -> None:
        variants = [
            _variant("foo-Q5_K_M.gguf", "Q5_K_M", 6_500_000_000),
            _variant("foo-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000),
            _variant("foo-Q8_0.gguf", "Q8_0", 8_000_000_000),
        ]
        result = hf_discovery._pick_variant(variants)
        self.assertIsNotNone(result)
        quant, filename, size = result  # type: ignore[misc]
        self.assertEqual(quant, "Q4_K_M")
        self.assertEqual(filename, "foo-Q4_K_M.gguf")
        self.assertEqual(size, 5_400_000_000)

    def test_falls_back_through_preferred_list(self) -> None:
        variants = [_variant("bar-Q5_K_M.gguf", "Q5_K_M", 6_500_000_000)]
        result = hf_discovery._pick_variant(variants)
        self.assertIsNotNone(result)
        quant, _filename, _size = result  # type: ignore[misc]
        self.assertEqual(quant, "Q5_K_M")

    def test_returns_none_when_no_preferred_quant(self) -> None:
        variants = [_variant("baz-Q8_0.gguf", "Q8_0", 8_000_000_000)]
        self.assertIsNone(hf_discovery._pick_variant(variants))

    def test_quant_lookup_is_case_insensitive(self) -> None:
        variants = [_variant("baz-q4_k_m.gguf", "q4_k_m", 5_000_000_000)]
        result = hf_discovery._pick_variant(variants)
        self.assertIsNotNone(result)
        quant, _filename, _size = result  # type: ignore[misc]
        self.assertEqual(quant, "Q4_K_M")


class PublisherTrustedTest(unittest.TestCase):
    def test_allowlisted_publisher_passes(self) -> None:
        self.assertTrue(
            hf_discovery._publisher_trusted(
                publisher="bartowski",
                source_hf_id="meta-llama/Llama-4-9B",
                allowlist=["bartowski", "unsloth"],
            )
        )

    def test_official_org_passes_even_without_allowlist(self) -> None:
        # First-party GGUF from the model family's own org always
        # qualifies, even if that org isn't explicitly allowlisted.
        self.assertTrue(
            hf_discovery._publisher_trusted(
                publisher="Qwen",
                source_hf_id="Qwen/Qwen3-Coder-30B-A3B-Instruct",
                allowlist=["bartowski"],
            )
        )

    def test_unknown_publisher_rejected(self) -> None:
        self.assertFalse(
            hf_discovery._publisher_trusted(
                publisher="shady-org",
                source_hf_id="meta-llama/Llama-4-9B",
                allowlist=["bartowski"],
            )
        )


class ProjectCandidateTest(unittest.TestCase):
    def test_official_qwen_moe_candidate_carries_params_active(self) -> None:
        cand = _fake_candidate(
            repo_id="Qwen/Qwen3-Next-80B-A3B-Instruct-GGUF",
            base_model="Qwen/Qwen3-Next-80B-A3B-Instruct",
            parameter_count=80_000_000_000,
            parameter_count_active=3_000_000_000,
            is_moe=True,
            gguf_variants=[
                _variant("qwen3-next-80b-Q4_K_M.gguf", "Q4_K_M", 48_000_000_000)
            ],
        )
        prefixes = {"Qwen/Qwen3-Next": ["general", "reasoning"]}
        defaults = ["general"]
        result = hf_discovery._project_candidate(cand, ["bartowski"], prefixes, defaults)
        self.assertIsNotNone(result)
        assert result is not None
        self.assertTrue(result.is_moe)
        self.assertEqual(result.params_active, 3_000_000_000)
        self.assertEqual(result.params, 80_000_000_000)
        self.assertEqual(result.gguf_publisher, "Qwen")
        self.assertEqual(result.source_hf_id, "Qwen/Qwen3-Next-80B-A3B-Instruct")
        self.assertEqual(result.quant, "Q4_K_M")
        self.assertEqual(result.weights_bytes, 48_000_000_000)
        self.assertEqual(result.task_hints, ["general", "reasoning"])

    def test_bartowski_quant_of_external_source_passes(self) -> None:
        cand = _fake_candidate(
            repo_id="bartowski/Llama-4-9B-Instruct-GGUF",
            base_model="meta-llama/Llama-4-9B-Instruct",
            parameter_count=9_000_000_000,
            gguf_variants=[
                _variant("llama-4-9b-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000)
            ],
        )
        prefixes = {"meta-llama/Llama-4": ["general", "reasoning"]}
        defaults = ["general"]
        result = hf_discovery._project_candidate(cand, ["bartowski"], prefixes, defaults)
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.gguf_publisher, "bartowski")
        self.assertEqual(result.source_hf_id, "meta-llama/Llama-4-9B-Instruct")
        self.assertEqual(result.task_hints, ["general", "reasoning"])

    def test_untrusted_publisher_dropped(self) -> None:
        cand = _fake_candidate(
            repo_id="shady-org/Llama-4-9B-GGUF",
            base_model="meta-llama/Llama-4-9B",
            parameter_count=9_000_000_000,
            gguf_variants=[
                _variant("llama-4-9b-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000)
            ],
        )
        result = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertIsNone(result)

    def test_candidate_without_gguf_variants_dropped(self) -> None:
        cand = _fake_candidate(
            repo_id="test/no-gguf-published",
            parameter_count=7_000_000_000,
            gguf_variants=[],
        )
        result = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertIsNone(result)

    def test_falls_back_to_repo_id_when_base_model_missing(self) -> None:
        # GGUF-only publishers (no base_model link) must still produce
        # a row — source_hf_id falls back to the repo id.
        cand = _fake_candidate(
            repo_id="bartowski/foo-9B-GGUF",
            base_model=None,
            parameter_count=9_000_000_000,
            gguf_variants=[_variant("foo-9b-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000)],
        )
        result = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.source_hf_id, "bartowski/foo-9B-GGUF")

    def test_size_falls_back_to_quant_density_estimate(self) -> None:
        # whichllm sometimes returns file_size_bytes=0; the estimator
        # should plug a plausible size from params × density so the
        # recommender's fit predicate doesn't see a zero footprint.
        cand = _fake_candidate(
            repo_id="bartowski/foo-9B-GGUF",
            parameter_count=9_000_000_000,
            gguf_variants=[_variant("foo-9b-Q4_K_M.gguf", "Q4_K_M", 0)],
        )
        result = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        assert result is not None
        # Q4_K_M density is 0.60 GB/Bparam → ~5.4 GB for a 9B model.
        self.assertGreater(result.weights_bytes, 4_000_000_000)
        self.assertLess(result.weights_bytes, 7_000_000_000)


class QuantFilenameMatchTest(unittest.TestCase):
    """Kept for the legacy helper that scrapes quant from filename.
    Unused in the current pipeline but exercised so a future re-add
    has a known-good reference."""

    def test_dash_separator(self) -> None:
        self.assertTrue(hf_discovery._quant_matches_filename("model-Q4_K_M.gguf", "Q4_K_M"))

    def test_dot_separator(self) -> None:
        self.assertTrue(hf_discovery._quant_matches_filename("model.Q4_K_M.gguf", "Q4_K_M"))

    def test_case_insensitive(self) -> None:
        self.assertTrue(hf_discovery._quant_matches_filename("model-q4_k_m.gguf", "Q4_K_M"))

    def test_rejects_substring_match(self) -> None:
        # Must be a full suffix match, not just contains.
        self.assertFalse(hf_discovery._quant_matches_filename("model-Q4_K_M-fp16.gguf", "Q4_K_M"))


class TaskHintsAndAllowlistFilesTest(unittest.TestCase):
    """Smoke-test the YAML loaders against the real side-files."""

    def test_load_task_hints_returns_non_empty_prefixes(self) -> None:
        prefixes, defaults = hf_discovery.load_task_hints(REPO_ROOT)
        self.assertGreater(len(prefixes), 0)
        self.assertEqual(defaults, ["general"])
        # Spot-check: Qwen3-Coder must be tagged code.
        self.assertEqual(prefixes.get("Qwen/Qwen3-Coder"), ["code"])

    def test_load_publisher_allowlist_contains_known_orgs(self) -> None:
        allowlist = hf_discovery.load_publisher_allowlist(REPO_ROOT)
        self.assertIn("bartowski", allowlist)
        self.assertIn("unsloth", allowlist)


if __name__ == "__main__":
    unittest.main()
