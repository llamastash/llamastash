---
title: "feat: SGLang backend on the shared safetensors substrate"
type: feat
status: active
date: 2026-09-09
origin: https://github.com/llamastash/llamastash/issues/36
depends-on: docs/plans/2026-08-10-001-feat-vllm-backend-plan.md
---

# feat: SGLang backend on the shared safetensors substrate

> The second half of [#36](https://github.com/llamastash/llamastash/issues/36).
> Consumes everything the vLLM backend paid for in
> [`2026-08-10-001-feat-vllm-backend-plan.md`](2026-08-10-001-feat-vllm-backend-plan.md):
> the `discovery::hf_repos` substrate, the process-per-model orchestration
> for a directory-shaped model, the non-GGUF admission gate, the served-name
> readiness contract. Where this plan differs from that one, it is because
> SGLang's memory model differs.

## Overview

Add SGLang as a fifth backend behind the `Backend` seam: direct,
process-per-model, serving safetensors HF repos through `sglang serve`'s
OpenAI-compatible HTTP server. It rides the generic supervisor and proxy
unchanged, so the work is a backend module plus the minimum registry wiring —
and one guard that could not be copied from vLLM.

## Verified environment facts

Read from `lmsysorg/sglang:v0.5.18-cu130` on a DGX Spark (GB10), 2026-09-09.
No model was loaded for any of these; `--help` and a live server's read-only
endpoints only.

- The image ships a `sglang` console script at `/usr/local/bin/sglang`;
  `sglang serve --model-path <dir>` is the launch shape. `python3 -m
  sglang.launch_server` is the same parser.
- `--served-model-name` takes **one** string (vLLM's takes a list).
- `--mem-fraction-static` is "the fraction of the memory used for static
  allocation (model weights and KV cache memory pool)" — a fraction of the
  whole pool. `--max-total-tokens` is "the maximum number of tokens in the
  memory pool. If not specified, it will be automatically calculated based on
  the memory usage fraction." There is **no byte-level cap**.
- `--enable-unified-memory` replaces "the statically-partitioned hybrid-model
  pools (full-attn KV + SWA/Mamba state) with one byte buffer split
  dynamically between sub-pools." It is not a host unified-memory switch.
- `--tool-call-parser`, `--reasoning-parser` and `--quantization` are argparse
  `choices` (closed sets). The two parser lists in `knobs.rs` are transcribed
  from the `v0.5.19` tag's detector maps (0.5.19 added `dots`, `ling3`,
  `spark25` over 0.5.18); `docs/sglang-setup.md` records the version so the
  next reader can diff.
- `/get_server_info` returns every resolved server argument (479 keys),
  including `context_length`, `max_total_tokens` (the requested cap, `null`
  when unset), `max_total_num_tokens` (the resolved pool) and
  `served_model_name`. `/v1/models` has no context field.
- The HTTP server adds a permissive CORS middleware unconditionally, with no
  flag to narrow it.

Casebook record: `spark-casebook/casebook/2026-09-09-sglang-memory-levers.md`.

## Key technical decisions

1. **The guard is a token cap, not a byte cap.** vLLM's unified-memory guard
   sets `--kv-cache-memory-bytes`; SGLang has no such flag. The shared byte
   budget (`launch::admission::unified_kv_cache_budget`, hoisted out of the
   vLLM module so both engines carry one policy) is divided by the model's KV
   bytes per token, read from `config.json`: `2 × layers × kv_heads ×
   head_dim × dtype_bytes` for GQA/MHA, `layers × (kv_lora_rank +
   qk_rope_head_dim) × dtype_bytes` for multi-head latent attention,
   `text_config` for multimodal repos. Hybrid layers are priced as full
   attention — an overestimate, so the cap lands smaller: the safe direction.
2. **Unreadable geometry refuses the launch** with `max-total-tokens` named as
   the override, mirroring the byte budget's `None` means refuse. A guess in
   the wrong direction on a unified host is the freeze the guard exists to
   prevent.
3. **`served-model-name` is not a knob.** The launcher always passes the repo
   id, readiness matches it, and argparse keeps the last occurrence — so a
   user-set one would leave the launch waiting out its probe budget. It is
   refused in extras for the same reason.
4. **Priority 4, below vLLM's 5.** On a host with both engines the
   longer-validated one stays the `auto` default; `--backend sglang` selects
   SGLang. Two projectors for one snapshot now merge into one catalog row
   (`daemon::discovery_task::merge_by_path`); before this the catalog, keyed
   by path, kept only the last projector's row.
5. **Shared code moved to the substrate rather than copied.** Safetensors
   eligibility, row projection, snapshot detection and repo-id recovery moved
   from `backend/vllm/discovery.rs` into `discovery::hf_repos`; the extras
   strip moved into `launch::params`. The vLLM leaf file is gone.
6. **Knob shapes follow the registry.** `quantization` shares one declaration
   with vLLM (the registry requires one kind per id), so SGLang's dashed
   methods go through extras. The two parser knobs are closed `Enum`s from
   0.5.18's own choices.

## Scope boundaries

- Single host. `--tp-size` is allowed through extras; multi-node, data
  parallel and prefill/decode disaggregation are refused.
- No CORS switch — there is no flag to project.
- The guard is unit/integration-tested against a fixture and was
  load-tested once on a DGX Spark with the 0.5B (see the UAT record); a large
  model where the cap lands below `--ctx` is untested.

## Implementation units

- [x] Knob table (`src/backend/sglang/knobs.rs`), with `max-total-tokens` and
      `enable-unified-memory` added.
- [x] Substrate hoists: `hf_repos` leaf helpers, the unified-host budget, the
      extras strip.
- [x] One merged row per snapshot when two engines project it.
- [x] `Backend` impl (`src/backend/sglang/mod.rs`), guard
      (`src/backend/sglang/guard.rs`), registration, `--sglang` /
      `LLAMASTASH_SGLANG`.
- [x] Fixture (`tests/fixtures/fake_sglang_server.rs`) and integration tests
      (`tests/sglang_backend_test.rs`).
- [x] Docs: `docs/sglang-setup.md`, usage, architecture, troubleshooting,
      config example, README, CHANGELOG, TODO.
- [x] `scripts/sglang/{probe.sh,uat.sh}`.
- [x] Real-hardware UAT on a DGX Spark (2026-09-09, spark2, containerised
      0.5.18 behind a wrapper): `--max-total-tokens 699050` came back as
      `max_total_num_tokens`, host memory peaked at 22 GiB of 121 GiB,
      `resolved_ctx` read back, chat served, replay re-resolved the cap, stop
      reaped the container. Record:
      `spark-casebook/casebook/2026-09-09-llamastash-sglang-token-cap-uat.md`.
- [ ] A launch whose cap lands below `--ctx` (the warning path) on a large
      model.

## Known limitation

With both safetensors engines on, vLLM (registered first) mints the
`ModelIdentity::Backend { backend, name }` for every safetensors directory, so
`identity.backend` reads `vllm` even for a `--backend sglang` launch.
`resolved_backend` records the real engine and the running row reports it
correctly; both engines derive the same `name`, so keys stay consistent.
Cosmetic today; it matters the day `identity.backend` drives routing.

## Review — PR #79

Findings from the maintainer's review of
[#79](https://github.com/llamastash/llamastash/pull/79), each with its fix.
Tick as landed.

- [x] **RV1 — `kv_lora_rank: 0` uncapped the pool.** The latent branch
      triggered on the key's *presence*, so a "not MLA" zero priced the model
      at 0 bytes per token, and `max(1)` in the cap turned that into the whole
      budget as tokens, clamped to `u32::MAX`: the guard off on the host it
      exists for. Same for `num_hidden_layers: 0`.
      *Fix:* the latent branch is gated on `rank > 0` and falls through to the
      head geometry; zero layers, and any zero product, is `None` (unreadable,
      refuse). `tokens_for_budget` refuses a zero per-token cost outright.
- [x] **RV2 — the test pinned RV1.** `a_zero_per_token_cost_does_not_divide_by_zero`
      asserted the zero case was fine. Flipped to assert refusal; two new
      cases cover the zero rank and the zero layer count.
- [x] **RV3 — the unsampled path skipped the token floor.** Before the first
      host sample the cap was `DEFAULT / per_token` with no floor, so a model
      over ~4 MiB per token launched with a sub-2048-token pool.
      *Fix:* both paths spend their budget through `guard::tokens_for_budget`,
      which carries the floor; the unsampled path refuses with the override
      named. Daemon-driven regression test.
- [x] **RV4 — parser lists were a release stale.** Re-verified against the
      `v0.5.19` tag: three tool-call and two reasoning additions. The source
      version is now recorded in `docs/sglang-setup.md`.
- [x] **RV5 — the 256 KiB body cap bounded nothing.** `resp.bytes()` buffered
      the whole body before `truncate`, and a truncated body would not parse.
      *Fix:* the body is read in chunks and rejected past the cap. The
      `/v1/models` helper in `daemon::orphans` has the same shape and is left
      as is.
- [x] **RV6 — `docs/reviews/pr-79.md` removed.** Its durable parts are this
      section and the limitation above; the open items were already in
      `TODO.md`.
- [ ] **RV7 — the fraction projection double-counts weights.**
      `mem_fraction_static` covers weights and the pool per 0.5.18
      `ServerArgs`, and the projection multiplies by free rather than the
      pool, so the gate over-refuses (fails safe). vLLM's
      `gpu_memory_utilization` projection is the same code. The comment now
      says what the code does; the shared fix is a follow-up in `TODO.md`.
- [x] **RV8 — `build_options` took ten positional bools.** Landed on main
      after the merge as `BuildOptionsArgs`, spread over
      `BuildOptionsArgs::new(cli, config)` rather than `Default` (the `cli` /
      `config` borrows rule out deriving it). The four backend bools are one
      `BTreeMap<String, bool>` from the CLI down, OR-ed with each backend's
      `LLAMASTASH_*` var through the `FORCE_FLAG_ENV` table, so a new backend
      adds a row instead of an argument. `handle_start` lost its positional
      bools too.

