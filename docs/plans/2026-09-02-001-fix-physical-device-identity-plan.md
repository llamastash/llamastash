# Plan: physical-device identity for alternate compute selectors

Tracking TODO.md (General Roadmap → High priority).

## Problem

The multi-GPU UI turns on for a host with exactly one physical GPU.

`llamacpp-ROCmFP4` is one binary compiled with **both** HIP and Vulkan, so its
`--list-devices` reports the same Radeon 8060S twice — once per compute API:

```
$ q38rocm-llama-server --list-devices          # verified 2026-09-02
Available devices:
  ROCm0: AMD Radeon 8060S Graphics (126976 MiB, 61394 MiB free)
  Vulkan0: AMD Radeon 8060S Graphics (RADV STRIX_HALO) (128505 MiB, 89217 MiB free)
```

Live `status` on the same host (2026-09-02, real daemon, `LLAMASTASH_DEBUG_FAKE_GPUS` unset):

| server | devices |
|---|---|
| `llamacpp-rocm` | `ROCm0` |
| `llamacpp-vulkan` | `Vulkan0` |
| `llamacpp-ROCmFP4` | `ROCm0`, `Vulkan0` |
| `llamacpp-DFlash2` | `Vulkan0` |

`host.gpu_device_count: 1`.

Two independent defects fall out of this:

1. **Selector count is used as physical-GPU count.** `App::multi_device`
   (`src/tui/app.rs:1513`), `LaunchPickerState::multi_device`
   (`src/tui/launch_picker.rs:953`) and `cli::resolve::multi_device`
   (`src/cli/resolve.rs:174`) all answer "more than one device?" with
   `devices.len() > 1`. `llamacpp-ROCmFP4` trips it, so the Models-list
   `Device` column, the CLI `list` `DEVICE` column and the whole **Multi-GPU
   placement** knob group appear on a single-GPU host. `doctor` reports the
   same server as `llamacpp-ROCmFP4 (2 GPUs)` (`src/init/doctor.rs:456`).
   The existing guard (commit for `multi_device_gates_on_within_server_count_…`)
   only covers the *cross-server* case — one GPU seen by two builds — and this
   is the *within-server* case.

2. **The running-launch settings view is not server-scoped.**
   `src/tui/tabs/settings.rs:175` gates the read-only knob groups on
   `app.multi_device()`, which is a fact about the *whole catalog*. A model
   running on `llamacpp-rocm` (one device, no placement choice possible) shows
   `tensor-split` / `main-gpu` / `split-mode` because some *other* server in the
   catalog has two selectors. The editable picker is already scoped to the
   selected server; the read-only view is not.

`list_devices.rs` keeping both selectors is correct and stays: they are
genuinely different launch options on that binary, and on this host the
difference is measured at ~27% decode (see the `Qwen3.8-27B-ROCmFP4-FAST`
preset notes in the user's `config.yaml`). The bug is that nothing downstream
knows the two selectors are **one card**.

## What must not regress

- The `device` row must stay selectable on `llamacpp-ROCmFP4`. Picking
  `Vulkan0` over `ROCm0` there is a real, measured launch decision, and every
  shipped preset for that file pins it.
- A genuine two-GPU server (`ROCm0` + `ROCm1`) must still open everything.
- `LLAMASTASH_DEBUG_FAKE_GPUS=N` must still light up the multi-GPU surfaces on
  a single-GPU host (it fans out one device into N same-family clones, so it
  stays N physical under the new rule).
- Two identical cards (`Vulkan0` + `Vulkan1`, byte-identical names) must never
  collapse into one.

## Design

### Physical identity

A **physical slot** is one card. Selectors map onto slots with one rule:
*within a compute family (`ROCm` / `Vulkan` / `CUDA` / `Metal`) every index is
a distinct card; across families, equal adapter names are the same card.*

```
for each device (probe order):
  key  = normalized(device.name)
  fam  = device.gpu_backend
  attach to the first existing slot whose key == key AND that has no
  device from `fam` yet; otherwise open a new slot
physical_device_count = slots.len()
```

Worked cases:

| devices | slots | why |
|---|---|---|
| `ROCm0` + `Vulkan0`, same name | 1 | different families, same name |
| `Vulkan0` + `Vulkan1`, same name | 2 | same family → distinct indices |
| `CUDA0,CUDA1` + `Vulkan0,Vulkan1`, all one name | 2 | each Vulkan pairs with a CUDA slot that lacks a Vulkan |
| `ROCm0` (AMD) + `Vulkan0` (NVIDIA) | 2 | names differ |
| empty / unparsed name | own slot | no identity to compare on |

Name normalization: lowercase, strip every `(...)` group, collapse whitespace.
That turns `AMD Radeon 8060S Graphics (RADV STRIX_HALO)` and
`AMD Radeon 8060S Graphics` into the same key, and survives
`Intel(R) Arc(TM) A770` shapes.

Memory size is deliberately **not** part of the key: the same card reports
126976 MiB under ROCm and 128505 MiB under Vulkan, so an equality test would
break the dedup and a tolerance would be a magic number for no gain.

Failure direction is safe: an unmatched pair counts as two slots, i.e. the
current behavior. The dedup can only ever hide a control that has no effect,
never hide a real second GPU.

### Two gates instead of one

Today one fact (`multi_device`) gates four knobs that answer two different
questions. Split it:

| fact | true when | gates |
|---|---|---|
| `device_choice` | the scoped server exposes > 1 **selector** | `device` |
| `multi_device` | the scoped server sees > 1 **physical slot** | `tensor-split`, `main-gpu`, `split-mode`, vLLM's `tensor-parallel-size`; the TUI + CLI Device column |

That is expressed through the existing group machinery: a new `Group::Device`
("Device", ordered just before `MultiGpu`) holds the `device` knob with a new
`GroupGate::DeviceChoice`; `Group::MultiGpu` keeps the three placement knobs on
`GroupGate::MultiDevice`, now fed the physical count.

On this host that yields: `llamacpp-ROCmFP4` → Device row visible, Multi-GPU
placement hidden. `llamacpp-rocm` → both hidden. A real 2-GPU box → both shown.

### Running-view scoping

`settings.rs` resolves the launch's owning server before asking the gates,
mirroring the daemon's own resolution order in
`launch_service::pick_launch_binary`:

1. `ManagedRow.server` id → catalog lookup;
2. else the server owning `ManagedRow.device`'s selector;
3. else the first catalog server of `ManagedRow.backend`, preferring the one
   whose `binary` matches `daemon_info.server_path`;
4. unresolved → fall back to the app-wide facts (today's behavior), so a
   catalog that has not finished probing never hides a row that exists.

## Changes

### Stage 1 — identity (`src/backend/server.rs`)

- [x] `fn physical_key(name: &str) -> String` — the normalizer.
- [x] `Server::physical_device_count(&self) -> usize` — the slot walk, plus a
      free function over `&[Device]` so the CLI can call it off the wire.
- [x] Module doc gains one line: the probe keeps every selector, identity/dedup
      happens here.

### Stage 2 — gates (`src/launch/knobs/def.rs`, `src/backend/llama_cpp/knobs.rs`)

- [x] `Group::Device` (title `Device`) added to the enum + `all()`, before
      `MultiGpu`.
- [x] `GroupGate::DeviceChoice`.
- [x] `gate_open` takes a `GateFacts { device_choice, multi_device, mtp_capable }`
      struct instead of positional bools (three adjacent bools at four call
      sites is a swap waiting to happen).
- [x] `device` knob moves `Group::MultiGpu` → `Group::Device`. No other knob moves.

### Stage 3 — call sites

- [x] `src/tui/app.rs` — `multi_device()` counts physical slots.
- [x] `src/tui/launch_picker.rs` — `multi_device()` → scoped server's physical
      count; new `device_choice()` → `current_devices().len() > 1`.
- [x] `src/tui/tabs/settings.rs` — new `App::server_for_managed(&ManagedRow)`;
      gates read from that server, not the catalog.
- [x] `src/cli/resolve.rs` — `multi_device` reads each server's `devices` into
      `Vec<Device>` and calls the Stage 1 helper. No wire change:
      `status.servers[].devices[]` already carries `gpu_backend` + `name`.
      Parsed field by field rather than through `serde` — a strict
      `from_value` drops a whole server on one missing key, which would hide
      the column on a real multi-GPU host. A device with no `name` takes its own
      slot, degrading to the old selector count.
- [x] `src/init/doctor.rs` — summary reads `llamacpp-ROCmFP4 (1 GPU, 2 selectors)`
      when the two counts differ, `(N GPUs)` when they agree.

### Stage 4 — tests

- [x] `server.rs` unit tests for all five rows of the worked-cases table, using
      the verbatim strings from the real probe above.
- [x] `app.rs`: the within-server alternate-selector case, plus
      `gate_facts_for_managed` over all five resolution paths (explicit pick,
      selector owner, backend default, stale id, unplaceable row).
- [x] `launch_picker.rs`: on a 1-card/2-selector server the `device` row is in
      `ordered_fields()` and `tensor-split` is not.
- [x] `settings.rs`: catalog holds a 2-GPU server + a 1-device server; a launch
      on the 1-device server renders no placement group, and one on the 2-GPU
      server does. This is the regression test for defect 2.
- [x] `cli/resolve.rs`: mirror of the server.rs cases through the JSON path,
      keeping the existing malformed-payload assertions green.
- [x] `tests/knob_parity_test.rs` needs no change (`two_device_server` uses one
      family, so it stays two slots) — confirm it still passes rather than
      assuming.

### Stage 5 — optional, decide before implementing

- [ ] When a server has one physical slot but several selectors, make the
      device row **exclusive**: `Space` selects one compute path and clears the
      others, and the row never normalizes to "all ticked → unset". Today "all
      ticked" collapses to no `--device` flag, which is exactly the state that
      lets ggml register ROCm and Vulkan for the same iGPU and split the graph
      across both — the config.yaml notes record that wedging the GPU on
      2026-08-27. Small change (`toggle_focused_device` + the row label), but it
      changes an interaction, so it ships only with the behavior confirmed on
      this host first.

### Docs (same commit, per AGENTS.md)

- [x] `docs/usage.md` — the group table gains the `Device` row; the "appear only
      when more than one GPU device is detected" paragraph is rewritten to state
      the two gates and that alternate compute selectors for one card count as
      one GPU; `llamastash knobs --json` emits the new group title.
- [x] `docs/architecture.md:167` — the server→device paragraph states the
      identity rule.
- [x] `CHANGELOG.md` — one line under `[Unreleased]`.
- [x] `TODO.md` — struck; a fresh one-liner tracks Stage 5.

### E2E — run 2026-09-02 on the Strix Halo host, all green

```bash
cargo build --bin llamastash
target/debug/llamastash daemon stop && target/debug/llamastash daemon start
target/debug/llamastash list                     # no DEVICE column
target/debug/llamastash doctor                   # ROCmFP4 reads 1 GPU, 2 selectors
target/debug/llamastash --render --render-size 160x45
```

Results against the real daemon (driven with `scripts/tui/tui_drive.py`):

- `list` — DEVICE column gone; `status.servers` still carries both selectors on
  `llamacpp-ROCmFP4`, so nothing was dropped from the catalog.
- `doctor` — `llamacpp-ROCmFP4 (1 GPU, 2 selectors)`, siblings `(1 GPU)`.
- Picker on the FP4 file (seeded to `llamacpp-ROCmFP4` by `pi-fast`) — **Device**
  group with `device  [ ] ROCm0 · 1 of 2`, no **Multi-GPU placement** group.
- Picker on a model defaulting to `llamacpp-rocm` — neither group.
- Running launch (`start Llama-3.2-1B --server llamacpp-rocm`) — the read-only
  view shows Context → Offload → Attention → Throughput → Memory and no
  placement rows, with the two-selector build still in the catalog. This is
  defect 2 reproduced and fixed.
- `LLAMASTASH_DEBUG_FAKE_GPUS=2` — both groups and the DEVICE column return.

## Open decisions

1. **Device column gate.** The plan gates the Models-list / `list` `DEVICE`
   column on the *physical* count, so it disappears here. The column would
   otherwise be the only place in the list showing which compute path a launch
   took (there is no Server column). Taking the physical gate because the
   column's own semantic is placement — it renders `all` for an unpinned
   launch, and on one card `all` and `ROCm0` are the same placement.
2. **Stage 5** as written above.
