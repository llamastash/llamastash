# vLLM setup

vLLM serves **safetensors HuggingFace repos** — the non-GGUF half of your model
cache. It sits alongside llama.cpp rather than competing for the same files: a
GGUF still binds llama.cpp (or ds4), and a safetensors repo binds vLLM.

LlamaStash never installs vLLM. You supply the launcher; the backend is on by
default whenever a `vllm` is found, and contributes nothing when it isn't.

> **Experimental.** Validated against vLLM `0.27.1` on one Strix Halo / ROCm
> host. Behaviour and config may change.

## Install

A native install is the recommended route on both vendors. LlamaStash spawns a
binary, so anything that puts a real `vllm` on disk works.

### NVIDIA / CUDA

```bash
python3 -m venv ~/.venvs/vllm
~/.venvs/vllm/bin/pip install vllm
```

### AMD / ROCm

vLLM publishes prebuilt ROCm wheels, and gfx1151 (Strix Halo / Ryzen AI Max) is
on its supported-GPU list. They need **Python 3.12** — on any other version pip
silently falls back to the CUDA wheel, which fails later with
`libcudart.so: cannot open shared object file`.

```bash
uv venv --python 3.12 --seed --managed-python ~/.venvs/vllm
VIRTUAL_ENV=~/.venvs/vllm uv pip install vllm==0.27.1 \
  --extra-index-url https://wheels.vllm.ai/rocm/0.27.1/rocm723
```

Pick the variant matching your ROCm install; `curl -s https://wheels.vllm.ai/rocm/vllm`
lists what is current. ROCm 7.0.2 or newer is required for gfx1151.

### Pointing LlamaStash at it

```yaml
backend:
  vllm:
    servers:
      - binary: /home/you/.venvs/vllm/bin/vllm
```

### Distro caveats

The ROCm wheels are built for glibc-2.34 manylinux and assume a Debian-shaped
system. On Arch, two libraries are missing:

- **`libhipsparselt.so.0`** — `sudo pacman -S hipsparselt`, matching your ROCm
  version.
- **`libmpi_cxx.so.40`** — the wheel links OpenMPI 4's C++ bindings, which
  OpenMPI 5 dropped. `libmpi.so.40` itself is still present, so only the
  bindings are missing. Extract them from a Debian `libopenmpi3` package into
  `site-packages/torch/lib/`, then give each copied library its own RUNPATH so
  the transitive dependencies resolve — `DT_RUNPATH` is not inherited, so
  torch's own `$ORIGIN` does not cover them:

  ```bash
  TL=~/.venvs/vllm/lib/python3.12/site-packages/torch/lib
  # copy libmpi, libmpi_cxx, libopen-pal, libopen-rte into $TL, then:
  for f in "$TL"/lib{mpi,mpi_cxx,open-pal,open-rte}.so.40.*; do
    patchelf --set-rpath '$ORIGIN' "$f"
  done
  ```

Check the install before wiring it up:

```bash
~/.venvs/vllm/bin/vllm --version
```

### Containers

The container images work too, and were the only ROCm route before the wheels
existed. LlamaStash spawns a binary, so you need a wrapper script that `exec`s
`docker run` with `"$@"` appended, pointed at by `servers[0].binary`.

Three things to know before choosing this route:

- **Do not read `$HF_HOME` in the wrapper.** LlamaStash strips `HF_HOME`,
  `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` and `HF_ENDPOINT` from every backend
  child, so the variable is guaranteed absent. Under `set -u` the wrapper aborts
  before docker runs. Hardcode the path and bind-mount it at the same path
  inside and out, since LlamaStash passes an absolute host path as the model
  argument.
- **The supervised process is the docker client, not vLLM.** SIGTERM forwards,
  but the SIGKILL escalation does not: if the graceful window expires, the
  container keeps running, keeps its GPU allocation and keeps the port, while
  LlamaStash reports the model stopped. A native install has no such gap.
- **`--group-add` must be numeric** (`$(getent group render | cut -d: -f3)`).
  The images have no `render` / `video` group entries by name.

Also pass `--network host` so the port LlamaStash reserved is the one vLLM
binds, and `--device /dev/kfd --device /dev/dri` on ROCm.

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
| `kv_cache_memory_bytes` | `--kv-cache-memory-bytes` |
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
`--allowed-local-media-path`, `--pipeline-parallel-size`, the
`--data-parallel-*` family, and `--distributed-executor-backend` (which is how
Ray is selected), plus the shared `--host` / `--ssl-*` denylist.

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
- **On unified-memory hosts, the KV cache is capped automatically.** This is
  the sharpest edge here, so it is worth knowing what the cap is protecting you
  from. On an APU (Strix Halo / Ryzen AI Max and friends) there is no separate
  VRAM pool — the GPU allocates out of the same DRAM your OS is using. vLLM
  sizes its KV cache to fill whatever `gpu_memory_utilization` allows, and that
  is a fraction of the **pool**, not of your model. Measured on a 121 GB box:
  even `0.15` on a 0.5B model reserved 15.1 GiB of KV cache (1.3M tokens, 644x
  concurrency for a 2048-token model) and cost 21.2 GB of RAM. The `0.92`
  default projects to roughly 106 GB and has frozen the machine outright.

  Clamping the fraction does not fix this, because the arithmetic is against
  the wrong number. LlamaStash instead sets `--kv-cache-memory-bytes`, an
  absolute cap that makes vLLM skip memory profiling and honour the figure.
  When the host is unified-memory and you have set neither
  `kv_cache_memory_bytes` nor `gpu_memory_utilization`, the launcher picks a
  budget from live free memory: 8 GiB, or less if weights plus an 8 GiB host
  reserve leave less. Set either knob yourself and the auto-cap steps aside.
  Discrete-GPU hosts are untouched; there the fraction applies to real VRAM.
- **The model name is the repo id.** LlamaStash passes `--served-model-name`, so
  `/v1/models` and your requests use `owner/name`, not the cache path.
- **No GGUF on vLLM.** A GGUF binds llama.cpp (or ds4). vLLM claims safetensors
  repos only.
- **Single-host only.** Tensor parallel across local GPUs is exposed;
  multi-node and Ray are out of scope.
- **No memory admission gate.** The pre-spawn OOM projection is GGUF-header
  math and is skipped for vLLM; `gpu_memory_utilization` is what governs.
