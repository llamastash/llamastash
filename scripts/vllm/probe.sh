#!/usr/bin/env bash
# Probe whether vLLM actually runs on this gfx1151 (Strix Halo) box.
# Usage: vllm_probe.sh <serve|help|shell> [extra args...]
set -uo pipefail

IMAGE="rocm/vllm:rocm7.13.0_gfx1151_ubuntu24.04_py3.13_pytorch_2.10.0_vllm_0.19.1"
MODEL="Qwen/Qwen2.5-0.5B-Instruct"
PORT="${VLLM_PORT:-8010}"
HF_DIR="/mnt/work/huggingface"

common_args=(
  --rm
  --device /dev/kfd --device /dev/dri
  # Numeric GIDs: the image has no `render`/`video` group entries by name.
  --group-add "$(getent group render | cut -d: -f3)"
  --group-add "$(getent group video  | cut -d: -f3)"
  --ipc host --shm-size 8g
  --security-opt seccomp=unconfined
  -v "${HF_DIR}:/hf"
  -e HF_HOME=/hf
  -e HF_HUB_OFFLINE=1
)

case "${1:-serve}" in
  help)
    # Ground truth for the flag surface: the real binary, not docs.
    docker run "${common_args[@]}" --entrypoint vllm "$IMAGE" serve --help
    ;;
  version)
    docker run "${common_args[@]}" --entrypoint bash "$IMAGE" -c \
      'vllm --version; python -c "import torch;print(\"torch\",torch.__version__,\"hip\",torch.version.hip)"; python -c "import torch;print(\"gpu\",torch.cuda.is_available(),torch.cuda.get_device_name(0) if torch.cuda.is_available() else \"none\")"'
    ;;
  shell)
    shift
    docker run -it "${common_args[@]}" --entrypoint bash "$IMAGE" "$@"
    ;;
  serve)
    shift
    docker run "${common_args[@]}" --name vllm-probe -p "127.0.0.1:${PORT}:8000" \
      --entrypoint vllm "$IMAGE" serve "$MODEL" \
      --host 0.0.0.0 --port 8000 \
      --max-model-len 2048 \
      --gpu-memory-utilization 0.85 \
      --enforce-eager \
      "$@"
    ;;
  *)
    echo "usage: $0 <serve|help|version|shell>" >&2
    exit 2
    ;;
esac
