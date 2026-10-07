---
title: "spike: llama.cpp slot KV save/restore as a cross-process prompt cache"
date: 2026-10-07
status: done
unblocks: ["R14 batch 4 — keep a model's prompt cache across an unload"]
todo: "Re-run scripts/bench/slot-kv/ on a GPU with nothing else resident to get uncontended absolutes."
---

## Question

`llama-server` can write one slot's KV cache to disk (`--slot-save-path PATH` plus
`POST /slots/{id_slot}?action=save`) and read it back into a fresh process
(`action=restore`). Does save + restore beat reprocessing a long prompt after an
eviction, and what does the launcher have to get right to use it?

## How it was measured

- `scripts/bench/slot-kv/spike.py` — one prompt built to a token target through
  `/tokenize` (so the length is the engine's own count), one request to fill the
  slot, `action=save`, `SIGTERM`, a **second process** on the same argv and save
  dir, `action=restore`, then the same prompt again. Cross-process is the whole
  point: an in-process save/restore would measure the ordinary prefix cache.
- `scripts/bench/slot-kv/edge_cases.py` — the cases a launcher can produce
  (different ctx, different model, fewer/more slots, truncated file, bad
  filename, changed offload, two conversations at once, output identity).
- Times are wall clock around the HTTP call, which is what a caller waits for.
  `eval tokens` comes from the engine's own `prompt eval time` log block.

Versions tested: `llama-server` 0.6.0-dev **b11457**, HIP/ROCm build on gfx1151,
model `bartowski/Llama-3.2-1B-Instruct-GGUF` `Q4_K_M` (16 layers, 8 KV heads,
head dim 64, f16 KV), `-c 131072 -np 1 -ngl 99`. Upstream source read at
commit `5ad1c5da0` (`tools/server/server-context.cpp`); the endpoint pair and
`--slot-save-path` are also in `tools/server/README.md` and were reported present
on build 11390. **A Halogen container was resident on the same GPU for every
run**, so the absolute seconds are pessimistic; the ratios are what the decision
uses.

## Measured

| prompt tokens | reprocess s | save s | file MB | restore s | first request after restore s | eval tokens on that request | slot selected by |
|---:|---:|---:|---:|---:|---:|---:|---|
| 8,380 | 2.13 | 0.03 | 274.8 | 0.02 | 0.01 | 1 | LCP similarity |
| 33,460 | 15.98 | 2.83 | 1097.0 | 0.09 | 0.03 | 1 | LCP similarity |
| 102,031 | 78.64 | 2.50 | 3345.0 | 0.31 | 0.07 | 1 | LCP similarity |

- **Go.** At 100k tokens: 78.6 s of reprocessing against 0.38 s of restore plus
  first request, about 200x. The cost is paid at eviction (2.5 s to write 3.3 GB,
  ~1.3 GB/s) and is only paid at all when something is actually evicted.
- The restored cache really is used: `eval tokens` is 1, not 102k, and the log
  says `selected slot by LCP similarity`. The engine matches the returning
  conversation to the slot itself; the launcher never has to.
- File size is linear in prompt tokens at **32.8 KB/token** on this model
  (2 · K+V · 16 layers · 8 KV heads · 64 head dim · 2 B = 32 KB, plus ~1 KiB of
  header). Predicting it from the GGUF header with
  [`crate::gguf::memory::kv_bytes`] gives 3,345,016,576 B against a measured
  `n_written` of 3,345,017,524 B at 102,031 tokens, so the projection is good
  enough to gate a save before writing. The same model family at 70B would be
  roughly 10x per token, which is why the cap has to be checked before the write
  and not only after it.

## Edge cases

| case | engine answer | slot usable after |
|---|---|---|
| restore into a different slot id than the save | 400 `No available space in KV cache or invalid slot save file` | yes |
| save/restore on a slot id beyond `-np` | 200 with `n_saved: 0` / a few dozen bytes (no error) | yes |
| save a slot that never got a prompt | 200, `n_saved: 0`, `n_written: 36` | yes |
| file truncated to half | 400, same message | yes |
| file absent | 400, same message | yes |
| restored prompt larger than the new slot's ctx | 400, same message | yes |
| file from a different model | 400, same message | yes |
| `--cache-type-k/v f16` file restored into `q8_0` (and the reverse) | 400, same message | yes |
| `-ngl 99` file restored into `-ngl 0` | 200, `n_restored` = full prompt, cache hit | yes |
| filename `../escape.bin` or `sub/dir.bin` | 400 `Invalid filename` | yes |
| filename `ok-1_2.bin` | 200 | yes |
| two conversations in two slots, both saved, both restored | both hit (1 eval token each) | yes |
| greedy continuation (temp 0, 48 tokens) cold vs warm vs restored | all three byte-identical | — |

Every failure is the same 400 with the same message, and in every case the slot
stays usable: a failed restore means "process the prompt normally", never a
wedged slot. `fs_validate_filename` is enforced on the `filename` field, so a
flat generated name is required. `--slot-save-path` must name a directory that
**already exists** (`error while handling argument "--slot-save-path": not a
directory:` and the server exits), so the launcher has to create it before the
spawn. With the default `-np` the engine put the first request on slot 3 of 4 and
reported `"id_slot": 3` in the response body, so slot ids are not predictable
from the launcher side; `GET /slots` lists them with per-slot
`n_prompt_tokens` (603 for a 603-token prompt, still there after the request),
which is the cheap gate for "is this slot worth saving".

Offload depth and cache type are **not** in the file's validity check in
different directions: a different `-ngl` restores fine, a different
`--cache-type-*` is rejected. Both are the safe outcome, and neither has to be in
the launcher's key: rejection is a normal failed restore.

## Decisions taken

- **When to save: only on the eviction path** (`src/proxy/eviction.rs`, before the
  stop). A manual `stop` is a deliberate teardown and not worth the write; the
  sweep and `make_room` are the two paths that lose a cache the user may want
  back. Saving delays the eviction that is freeing memory, so the phase is
  time-boxed.
- **When to restore: after the engine answers its readiness probe, before the
  launch is marked Ready**, so the first proxied request already sees the cache.
  Bounded, and a failed restore leaves Ready unaffected.
- **Matching is by model identity, not by prompt.** The launcher keys a cache on
  the model file (path + size + mtime) and the engine matches conversations to
  slots by LCP similarity on its own. That is enough because a KV block is only
  reused for the token prefix it was computed from: knobs that change tokens
  (chat template) or placement (`-ngl`, sampling) cannot make a wrong KV block
  match. A cache-type change is caught by the engine.
- **Files: flat dir under the cache dir**, `cache_dir/slot-cache/`, one file per
  slot named `<fingerprint>-<slot>.bin`, plus a `manifest.json` holding the
  entries (slot, tokens, bytes, saved-at, failure count). The directory is
  deleted-safe by construction: losing it costs reprocessing.
- **Cap and cleanup: checked before the write and after.** A projected size
  (`kv_bytes` at the slot's token count and cache type) over `max_bytes` is not
  written at all; after writing, entries are dropped by TTL then oldest-first
  until the total fits. Entries that fail to restore twice are deleted with their
  file, so a doomed file stops costing a restore on every launch.
- **Saved empty and sub-threshold slots are not kept.** An empty save answers 200
  with `n_saved: 0` and a few dozen bytes, so the counts and `GET /slots` decide,
  not the HTTP status.
- **Default off** (`backend.llamacpp.slot_cache.enabled: false`): the writes are
  gigabytes, disk budgets vary, and keeping default argv unchanged keeps the
  bench parity harness meaningful.
