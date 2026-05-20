#!/usr/bin/env python3
"""Unit tests for deployment/homebrew/packager.py.

Run directly:
    python3 deployment/homebrew/packager_test.py

Wired into .github/workflows/ci.yml as a release-readiness step so template
or argv regressions surface on every PR, not just at release time.

This file is excluded from the published crate via Cargo.toml's
`exclude = ["deployment/*"]`.
"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

# Import the module under test from the same directory.
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import packager  # noqa: E402

GOOD_SHA = "a" * 64
TEMPLATE_PATH = str(HERE / "llamastash.rb.template")


def _render_with_defaults(**overrides):
    """Render via packager.render with a known-good baseline + overrides."""
    out = tempfile.NamedTemporaryFile(mode="w", suffix=".rb", delete=False)
    out.close()
    args = {
        "version": "0.0.1",
        "template_path": TEMPLATE_PATH,
        "output_path": out.name,
        "sha_aarch64_darwin": "a" * 64,
        "sha_x86_64_darwin": "b" * 64,
        "sha_aarch64_linux": "c" * 64,
        "sha_x86_64_linux": "d" * 64,
    }
    args.update(overrides)
    try:
        return packager.render(**args), out.name
    finally:
        # Caller can read out.name; we just don't want to leak on exception
        # paths. Best-effort cleanup happens via tearDown in tests that need
        # the file to persist across the call.
        pass


class TestRender(unittest.TestCase):
    def setUp(self):
        self.tempfiles = []

    def tearDown(self):
        for f in self.tempfiles:
            try:
                os.unlink(f)
            except OSError:
                pass

    def _render(self, **overrides):
        rendered, path = _render_with_defaults(**overrides)
        self.tempfiles.append(path)
        return rendered

    # --- happy path -----------------------------------------------------

    def test_happy_path_substitutes_all_placeholders(self):
        rendered = self._render(version="0.0.1")
        self.assertIn('version "0.0.1"', rendered)
        self.assertIn('sha256 "' + ("a" * 64) + '"', rendered)
        self.assertIn('sha256 "' + ("b" * 64) + '"', rendered)
        self.assertIn('sha256 "' + ("c" * 64) + '"', rendered)
        self.assertIn('sha256 "' + ("d" * 64) + '"', rendered)

    def test_version_with_leading_v_is_stripped(self):
        rendered = self._render(version="v0.0.1")
        self.assertIn('version "0.0.1"', rendered)
        self.assertNotIn('version "v0.0.1"', rendered)

    def test_prerelease_version_is_accepted(self):
        rendered = self._render(version="0.0.0-rc1")
        self.assertIn('version "0.0.0-rc1"', rendered)

    def test_no_dollar_placeholders_survive(self):
        rendered = self._render()
        # Catches typos like $sha_arm64_darwin or untemplated $foo blocks.
        # The template uses Ruby's `#{version}` for runtime interpolation,
        # which is fine to leave; we only flag `$identifier` blocks.
        import re

        survivors = re.findall(r"\$[a-zA-Z_][a-zA-Z0-9_]*", rendered)
        self.assertEqual(
            survivors,
            [],
            f"unresolved $placeholders survived: {survivors!r}",
        )

    # --- input shape rejection -----------------------------------------

    def test_invalid_version_is_rejected(self):
        for bad in ["", "not-a-version", "0.0", "0.0.1.5", "0.0.1+meta", "0.0.1 ; rm -rf"]:
            with self.subTest(version=bad):
                with self.assertRaises(SystemExit) as cm:
                    self._render(version=bad)
                self.assertEqual(cm.exception.code, 2)

    def test_invalid_sha256_is_rejected(self):
        for bad in ["", "tooshort", "a" * 63, "a" * 65, "g" * 64, "a" * 32 + "!" * 32]:
            with self.subTest(sha=bad):
                with self.assertRaises(SystemExit) as cm:
                    self._render(sha_aarch64_darwin=bad)
                self.assertEqual(cm.exception.code, 2)

    def test_missing_template_path_raises(self):
        with self.assertRaises(FileNotFoundError):
            self._render(template_path="/nonexistent.template")

    # --- placeholder regression catch ----------------------------------

    def test_unsubstituted_placeholder_is_caught(self):
        """A template that references an unknown $placeholder must fail."""
        bad_template = tempfile.NamedTemporaryFile(
            mode="w", suffix=".template", delete=False
        )
        try:
            bad_template.write(
                'class Llamastash < Formula\n'
                '  version "$version"\n'
                '  sha256 "$sha_unknown_target"\n'  # not in mapping
                "end\n"
            )
            bad_template.close()
            with self.assertRaises(SystemExit) as cm:
                self._render(template_path=bad_template.name)
            self.assertEqual(cm.exception.code, 2)
        finally:
            os.unlink(bad_template.name)


class TestArgvHandling(unittest.TestCase):
    def test_wrong_argc_exits_2(self):
        with self.assertRaises(SystemExit) as cm:
            packager.main(["packager.py", "0.0.1"])  # missing args
        self.assertEqual(cm.exception.code, 2)

    def test_correct_argc_renders(self):
        out = tempfile.NamedTemporaryFile(mode="w", suffix=".rb", delete=False)
        out.close()
        try:
            packager.main(
                [
                    "packager.py",
                    "0.0.1",
                    TEMPLATE_PATH,
                    out.name,
                    "a" * 64,
                    "b" * 64,
                    "c" * 64,
                    "d" * 64,
                ]
            )
            rendered = Path(out.name).read_text()
            self.assertIn('version "0.0.1"', rendered)
        finally:
            os.unlink(out.name)


if __name__ == "__main__":
    unittest.main(verbosity=2)
