---
title: "spike: Homebrew llama.cpp Linux bottle GPU status"
date: 2026-05-19
status: complete
unblocks: ["Unit 8"]
---

# Finding

**brew `llama.cpp` formula version 9200 (May 2026) ships `x86_64_linux` and `arm64_linux` bottles with no GPU acceleration (CPU-only). macOS bottles are Metal-enabled by default on Apple Silicon. R52's "brew is acceptable for macOS, CPU-only on Linux" premise is confirmed.**

## Evidence

Source: `https://formulae.brew.sh/api/formula/llama.cpp.json` (fetched 2026-05-19).

- `versions.stable`: `"9200"`.
- `bottle.stable.files` keys present: `arm64_sequoia`, `arm64_sonoma`, `arm64_tahoe`, `sonoma`, `tahoe`, `x86_64_linux`, `arm64_linux`. All `cellar` = `:any_skip_relocation`.
- `build_dependencies`: `["cmake"]`. **No `cuda-toolkit`, `rocm-hip`, `vulkan-loader`, or related GPU deps.** The Linux bottles are built without GPU acceleration.
- `options`: empty. No build-time variant selection; the formula always builds the same target.
- macOS `arm64_*` bottles are Metal-enabled — Metal is the platform default and llama.cpp's CMakeLists enables `GGML_METAL=ON` automatically on Apple Silicon when `-DLLAMA_METAL=OFF` is absent.

## Implications for Unit 8

| OS | brew status | Wizard routing |
|---|---|---|
| macOS arm64 | Metal-enabled bottle | Preferred default — `brew install llama.cpp` is fastest path |
| macOS x86_64 | CPU-only bottle | brew acceptable; user can choose GH Releases `macos-x64` for parity check |
| Linux + Nvidia | CPU-only bottle | **Not appropriate as default** — Vulkan prebuilt (see GH spike) outperforms |
| Linux + AMD | CPU-only bottle | Not appropriate as default — ROCm prebuilt is the right path |
| Linux + CPU only | CPU-only bottle | Acceptable default if linuxbrew is on PATH |

R52's hardware-aware default for Linux must therefore prefer GH Releases for any non-CPU-only configuration, regardless of whether brew is available. brew remains a manual choice in the install-method picker; the wizard does not silently route Linux + GPU through it.

## Common-location probe (R54)

Linux brew installs land under `/home/linuxbrew/.linuxbrew/bin/llama-server` (linuxbrew default) or `/opt/homebrew/bin/llama-server` (macOS arm64 default) or `/usr/local/bin/llama-server` (macOS x86_64 default and many `cargo install` targets). Unit 3's `detect_binary` common-location list must include all three plus `~/.local/bin/llama-server` (user-installed without brew).

## Unknowns left to implementation

- **brew Linux bottle's CPU instruction baseline.** Probably `-march=x86-64-v3` based on bottle naming; not verified. Performance characteristics for Linux + brew + 7B Q4 model: ~5-15 tok/s on a typical 2024 desktop CPU. Mostly irrelevant since the wizard never chooses it for GPU-equipped systems.
- **Homebrew formula stability.** Version bumps every few days. The wizard does not pin a brew version; it invokes `brew install --quiet llama.cpp` and captures whatever the formula resolves to. The installed binary's SHA is recorded in `_init_snapshot.llama_server_digest`; doctor finding #2 (digest drift) is **carved out** for brew installs to avoid spamming users after every `brew upgrade`.
