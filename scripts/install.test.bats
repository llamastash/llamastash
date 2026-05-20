#!/usr/bin/env bats
#
# install.test.bats — integration tests for scripts/install.sh.
#
# Strategy: generate fixture tarballs at setup time, serve them via a local
# Python HTTP server on a random port, and point install.sh at that server
# via LLAMASTASH_BASE_URL + LLAMASTASH_LATEST_URL. Each test owns a fresh
# install dir under a per-test temp directory.
#
# Run: bats scripts/install.test.bats
# Requires: bats-core, python3, tar, curl-or-wget, sha256sum-or-shasum.

INSTALL_SH_REL="scripts/install.sh"

# ---- fixture helpers --------------------------------------------------------

# Compute SHA-256 of a file using whichever tool is available.
_sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

# Detect the native Rust target triple from the host's uname output.
_native_target() {
  case "$(uname -s)" in
    Linux) os_part=unknown-linux-gnu ;;
    Darwin) os_part=apple-darwin ;;
    *) echo "unsupported-host"; return ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch_part=x86_64 ;;
    aarch64|arm64) arch_part=aarch64 ;;
    *) echo "unsupported-host"; return ;;
  esac
  echo "${arch_part}-${os_part}"
}

# Build the fixture tree under $TEST_TMP/fixtures:
#   fixtures/releases/v0.2.0/llamastash-0.2.0-${TARGET}.tar.gz
#   fixtures/releases/v0.2.0/SHA256SUMS
#   fixtures/api/latest.json
# The fake binary in the tarball is a shell script that echoes "llamastash 0.2.0".
_build_fixtures() {
  version_bare="0.2.0"
  tag="v${version_bare}"

  build_dir="$TEST_TMP/build/llamastash-${version_bare}-${TARGET}"
  mkdir -p "$build_dir"
  cat >"$build_dir/llamastash" <<EOF
#!/bin/sh
echo "llamastash ${version_bare}"
EOF
  chmod +x "$build_dir/llamastash"
  : >"$build_dir/LICENSE"
  : >"$build_dir/README.md"

  rel_dir="$TEST_TMP/fixtures/releases/${tag}"
  mkdir -p "$rel_dir"
  tar -czf "$rel_dir/llamastash-${version_bare}-${TARGET}.tar.gz" \
    -C "$TEST_TMP/build" "llamastash-${version_bare}-${TARGET}"

  sha=$(_sha256 "$rel_dir/llamastash-${version_bare}-${TARGET}.tar.gz")
  printf '%s  %s\n' "$sha" "llamastash-${version_bare}-${TARGET}.tar.gz" \
    > "$rel_dir/SHA256SUMS"

  mkdir -p "$TEST_TMP/fixtures/api"
  printf '{"tag_name": "%s", "name": "llamastash %s"}\n' "$tag" "$version_bare" \
    > "$TEST_TMP/fixtures/api/latest.json"
}

# Start a Python HTTP server in $TEST_TMP/fixtures on a free port; export PORT,
# SERVER_PID, LLAMASTASH_BASE_URL, LLAMASTASH_LATEST_URL.
#
# Lifecycle note: the subshell uses `exec` so it becomes the python process,
# making $! the python PID directly. Without `exec`, killing $SERVER_PID would
# only kill the subshell and orphan the python child, leaking ports per test.
_start_server() {
  PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
  ( cd "$TEST_TMP/fixtures" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) &
  SERVER_PID=$!

  # Poll for server readiness.
  ready=0
  i=0
  while [ "$i" -lt 50 ]; do
    if curl -sf "http://127.0.0.1:$PORT/" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.1
    i=$((i + 1))
  done
  if [ "$ready" -ne 1 ]; then
    echo "fixture server failed to start on port $PORT" >&2
    return 1
  fi

  export PORT SERVER_PID
  export LLAMASTASH_BASE_URL="http://127.0.0.1:$PORT/releases"
  export LLAMASTASH_LATEST_URL="http://127.0.0.1:$PORT/api/latest.json"
}

# ---- bats lifecycle ---------------------------------------------------------

setup() {
  TARGET=$(_native_target)
  if [ "$TARGET" = "unsupported-host" ]; then
    skip "unsupported test host: $(uname -s) $(uname -m)"
  fi
  export TARGET

  TEST_TMP=$(mktemp -d)
  export TEST_TMP
  export LLAMASTASH_INSTALL_DIR="$TEST_TMP/bin"
  export LLAMASTASH_QUIET=1

  _build_fixtures
  _start_server

  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/.." && pwd)"
  INSTALL_SH="$REPO_ROOT/$INSTALL_SH_REL"
  export INSTALL_SH
}

teardown() {
  if [ -n "${SERVER_PID:-}" ]; then
    # Gentle TERM first; force-kill after a short grace if it survives.
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    # Brief wait, then check; SIGKILL if still alive.
    for _ in 1 2 3; do
      if ! kill -0 "$SERVER_PID" 2>/dev/null; then break; fi
      sleep 0.1
    done
    kill -KILL "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "${TEST_TMP:-}"
}

# ---- happy paths ------------------------------------------------------------

@test "happy path: install latest on native target" {
  run "$INSTALL_SH"
  [ "$status" -eq 0 ]
  [ -x "$LLAMASTASH_INSTALL_DIR/llamastash" ]
  run "$LLAMASTASH_INSTALL_DIR/llamastash" --version
  [ "$status" -eq 0 ]
  [ "$output" = "llamastash 0.2.0" ]
}

@test "happy path: --version flag pins to a specific tag" {
  run "$INSTALL_SH" --version "v0.2.0"
  [ "$status" -eq 0 ]
  [ -x "$LLAMASTASH_INSTALL_DIR/llamastash" ]
}

@test "happy path: LLAMASTASH_VERSION env pins to a specific tag" {
  LLAMASTASH_VERSION="v0.2.0" run "$INSTALL_SH"
  [ "$status" -eq 0 ]
  [ -x "$LLAMASTASH_INSTALL_DIR/llamastash" ]
}

@test "happy path: bare version (no leading v) is accepted" {
  run "$INSTALL_SH" --version "0.2.0"
  [ "$status" -eq 0 ]
  [ -x "$LLAMASTASH_INSTALL_DIR/llamastash" ]
}

@test "happy path: --prefix overrides install directory" {
  alt_dir="$TEST_TMP/alt-bin"
  # --prefix must override the env-var default.
  LLAMASTASH_INSTALL_DIR= run "$INSTALL_SH" --prefix "$alt_dir"
  [ "$status" -eq 0 ]
  [ -x "$alt_dir/llamastash" ]
}

# ---- idempotence ------------------------------------------------------------

@test "idempotence: re-running the same version is a no-op" {
  run "$INSTALL_SH"
  [ "$status" -eq 0 ]

  first_inode=$(ls -i "$LLAMASTASH_INSTALL_DIR/llamastash" | awk '{print $1}')

  run "$INSTALL_SH"
  [ "$status" -eq 0 ]

  second_inode=$(ls -i "$LLAMASTASH_INSTALL_DIR/llamastash" | awk '{print $1}')

  # Same inode = file wasn't replaced. The script's idempotence branch
  # short-circuits before touching the destination.
  [ "$first_inode" = "$second_inode" ]
}

# ---- PATH hint --------------------------------------------------------------

@test "PATH hint: warning shown when install dir is not on PATH" {
  # Override QUIET locally so output is visible to the assertion.
  PATH="/usr/bin:/bin" LLAMASTASH_QUIET= run "$INSTALL_SH"
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "not on your"
}

@test "PATH hint: warning omitted when install dir is on PATH" {
  PATH="$LLAMASTASH_INSTALL_DIR:/usr/bin:/bin" LLAMASTASH_QUIET= run "$INSTALL_SH"
  [ "$status" -eq 0 ]
  ! echo "$output" | grep -q "not on your"
}

# ---- error paths ------------------------------------------------------------

@test "checksum mismatch: refuses install with exit 2" {
  # Sabotage SHA256SUMS so the verifier rejects the tarball.
  rel_dir="$TEST_TMP/fixtures/releases/v0.2.0"
  tarball_name="llamastash-0.2.0-${TARGET}.tar.gz"
  printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$tarball_name" \
    > "$rel_dir/SHA256SUMS"

  run "$INSTALL_SH"
  [ "$status" -eq 2 ]
  [ ! -e "$LLAMASTASH_INSTALL_DIR/llamastash" ]
}

@test "missing asset: SHA256SUMS without our tarball line exits 1" {
  rel_dir="$TEST_TMP/fixtures/releases/v0.2.0"
  printf '0000000000000000000000000000000000000000000000000000000000000000  some-other-tarball.tar.gz\n' \
    > "$rel_dir/SHA256SUMS"

  run "$INSTALL_SH"
  [ "$status" -eq 1 ]
}

@test "nonexistent version exits 1" {
  run "$INSTALL_SH" --version "v9.9.9"
  [ "$status" -eq 1 ]
  [ ! -e "$LLAMASTASH_INSTALL_DIR/llamastash" ]
}

@test "unknown flag exits 64" {
  run "$INSTALL_SH" --unknown-flag
  [ "$status" -eq 64 ]
}

@test "missing --version argument exits 64" {
  run "$INSTALL_SH" --version
  [ "$status" -eq 64 ]
}

@test "missing --prefix argument exits 64" {
  run "$INSTALL_SH" --prefix
  [ "$status" -eq 64 ]
}

@test "--help exits 0 and prints usage" {
  LLAMASTASH_QUIET= run "$INSTALL_SH" --help
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "install.sh — install llamastash"
}

# ---- platform refusal via uname stub ----------------------------------------

# Create a stub uname earlier on PATH so install.sh sees the spoofed OS/arch.
# The stub falls through to the real uname for any flag it doesn't care about,
# so other tools that happen to call uname during the test still work.
_install_uname_stub() {
  spoofed_s="$1"
  spoofed_m="$2"
  stub_dir="$TEST_TMP/stubs"
  mkdir -p "$stub_dir"
  real_uname=$(command -v uname)
  cat >"$stub_dir/uname" <<EOF
#!/bin/sh
case "\$1" in
  -s) echo "${spoofed_s}" ;;
  -m) echo "${spoofed_m}" ;;
  *) ${real_uname} "\$@" ;;
esac
EOF
  chmod +x "$stub_dir/uname"
  echo "$stub_dir"
}

@test "Windows host is refused with exit 64 and a clear message" {
  stub_dir=$(_install_uname_stub "MINGW64_NT-10.0" "x86_64")
  PATH="$stub_dir:$PATH" LLAMASTASH_QUIET= run "$INSTALL_SH"
  [ "$status" -eq 64 ]
  echo "$output" | grep -q "Windows"
}

@test "unsupported Linux arch is refused with exit 64" {
  stub_dir=$(_install_uname_stub "Linux" "riscv64")
  PATH="$stub_dir:$PATH" LLAMASTASH_QUIET= run "$INSTALL_SH"
  [ "$status" -eq 64 ]
  echo "$output" | grep -q "unsupported Linux arch"
}

@test "unsupported macOS arch is refused with exit 64" {
  stub_dir=$(_install_uname_stub "Darwin" "powerpc")
  PATH="$stub_dir:$PATH" LLAMASTASH_QUIET= run "$INSTALL_SH"
  [ "$status" -eq 64 ]
  echo "$output" | grep -q "unsupported macOS arch"
}

@test "exotic OS is refused with exit 64" {
  stub_dir=$(_install_uname_stub "Plan9" "x86_64")
  PATH="$stub_dir:$PATH" LLAMASTASH_QUIET= run "$INSTALL_SH"
  [ "$status" -eq 64 ]
  echo "$output" | grep -q "unsupported OS"
}
