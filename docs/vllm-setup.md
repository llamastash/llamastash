# vLLM setup

vLLM serves **safetensors HuggingFace repos** — the non-GGUF half of your model
cache. It sits alongside llama.cpp rather than competing for the same files: a
GGUF still binds llama.cpp (or ds4), and a safetensors repo binds vLLM.

LlamaStash never installs vLLM. You supply the launcher; the backend is on by
default whenever a `vllm` is found, and contributes nothing when it isn't.

> **Experimental.** Validated against vLLM `0.19.1` on one Strix Halo / ROCm
> host. Behaviour and config may change.

## Install

### NVIDIA / CUDA

The published wheels are CUDA-only, so a venv is the straightforward route:

```bash
python3 -m venv ~/.venvs/vllm
~/.venvs/vllm/bin/pip install vllm
```

Then point LlamaStash at it:

```yaml
backend:
  vllm:
    servers:
      - binary: /home/you/.venvs/vllm/bin/vllm
```

### AMD / ROCm — the container route

There is no ROCm wheel on PyPI. AMD publishes prebuilt images, including
architecture-specific ones (`gfx1151` for Strix Halo / Ryzen AI Max):

```bash
docker pull rocm/vllm:rocm7.13.0_gfx1151_ubuntu24.04_py3.13_pytorch_2.10.0_vllm_0.19.1
```

LlamaStash spawns a **binary**, so wrap the container in a small script and
point `servers[0].binary` at that. Save as `~/bin/vllm`, `chmod +x`:

```bash
#!/usr/bin/env bash
set -euo pipefail
IMAGE="rocm/vllm:rocm7.13.0_gfx1151_ubuntu24.04_py3.13_pytorch_2.10.0_vllm_0.19.1"
# Hardcode the cache dir — do NOT read $HF_HOME here. LlamaStash strips
# HF_HOME (along with HF_TOKEN) from every backend child, so under `set -u`
# the wrapper would abort before docker ever runs.
HF_DIR="/home/you/.cache/huggingface"
exec docker run --rm --network host \
  --device /dev/kfd --device /dev/dri \
  --group-add "$(getent group render | cut -d: -f3)" \
  --group-add "$(getent group video  | cut -d: -f3)" \
  --ipc host --shm-size 8g --security-opt seccomp=unconfined \
  -v "$HF_DIR:$HF_DIR" -e "HF_HOME=$HF_DIR" \
  --entrypoint vllm "$IMAGE" "$@"
```

Four things in there are load-bearing:

- **The cache path is hardcoded, not read from `$HF_HOME`.** LlamaStash strips
  `HF_HOME`, `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` and `HF_ENDPOINT` from every
  backend child as a credential-hygiene measure, so the variable is guaranteed
  absent inside the wrapper. Set it explicitly and re-export it into the
  container.
- **`--group-add` must be numeric.** The image has no `render` / `video` group
  entries by name, so `--group-add render` fails with
  `unable to find group render`.
- **`--network host`** so the port LlamaStash reserved is the port vLLM binds.
- **The HF cache is bind-mounted at the same path inside and out**, because
  LlamaStash passes an absolute host path as the model argument.

Your user needs to be in the `docker`, `video` and `render` groups.

## Enabling and disabling

vLLM is on whenever the launcher resolves. The tri-state mirrors the other
detected backends:

| `backend.vllm.enabled` | Result |
|---|---|
| unset (default) | on when a `vllm` resolves, silent no-op when it doesn't |
| `true` | force on |
| `false` | force off even when present |

`daemon start --vllm` and `LLAMASTASH_VLLM=1` force it on over an explicit
`false`. Check what the daemon decided:

```bash
llamastash status --json | jq '.backends[] | select(.id == "vllm")'
```

## Tuning

Context length is the shared `--ctx` knob and maps to `--max-model-len`.
Everything else vLLM-specific is a native knob, settable in the TUI launch
picker or saved in a preset:

| Knob | Flag |
|---|---|
| `gpu_memory_utilization` | `--gpu-memory-utilization` |
| `max_num_seqs` | `--max-num-seqs` |
| `tensor_parallel_size` | `--tensor-parallel-size` |
| `dtype` | `--dtype` |
| `kv_cache_dtype` | `--kv-cache-dtype` |
| `quantization` | `--quantization` |
| `enforce_eager` | `--enforce-eager` |
| `trust_remote_code` | `--trust-remote-code` |

vLLM has ~240 flags; the rest ride the `-- <extras>` tail. A handful are
refused there because they would undo the loopback-only posture or spawn
processes the supervisor cannot reap: `--api-key`, `--allowed-origins`,
`--allowed-local-media-path`, `--pipeline-parallel-size`, `--data-parallel`,
`--distributed-executor-backend`, `--ray`, plus the shared `--host` / `--ssl-*`
denylist.

## Notes and limitations

- **Detection is a filesystem check, never an exec.** vLLM builds its argument
  parser through a device probe, so even `vllm --version` fails with
  `RuntimeError: Failed to infer device type` on a host with no usable
  accelerator. LlamaStash only checks that the path exists — which is also why
  a wrapper script works.
- **Startup is slow.** Weight load is quick, but engine init (memory profiling
  plus KV-cache build) took 10-27 s on a 0.5B and runs far longer on real
  models. Readiness waits for `/v1/models` to advertise the model, not just for
  the port to answer.
- **Unified-memory hosts see the whole pool.** On Strix Halo, vLLM profiled
  101.9 GiB of KV cache rather than the 4 GiB dedicated VRAM carve-out.
- **The model name is the repo id.** LlamaStash passes `--served-model-name`, so
  `/v1/models` and your requests use `owner/name`, not the cache path.
- **No GGUF on vLLM.** A GGUF binds llama.cpp (or ds4). vLLM claims safetensors
  repos only.
- **Single-host only.** Tensor parallel across local GPUs is exposed;
  multi-node and Ray are out of scope.
- **No memory admission gate.** The pre-spawn OOM projection is GGUF-header
  math and is skipped for vLLM; `gpu_memory_utilization` is what governs.
