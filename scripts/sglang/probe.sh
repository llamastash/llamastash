#!/usr/bin/env bash
# Read the facts the SGLang backend is built on off the real image.
# Usage: scripts/sglang/probe.sh <version|help|flags|info|shell> [extra args...]
#
# `version`, `help`, `flags` and `shell` need no GPU and load no model — the
# parser builds without one. `info` only reads from a server you already run.
set -uo pipefail

IMAGE="${SGLANG_IMAGE:-lmsysorg/sglang:v0.5.18-cu130}"
PORT="${SGLANG_PORT:-30000}"

# The heads the backend declares as knobs, refuses in extras, or reads.
INTEREST='--model-path|--served-model-name|--host |--port |--context-length|--mem-fraction-static|--max-total-tokens|--enable-unified-memory|--max-running-requests|--quantization |--trust-remote-code|--tool-call-parser|--reasoning-parser|--api-key|--admin-api-key|--enable-ssl-refresh|--dist-init-addr|--nnodes|--node-rank|--dp-size|--pp-size|--tp-size|--disaggregation-mode|--grpc-mode|--grpc-port|--sidecar|--fastapi-root-path|--file-storage-path|--config '

case "${1:-help}" in
  version)
    docker run --rm --entrypoint sh "$IMAGE" -c \
      'command -v sglang || echo "no sglang console script"; python3 -c "import sglang; print(sglang.__version__)"'
    ;;
  help)
    # Ground truth for the flag surface: the real binary, not docs.
    docker run --rm --entrypoint sglang "$IMAGE" serve --help
    ;;
  flags)
    docker run --rm --entrypoint sglang "$IMAGE" serve --help 2>/dev/null \
      | grep -E -A3 -- "^  ($INTEREST)"
    ;;
  info)
    # Read-only against a server you already run on $PORT.
    curl -s --max-time 5 "http://127.0.0.1:${PORT}/get_server_info" \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); [print(k,"=",repr(d[k])[:80]) for k in sorted(d) if any(s in k for s in ("token","context","model","mem","served","version","port","host"))]'
    echo "--- /v1/models"
    curl -s --max-time 5 "http://127.0.0.1:${PORT}/v1/models"
    echo
    ;;
  shell)
    shift
    docker run -it --rm --entrypoint bash "$IMAGE" "$@"
    ;;
  *)
    echo "usage: $0 <version|help|flags|info|shell>" >&2
    exit 2
    ;;
esac
