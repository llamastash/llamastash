# src/launch

## Built-in defaults table

The static `(arch, gpu_backend) → TypedKnobs` table is `src/launch/defaults_table.rs`. When `data/benchmark-snapshot.json` gains a recommender pick, audit coverage:

- No `n_gpu_layers` is pinned anywhere — offload placement is delegated to llama-server's `--fit` (a layer-less `n_gpu_layers` seeds `Auto` and emits no `-ngl`). Archs missing from `COVERED_ARCHS` fall through to the empty `*` row.
- `FLASH_ATTN_ELIGIBLE` is opt-in only; extend it when measurement confirms an arch is clean on NVIDIA / Apple Metal. AMD/HIP coverage is uneven — leave it to `config.yaml arch_defaults`.
- Folklore-only flags (`mlock`, `no_mmap`, KV-cache quant types) stay unset until measurement supports them.
