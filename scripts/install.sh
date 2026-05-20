#!/bin/sh
#
# install.sh — install llamastash from GitHub Releases.
#
# Usage:
#   curl -fsSL https://github.com/llamastash/llamastash/releases/latest/download/install.sh | sh
#   curl -fsSL https://llamastash.cli.rs/install.sh | sh
#
# Flags:
#   --version <vX.Y.Z>   Install a specific tag instead of the latest release.
#   --prefix <dir>       Install into <dir> instead of $HOME/.local/bin.
#   --quiet              Suppress progress chatter; errors still print.
#   -h, --help           Print this help and exit.
#
# Environment variables (equivalents to the flags above):
#   LLAMASTASH_VERSION       Same as --version.
#   LLAMASTASH_INSTALL_DIR   Same as --prefix.
#   LLAMASTASH_QUIET=1       Same as --quiet.
#
# Test-only overrides (do not set unless running the bats suite):
#   LLAMASTASH_BASE_URL      Override the GH Releases download base URL.
#   LLAMASTASH_LATEST_URL    Override the GH API latest-release endpoint.
#
# Exit codes:
#   0   success
#   1   generic failure (download error, network, unknown)
#   2   checksum verification failed
#   64  unsupported platform or invalid usage
#
# Target detection (uname -s, uname -m) -> Rust target triple:
#   linux x86_64  -> x86_64-unknown-linux-gnu
#   linux aarch64 -> aarch64-unknown-linux-gnu
#   darwin x86_64 -> x86_64-apple-darwin
#   darwin arm64  -> aarch64-apple-darwin
#
# This script must run under POSIX sh and Bash 3.2+ (macOS default). It avoids
# Bash 4-only features (mapfile, readarray, ${var,,}) so it works everywhere.

set -eu

REPO_DEFAULT="llamastash/llamastash"
BIN_NAME="llamastash"

# Test-only overrides; in production these resolve to GitHub's real endpoints.
BASE_URL="${LLAMASTASH_BASE_URL:-https://github.com/${REPO_DEFAULT}/releases/download}"
LATEST_URL="${LLAMASTASH_LATEST_URL:-https://api.github.com/repos/${REPO_DEFAULT}/releases/latest}"

# State populated from args / env later.
REQUESTED_VERSION="${LLAMASTASH_VERSION:-}"
INSTALL_DIR="${LLAMASTASH_INSTALL_DIR:-$HOME/.local/bin}"
QUIET="${LLAMASTASH_QUIET:-}"

log() {
  if [ -z "$QUIET" ]; then
    printf '%s\n' "$*"
  fi
}

err() {
  printf 'error: %s\n' "$*" >&2
}

usage() {
  # Print the script header (lines starting with '#' up to the first blank line
  # after the shebang) so --help stays in sync with the docs above.
  sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
}

# ----- argument parsing --------------------------------------------------------

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      if [ "$#" -lt 2 ]; then
        err "--version requires a tag argument (e.g., v0.2.0)"
        exit 64
      fi
      REQUESTED_VERSION="$2"
      shift 2
      ;;
    --version=*)
      REQUESTED_VERSION="${1#--version=}"
      shift
      ;;
    --prefix)
      if [ "$#" -lt 2 ]; then
        err "--prefix requires a directory argument"
        exit 64
      fi
      INSTALL_DIR="$2"
      shift 2
      ;;
    --prefix=*)
      INSTALL_DIR="${1#--prefix=}"
      shift
      ;;
    --quiet|-q)
      QUIET=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      err "unknown argument: $1"
      err "run with --help for usage"
      exit 64
      ;;
  esac
done

# ----- platform detection ------------------------------------------------------

detect_target() {
  uname_s=$(uname -s 2>/dev/null || echo unknown)
  uname_m=$(uname -m 2>/dev/null || echo unknown)

  case "$uname_s" in
    Linux)
      case "$uname_m" in
        x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
        *)
          err "unsupported Linux arch: $uname_m (supported: x86_64, aarch64)"
          exit 64
          ;;
      esac
      ;;
    Darwin)
      case "$uname_m" in
        x86_64) echo "x86_64-apple-darwin" ;;
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        *)
          err "unsupported macOS arch: $uname_m (supported: x86_64, arm64)"
          exit 64
          ;;
      esac
      ;;
    MINGW*|MSYS*|CYGWIN*|Windows_NT)
      err "Windows is not supported — install with 'cargo install llamastash' instead"
      exit 64
      ;;
    *)
      err "unsupported OS: $uname_s"
      exit 64
      ;;
  esac
}

# ----- network helpers ---------------------------------------------------------

# Wrap downloads so the rest of the script doesn't care whether curl or wget is
# present. Both are common on Linux + macOS; Alpine/Wolfi sometimes ship only
# wget. We require -fsSL semantics (fail-on-error, silent, follow redirects).
download() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --retry-delay 1 -o "$dest" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$dest" "$url"
  else
    err "neither curl nor wget is available on PATH; cannot download"
    exit 1
  fi
}

# ----- version resolution ------------------------------------------------------

resolve_version() {
  if [ -n "$REQUESTED_VERSION" ]; then
    # Normalise "v0.2.0" / "0.2.0" both work.
    case "$REQUESTED_VERSION" in
      v*) echo "$REQUESTED_VERSION" ;;
      *)  echo "v${REQUESTED_VERSION}" ;;
    esac
    return
  fi

  tmp=$(mktemp)
  if ! download "$LATEST_URL" "$tmp" 2>/dev/null; then
    rm -f "$tmp"
    err "could not fetch latest release info from $LATEST_URL"
    exit 1
  fi

  # Prefer jq when available; fall back to grep+sed. The grep fallback is
  # deliberately tolerant: it matches the first "tag_name" key with a string
  # value, which is the GitHub API contract.
  if command -v jq >/dev/null 2>&1; then
    tag=$(jq -r '.tag_name // empty' < "$tmp")
  else
    tag=$(grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' "$tmp" \
          | head -1 \
          | sed -e 's/.*"tag_name"[[:space:]]*:[[:space:]]*"//' -e 's/"$//')
  fi
  rm -f "$tmp"

  if [ -z "$tag" ]; then
    err "could not parse tag_name from $LATEST_URL response"
    exit 1
  fi
  echo "$tag"
}

# ----- checksum verification --------------------------------------------------

# Resolve a portable SHA-256 verifier. macOS ships `shasum -a 256`; most Linux
# distros ship both `sha256sum` and `shasum`. Pick whichever exists and use it
# in -c (check) mode against a SHA256SUMS file.
sha256_check() {
  sums_file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$sums_file"
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$sums_file"
  else
    err "neither shasum nor sha256sum is available; cannot verify download"
    exit 1
  fi
}

# Compute SHA-256 of one file. Used for the idempotence check (compare the
# existing on-disk binary against the SHA recorded in SHA256SUMS for the
# extracted binary path).
sha256_compute() {
  path="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    echo ""
  fi
}

# ----- main ------------------------------------------------------------------

target=$(detect_target)
version=$(resolve_version)

# Strip leading 'v' for the tarball stem (asset filenames are bare-version).
version_bare="${version#v}"
tarball="llamastash-${version_bare}-${target}.tar.gz"
tarball_url="${BASE_URL}/${version}/${tarball}"
sums_url="${BASE_URL}/${version}/SHA256SUMS"

log "Installing llamastash ${version} for ${target}"
log "  source: ${tarball_url}"
log "  prefix: ${INSTALL_DIR}"

# Idempotence: if the install dir already contains a binary whose SHA matches
# the published one for this version+target, exit early without downloading
# the tarball. We still need SHA256SUMS to know the expected hash.
mkdir -p "$INSTALL_DIR"
existing="$INSTALL_DIR/$BIN_NAME"

work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT

log "Fetching SHA256SUMS..."
if ! download "$sums_url" "$work_dir/SHA256SUMS"; then
  err "could not fetch $sums_url"
  exit 1
fi

# Extract the published hash for our tarball.
expected_tarball_sum=$(grep "  ${tarball}\$" "$work_dir/SHA256SUMS" \
  | awk '{print $1}' \
  | head -1)

if [ -z "$expected_tarball_sum" ]; then
  err "SHA256SUMS at $sums_url does not list $tarball"
  err "release may be partially uploaded; try again in a minute"
  exit 1
fi

# Idempotence check: compute hash of the *tarball* equivalent on disk if any.
# We can't directly compare the installed binary against the tarball hash
# (the binary is one file inside the tarball), so the check downloads + extracts
# + then compares. Cheaper short-circuit: skip if a binary at the install path
# already prints the requested version when invoked with --version. This is
# fast and reliable; falling back to "always download" on any error is fine.
if [ -x "$existing" ]; then
  current_version=$("$existing" --version 2>/dev/null | head -1 | awk '{print $NF}' || true)
  if [ -n "$current_version" ] && [ "v${current_version}" = "$version" ]; then
    log "llamastash ${version} already installed at ${existing} — nothing to do"
    log "Run \`${existing} --help\` to get started."
    exit 0
  fi
fi

log "Downloading tarball..."
if ! download "$tarball_url" "$work_dir/$tarball"; then
  err "could not download $tarball_url"
  exit 1
fi

log "Verifying checksum..."
# Build a single-line SHA256SUMS containing only our tarball so the verifier
# doesn't complain about missing peer files.
{
  echo "${expected_tarball_sum}  ${tarball}"
} > "$work_dir/expected.sums"

(cd "$work_dir" && sha256_check expected.sums >/dev/null 2>&1) || {
  err "checksum mismatch for ${tarball}"
  err "expected: ${expected_tarball_sum}"
  err "got:     $(sha256_compute "$work_dir/$tarball")"
  exit 2
}

log "Extracting..."
# The tarball ships a single directory `llamastash-<version>-<target>/` whose
# contents include the binary. Extract into the work dir, then copy the binary
# out atomically.
(cd "$work_dir" && tar -xzf "$tarball") || {
  err "tar -xzf failed on $tarball"
  exit 1
}

extracted_bin="$work_dir/llamastash-${version_bare}-${target}/$BIN_NAME"
if [ ! -x "$extracted_bin" ]; then
  err "extracted tarball does not contain an executable at $BIN_NAME"
  exit 1
fi

# Atomic install: write to a temp path next to the destination, then mv.
install_tmp="${INSTALL_DIR}/.${BIN_NAME}.tmp.$$"
cp "$extracted_bin" "$install_tmp"
chmod 0755 "$install_tmp"
mv "$install_tmp" "$existing"

log ""
log "Installed llamastash ${version} -> ${existing}"

# PATH hint (no mutation). Detect colon-separated $PATH membership in a way
# that works under POSIX sh; ksh/bash glob matching against ":$PATH:".
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    log "Run \`${BIN_NAME} --help\` to get started."
    ;;
  *)
    log ""
    log "Note: ${INSTALL_DIR} is not on your \$PATH."
    log "Add it to your shell's startup file, e.g.:"
    log "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    log ""
    log "Then run \`${BIN_NAME} --help\` to get started."
    ;;
esac
