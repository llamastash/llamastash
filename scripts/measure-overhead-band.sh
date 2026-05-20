#!/usr/bin/env bash
# Wrapper around measure-overhead-band.py. Downloads the canonical
# 7B Q4_K_M GGUF if missing, auto-detects the backend, runs the
# measurement, and drops a timestamped JSON into
# data/overhead-band-measurements/.
#
# See docs/runbooks/measure-vram-overhead-band.md for the full
# procedure including per-backend gotchas.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODEL_REPO="${MODEL_REPO:-Qwen/Qwen2.5-7B-Instruct-GGUF}"
MODEL_FILE="${MODEL_FILE:-qwen2.5-7b-instruct-q4_k_m.gguf}"
MODEL_PATH="${LLAMASTASH_MODEL_PATH:-$REPO_ROOT/.cache/$MODEL_FILE}"

OUT_DIR="${LLAMASTASH_MEASURE_OUT:-$REPO_ROOT/data/overhead-band-measurements}"

LLAMA_SERVER="${LLAMA_SERVER:-}"
BACKEND="${BACKEND:-}"
RUNS="${RUNS:-5}"
CTX="${CTX:-4096}"
NGL="${NGL:-99}"
PORT="${PORT:-8089}"
GPU_ID="${GPU_ID:-0}"
SKIP_DOWNLOAD="${SKIP_DOWNLOAD:-0}"

usage() {
    cat <<EOF
Usage: $0 [options]

Options (also overridable via env var of the same uppercased name):
  --llama-server PATH    llama-server binary (default: \$PATH lookup)
  --model PATH           path to a 7B Q4_K_M GGUF
                         (default: \$REPO_ROOT/.cache/$MODEL_FILE)
  --backend {cuda|hip|vulkan|metal}
                         override auto-detection
  --runs N               sample count (default: $RUNS)
  --ctx N                --ctx-size (default: $CTX)
  --ngl N                --n-gpu-layers (default: $NGL)
  --port N               server port (default: $PORT)
  --gpu-id N             GPU index for multi-GPU hosts (default: $GPU_ID)
  --out-dir PATH         output directory (default: $OUT_DIR)
  --skip-download        do not fetch the model if missing
  -h, --help             show this message

Hardware coverage matrix (run once per backend; llamastash targets
Linux + macOS only, so Windows hosts are out of scope):
  CUDA    — NVIDIA on Linux with CUDA-built llama-server
  HIP     — AMD on Linux with ROCm-built llama-server
  Vulkan  — AMD/NVIDIA/Intel on Linux with Vulkan-built llama-server
  Metal   — Apple Silicon with Metal-built llama-server
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --llama-server) LLAMA_SERVER="$2"; shift 2;;
        --model) MODEL_PATH="$2"; shift 2;;
        --backend) BACKEND="$2"; shift 2;;
        --runs) RUNS="$2"; shift 2;;
        --ctx) CTX="$2"; shift 2;;
        --ngl) NGL="$2"; shift 2;;
        --port) PORT="$2"; shift 2;;
        --gpu-id) GPU_ID="$2"; shift 2;;
        --out-dir) OUT_DIR="$2"; shift 2;;
        --skip-download) SKIP_DOWNLOAD=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "unknown arg: $1" >&2; usage >&2; exit 2;;
    esac
done

if [[ -z "$LLAMA_SERVER" ]]; then
    if command -v llama-server >/dev/null 2>&1; then
        LLAMA_SERVER="$(command -v llama-server)"
    else
        echo "error: llama-server not found on PATH." >&2
        echo "       set --llama-server /path/to/llama-server or LLAMA_SERVER=..." >&2
        exit 1
    fi
fi

if [[ ! -x "$LLAMA_SERVER" ]]; then
    echo "error: $LLAMA_SERVER is not executable" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "error: python3 not found on PATH" >&2
    exit 1
fi

mkdir -p "$OUT_DIR" "$(dirname "$MODEL_PATH")"

if [[ ! -f "$MODEL_PATH" ]]; then
    if [[ "$SKIP_DOWNLOAD" == "1" ]]; then
        echo "error: model not found at $MODEL_PATH and --skip-download set" >&2
        exit 1
    fi
    echo "==> model not found at $MODEL_PATH — fetching $MODEL_REPO/$MODEL_FILE" >&2
    if command -v huggingface-cli >/dev/null 2>&1; then
        huggingface-cli download "$MODEL_REPO" "$MODEL_FILE" \
            --local-dir "$(dirname "$MODEL_PATH")"
    else
        URL="https://huggingface.co/$MODEL_REPO/resolve/main/$MODEL_FILE?download=true"
        if command -v curl >/dev/null 2>&1; then
            curl -L --fail --progress-bar -o "$MODEL_PATH.part" "$URL"
            mv "$MODEL_PATH.part" "$MODEL_PATH"
        elif command -v wget >/dev/null 2>&1; then
            wget --show-progress -O "$MODEL_PATH.part" "$URL"
            mv "$MODEL_PATH.part" "$MODEL_PATH"
        else
            echo "error: need huggingface-cli, curl, or wget to fetch the model" >&2
            exit 1
        fi
    fi
fi

HOST_SHORT="$(
    if command -v hostname >/dev/null 2>&1; then
        hostname -s 2>/dev/null || hostname
    elif [[ -f /etc/hostname ]]; then
        cut -d. -f1 /etc/hostname
    else
        uname -n | cut -d. -f1
    fi
)"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="${BACKEND:-auto}"
OUT_JSON="$OUT_DIR/${HOST_SHORT}-${TAG}-${TS}.json"
LOG_DIR="$OUT_DIR/${HOST_SHORT}-${TAG}-${TS}-logs"

BACKEND_ARGS=()
if [[ -n "$BACKEND" ]]; then
    BACKEND_ARGS=(--backend "$BACKEND")
fi

echo "==> measuring overhead band" >&2
echo "    llama-server: $LLAMA_SERVER" >&2
echo "    model:        $MODEL_PATH" >&2
echo "    backend:      ${BACKEND:-auto-detect}" >&2
echo "    runs:         $RUNS" >&2
echo "    ctx:          $CTX" >&2
echo "    out:          $OUT_JSON" >&2
echo >&2

python3 "$SCRIPT_DIR/measure-overhead-band.py" \
    --llama-server "$LLAMA_SERVER" \
    --model "$MODEL_PATH" \
    --runs "$RUNS" \
    --ctx "$CTX" \
    --ngl "$NGL" \
    --port "$PORT" \
    --gpu-id "$GPU_ID" \
    --log-dir "$LOG_DIR" \
    --out "$OUT_JSON" \
    "${BACKEND_ARGS[@]}"

echo >&2
echo "==> done. JSON: $OUT_JSON" >&2
echo "    logs: $LOG_DIR/" >&2
echo "    commit the JSON to data/overhead-band-measurements/ and PR" >&2
echo "    the snapshot update once all backends are covered." >&2
