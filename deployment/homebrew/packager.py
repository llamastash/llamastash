#!/usr/bin/env python3
"""Generate Formula/llamastash.rb from the template + release SHA-256s.

Usage:
    packager.py <version> <template_path> <output_path> \\
        <sha_aarch64_apple_darwin> <sha_x86_64_apple_darwin> \\
        <sha_aarch64_unknown_linux_gnu> <sha_x86_64_unknown_linux_gnu>

Mirrors kdash's deployment/homebrew/packager.py shape: a thin substitute
over string.Template, hardened with input shape checks and a post-render
assertion that no `$placeholder` survives.

This script is in `Cargo.toml`'s `exclude` list (via `deployment/*`) so it
does not ship in the published crate.
"""

from __future__ import annotations

import re
import sys
from string import Template

# Semver loose form. Stable: 0.1.2 / 1.0.0. Pre-release: 0.1.0-rc1 /
# 1.2.3-alpha.4. No build metadata — release tags don't use it.
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?$")
SHA256_RE = re.compile(r"^[a-fA-F0-9]{64}$")

EXPECTED_ARGC = 8  # script name + 7 args


def _die(msg: str, code: int = 2) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


def render(
    version: str,
    template_path: str,
    output_path: str,
    sha_aarch64_darwin: str,
    sha_x86_64_darwin: str,
    sha_aarch64_linux: str,
    sha_x86_64_linux: str,
) -> str:
    """Render the formula. Returns the rendered text.

    Raises SystemExit on any input shape failure or surviving placeholder.
    """
    # Strip a leading 'v' if a tag (vX.Y.Z) was passed instead of the bare
    # version. Mirrors the workflow's normalization.
    version = version.lstrip("v")

    if not VERSION_RE.match(version):
        _die(f"invalid version: {version!r} (expected X.Y.Z or X.Y.Z-suffix)")
    for label, sha in (
        ("sha_aarch64_apple_darwin", sha_aarch64_darwin),
        ("sha_x86_64_apple_darwin", sha_x86_64_darwin),
        ("sha_aarch64_unknown_linux_gnu", sha_aarch64_linux),
        ("sha_x86_64_unknown_linux_gnu", sha_x86_64_linux),
    ):
        if not SHA256_RE.match(sha):
            _die(f"invalid {label}: {sha!r} (expected 64 hex chars)")

    with open(template_path, "r", encoding="utf-8") as fh:
        template_src = fh.read()

    template = Template(template_src)
    mapping = {
        "version": version,
        "sha_aarch64_darwin": sha_aarch64_darwin,
        "sha_x86_64_darwin": sha_x86_64_darwin,
        "sha_aarch64_linux": sha_aarch64_linux,
        "sha_x86_64_linux": sha_x86_64_linux,
    }
    rendered = template.safe_substitute(mapping)

    # Assert every template placeholder we own has been substituted. The
    # template uses Ruby's `#{...}` for in-Ruby interpolation, which Python's
    # Template ignores; we only care about `$identifier` style placeholders.
    # `safe_substitute` silently leaves unknown $placeholders in the output,
    # so without this check a template typo (e.g. `$sha_arm64_darwin`) would
    # ship a broken formula.
    surviving = set(Template.pattern.findall(rendered))
    # Template.pattern groups: (escaped, named, braced, invalid). Flatten.
    leftover = {
        name
        for groups in surviving
        for name in groups
        if name and name not in {"$"}  # $$ → literal $
    }
    if leftover:
        _die(
            "template has unresolved $placeholders after substitution: "
            + ", ".join(sorted(leftover))
        )

    with open(output_path, "w", encoding="utf-8") as fh:
        fh.write(rendered)

    return rendered


def main(argv: list[str]) -> None:
    if len(argv) != EXPECTED_ARGC:
        print(__doc__.strip() if __doc__ else "", file=sys.stderr)
        _die(f"expected {EXPECTED_ARGC - 1} args, got {len(argv) - 1}")

    version = argv[1].strip()
    template_path = argv[2]
    output_path = argv[3]
    sha_aarch64_darwin = argv[4].strip()
    sha_x86_64_darwin = argv[5].strip()
    sha_aarch64_linux = argv[6].strip()
    sha_x86_64_linux = argv[7].strip()

    print("Generating formula")
    print(f"     VERSION: {version}")
    print(f"     TEMPLATE PATH: {template_path}")
    print(f"     SAVING AT: {output_path}")
    print(f"     SHA aarch64-apple-darwin: {sha_aarch64_darwin}")
    print(f"     SHA x86_64-apple-darwin: {sha_x86_64_darwin}")
    print(f"     SHA aarch64-unknown-linux-gnu: {sha_aarch64_linux}")
    print(f"     SHA x86_64-unknown-linux-gnu: {sha_x86_64_linux}")

    rendered = render(
        version,
        template_path,
        output_path,
        sha_aarch64_darwin,
        sha_x86_64_darwin,
        sha_aarch64_linux,
        sha_x86_64_linux,
    )

    print("\n================== Generated formula ==================\n")
    print(rendered)
    print("\n=======================================================\n")


if __name__ == "__main__":
    main(sys.argv)
