# Media recipes on the Z13 - sd.cpp (Krea 2, MOSS-Image), FastFlowLM ASR, whisper.cpp

**Host:** `deepu-flowz13-arch`, ASUS ROG Flow Z13 GZ302EA, AMD Ryzen AI MAX+ 395 (Strix Halo: XDNA2 NPU, 8 columns; Radeon 8060S, gfx1151, 124.5 GiB unified). Kernel 7.2.11-arch1-1, ROCm 7.2.53211, NPU FW 1.1.2.65, `amdxdna` 0.10.
**Binary under test:** `target/debug/llamastash` from `main` at `71902d10`, with the media routes of PR #120.
**Engine versions (all real binaries, no fixtures):**

| Component | Version tested | Latest upstream on 2026-10-24 | Install path |
|---|---|---|---|
| `flm` (FastFlowLM) | v1.0.10 | v1.0.10, released 2026-10-22 (same) | portable `fastflowlm_1.0.10_linux.tar.gz` (38 MB) into `~/.local/opt/flm1010`; the box also keeps v1.0.9 at `~/.local/opt/fastflowlm`, which the live config points at. `flm validate` passes |
| `sd-server` (stable-diffusion.cpp) | `master-1000-95ff762` | `master-1000-95ff762`, released 2026-10-23 (same) | built here with `cmake -B build-hip -DSD_HIPBLAS=ON -DGPU_TARGETS=gfx1151`, `/mnt/work/Workspace/llms/stable-diffusion.cpp/build-hip/bin/sd-server` |
| `whisper-server` (whisper.cpp) | v1.8.4 | - | Lemonade 2026.43.3's `whispercpp:cpu` backend, `~/.cache/lemonade/bin/whispercpp/cpu/whisper-server` |

**Weights pulled for the runs** (all new downloads, 21.3 GiB, into `/mnt/work/sd-models`): `MOSS-Image-SFT-Q8_0.gguf` 5.7 GB (`leejet/MOSS-Image-SFT-GGUF`), `Qwen3.5-9B-Q4_K_M.gguf` 5.7 GB (`unsloth/Qwen3.5-9B-GGUF`), `flux2-vae.safetensors` 336 MB (`Comfy-Org/MOSS-Image`), `tokenizer.json` 20 MB (`OpenMOSS-Team/MOSS-Image-Base`), `Krea-2-Base-Q4_K_M.gguf` 7.3 GB (`realrebelai/KREA-2_GGUFs`), `Qwen3VL-4B-Instruct-Q4_K_M.gguf` 2.5 GB (`Qwen/Qwen3-VL-4B-Instruct-GGUF`), `wan_2.1_vae.safetensors` 242 MB (`Comfy-Org/Wan_2.1_ComfyUI_repackaged`). FLM's `Whisper-V3-Turbo-NPU2` (620 MB `model.q4nx`) was already installed.

All LlamaStash runs used isolated daemons (`LLAMASTASH_STATE_DIR` / `_CONFIG_DIR` / `_CACHE_DIR` under `~/.local/state/ls-media-z13` and `ls-whisper-z13`, proxy ports 11635 and 11636). The installed `llamastash` and the user's live daemon were never used or restarted. Test audio: whisper.cpp's `samples/jfk.wav`, 11 s of real speech, not a tone.

## Verdict

Every media route a Z13 can serve now works through the proxy with a generic entry, and the two image models are recipe-documented in [`RECIPES.md`](../../RECIPES.md) with their numbers. The audio side needed no code: `modes: [stt]` plus the engine's own route shape is enough, and the alias and mode-fallback paths both reach an NPU Whisper. The image side also needed no code, but the sd.cpp HIP backend is the first time this project runs an image engine on ROCm, and it holds up: both models load fully on the GPU with flash attention enabled and produce correct images with no flags beyond the model paths.

Three things are worth knowing, all in § Gaps: an `stt` or `image` launch prints `chat` in the `status` launch table, `dall-e-3`-style fallback names are ambiguous as soon as two entries declare one mode, and `llamastash start` right after `daemon start` can fail with `no model matches ... (0 known)` while the first scan is still running.

## A. Speech-to-text through the proxy

Entries: `whisper-npu` (`flm serve --asr 1`, `modes: [stt]`) and `whisper-tiny` (whisper.cpp with `--request-path /v1/audio --inference-path /transcriptions`, `modes: [stt]`).

| # | Check | Result |
|---|---|---|
| 1 | `POST /v1/audio/transcriptions` naming the entry | pass. FLM on the NPU: 200 in 3.8 s (2.8 s TTFB), text `And so, my fellow Americans, ask not what your country can do for you, ask what you can do for your country.` whisper.cpp on CPU: 200 in 0.27 s, same words |
| 2 | Same route with `model: whisper-1` and `proxy.aliases: whisper-1: whisper-npu` | pass, 200, and `requests` logs the row as `alias` |
| 3 | Same route with a name nothing answers to | pass. `x-llamastash-served-by: whisper-npu`, `x-llamastash-fallback-reason: mode_match`, logged as `fallback (mode_match)`. FLM echoes whatever `model` the client sent, so the reply says `some-random-name` |
| 4 | `POST /v1/audio/translations` | 404 from FLM (`{"error":"Not Found"}`). The proxy forwards the route; FLM only serves transcriptions, and whisper.cpp is the same |
| 5 | `list --json` for a media row | `metadata.mode_hint: "stt"` and `weights_bytes` from `memory_gib`, before and after launch |
| 6 | whisper.cpp route renaming | `--request-path /v1/audio --inference-path /transcriptions` is what puts it on the OpenAI path, and `ready` has to follow the prefix (`/v1/audio/health`). Without the flags the entry is ready but every transcription 404s at the engine |
| 7 | Two `stt` entries configured, `whisper-1` with no alias | refused as ambiguous (`2 entries declare this endpoint's mode and none is running` style), which is the documented rule, not a bug. The alias settles it |

## B. Image generation on the sd.cpp HIP build

Both entries: `sd-server` with `--listen-ip {host} --listen-port {port}`, `ready: /v1/models`, `modes: [image]`. No `--offload-to-cpu`: on 124.5 GiB of unified memory the weights all go to the GPU (`total params memory size = 10213.61MB (VRAM 10213.61MB, RAM 0.00MB)`), and the engine turned flash attention on by itself (`flash attention auto: ROCm0 -> on (HIP MMA)`).

| Model | Files | Steps | Wall at 1024x1024 | Output |
|---|---|---|---|---|
| MOSS-Image SFT Q8_0 | + Qwen3.5-9B Q4_K_M + FLUX.2 VAE + external `tokenizer.json`, `--cfg-scale 3.0`; 10.2 GiB resident | 20 (engine default) | 5m26s cold, load and first-run graph build included | correct: a red fox on a mossy rock, clean photoreal, no artifacts |
| Krea 2 Base Q4_K_M | + Qwen3-VL-4B Q4_K_M + Wan 2.1 VAE, `--cfg-scale 3.5`; 9.5 GiB resident | 20 | 9m57s | style correct (ink and watercolor poster), but the lettering inside the image came out garbled |
| Krea 2 Base Q4_K_M | same | 28 (`--steps` through `sd_cpp_extra_args`), `seed 1234` | 9m36s | `HELLO SUNSHINE` rendered exactly, headline legible; only the small filler text is garbled |

Notes that belong with the recipe:

- `--tokenizer` is not optional for MOSS-Image. Its Qwen3.5 vocabulary is not the embedded Qwen tokenizer, and the engine says so (`using external tokenizer (main)`).
- The recipe defaults from sd.cpp's own `examples/server/recipes/*.json` (MOSS 3.0, Krea 2 3.5, both 20 steps, `flow_shift` 5 for MOSS) differ from the server's argv defaults (guidance 7.0), so they have to be set on argv or per request.
- The OpenAI endpoint takes only `prompt`, `n`, `size`, `output_format`, `output_compression`. Steps, guidance, seed and `flow_shift` ride in as `<sd_cpp_extra_args>{...}` appended to the prompt. The response always carries `b64_json`, and `response_format` is ignored.
- Sizes must be divisible by 16 (the engine's own note), which 512/768/1024 all satisfy.
- Cold and warm timings are close, so most of the wall time is sampling, not loading: ~16 s/step for MOSS 5B Q8 and ~20 s/step for Krea 2 12B Q4 at 1024x1024 on the 8060S. Two engines can be loaded at once here (9.5 + 10.2 GiB), but nothing about the proxy forces that.

## C. Driving them from an agent

Both harnesses get one CLI, `ls-image` (personal dotfiles, not shipped here): it reads the proxy URL out of `llamastash status --json`, picks the model from `LS_IMAGE_MODEL` then `~/.config/llamastash/image-default` (per machine: `krea2` on the Z13, `moss-image` on the XPS) then the only ready or only configured image row, starts it if it is stopped, waits for ready, posts the request and writes the PNGs. Steps, guidance, seed and `flow_shift` go through the `sd_cpp_extra_args` channel, and `/v1/images/edits` is available with `--edit`.

## Gaps

1. **An `stt` or `image` launch prints `chat` in `status`.** `list --json` reports the row's `mode_hint` correctly (`stt`, `image`), and the launch table's `MODE` column shows `chat` for both media launches, which is the launch's mode default rather than what the entry serves. Filed in [`TODO.md`](../../TODO.md).
2. **`daemon start` returns before the catalog is scanned.** `llamastash start <entry>` issued right after it fails with `no model matches \`moss-image\` (0 known)` and works seconds later; this happened on every fresh daemon here. Same family as the Lemonade umbrella case in the 2026-10-17 run: nothing re-scans or blocks until the first scan lands. Filed in [`TODO.md`](../../TODO.md).
3. **`dall-e-3` and `whisper-1` stop being magic with a second entry.** One entry per mode makes the fallback unambiguous; two makes it a refusal. The fix is a `proxy.aliases` line, and it is worth saying in the media docs that configuring two image or two `stt` engines costs the bare OpenAI names.
