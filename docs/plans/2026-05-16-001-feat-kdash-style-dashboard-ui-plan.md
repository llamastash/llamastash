---
title: "feat: KDash-style dashboard UI for LlamaStash"
type: feat
status: active
date: 2026-05-16
deepened: 2026-05-16
origin: docs/brainstorms/llamatui-requirements.md
---

# feat: KDash-style dashboard UI for LlamaStash

## Overview

Replace the current one-line banner + 50/50 horizontal split with a KDash-style dashboard:

- **Row 1 (1 line)** — accent-bg title bar. Left: `LlamaStash v0.1.0 · ● daemon`. Right: a fixed set of **global** key hints (`?:help  t:theme  /:filter  q:quit`). Panel-specific hints relocate **into each panel's own header**, not into this title row.
- **Row 2 (7 lines)** — three side-by-side panels:
  - **Host** (32 cols × 5 inner rows) — bar gauges for CPU / RAM / GPU util / VRAM with inline temperature readings + a backend tag line.
  - **Daemon** (flex middle) — socket+pid, uptime+build, server path+flavor, discovery counters, one-line running summary.
  - **Logo** (~25 cols, hides on narrow terminals) — a doubled-stroke ASCII "L-" glyph. Block title shows `<theme> · t:theme` instead of a separate version/theme zone.
- **Row 3 (rest)** — body split 60/40: **Models** list (existing `list_pane`) left, **right pane** (existing `right_pane` with Logs / Chat / Embed / Rerank tab strip) right. Each panel's block title carries its own hint chips.
- **Filter input** still renders as a transient line above the body when `/` is active. The active filter is also reflected as a chip in the Models block title (`[/qwen]`).
- **No bottom help bar** — global hints live on the title row, panel-specific hints in the panel headers.

The change touches three concerns:

1. **Data plumbing.** A new host-metrics sampler (system-wide CPU% and RAM via `sysinfo`, GPU util/temp via existing `src/gpu/*` modules extended) and a new `host` field on the `status` IPC response.
2. **Three new TUI components** (`host_stats_pane`, `info_pane`, `logo_pane`) plus a compact ASCII variant of `src/banner.rs`.
3. **A render-layout rewrite** in `src/tui/render.rs` and a repurposed `help_bar` that draws on the title row instead of the bottom.

The list pane, right pane, modal overlays (launch picker, advanced panel), and all keybindings stay behaviorally identical — they just move on screen.

## Problem Frame

LlamaStash today renders a thin one-line top banner and the two body panes. The visual density doesn't read as a "dashboard" the way KDash does for Kubernetes — the user's two prior Rust TUIs (`kdash`, `jwt-ui`) lean on a richer top region that surfaces system health, daemon health, and product branding at a glance. The current layout also keeps host RAM/CPU/GPU readouts entirely off-screen even though they're the single most useful signal when deciding whether to launch another model on the same machine.

The mock at `dash.png` and the KDash reference at `screenshot_2026-05-16_20-45-55.jpg` describe the target shape directly: top info row + main body row + minimal chrome.

## Requirements Trace

- **R17** — per-model live RAM/VRAM/CPU. Preserved: the compact "running" summary in the **Daemon** panel and the focused-model header in the right pane both surface this. Host-level gauges in the **Host** panel are additive context, not a substitute.
- **R23** — model-list left, right pane right, contextual help bar. Preserved in spirit; help relocates from the bottom to the title row, focus-aware behavior unchanged.
- **R25** — fuzzy filter activated by `/`. Preserved — filter input still appears above the body.
- **R26** / **R28** — themes + glyphs. All new components consume the existing `Palette` and `status_icons` modules; no theme-specific hard-coding.
- **R29** — perceived warm-attach <200 ms, input-to-redraw <16 ms. The host sampler runs at 1 Hz on a background tokio task and pushes through the existing `status` polling path; the render budget on the new layout is dominated by the same widgets that already pass the 16 ms bar.
- **R31** — tab-driven right pane (Logs default, Chat/Embed/Rerank when Ready). Preserved bit-for-bit.
- **R44** — GPU detection on NVIDIA/AMD/Metal. The host-metrics surface extends, not replaces, the existing probes.

## Scope Boundaries

Explicit non-goals for this plan:

- **No new product behavior.** No new keybindings, no new IPC actions, no new modes. This is a relayout + an added telemetry surface.
- **No per-PID GPU util/VRAM attribution.** The Host panel is host-level only; per-model RAM/VRAM stays the existing per-PID + system-VRAM-total story until a future plan picks up NVML per-PID attribution.
- **No HTTP / MCP exposure.** `AGENTS.md` explicitly defers both to v2; this plan does not add a new transport.
- **No mouse support.** R27 already says optional polish; not adding here.
- **No sparklines / trend graphs** in the Host panel. Bar gauges only for v1. Sparklines listed in [Future Considerations](#future-considerations).
- **No rename or theme additions.** Continues to ship Macchiato / Latte / Gruvbox / Solarized / Monochrome (per `AGENTS.md`).
- **No changes to `list_pane`, `right_pane`, `launch_picker`, `advanced_panel`, or tab implementations** beyond what's needed to honor the new outer layout.

## Context & Research

### Relevant Code and Patterns

- `src/tui/render.rs` — current 2-region layout. The new layout adds a fixed-height info region between `chunks[0]` (title) and the body.
- `src/tui/help_bar.rs` — emits a `Paragraph` line of `(label, description)` chips from `bindings_for(focus)`. The same renderer can be invoked at the right-aligned slot of the title row; only the call site moves.
- `src/tui/list_pane.rs` and `src/tui/right_pane.rs` — left/right body components, unchanged.
- `src/tui/keybindings.rs` (referenced by `bindings_for`) — source of focus-aware hints. Untouched.
- `src/daemon/resources.rs` — per-PID sysinfo sampler. Pattern for the new host sampler: same `sysinfo::System` shape, but `refresh_cpu_specifics` / `refresh_memory` instead of per-PID refresh, and a singleton tokio task instead of one-per-launch.
- `src/gpu/nvidia.rs`, `src/gpu/amd.rs`, `src/gpu/metal.rs` — existing best-effort probes. Today they parse `name,memory.total,memory.used`. We extend the NVIDIA query to also pull `utilization.gpu,temperature.gpu`, and add ROCm equivalents.
- `src/banner.rs` — 6×75 ASCII banner. Stays as-is for clap's `--help`; we add a smaller variant.
- `src/ipc/methods.rs` — `status` handler. Already serializes `gpu: GpuInfo`. We add a sibling `host: HostMetrics` field, parsed in the TUI by extending `App::ingest_status`.
- `src/tui/app.rs` — owns `App` state. We add a new `host: HostMetricsSnapshot` field updated by `ingest_status` for the render pipeline.

### Institutional Learnings

`docs/solutions/` is empty in this repo (greenfield project). Carry-forward from memory:

- The user is the author of `kdash` and `jwt-ui`; the visual reference is `kdash` overview's title + 3-line key-hint strip + banner block + main body pattern. We diverge by keeping the title to a single line (matches `dash.png`).
- Stack preference is `ratatui` + `crossterm` + `tokio` + `clap` — already in use; no new deps.

### External References

- [`sysinfo` 0.30 docs](https://docs.rs/sysinfo) — `refresh_cpu_usage`, `refresh_memory`, `global_cpu_info` for host-level metrics (different code paths than the per-PID `refresh_processes_specifics` we already use).
- `nvidia-smi --query-gpu=utilization.gpu,temperature.gpu --format=csv,noheader,nounits` — the canonical addition to the existing query. Returns `int %` and `int °C` per device.
- `rocm-smi --showuse --showtemp --json` — utilization + temperature for AMD. Same JSON-output shape as the existing `--showmeminfo` call.

### Slack / Issue Context

Not consulted. Internal greenfield project, no upstream Slack/GitHub discussion to pull from.

## Key Technical Decisions

- **Host metrics travel on the existing `status` poll, not a new IPC method.** The TUI already polls `status` on a tick; adding a `host` field is one round-trip vs. two and keeps the daemon's surface area small. The daemon-side sampler runs at 1 Hz so consecutive polls within a second return the same snapshot.
- **Per-PID resource readings (CPU% + RSS) ship in v1; per-PID VRAM defers to v2.** The existing `src/daemon/resources.rs` already samples per-PID — we wire it through the supervisor into the `status` response so the right-pane block title can show `4.2G RAM · 312% CPU` per launched model. Per-PID VRAM attribution needs NVML's `nvmlDeviceGetComputeRunningProcesses` and is called out as a v2 deferral in the README, not delivered here.
- **GPU util / temp are best-effort, not load-bearing.** When `nvidia-smi` doesn't expose them (or the path falls through to Vulkan / CpuOnly), the Host pane renders the bar as a `—` placeholder rather than empty space. Same pattern as today's `GpuInfo::CpuOnly`.
- **Multi-GPU aggregates into a single bar set.** When `Nvidia { devices }` reports 2+ cards, the Host panel shows mean util%, max temp, and summed VRAM. Backend line pluralizes (`NVML · 2 GPUs`). Per-card detail is out of scope until a future plan adds an inspector view; the cost of auto-expanding the info row to fit 2 cards (≈11 rows) is too high for the headline panel.
- **Apple Silicon is unified-memory.** On Metal, "VRAM" and "RAM" are the same pool. The Host pane collapses to three rows (CPU / RAM (unified) / GPU unified memory text) instead of four. Decision recorded so the `host_stats_pane` renderer doesn't draw a misleading second bar.
- **Bar fill style is `█` + `░` (KDash convention).** Solid block fill on a light-shade trough. Bar color tiers: green ≤60% / yellow 60–85% / red ≥85%. Temperatures get **their own** color tiers, independent of the gauge they sit next to — CPU green ≤65°C / yellow 65–80°C / red ≥80°C; GPU green ≤70°C / yellow 70–82°C / red ≥82°C.
- **Compact ASCII logo is a new constant, not a runtime-generated downscaling of the full banner.** Trying to wrap or scale the 6×75 banner produces unreadable output; a hand-tuned doubled-stroke "L-" glyph reads cleanly inside the ~22-col Logo panel. The existing `BANNER` const stays for clap's `before_help`. The Logo block title carries the current theme name + `t:theme` hint (no separate version/theme zone).
- **Title row uses accent background; panel blocks stay neutral.** The title row fills with `palette.accent` and renders text in `palette.bg` (KDash convention). All info-row and body panels keep the existing border-only treatment so the title row is the only branded zone. The daemon dot (`●`) carries the daemon-state color and renders over the accent bg — for Macchiato this means a green/yellow/red dot against mauve, which still reads cleanly.
- **Title-row hints are global only.** The right-aligned slot shows a fixed `?:help  t:theme  /:filter  q:quit` set, **never** focus-aware. Panel-specific hints (Enter:launch, Tab:cycle, Ctrl+Enter:send, etc.) relocate into each panel's own block title. This means the `help_bar::render` function shrinks to a small static renderer; the per-focus binding strings move into `list_pane`, `right_pane`, and the tab dispatchers.
- **Models block title carries the count + (optional) filter chip + hint chips.**
  - Filter inactive: `Models [127]  Enter:launch  /:filter  s:stop  f:fav  y:yank`.
  - Filter active: `Models [127]  [/qwen]  Enter:launch  s:stop  f:fav  y:yank` — the `[/qwen]` chip and the `/:filter` hint share a slot, never both at once.
  - On overflow, drop hint chips right-to-left in this priority: `y:yank` → `f:fav` → `s:stop` → `/:filter`. `Enter:launch` is never dropped. The `[count]` and (when present) the `[/query]` chip are never dropped.
- **Right-pane block title shows per-model RAM + CPU%.** Format: ` qwen3-7b · :41100 ready · 4.2G RAM · 312% CPU `. RAM is the latest RSS reading; CPU% is the latest sysinfo cpu-usage value (multi-core, may exceed 100%). VRAM is intentionally absent in v1 (deferred to v2 with NVML).
- **Right-pane tab strip uses per-tab dynamic hints.** Hints update with the active tab:
  - Logs: `Tab:next  j/k:scroll  L:auto-scroll  Esc:back`
  - Chat: `Tab:next  Ctrl+Enter:send  r:reasoning  Esc:back`
  - Embed: `Tab:next  Enter:embed  Esc:back`
  - Rerank: `Tab:next  Enter:rerank  Esc:back`
- **Hide the tab strip when only one tab is reachable.** When the focused model is not Ready, only `Logs` is reachable; the tab strip is suppressed and the inner area is fully Logs. Block title still names the focused model + state, so the user still knows what they're looking at.
- **Models list selection uses `> ` caret, not `▌ ` bar.** Two-column gutter is unchanged; only the glyph changes. `ROW_CHROME` constant in `list_pane.rs` stays at the same width.
- **Layout heights are absolute, not percentage-based.** `Constraint::Length(1)` for the title, `Constraint::Length(7)` for the info row, `Constraint::Min(0)` for the body. Percentage layouts collapse the host panel to unreadable widths on narrow terminals; fixed heights give predictable bar-gauge dimensions and reserve the rest for the table.
- **Logo panel hides at <18 cols inner width.** Below the threshold, the Logo block disappears entirely and the Daemon panel claims the freed space via `Constraint::Min(0)`. The Host panel keeps its fixed 32-col width.
- **Below a minimum terminal height, the info row collapses, not the body.** When `area.height < 18`, render skips the info row entirely (just title + body + filter). This keeps `cargo run` inside a tiny pane usable; the table is always the priority. Detection lives in `render` as a one-liner, not a new component.
- **Path truncation uses `…/` left-truncate.** Applied to long socket paths in the Daemon panel's `socket` row and long `llama-server` paths in the `server` row. The truncation budget is `inner_width - label_width - flavor_width`.
- **Daemon panel's `running` row is one line.** Shows the first managed launch (`name :port state`) plus `+N more` when `managed.len() > 1`. State-agnostic — `+N more` covers Loading, Launching, Stopping. When `managed.len() == 0`, the line collapses to `running  —`.

## Open Questions

### Resolved During Planning

- **Right pane shape** — keep current tab strip (Logs / Chat / Embed / Rerank).
- **Host metrics depth** — full set (CPU / RAM / GPU util / VRAM / GPU temp).
- **Top bar shape** — single line, accent background, no theme name.
- **Info panel contents** — daemon endpoint + uptime, discovery counters, active running models, `llama-server` build info.
- **Body split** — 60 / 40 in favor of the model list.
- **Top info row height** — 7 rows.
- **Hint scope on the title row** — fixed global hints only (`?:help  t:theme  /:filter  q:quit`); panel-specific hints relocate into their respective panel headers.
- **Host bar fill** — `█` solid + `░` light-shade trough.
- **Host bar thresholds** — green ≤60% / yellow 60–85% / red ≥85%; temperatures get separate CPU (65/80°C) and GPU (70/82°C) tiers.
- **Multi-GPU strategy** — aggregate into a single bar set (mean util%, max temp, sum VRAM); backend line pluralizes.
- **Daemon panel rows** — keep 5-row label-prefixed format from the plan default.
- **Path truncation** — left-truncate with `…/` prefix for both socket and server rows.
- **Daemon panel `running` row** — one line, `+N more` for the rest; collapses to `—` when zero running.
- **Logo glyph** — doubled-stroke "L-" (kdash style); no version or metadata text in the panel.
- **Logo block title** — `<theme> · t:theme`.
- **Logo narrow-terminal fallback** — hide the panel entirely below 18 cols inner width.
- **Models block title** — `Models [count]  [optional /query chip]  Enter:launch  /:filter  s:stop  f:fav  y:yank`; `/:filter` and `[/query]` chip share a slot.
- **Models hint truncation** — drop hint chips right-to-left: `y:yank` → `f:fav` → `s:stop` → `/:filter`; never drop `Enter:launch`, count, or filter chip.
- **Models selection style** — `> ` caret (was `▌ ` bar).
- **Right-pane block title** — `name · :port state · RAM · CPU%`. RAM and CPU% sourced from the existing per-PID `resources::sample_loop`, wired through the supervisor + `status` response in Unit 7 of this plan.
- **Right-pane tab strip styling** — underline + bold for active tab (today's style).
- **Right-pane hints** — per-tab dynamic strings; suppressed entirely when only one tab is reachable.

### Deferred to Implementation

- **Exact `nvidia-smi` argument order vs. existing parser.** Today's NVIDIA parser walks columns positionally. Adding two new columns (`utilization.gpu`, `temperature.gpu`) needs the column indices updated; the test fixture in `src/gpu/nvidia.rs` has to grow correspondingly. Implementer will see this immediately when extending the parser.
- **ROCm-smi JSON path under multi-card systems.** Today's parser handles `card0` — adding `--showuse --showtemp` may surface per-card keys we don't yet handle. Defer to the implementer with a "sum across cards for util%, max for temp" decision and a fixture that demonstrates both.
- **Compact logo glyph shape.** The wireframe sketches one — final form is a per-character decision the implementer makes once they see it in a real terminal at 22 cols × 5 rows. Constraint: must be readable at both Catppuccin Macchiato (dark) and Latte (light).
- **Whether the Daemon panel's "running" summary truncates with `…` or scrolls.** With 7 rows total minus the 4 fixed lines, only ~3 rows for the running list. With >3 running models, truncate to "+N more". Final ellipsis behavior is a small render decision, not an architecture one.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification.*

### Final wireframe (target 100 cols × 30 rows)

```
█████████████████████████████████████████████████████████████████████████████████████████████████████
█ LlamaStash v0.1.0 · ● daemon                                  ?:help  t:theme  /:filter  q:quit  █
█████████████████████████████████████████████████████████████████████████████████████████████████████
┌─ Host ───────────────────────┐┌─ Daemon ───────────────────────────────┐┌─ macchiato · t:theme ─┐
│ CPU  ███████░░░░  58%  71°C  ││ socket  …/daemon.sock  pid 1234        ││  ╔╗                   │
│ RAM  █████░░░░░░  11.4/32 G  ││ uptime  3h12m   build  v0.1.0          ││  ║║                   │
│ GPU  ██████████░  84%  68°C  ││ server  …/build-cuda/bin/llama-server (cuda)  ║║     ══         │
│ VRAM ███████░░░░  14.2/24 G  ││ counts  127 found · 3 ready · 7 ★      ││  ║║                   │
│ backend  NVML · 1 GPU        ││ running qwen3-7b :41100 ready  +2 more ││  ╚╩═══════            │
└──────────────────────────────┘└────────────────────────────────────────┘└───────────────────────┘
┌─ Models [127]  Enter:launch  /:filter  s:stop  f:fav  y:yank ─────────┐┌─ qwen3-7b · :41100 ready · 4.2G RAM · 312% CPU ─┐
│ ★ Favorites                                                            ││ Logs · Chat        Tab:next  Ctrl+Enter:send  r:reasoning  Esc:back │
│ > ● qwen3-7b-instruct       llama · Q4_K · 8192 · 4.2G chat            ││                                                  │
│   ★ gemma3-12b-it           gemma · Q5_K · 8192 · 7.1G chat            ││ 14:02:11  prompt eval: 142 tok/s                │
│ /home/d/models                                                         ││ 14:02:11  gen: 38 tok/s                          │
│   ○ qwen3-1.7b              qwen3 · Q4_K · 4096 · 1.1G chat            ││ 14:02:13  /v1/chat completed                     │
│   ◔ bge-rerank-base         bert  · F16  · 512  · 0.3G rerk            ││                                                  │
│ ~/.cache/huggingface/hub                                               ││                                                  │
│   ○ phi-3-mini              phi3  · Q8_0 · 4096 · 3.9G chat            ││                                                  │
│ ⇪ phi-4-q4 (external)       pid 4421 :8080                             ││                                                  │
└────────────────────────────────────────────────────────────────────────┘└──────────────────────────────────────────────────┘
```

**Filter active variant** — Models block title swaps `/:filter` hint for a `[/query]` chip; filter input line appears above the body:

```
┌─ Models [127]  [/qwen]  Enter:launch  s:stop  f:fav  y:yank ──────────┐┌─ qwen3-7b · :41100 ready · 4.2G RAM · 312% CPU ─┐
│ > ● qwen3-7b-instruct       llama · Q4_K · 8192 · 4.2G chat            ││ Logs · Chat        Tab:next  Ctrl+Enter:send …  │
│   ○ qwen3-1.7b              qwen3 · Q4_K · 4096 · 1.1G chat            ││                                                  │
└────────────────────────────────────────────────────────────────────────┘└──────────────────────────────────────────────────┘
/ qwen│
```

**Model not Ready variant** — right pane suppresses the tab strip and fills with Logs only:

```
┌─ gemma3-12b · :41101 loading · 7.1G RAM · 28% CPU ────────────────────┐
│ 14:02:08  loaded session: SessionParams... n_ctx = 8192               │
│ 14:02:09  llm_load_tensors:  CUDA0 buffer size =  5520.41 MiB         │
│ ...                                                                    │
└────────────────────────────────────────────────────────────────────────┘
```

### Layout constraints (ratatui)

```
Vertical:
  Constraint::Length(1)    // title row
  Constraint::Length(7)    // info row  (skipped when area.height < 18)
  Constraint::Min(0)       // body
  Constraint::Length(1)    // filter (only when Focus::Filter)

Horizontal (info row):
  Constraint::Length(32)   // Host
  Constraint::Min(0)       // Daemon (flex)
  Constraint::Length(25)   // Logo

Horizontal (body):
  Constraint::Percentage(60)  // Models
  Constraint::Percentage(40)  // Right pane
```

### Data flow for host metrics

```
sysinfo::System (1Hz)               GpuInfo + util + temp (1Hz)
        │                                       │
        └─────────► host_metrics ◄──────────────┘
                        │
                        │ pushed into daemon state
                        ▼
                  status response
                  { gpu, host, models, external }
                        │
                        ▼
                App::ingest_status (TUI)
                        │
                        ▼
                host_stats_pane::render
```

The supervisor's per-PID sampler (`resources::sample_loop`) is untouched; the host sampler is a separate task started in `daemon::run_foreground` alongside the existing supervisor + discovery tasks.

### Right-aligned title-row help

```
title_area = chunks[0]            // Length(1)
[title_pill_area | flex | help_area] = Horizontal split(
  Length(visible_title_width),
  Min(0),
  Length(remaining_for_hints)
)
help_bar::render(help_area, focus, toast, palette)  // same renderer
```

## Implementation Units

- [x] **Unit 1: Host-metrics sampler + IPC surface**

**Goal:** Capture host CPU%, RAM, GPU util/temp at 1 Hz and surface them via the existing `status` IPC response.

**Requirements:** R17 (companion), R29, R44.

**Dependencies:** None.

**Files:**
- Create: `src/daemon/host_metrics.rs`
- Modify: `src/daemon/mod.rs` — register the module.
- Modify: `src/daemon/server.rs` (or `daemon/mod.rs::run_foreground`, whichever owns task startup) — spawn the sampler at daemon start, store the latest snapshot behind an `Arc<RwLock<HostMetricsSnapshot>>` in the daemon state.
- Modify: `src/ipc/methods.rs` — extend the `status` handler to include `host: HostMetricsSnapshot`.
- Test: `src/daemon/host_metrics.rs` inline `#[cfg(test)] mod tests`.
- Test: `tests/ipc_status_test.rs` (or the existing equivalent) — verify the new `host` field appears in the JSON response.

**Approach:**
- Mirror `src/daemon/resources.rs` shape: one-shot `sample()` returning a snapshot struct; a `sample_loop` that pushes into a shared cell at the configured interval.
- Snapshot fields: `cpu_pct: f32`, `ram_used_bytes: u64`, `ram_total_bytes: u64`, `gpu_util_pct: Option<f32>`, `gpu_mem_used: Option<u64>`, `gpu_mem_total: Option<u64>`, `gpu_temp_c: Option<f32>`, `gpu_backend: String` (the existing `GpuInfo::label`).
- The GPU side reuses `crate::gpu::probe()` for backend identity, then issues a per-tick util/temp re-probe via the extended NVIDIA / AMD parsers (Unit 2). Apple Metal sets util/temp to `None` since unified memory + macOS doesn't expose them via `system_profiler` cleanly.
- Sampler runs on a tokio task, not blocking the daemon's main loop. Drop guard so the task exits on daemon shutdown.

**Patterns to follow:**
- `src/daemon/resources.rs` for the sampler shape + `Serialize` derive.
- `src/daemon/server.rs` for task spawn + shutdown registration.

**Test scenarios:**
- Happy path: `sample()` for the host returns a snapshot with `ram_total_bytes > 0` and a finite `cpu_pct`.
- Edge case: first call returns `cpu_pct == 0.0` until the sampler primes — `sample_loop` must discard the first reading (mirrors today's `resources::sample_loop` behavior).
- Edge case: `GpuInfo::CpuOnly` ⇒ snapshot has `gpu_util_pct == None` and `gpu_backend == "cpu_only"`.
- Edge case: Apple Metal ⇒ `gpu_mem_used == None`, `gpu_mem_total == Some(unified_bytes)`, `gpu_util_pct == None`, `gpu_temp_c == None`.
- Integration: status response JSON contains the new `host` field with all keys present (verify via `serde_json::from_str` round-trip in the IPC integration test).

**Verification:** `cargo run -- daemon start` in one terminal + `cargo run -- status --json` in another should print a `"host": { ... }` block alongside the existing `gpu` block.

- [x] **Unit 2: Extend GPU probes with utilization + temperature**

**Goal:** Add util% and temperature fields to NVIDIA + AMD probes, gracefully `None` on Metal / Vulkan / CpuOnly.

**Requirements:** R44.

**Dependencies:** Unit 1 (consumes this).

**Files:**
- Modify: `src/gpu/mod.rs` — add `utilization_pct: Option<f32>` and `temperature_c: Option<f32>` to `GpuDevice`.
- Modify: `src/gpu/nvidia.rs` — extend the `--query-gpu` arg list to include `utilization.gpu,temperature.gpu`; update column-position parsing.
- Modify: `src/gpu/amd.rs` — additionally invoke `rocm-smi --showuse --showtemp --json` (or fold into the existing call) and merge the readings per card.
- Modify: `src/gpu/metal.rs` — leave fields as `None` (no upstream surface).
- Modify: `src/gpu/vulkan.rs` — leave fields as `None`.
- Test: inline `#[cfg(test)] mod tests` in each modified module — extend the existing fixture strings.

**Approach:**
- NVIDIA parser today reads `name, memory.total, memory.used` as positional CSV. New order: `name, memory.total, memory.used, utilization.gpu, temperature.gpu`. The fixture in `nvidia.rs::tests` becomes the source of truth.
- AMD parser: `rocm-smi --showmeminfo vram --showuse --showtemp --json` returns nested keys per card. Add fallbacks for naming variants (`GPU use (%)` vs `GPU Use (%)`) the way today's parser handles `VRAM` vs `vram`.
- Backwards-compat for serde: the new fields are `Option` so older JSON consumers don't break.

**Patterns to follow:**
- Existing case-fallback list in `src/gpu/amd.rs` (`pick_u64(card, &["VRAM Total Memory (B)", "vram total memory (B)"])`).
- Existing test fixture pattern: an embedded `&str` of fake `nvidia-smi`/`rocm-smi` output parsed against expected values.

**Test scenarios:**
- Happy path: NVIDIA fixture with all 5 columns parses into a `GpuDevice` with `utilization_pct == Some(84.0)` and `temperature_c == Some(68.0)`.
- Edge case: a fixture missing util/temp columns falls back to `None` rather than panicking (forward-compat for older `nvidia-smi`).
- Edge case: AMD multi-card fixture produces two `GpuDevice`s, each with their own util/temp.
- Edge case: Metal / Vulkan / CpuOnly cases keep `utilization_pct == None` and `temperature_c == None`.
- Integration: `probe()` returning `GpuInfo::Nvidia { devices }` carries the new fields through to the JSON serialization assertions in `gpu/mod.rs::tests::json_carries_tag_field`.

**Verification:** `cargo test --features test-fixtures gpu` passes; a hand-run of `cargo run -- status --json` on a CUDA box shows non-`null` util/temp.

- [x] **Unit 3: `host_stats_pane` component**

**Goal:** Render the Host panel — 4 bar gauges (CPU / RAM / GPU util / VRAM) + GPU backend tag.

**Requirements:** R26, R28.

**Dependencies:** Unit 1 (data source).

**Files:**
- Create: `src/tui/host_stats_pane.rs`
- Modify: `src/tui/mod.rs` — re-export.
- Modify: `src/tui/app.rs` — store the latest `HostMetricsSnapshot` on `App`, populated by `ingest_status`.
- Test: `src/tui/host_stats_pane.rs` inline tests + a `TestBackend`-based render assertion in `tests/tui_smoke_test.rs` (or sibling).

**Approach:**
- Component signature: `pub fn render(frame: &mut Frame<'_>, area: Rect, host: &HostMetricsSnapshot, palette: &Palette)`.
- Use `ratatui::widgets::Gauge` for each bar, sized at `Constraint::Length(1)` per row inside a 5-row inner area; the outermost block has `borders(Borders::ALL).title(" Host ")`.
- Gauge color tiers from `palette.success / warning / danger` — green <60 %, yellow 60–85 %, red >85 % (same thresholds for all four bars).
- Apple Silicon branch: render only `CPU`, `RAM (unified)`, and a one-line `GPU  unified memory` text row in place of the GPU/VRAM bars.
- CpuOnly branch: hide the GPU + VRAM bars; render `backend  cpu only` instead.

**Patterns to follow:**
- `src/tui/list_pane.rs` for the block + inner-area + style-from-palette pattern.
- `kdash/src/ui/utils.rs` (reference repo) uses `ratatui::widgets::Gauge` heavily — the same approach.

**Test scenarios:**
- Happy path: a snapshot with full GPU data renders four gauges and one backend line (assert via `Buffer` content checks in `TestBackend`).
- Edge case: `gpu_backend == "cpu_only"` renders 2 gauges (CPU, RAM) + `backend  cpu only`.
- Edge case: Metal renders 1 gauge (CPU) + `RAM (unified)` gauge + `GPU  unified memory` text.
- Edge case: terminal width < 25 cols — bars still render but truncate cleanly, no panic.
- Edge case: `cpu_pct > 100` (multi-core sum) clamps to 100 in the gauge; the numeric label still shows the true value.
- Edge case: `ram_total_bytes == 0` (sysinfo glitch) renders an inactive bar with `—/—` instead of a div-by-zero.

**Verification:** `cargo test --features test-fixtures host_stats_pane` passes; visual check via `cargo run`.

- [x] **Unit 4: `info_pane` component**

**Goal:** Render the Daemon panel — socket + uptime + build + `llama-server` info + counters + compact running list.

**Requirements:** R17 (compact summary surface).

**Dependencies:** None functionally; consumes data already on `App`.

**Files:**
- Create: `src/tui/info_pane.rs`
- Modify: `src/tui/mod.rs` — re-export.
- Modify: `src/tui/app.rs` — small additions for daemon-startup-time metadata (already partially present: `daemon_connected`; needs `daemon_socket_path: Option<String>`, `daemon_started_at: Option<Instant>`, `llama_server_path: Option<String>`, `llama_server_flavor: Option<String>` — all populated from the `status` response).
- Modify: `src/ipc/methods.rs` — the `status` response already carries `socket` info implicitly; explicitly include `daemon_started_at` (epoch seconds) and `llama_server: { path, flavor }` so the TUI doesn't have to read the daemon's environment.
- Test: `src/tui/info_pane.rs` inline tests.

**Approach:**
- Component signature: `pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette)`.
- Inner layout: 5 fixed rows — `socket / uptime+build / server path+flavor / counts / running summary`.
- Counts: `{N} found · {M} ready · {K} ★` derived from `app.models`, `app.managed`, `app.favorites`.
- Running summary: at most 2 lines of `name :port state · mem`. If `managed.len() > 2`, the second line collapses to `+{N} more running`.
- `llama_server_path` truncates from the left with `…/` when it exceeds the available width — common case is a long `/usr/local/lib/.../bin/llama-server`.
- Uptime formats as `Hh Mm` (drop seconds — info panel updates at 1 Hz, seconds add no signal).

**Patterns to follow:**
- `src/tui/render.rs::render_banner` for `Span` composition + theming.
- `src/util/paths::model_display_name` (already used by right_pane) for short paths.

**Test scenarios:**
- Happy path: full data renders 5 rows; `counts` line shows `N found · M ready · K ★`.
- Edge case: 0 models, 0 running, 0 favorites — counts line still renders, no panic.
- Edge case: > 2 running models — running line shows the first model + `+N more running`.
- Edge case: `llama_server_path` longer than available width truncates from the left.
- Edge case: `daemon_started_at == None` (transient at startup) — uptime renders as `—`.

**Verification:** `cargo test --features test-fixtures info_pane` passes; visual check via `cargo run -- daemon start` + `cargo run`.

- [x] **Unit 5: `logo_pane` + compact ASCII banner**

**Goal:** Render the third top-row panel — a compact ASCII LlamaStash glyph + version + theme name.

**Requirements:** R26 (theme awareness).

**Dependencies:** None.

**Files:**
- Create: `src/tui/logo_pane.rs`
- Modify: `src/banner.rs` — add `pub const COMPACT_BANNER: &str = …;` (~5×20 glyph). Keep `BANNER` untouched.
- Modify: `src/tui/mod.rs` — re-export.
- Test: `src/tui/logo_pane.rs` inline tests.

**Approach:**
- Component signature: `pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette)`.
- Outer block: `borders(Borders::ALL).title(" LlamaStash ").border_style(palette.accent)`.
- Inside the block, draw `COMPACT_BANNER` styled in `palette.accent`, plus one line below with `v{version} · {theme}`.
- When the inner area is too narrow (e.g., < 18 cols at unusual terminal sizes), fall back to a single line `LlamaStash v0.1.0`. No panic.

**Patterns to follow:**
- `src/tui/render.rs::render_banner` again for `Span` composition.

**Test scenarios:**
- Happy path: 22×6 inner area renders 5 banner lines + version-theme line.
- Edge case: 12×3 inner area falls back to single-line text.
- Edge case: theme cycling causes the accent color to change — assert the rendered style after `app.cycle_theme()`.

**Verification:** `cargo test --features test-fixtures logo_pane` passes; visual check across the 5 themes via the `t` hotkey.

- [x] **Unit 6: New top-level layout + accent title bar + hint relocation**

**Goal:** Rewrite `render.rs` to draw the accent-bg title row + info row + body + filter. Relocate panel-specific hints into each panel's block title; the title row carries only global hints. Drop the bottom help bar.

**Requirements:** R23, R25, R29.

**Dependencies:** Units 3, 4, 5.

**Files:**
- Modify: `src/tui/render.rs`
- Modify: `src/tui/help_bar.rs` — shrink to a small static renderer that draws the global hint set (`?:help  t:theme  /:filter  q:quit`) in `palette.bg` on `palette.accent`. Old focus-aware code path moves into the panels that own each focus.
- Modify: `src/tui/list_pane.rs` — render the Models block title with `[count]`, optional `[/query]` chip, and the hint-chip strip (`Enter:launch  /:filter  s:stop  f:fav  y:yank`). Apply right-to-left truncation when over budget. Replace `▌ ` selection glyph with `> `.
- Modify: `src/tui/right_pane.rs` — block title format becomes `name · :port state · RAM · CPU%`. Tab strip emits per-tab dynamic hints; suppress the strip entirely when `available_right_tabs()` returns a single entry.
- Modify: `tests/tui_smoke_test.rs` — replace old banner-string assertions with new title format + accent-bg check via the rendered `Buffer`'s style cell.
- Test: `src/tui/render.rs` inline tests for the height-fallback path (`area.height < 18` collapses the info row) and the width-fallback (`inner_logo_width < 18` hides the Logo panel).

**Approach:**
- New top-level constraints:
  ```
  vertical = [Length(1), Length(7)*, Min(0), Length(1)?]
  *: only when area.height >= 18
  ?: only when Focus::Filter
  ```
- Title row: paint the full area with `Style::default().bg(palette.accent).fg(palette.bg)`, then internal horizontal split:
  ```
  horizontal = [Length(visible_title_width), Min(0), Length(global_hint_width)]
  ```
  Left slot renders `LlamaStash v0.1.0 · ● daemon` (dot color from `palette.success / warning / danger` based on `daemon_connected` and the daemon-state probe). Right slot renders the static global hint string.
- Info row internal split:
  ```
  horizontal = [Length(32), Min(0), Length(25)*]
  *: Logo's Length(25) drops when inner_logo_width would be < 18; daemon flexes
  ```
- Body internal split: `[Percentage(60), Percentage(40)]`.
- Modal overlays (`launch_picker`, `advanced_panel`) keep drawing over the full `area`.

**Patterns to follow:**
- `src/tui/render.rs::render` flow (declare constraints, split, dispatch).
- `kdash/src/ui/mod.rs::draw_app_title` for the accent-bg styling pattern.

**Test scenarios:**
- Happy path: 100×30 `TestBackend` — buffer cells in row 0 carry `bg == palette.accent` and `fg == palette.bg`; all info-row block titles render (`Host`, `Daemon`, theme-dependent Logo title); body shows the new `Models [...]` block title and the per-model right-pane block title.
- Edge case: `Focus::Filter` active — title-row hints unchanged (`/:filter` stays as a global hint), `[/query]` chip appears in the Models block title, transient filter input line appears at the bottom.
- Edge case: daemon disconnected — title row dot color flips to `palette.warning`; Daemon panel `uptime` collapses to `—`.
- Edge case: `area.height < 18` — info row is skipped; title row + body + optional filter still render with no panic.
- Edge case: terminal width that gives the Logo panel <18 cols inner — Logo block disappears; Daemon flexes to fill.
- Edge case: terminal width that overflows the Models hint-chip strip — `y:yank` drops first, then `f:fav`, `s:stop`, `/:filter`. `Enter:launch` and the count chip stay.
- Edge case: focused model is not Ready — right pane tab strip is suppressed; block title still shows `name · :port loading · RAM · CPU%`.
- Edge case: `available_right_tabs()` returns exactly 1 tab — strip suppressed.
- Edge case: theme cycle via `t` — title-row bg color flips per theme; Logo block title text updates to the new theme name.
- Integration: launch picker over the new layout — modal still centers; underlying frame state preserved.

**Verification:** `cargo test --features test-fixtures` full suite passes; `cargo run` shows the new layout; `q`/`/`/`t`/`Enter` still work end-to-end against a live daemon.

- [x] **Unit 7: Per-PID resource pipeline (RAM + CPU%) into the status response**

**Goal:** Wire the existing `src/daemon/resources::sample_loop` into the supervisor per launched model, surface the latest reading in the `status` response per-launch, and consume it in the TUI so the right-pane block title can show `4.2G RAM · 312% CPU` per model.

**Requirements:** R17 (the launched-model RAM/CPU half — VRAM still deferred).

**Dependencies:** None (parallel-safe with all UI units; the right-pane block title in Unit 6 reads the new field).

**Files:**
- Modify: `src/daemon/supervisor.rs` — spawn `resources::sample_loop(pid, Duration::from_secs(1))` per managed launch; store the latest `ResourceReading` on the per-launch state struct; clean up on stop.
- Modify: `src/ipc/methods.rs` — extend the `status` per-model row with `latest_rss_bytes: Option<u64>` and `latest_cpu_pct: Option<f32>`.
- Modify: `src/tui/app.rs` — extend `ManagedRow` with `rss_bytes: Option<u64>` and `cpu_pct: Option<f32>`; parse them in `parse_status_row`.
- Modify: `src/tui/right_pane.rs` — read the new fields and format them in the block title (already covered by Unit 6's right_pane changes; this unit provides the data).
- Test: `src/daemon/supervisor.rs` inline tests for sampler attach/detach.
- Test: an integration test (`tests/status_with_resources_test.rs` or extend existing) that asserts the response carries the new fields for a fake supervised launch.

**Approach:**
- Sampler lives alongside the existing per-launch state; mirrors today's spawn pattern for the IPC `_test_sleep` task.
- First-tick zero-CPU quirk (same as `host_metrics`): the sampler discards the first reading.
- When the PID disappears mid-launch (process exited but supervisor hasn't transitioned state yet), keep `latest_*` as `Some(last_known_value)` until the supervisor moves the launch state — avoids flicker on the right-pane title.
- Render format in the right pane:
  - `latest_rss_bytes == None` → `— RAM`
  - `latest_cpu_pct == None` → `— CPU`
  - both present → `4.2G RAM · 312% CPU` (format bytes via the existing `format_bytes` helper in `list_pane.rs`; reuse via small refactor or duplicate).

**Patterns to follow:**
- `src/daemon/resources.rs::sample_loop` (existing).
- `src/daemon/supervisor.rs` per-launch state struct (existing).

**Test scenarios:**
- Happy path: a supervised launch with a live PID surfaces `latest_rss_bytes > 0` and a finite `latest_cpu_pct` after two sample ticks.
- Edge case: first tick has `latest_cpu_pct == 0.0` — verify the sampler discards it (no zero-cpu reading reaches the status response in the first second).
- Edge case: PID disappears mid-launch — `latest_*` retains the last known value until the launch state transitions to Stopped, then becomes `None`.
- Edge case: stopping the launch unregisters the sampler (no zombie task leaking).
- Integration: TUI right-pane block title shows `4.2G RAM · 312% CPU` for a Ready focused model, and shows `— RAM · — CPU` when no readings are available yet (right after launch).

**Verification:** `cargo test --features test-fixtures supervisor` and the new integration test pass; live `cargo run` shows the per-model RAM/CPU in the right-pane title after a launch.

## System-Wide Impact

- **Interaction graph:** the new layout touches `render.rs`, `help_bar.rs`, `app.rs` (new fields), and `ipc/methods.rs` (new `host` field). Modal overlays, list_pane, right_pane, keybindings, and the event loop are untouched.
- **Error propagation:** GPU probe failures stay best-effort — the host sampler returns `None` per field, the renderer shows `—`, and the daemon's `status` response still serializes successfully. No new error paths surface to the user.
- **State lifecycle:** the host sampler is a singleton tokio task. Its drop guard is the daemon shutdown signal — if it leaks, the only consequence is a stale snapshot for one tick, which the renderer tolerates.
- **API surface parity:** `status` IPC response gains a new field; the CLI `status` subcommand (`src/cli/status.rs`) reads the same JSON, so `status --json` automatically surfaces the new `host` block. Updating the human-readable `status` output to include host gauges is **not** in scope here.
- **Integration coverage:** the new layout is exercised end-to-end by `tests/tui_smoke_test.rs`; the new IPC field is exercised by the existing IPC integration tests (with a small addendum).
- **Unchanged invariants:** list_pane behavior, right-pane tab strip, modal overlays, keybindings, filter activation, theme cycling, daemon socket auth, IPC framing, supervisor lifecycle, persistence, orphan adoption — none change. The `status` field addition is additive, not breaking.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Adding columns to `nvidia-smi --query-gpu=…` parser breaks for older driver versions that don't recognize `utilization.gpu`. | Probe `nvidia-smi` once at daemon start with the new arg set; if it fails, fall back to the old arg set and leave util/temp as `None`. Existing fixture-based tests cover the column-position swap. |
| `sysinfo` host CPU% returns 0 on the first refresh (same quirk as per-PID sampler today). | The `sample_loop` discards the first reading — mirrors `resources::sample_loop`. Tested. |
| 7-row info panel pushes the body below the visible area on tiny terminals. | `area.height < 18` collapses the info row; title + body + filter always render. Tested. |
| Compact ASCII logo looks unreadable inside the 22-col block. | Implementer iterates on the glyph during Unit 5; fall-back text path exists for narrow terminals. |
| Help-bar relocation breaks user muscle memory who rely on glancing at the bottom row. | Single, atomic relayout; the hint text content is unchanged, only position differs. Worth documenting in `docs/architecture.md` after merge (out of scope here). |
| Apple Silicon collapses three Host bars to two — could read as a bug. | The Host pane explicitly labels `RAM (unified)` and `GPU  unified memory` so the difference is visually intentional, not missing data. |

## Documentation / Operational Notes

- `docs/architecture.md` — short addendum after merge describing the new layout regions. Out of scope for this plan to write the doc itself, but flag it as a follow-up.
- **README v2 deferrals list** — add a bullet: "Per-PID VRAM attribution via NVML (`nvml-wrapper` crate). Today the right-pane block title surfaces per-model RAM + CPU%; per-model VRAM is reported only at the host level. v2 unlocks per-launch VRAM via NVML's `nvmlDeviceGetComputeRunningProcesses`."
- No migration or rollout concern: same single binary, same socket, same persisted state. Users running `cargo install` get the new layout on next start.
- `AGENTS.md` mentions the right-pane tab list (Logs / Chat / Embed / Rerank) under "Architecture in one breath" — unchanged. No edit needed.

## Future Considerations

These are explicitly **not** part of this plan but become reasonable once the new layout lands:

- **Per-model VRAM attribution via NVML** — pull `nvml-wrapper` for Linux + Windows. Surfaces per-launch VRAM in the right-pane block title (next to RAM and CPU%). Tracked in README v2 deferrals. AMD/Apple equivalents are out of scope until upstream surfaces parity.
- Sparkline trends in the Host panel (RAM/VRAM over the last 60s) — `ratatui::widgets::Sparkline`.
- Per-card detail view for multi-GPU systems — a dedicated inspector view rather than expanding the headline Host panel.
- KDash-style horizontal resource tabs in the body (Favorites / All / Running / Errors) — once the model count grows past ~50 in real use.
- Mouse support on the new Host gauges (click to toggle theme, etc.) — R27 says optional polish.

## Sources & References

- **Origin document:** [docs/brainstorms/llamatui-requirements.md](../brainstorms/llamatui-requirements.md)
- **Existing plan:** [docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md](2026-05-13-001-feat-llamatui-v1-launcher-plan.md) — Unit 6 (TUI shell) is what this revises.
- **AGENTS.md:** project-level guidance referenced throughout.
- **Visual references:** `dash.png` (user mock), `screenshot_2026-05-16_20-45-55.jpg` (kdash dashboard).
- **Reference repos:**
  - `kdash/src/ui/mod.rs::draw_overview` — title + banner + body shape.
  - `kdash/src/ui/overview.rs::draw_status_block` — info-row sub-split.
- **Relevant code:**
  - `src/tui/render.rs` — current layout (rewritten by Unit 6).
  - `src/daemon/resources.rs` — sampler pattern (mirrored by Unit 1).
  - `src/gpu/{mod,nvidia,amd,metal,vulkan}.rs` — GPU probes (extended by Unit 2).
  - `src/banner.rs` — full ASCII banner (sibling added by Unit 5).
