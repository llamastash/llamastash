---
title: "spike: ggml-org/llama.cpp GitHub Releases asset + checksum contract"
date: 2026-05-19
status: complete
unblocks: ["Unit 8"]
breaking_finding: "No `ubuntu-cuda-*` prebuilt exists; Linux + Nvidia cannot route to a CUDA-enabled GH Releases asset."
---

# Findings

## 1. Asset naming is stable

Across the 2 most recent releases (`b9219` 2026-05-18, `b9216` 2026-05-18) every binary asset follows the pattern:

```
llama-b<BUILD>-bin-<platform>-[<variant>-]<arch>.<tar.gz|zip>
```

Where:
- `<BUILD>` is a monotonic integer (`9216`, `9219`).
- `<platform>` ∈ {`ubuntu`, `macos`, `win`, `310p-openEuler`, `910b-openEuler`, `android`} for the platforms v2 cares about: `ubuntu`, `macos`, `win` (Windows is post-v2).
- `<variant>` ∈ {absent → CPU, `vulkan`, `rocm-<ver>`, `sycl-fp16`, `sycl-fp32`, `cuda-<ver>`, `cpu`, `hip-radeon`, `opencl-adreno`, `openvino-<ver>`, `kleidiai`} — see breaking finding below.
- `<arch>` ∈ {`x64`, `arm64`}.
- macOS Apple Silicon uses `macos-arm64` without an explicit `metal` tag — Metal is the default backend for that asset.

**Regex for v2:** `^llama-b\d+-bin-(?<platform>ubuntu|macos|win)(?:-(?<variant>[a-z0-9.-]+?))?-(?<arch>x64|arm64)\.(?:tar\.gz|zip)$`. The `cudart-llama-bin-win-cuda-*` Windows CUDA runtime is a sibling asset, not the main `llama-server` artifact.

## 2. SHA-256 lives in the API `digest` field, not the body

The GitHub REST API response (`GET /repos/ggml-org/llama.cpp/releases`) populates each `assets[].digest` with `sha256:<64-hex>`. **No discrete `.sha256` sidecar files. No SHA-256 in the rendered release body text.**

Unit 8 reads asset URL and SHA from the same JSON document — one round trip after the release listing, no body-text parser needed.

```jsonc
// from gh api repos/ggml-org/llama.cpp/releases?per_page=1
{
  "name": "llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
  "browser_download_url": "https://github.com/ggml-org/llama.cpp/releases/download/b9219/llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
  "size": 31474474,
  "digest": "sha256:a97b6e989ac00c438b8b702d0766e619c80f204c39016ebade534b7f997e0455"
}
```

## 3. Breaking finding — Linux + Nvidia has no CUDA prebuilt

**The release does not ship any `ubuntu-cuda-*` asset.** Linux CUDA prebuilds exist only as `cudart-llama-bin-win-cuda-*` (Windows runtime DLLs, irrelevant on Linux). The full Linux variant table is:

| Variant | Asset | Backend |
|---|---|---|
| CPU | `llama-bNNNN-bin-ubuntu-x64.tar.gz` | scalar / AVX |
| CPU ARM | `llama-bNNNN-bin-ubuntu-arm64.tar.gz` | scalar / NEON |
| Vulkan | `llama-bNNNN-bin-ubuntu-vulkan-x64.tar.gz` | Vulkan (works on Nvidia via Vulkan driver) |
| Vulkan ARM | `llama-bNNNN-bin-ubuntu-vulkan-arm64.tar.gz` | Vulkan |
| ROCm | `llama-bNNNN-bin-ubuntu-rocm-7.2-x64.tar.gz` | AMD |
| SYCL fp16 | `llama-bNNNN-bin-ubuntu-sycl-fp16-x64.tar.gz` | Intel oneAPI |
| SYCL fp32 | `llama-bNNNN-bin-ubuntu-sycl-fp32-x64.tar.gz` | Intel oneAPI |
| OpenVINO | `llama-bNNNN-bin-ubuntu-openvino-2026.0-x64.tar.gz` | Intel OpenVINO |
| s390x | `llama-bNNNN-bin-ubuntu-s390x.tar.gz` | IBM Z |

### Implication for R52 (hardware-aware install default)

The original plan's R52 routing matrix assumed `Linux + Nvidia → GH Releases CUDA prebuilt`. **That asset does not exist.** Unit 8 must update the routing:

| OS | GPU class | New default | Rationale |
|---|---|---|---|
| Linux | Nvidia | **Vulkan prebuilt** with banner "CUDA prebuilt unavailable upstream — running on Vulkan (~75% of CUDA perf). Point at a custom binary for native CUDA." | Closest working path without a build step. |
| Linux | AMD | ROCm 7.2 prebuilt | Unchanged. |
| Linux | Intel iGPU | SYCL fp16 prebuilt | New route. |
| Linux | CPU-only | CPU prebuilt | Unchanged. |
| macOS arm64 | Apple Silicon | brew (CPU+Metal) or GH Releases `macos-arm64` | Both viable; user preference. |
| macOS x86_64 | Apple Intel | GH Releases `macos-x64` | Metal not supported. |

A v2 *follow-up* should consider shipping a from-source compile recipe (with detected NVCC) for Linux+Nvidia users who want CUDA. v2 itself does not include a build step (per plan Scope Boundaries).

## 4. Rate limit (R71)

Unauthenticated GH REST API rate limit: 60/hr per source IP. The single `GET /releases?per_page=1` call costs 1 quota unit; the per-asset `browser_download_url` GET is *not* rate-limited (it's served from a CDN, not the API). Unit 4's `FetchClient` must classify 403 + `X-RateLimit-Remaining: 0` as `FetchError::RateLimited` so Unit 8's retry-once-then-fallback path triggers correctly.

## Unknowns left to implementation

- **Archive shape varies per platform.** `ubuntu-*` and `macos-*` are `tar.gz` with a `build/bin/llama-server` entry inside. `win-*` are `zip` with `llama-server.exe` at archive root. Unit 8's `safe_extract` must locate the entry by basename, not by hard-coded path.
- **macOS bottle vs GH Releases parity.** brew's `llama.cpp` formula version 9200 (May 2026) ships Metal-enabled binaries for `arm64_darwin`; the GH Releases `macos-arm64` ships Metal-enabled too. Both are valid; the wizard prefers brew when `brew` is on PATH (origin: R52 detection).
- **Variant detection heuristic for Linux + Nvidia + Vulkan-not-available case.** If `vulkan::probe()` returns "no Vulkan loader on this system", Vulkan prebuilt cannot run. Unit 3's `detect_hardware` should surface this so Unit 10's wizard can offer the custom-path fallback up-front instead of after a failed `--version` probe.
