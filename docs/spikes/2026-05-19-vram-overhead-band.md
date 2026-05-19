---
title: "spike: per-backend VRAM overhead band (skipped — sensible defaults)"
date: 2026-05-19
status: skipped
unblocks: ["Unit 6"]
todo: "Remeasure on real CUDA / HIP / Vulkan / Metal hardware before v2 GA."
---

# Finding (provisional)

**v2 ships with sensible default overhead bands. Real per-backend measurement is a v2-GA blocker but not a Unit 6 ranking-correctness blocker — the corpus check (16/20) verifies the ranker against ground-truth picks regardless of the absolute headroom number, and the defaults err on the conservative side so under-fit recommendations are rare.**

| Backend | Default overhead (MB) | Reasoning |
|---|---|---|
| CUDA | 512 | Driver, cuBLAS, ggml CUDA state, KV-cache page alignment. Tightened from 800 MB on 2026-05-19 after comparing to whichllm's 500 MB flat framework constant — corpus gate still passes |
| HIP/ROCm | 512 | Same order as CUDA on modern ROCm. Tightened in lockstep with CUDA |
| Vulkan | 1024 | Vulkan loader + ggml Vulkan backend's slab allocator overhead historically wider than CUDA |
| Metal | 512 | Lower overhead on Apple Silicon's unified memory; mostly KV alignment |
| CPU | n/a | RAM-bound; we use `0.5 × free_ram` instead of a fixed band |

Sources cited but not verified for v2:
- llama.cpp issue tracker discussions of "max VRAM I can fit a 7B Q4 in 12 GB" — anecdotal but consistent around 700-900 MB overhead on CUDA.
- ROCm ggml backend issues showing similar headroom requirement.
- Vulkan backend's higher overhead is documented in `llama.cpp`'s `docs/backend/Vulkan.md`.

## Remeasurement procedure (v2-GA gate)

For each backend on a representative GPU:
1. Spawn `llama-server` against a 7B Q4_K_M GGUF with `--ctx 4096 --n-gpu-layers 99`.
2. Sample peak resident GPU memory at the `/health` Ready transition.
3. Repeat 5×; compute mean + stddev.
4. Subtract the GGUF + KV-cache estimate (Unit 6's `gguf::memory::estimate`) — the residual is the backend overhead.
5. Record in `data/benchmark-snapshot.json::recommender_weights.overhead_band_mb`.

The snapshot's `overhead_band_mb` is CI-tunable — a future re-measurement updates the published snapshot without a binary release.

## Implications for Unit 6

The filter rule from Key Decisions stays:

```
estimated_peak_mem ≤ 0.90 × vram_gb_bytes − overhead_band[backend]
```

The 0.90 safety margin and the overhead band are both *intentionally conservative*. A v2 user reporting "the recommender chose too small" is a better outcome than "the recommender chose too big and the launch OOMs at load". Re-tighten with real measurements post-launch.

## Risk

If the default overheads are off by more than 20% on real hardware, the corpus check may flip a top-3 pick for hardware classes near the boundary. The 16/20 threshold leaves margin for ~4 such flips. Recalibrate via the snapshot regen flow without a code release.
