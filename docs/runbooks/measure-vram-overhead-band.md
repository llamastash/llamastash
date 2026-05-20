# Runbook: measure per-backend VRAM overhead band

Background: [`docs/spikes/2026-05-19-vram-overhead-band.md`](../spikes/2026-05-19-vram-overhead-band.md).
TODO entry: `v2-GA blockers` in [`TODO.md`](../../TODO.md).

This procedure replaces the provisional default overhead bands in
`data/benchmark-snapshot.json::recommender_weights.overhead_band_bytes`
with values measured on real hardware. Run once per backend across
the supported matrix (CUDA, HIP, Vulkan, Metal); the snapshot field is
CI-tunable, so a re-measurement updates the published snapshot without
a binary release.

## What "overhead band" means here

The recommender's fit predicate is:

```
estimate_peak_bytes(weights, ctx) ≤ 0.90 × vram − overhead_band[backend]
```

`estimate_peak_bytes` (in `src/init/recommender.rs`) accounts for
weights, activations, and KV cache. The **overhead band** is everything
*else* that lives in VRAM when llama-server is loaded and idle: the
driver, the runtime (cuBLAS, ROCm BLAS, Vulkan loader, Metal command
queues), and the backend's allocator slack. Today the defaults are
guessed from llama.cpp issue chatter — this runbook replaces them with
samples.

## What you'll need

- A `llama-server` binary built for the backend under test.
- A 7B Q4_K_M GGUF — the canonical model is
  `qwen2.5-7b-instruct-q4_k_m.gguf` (~4.36 GiB). The wrapper script
  will download it if missing.
- Python 3.11+.
- ~8 GB free VRAM (or unified RAM on Apple Silicon).
- ~10 minutes per host for a 5-run pass.

## Quickstart

```bash
# Build llama-server for the backend you're testing, then:
./scripts/measure-overhead-band.sh
```

The wrapper:

1. fetches the canonical model into `.cache/` if missing (uses
   `huggingface-cli`, `curl`, or `wget` in that order);
2. auto-detects the backend from the host (override with `--backend`);
3. runs the Python harness for `--runs 5` warm samples;
4. drops a JSON into
   `data/overhead-band-measurements/<host>-<backend>-<utc>.json`,
   plus per-run llama-server logs alongside.

Common overrides:

```bash
# Explicit backend, custom server build, multi-GPU laptop:
./scripts/measure-overhead-band.sh \
    --backend hip \
    --llama-server ~/builds/llama.cpp-rocm/llama-server \
    --gpu-id 1

# Quicker iteration (3 runs, 4k ctx already default):
./scripts/measure-overhead-band.sh --runs 3
```

Env-var equivalents exist for every flag (`LLAMA_SERVER`, `BACKEND`,
`RUNS`, `CTX`, `NGL`, `PORT`, `GPU_ID`, `LLAMASTASH_MODEL_PATH`,
`LLAMASTASH_MEASURE_OUT`).

## Per-backend gotchas

### CUDA (NVIDIA, Linux)

- Build `llama-server` with `cmake -DGGML_CUDA=ON` against the CUDA
  Toolkit matching your driver.
- The sampler reads `nvidia-smi --query-gpu=memory.used` and subtracts
  a pre-spawn baseline, so close other CUDA contexts (other models,
  Stable Diffusion, browser GPU acceleration where possible) before
  the run.
- On laptops with a dGPU + iGPU, pass `--gpu-id` to point at the dGPU
  (`nvidia-smi -L` to enumerate).

### HIP / ROCm (AMD, Linux)

- Build with `cmake -DGGML_HIP=ON` against ROCm 6.x. The driver must
  expose the AMDGPU kernel module — confirm with
  `ls /sys/class/drm/card*/device/mem_info_vram_used`.
- The sampler prefers
  `/sys/class/drm/card<N>/device/mem_info_vram_used` (no root needed,
  immune to rocm-smi schema drift) and falls back to
  `rocm-smi --showmeminfo vram --json`.
- On hybrid laptops, the dGPU is usually `card1` — pass `--gpu-id 1`.
  Confirm with `lspci -nn | grep VGA`.

### Vulkan (AMD / NVIDIA / Intel, Linux)

- Build with `cmake -DGGML_VULKAN=ON`.
- Vulkan itself doesn't expose per-process VRAM; the sampler reuses
  the vendor path (`nvidia-smi` on NVIDIA, AMDGPU sysfs on AMD) and
  takes a delta from baseline. Close other GPU consumers during the
  run.
- Intel Arc / iGPU Vulkan: the sampler currently doesn't know about
  Intel; treat the number as a lower-bound and bump the band by 20%
  before committing.

### Metal (Apple Silicon)

- Build with `cmake -DGGML_METAL=ON` (the default in upstream
  `llama.cpp` on macOS).
- Unified memory means there's no "VRAM" — the sampler reads the
  llama-server process's resident set size (≈ `phys_footprint`).
- Quit memory-heavy apps (browsers, Slack, Xcode) before the run so
  the RSS reading is dominated by llama-server.
- Disable Spotlight indexing on the model directory if you're running
  on a fresh box (`mdutil -i off ~/.cache/llamastash`).

## Reading the output

The harness emits a JSON with this shape:

```json
{
  "schema_version": 1,
  "host": "...",
  "backend": "cuda",
  "gpu": "NVIDIA GeForce RTX 4060 Laptop GPU",
  "model_bytes": 4683960320,
  "ctx": 4096,
  "estimate_peak_bytes": 6323346432,
  "runs": [
    {"run": 1, "baseline_bytes": ..., "peak_bytes": ..., "used_bytes": ...},
    ...
  ],
  "summary": {
    "used_mean_bytes": ...,
    "overhead_mean_bytes": ...,
    "overhead_stdev_bytes": ...,
    "recommended_band_bytes": ...
  }
}
```

`recommended_band_bytes` is `mean(overhead) + 1·stddev`, rounded up to
the nearest 32 MiB. That's the value to drop into the snapshot.

### When `overhead_mean_bytes` is negative

Means the AMDGPU / NVIDIA sysfs VRAM counter showed *less* allocation
than the recommender's `estimate_peak_bytes` predicted. This is **not
a script bug** — it usually indicates one of:

- **UMA / APU host.** Unified-memory hardware (Apple Silicon, AMD APUs
  like Strix Halo, Ryzen AI Max, etc.) doesn't have a discrete VRAM
  pool — the AMDGPU/Metal counter only reflects allocations to a
  driver-managed slab carved out of system RAM, while weights may live
  in GTT / process RSS that the VRAM counter doesn't see. The
  recommender's estimator is calibrated for discrete-VRAM commit
  semantics and overestimates the counter on UMA.
- **`-fit on` / auto memory fit** in newer llama.cpp builds adjusts
  allocation strategy at load time and can route allocations around the
  VRAM counter. Add `-fit off` (or set `LLAMA_ARG_NO_FIT=1`) to force
  the older behavior if you want a direct comparison.
- **MoE models** where only a subset of experts is resident.

A negative residual is still a valid *data point* — record it,
annotate the host class — but **don't merge it into
`overhead_band_bytes` directly**. The canonical numbers are for
discrete GPUs (RX 7900, Radeon Pro, MI300, RTX, Radeon dGPU). UMA
hosts will eventually need an APU-aware branch in the recommender
(tracked as a follow-up); until then, conservative defaults stay.

The wrapper clamps `recommended_band_bytes` to 0 when the mean is
negative — that's a "no usable overhead-band signal from this run"
sentinel, not a real recommendation to zero out the snapshot.

## Manual fallback (no scripts)

If the wrapper or harness can't run on the target host, the same data
is easy to collect by hand. The arithmetic at the bottom is the
load-bearing part.

1. Start the server:
   ```bash
   llama-server \
     --model qwen2.5-7b-instruct-q4_k_m.gguf \
     --ctx-size 4096 \
     --n-gpu-layers 99 \
     --port 8089 \
     --host 127.0.0.1 \
     --no-mmap
   ```
   `--no-mmap` matters — without it the weights stream from page cache
   and the measured VRAM peak under-reports the real footprint, which
   throws off the residual.

2. Wait for `/health` to flip:
   ```bash
   curl -sf http://127.0.0.1:8089/health
   # → {"status":"ok"}
   ```

3. Sample VRAM **once** after `/health=ok` (and capture a pre-spawn
   baseline if you're using a total-GPU sampler):
   - **CUDA**:
     ```bash
     nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits
     ```
     (MiB → ×1024×1024 for bytes)
   - **HIP**:
     ```bash
     cat /sys/class/drm/card0/device/mem_info_vram_used
     ```
     (already bytes)
   - **Vulkan**: whichever vendor tool matches the GPU (CUDA or HIP
     command above).
   - **Metal**:
     ```bash
     ps -o rss= -p "$(pgrep -f llama-server)"
     ```
     (KiB → ×1024 for bytes)

4. Repeat steps 1–3 five times; compute the mean of `(peak − baseline)`
   across runs.

5. Compute the residual:
   ```
   weights_bytes        = size of the .gguf on disk
   estimate_peak_bytes  = weights_bytes × (1.20 + 0.15 × ctx/4096)
                        = weights_bytes × 1.35 at ctx=4096

   overhead_band_bytes  = mean_used − estimate_peak_bytes
   ```

   For the canonical Qwen2.5-7B Q4_K_M (4.36 GiB), `estimate_peak` at
   ctx=4096 is ~5.89 GiB. Anything left over after subtraction is the
   backend overhead band you record.

## Submitting results

1. Commit the per-host JSONs to `data/overhead-band-measurements/`
   (one file per host × backend).
2. When all four backends are covered (or as many as your fleet
   reaches), update
   `data/benchmark-snapshot.json::recommender_weights.overhead_band_bytes`
   with the conservative pick — `mean + 1·stddev`, rounded up to
   32 MiB — for each backend.
3. Run the corpus gate locally to confirm the recalibration doesn't
   flip top-3 picks beyond the 16/20 threshold:
   ```bash
   cargo test --test recommender_corpus
   ```
4. Flip the spike-doc frontmatter from `status: skipped` to
   `status: measured` and update its summary table with the real
   numbers.
5. Strike the line in `TODO.md::v2-GA blockers`.
6. PR with title `feat(recommender): measured per-backend overhead
   band` and link to the measurement JSONs in the description.
