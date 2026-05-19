#!/usr/bin/env python3
"""Generate Formula/llamadash.rb from the template + release SHA-256s.

Usage:
    packager.py <version> <template_path> <output_path> \\
        <sha_aarch64_apple_darwin> <sha_x86_64_apple_darwin> \\
        <sha_aarch64_unknown_linux_gnu> <sha_x86_64_unknown_linux_gnu>

Mirrors kdash's deployment/homebrew/packager.py shape: a thin substitute
over string.Template. Run from .github/workflows/release.yml after the
build matrix completes and per-target SHA-256s are known.

This script is in `Cargo.toml`'s `exclude` list (via `deployment/*`) so it
does not ship in the published crate.
"""

import sys
from string import Template

argv = sys.argv
if len(argv) != 8:
    print(__doc__.strip(), file=sys.stderr)
    sys.exit(2)

version = argv[1].lstrip("v")
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

with open(template_path, "r", encoding="utf-8") as fh:
    template = Template(fh.read())

rendered = template.safe_substitute(
    version=version,
    sha_aarch64_darwin=sha_aarch64_darwin,
    sha_x86_64_darwin=sha_x86_64_darwin,
    sha_aarch64_linux=sha_aarch64_linux,
    sha_x86_64_linux=sha_x86_64_linux,
)

print("\n================== Generated formula ==================\n")
print(rendered)
print("\n=======================================================\n")

with open(output_path, "w", encoding="utf-8") as fh:
    fh.write(rendered)
