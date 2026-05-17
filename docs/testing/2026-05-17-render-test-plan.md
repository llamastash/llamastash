---
title: "Test plan: TUI --render against real hardware + LM Studio cache"
type: test-plan
status: active
date: 2026-05-17
target_plans:
  - docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md
  - docs/plans/2026-05-16-001-feat-kdash-style-dashboard-ui-plan.md
---

# Test plan: TUI `--render` against real hardware + LM Studio cache

## Scope

End-to-end exercise of the `--render` headless TUI snapshot path on the
author's actual workstation, with the LM Studio cache at
`/mnt/work/lmstudio-models` as the discovery target. Validate three
things:

1. **Discovery** finds the expected GGUFs.
2. **Host panel** identifies the AMD Radeon 8060S APU, not `cpu_only`,
   and surfaces non-zero VRAM / util / temp readings.
3. **Layout** matches the wireframe in
   `docs/plans/2026-05-16-001-feat-kdash-style-dashboard-ui-plan.md`
   across a representative set of terminal geometries, including the
   small-terminal fallback.

Out of scope for this pass: interactive event flow, modal overlays,
the right-pane chat/embed/rerank action handlers, persisted state.

## Test environment

- **CPU:** AMD Ryzen AI Max+ 395 (16 cores / 32 threads)
- **GPU:** AMD Radeon 8060S iGPU (gfx1151, 40 CUs, ROCm)
- **RAM:** 128 GB unified; BIOS split 64 GB CPU / 64 GB GPU
- **OS:** Arch Linux, kernel 7.0.5
- **ROCm:** rocm-smi 4.0.0+unknown / rocm-smi-lib 7.8.0 — available at
  `/opt/rocm/bin/rocm-smi`
- **`nvidia-smi`:** not installed (intentional — no NVIDIA card)
- **`vulkaninfo`:** not installed
- **LM Studio cache:** `/mnt/work/lmstudio-models/` (configured as
  `downloadsFolder` in `~/.lmstudio/settings.json`)
- **LM Studio home pointer:** `~/.lmstudio-home-pointer` → `dotfiles/.lmstudio-home-pointer`
- **`~/.lmstudio/models/`:** real directory, contains 2 GGUFs (gemma-3-12b-it +
  mmproj)

## Expected GGUF inventory

### `/mnt/work/lmstudio-models/` — 11 `.gguf` files

| Path | Role | Launchable? |
|------|------|------------|
| `lmstudio-community/Qwen3.6-27B-GGUF/Qwen3.6-27B-Q4_K_M.gguf` | chat | yes |
| `lmstudio-community/Qwen3.6-27B-GGUF/Qwen3.6-27B-Q6_K.gguf` | chat | yes |
| `lmstudio-community/Qwen3.6-27B-GGUF/Qwen3.6-27B-Q8_0.gguf` | chat | yes |
| `lmstudio-community/Qwen3.6-27B-GGUF/mmproj-Qwen3.6-27B-BF16.gguf` | projector | no (companion) |
| `lmstudio-community/Qwen3.6-35B-A3B-GGUF/Qwen3.6-35B-A3B-Q8_0.gguf` | chat | yes |
| `lmstudio-community/Qwen3.6-35B-A3B-GGUF/mmproj-Qwen3.6-35B-A3B-BF16.gguf` | projector | no |
| `lmstudio-community/gemma-4-31B-it-GGUF/gemma-4-31B-it-Q4_K_M.gguf` | chat | yes |
| `lmstudio-community/gemma-4-31B-it-GGUF/gemma-4-31B-it-Q8_0.gguf` | chat | yes |
| `lmstudio-community/gemma-4-31B-it-GGUF/mmproj-gemma-4-31B-it-BF16.gguf` | projector | no |
| `lmstudio-community/nomic-embed-code-GGUF/nomic-embed-code-Q4_K_M.gguf` | embed | yes |
| `mixedbread-ai/mxbai-embed-large-v1/mxbai-embed-large-v1-f16.gguf` | embed | yes |

### `~/.lmstudio/models/` — 2 `.gguf` files

| Path | Role |
|------|------|
| `lmstudio-community/gemma-3-12b-it-GGUF/gemma-3-12b-it-Q4_K_M.gguf` | chat |
| `lmstudio-community/gemma-3-12b-it-GGUF/mmproj-model-f16.gguf` | projector |

**Total launchable models expected:** 8 (chat + embed).
**Total rows expected if mmproj files surface:** 13.

Per the user's policy ("Show them only if there is any value in showing
it or if they are launchable"), the `mmproj-*.gguf` files should be
either **hidden** or **shown with a non-launchable badge**. Treating
them as launchable rows is the failure case — running `start_model` on
a projector fails downstream.

## Expected Host panel readings

| Field | Expected |
|-------|---------|
| `gpu_backend` | `"amd"` (not `"cpu_only"`, not `"unsampled"`) |
| `gpu_device_count` | `1` |
| `gpu_mem_total_bytes` | ≈ 64 GiB (68719476736 from `rocm-smi`) |
| `gpu_mem_used_bytes` | ≈ 3 GiB idle (matches the `VRAM Total Used Memory (B)` key currently emitted by rocm-smi 7.8.0) |
| `gpu_util_pct` | small finite value at idle (0–5 %) |
| `gpu_temp_c` | ≈ 50–60 °C at idle |
| `cpu_pct` | finite, non-zero after first 1 Hz tick |
| `ram_total_bytes` | ≈ 64 GiB visible to the OS (BIOS split assigns 64 GB to GPU; sysinfo only sees the CPU half) |

The reference rocm-smi output observed on this box:

```
{"card0": {"Temperature (Sensor edge) (C)": "54.0",
           "GPU use (%)": "2",
           "VRAM Total Memory (B)": "68719476736",
           "VRAM Total Used Memory (B)": "3171471360"}}
```

## Test matrix

All commands run from `/mnt/work/Workspace/oss-libs/llamatui` with the
daemon auto-spawned. Output captured to `/tmp/llamadash-render-*.txt`.

| ID | Geometry | Scan root | Purpose |
|----|----------|-----------|---------|
| T1 | 160x50 | `-p /mnt/work/lmstudio-models` + `--no-scan` | Pin to the LM Studio cache; full Models list visible. |
| T2 | 120x40 | `-p /mnt/work/lmstudio-models` + `--no-scan` | Default `--render` geometry; reference frame for the wireframe. |
| T3 | 100x30 | `-p /mnt/work/lmstudio-models` + `--no-scan` | Wireframe target from the plan. |
| T4 | 80x25  | `-p /mnt/work/lmstudio-models` + `--no-scan` | Narrow — exercises Logo-hide threshold + hint-chip truncation. |
| T5 | 50x12  | `-p /mnt/work/lmstudio-models` + `--no-scan` | Tiny — exercises `area.height < 18` info-row-collapse fallback. |
| T6 | 120x40 | default discovery (LM Studio + HF + Ollama) | Auto-resolution honours `~/.lmstudio/settings.json::downloadsFolder`. |

## Pass criteria

For each render frame:

1. Title row contains `LlamaDash v` and the global hint set
   (`?:help`, `t:theme`, `/:filter`, `q:quit`).
2. Host panel border title reads `Host`; bars labelled `CPU`, `RAM`,
   `GPU`, `VRAM` (or the unified-memory collapse for AMD APU).
3. Host panel `backend` line reads `amd · 1 GPU`, not `cpu only` /
   `unsampled` / `unknown`.
4. Daemon panel shows non-empty `socket`, `uptime`, `build`, `server`,
   `counts`, `running` rows.
5. Logo panel renders the `LlamaDash` block title with theme name (or
   is hidden in T4/T5 per width threshold).
6. Models block title is `Models [N]` with `N >= 8`.
7. Body shows source headers (`lm-studio` / `user`) followed by model
   rows.
8. T5 collapses the info row and renders only title + body.

## Hardware-info-vs-render sanity

The render frames go through a manual diff against:
- `rocm-smi --showmeminfo vram --showuse --showtemp --json` (live VRAM)
- `free -m` (RAM total)
- `nproc` (CPU count)
- The blog-post hardware section (BIOS split, 8060S iGPU)

Any number that disagrees with the live tool output is logged as an
issue. The `unsampled` sentinel is treated as a hard fail — the
`--render` path waits up to 1.5 s for the sampler, so it should not
ship `unsampled` to the user unless something is broken downstream.

## Iteration policy

1. Capture every issue found in **one** issues doc
   (`docs/testing/2026-05-17-render-issues.md`).
2. Fix in source. Tests added per issue where the bug is reproducible
   without live hardware.
3. Re-run the full T1–T6 matrix. Mark each issue resolved with a note
   showing the fixed render output.
4. `cargo test --features test-fixtures` + `cargo clippy --all-targets
   --features test-fixtures -- -D warnings` + `cargo fmt --all --
   --check` must all pass at the end.
