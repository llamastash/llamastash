# src/launch

## Built-in defaults table

The static `(arch, gpu_backend) → crate::launch::knobs::KnobSet` table is `src/launch/defaults_table.rs`. When `data/benchmark-snapshot.json` gains a recommender pick, audit coverage:

- No `n-gpu-layers` is pinned anywhere — offload placement is delegated to llama-server's `--fit` (a layer-less `n-gpu-layers` seeds `Auto` and emits no `-ngl`). Archs missing from `COVERED_ARCHS` fall through to the empty `*` row.
- `FLASH_ATTN_ELIGIBLE` is opt-in only; extend it when measurement confirms an arch is clean on NVIDIA / Apple Metal. AMD/HIP coverage is uneven — leave it to `config.yaml arch_defaults`.
- Folklore-only flags (`mlock`, `no-mmap`, KV-cache quant types) stay unset until measurement supports them.

## Knob declarations

A backend's tunables live in `src/backend/<id>/knobs.rs` as `KnobDef`s and
nowhere else. Every surface is generated from them, so adding a knob there adds
its `start` flag, its editor row and its preset key with no other edit. Keys are
the engine's own flag spelling minus the dashes. See `docs/architecture.md`
§ The knob registry, and `tests/knob_parity_test.rs` for what holds it in place.
