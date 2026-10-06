---
title: "spike: llama-server slot save/restore vs prompt reprocessing"
date: 2026-10-06
status: done
unblocks: ["R14 batch 4: keep a model's prompt cache across an unload"]
todo: "Repeat the 100k-token run on a large model. Re-run the chat check when ggml-org/llama.cpp#28194 closes."
---

# Finding

**Go for full-attention models, no-go for hybrid and sliding-window models on
llama.cpp b11457.** On Llama-3.2-1B a 100,000-token slot saves in 0.3 to 2.7 s
and restores in 0.3 to 1.1 s, against 73 to 82 s to process the prompt again.
On Qwen3.5-9B (`qwen35`) and gemma-4-E2B (`gemma4`) the restore succeeds and
the server then processes the whole prompt anyway.

Tested with `llama-server` build 11457 (commit `5ad1c5da0`, the newest release
on 2026-10-06), ROCm build, Radeon 8060S, 16 threads. Battery 98%, not
charging, `platform_profile` `performance`. The host was shared: another model
server held 22 GiB of GPU memory and other builds were running.

## 100,000 tokens, Llama-3.2-1B-Instruct Q4_K_M

`scripts/bench/slot_save_spike.py --tokens 100000 --ctx 131072`, one slot.

| | `-ngl 0` | `-ngl 99` |
|---|---|---|
| Process the prompt cold | 82.3 s (1215 tok/s) | 73.4 s (1362 tok/s) |
| Save the slot | 2.69 s | 0.29 s |
| Restore, file in page cache | 0.34 s | 0.28 s |
| Restore, file dropped from page cache | 1.08 s | 0.68 s |
| Returning prompt, 63 new tokens | 100,000 cached, 0.20 s | 100,000 cached, 0.12 s |
| Returning prompt that diverges at 90% | 90,000 cached | 90,000 cached |
| Model load, process start to `/health` 200 | 1.0 to 2.2 s | 0.8 to 1.6 s |

The file is 3,278,498,788 bytes (3.05 GiB), 32,784 bytes per token, in both
runs. The two save times differ by 9x and this spike did not find out why.

## What a returning chat turn reuses

Two turns through `/v1/chat/completions` (`--jinja`, `-c 16384`, `-ngl 99`).
Turn 2 is the history plus one new user message. "Cached" is `timings.cache_n`.

| Model (`general.architecture`) | Turn 2 prompt | Server kept running | Stop, start, restore | Stop, start |
|---|---|---|---|---|
| Llama-3.2-1B (`llama`, full attention) | 4562 | 4545 cached | 4545 cached | 0 |
| Qwen3.5-9B (`qwen35`, hybrid) | 7208 | 7165 cached | 0 | 0 |
| Qwen3.5-9B, thinking on | 7187 | 7163 cached | 0 | 0 |
| gemma-4-E2B (`gemma4`, sliding window) | 7205 | 7185 cached | 0 | 0 |

In every row the restore call returned 200 with `n_restored` equal to
`n_saved`.

With raw token prompts, Qwen3.5-9B reuses a restored slot only when the next
prompt continues exactly from the saved tokens (8003 of 8067 cached). A prompt
that drops the last 3 saved tokens gets 0. gemma-4-E2B gets 0 in both cases.

Cause, read in `tools/server/server-context.cpp` at `5ad1c5da0`: for these
models the server needs a context checkpoint to roll a slot back, a slot file
does not carry the checkpoints, and without one it logs `forcing full prompt
re-processing`. Upstream tracks this as
[ggml-org/llama.cpp#28194](https://github.com/ggml-org/llama.cpp/issues/28194)
(open). `Qwen3.8-27B-UD-Q6_K.gguf` also reports `qwen35` in its header, so it
is in the same group. It was not loaded here.

## Slot behaviour that shaped the design

Default slots (`--parallel` unset: 4 slots, one shared KV pool), Llama-3.2-1B:

- Conversation A landed in slot 3. When conversation B started in slot 2, slot 3
  dropped to 0 tokens. The server moves an idle slot into its host-memory prompt
  cache (`--cache-ram`, 8192 MiB by default) and clears it. No endpoint writes
  that cache to disk.
- After saving all four slots, restarting and restoring: B got 5000 of 5064
  tokens from cache, A got 0. So an eviction can keep the most recently used
  conversation, plus any others that are still in a live slot.
- A's request ran first after the restore and cleared B's restored slot the same
  way. B was still served from cache, because the server loaded it back from its
  host-memory prompt cache.
- Saving an empty slot returns 200 and a 36-byte file. Saving slot id 4 on a
  4-slot server also returns 200, so a slot id past the end is not an error.
- `GET /slots` reports `n_prompt_tokens` only for a slot that has served a
  request. A slot that was just restored reports none.
- Restoring a missing file returns 400 `Unable to restore slot: No available
  space in KV cache or invalid slot save file`. A failed restore clears the slot.

File size per cached token: 32,784 bytes on Llama-3.2-1B. Qwen3.5-9B writes a
fixed 50 MiB plus 32,796 bytes per token (315,158,172 bytes at 8003 tokens,
56,299,344 at 110). gemma-4-E2B writes 6,947 bytes per token at 8003 tokens.

## A probe that predicts reuse without a restart

On a running server: fill slot 0 with a 40-number prompt, save, erase, restore,
then send a prompt that shares the first 24 numbers. The erase drops the
in-memory checkpoints, so the result matches the restart case:

| Model | `cache_n` on the shorter prompt | Probe time |
|---|---|---|
| Llama-3.2-1B | 48 of 49 | 0.04 s |
| Qwen3.5-9B | 0 of 63 | 0.39 s |
| gemma-4-E2B | 0 of 64 | 0.19 s |

## Decisions

1. **When to save.** Before an eviction only: the idle sweep and make-room. Not
   on `stop` and not on daemon shutdown.
2. **Which launches.** Only where the probe shows reuse. The probe runs once per
   model and settings, before the launch is reported `Ready`, on auto-started
   launches. Hybrid and sliding-window models start saving on their own once the
   server reuses their restored slots.
3. **Where.** `<cache dir>/slots/`, one flat directory that is also the
   `--slot-save-path` of every launch. Files are named
   `<model stem>.<key>.slot<N>.bin`. The key is a hash of the model path, its
   header hash, the server build and the argv without the port, so a file is
   restored only into a launch with the same model and settings.
4. **Cap and cleanup.** `max_gib` (16) bounds the total: a slot whose estimated
   size does not fit is skipped, and the oldest files go first. `max_age_secs`
   (24 h) deletes old files. `min_tokens` (2048) skips small slots. A save is
   skipped when the disk has less than the estimate plus 1 GiB free. A restore
   deletes the file it read.
5. **Matching.** Each file goes back into the slot id it came from before the
   launch is `Ready`. The server's own prefix match then picks the slot.
   llamastash does not track conversations.
6. **Off by default** (`backend.llamacpp.slot_save.enabled`). Every eviction
   writes about 3 GiB per 100,000 cached tokens on a 1B model, and only
   full-attention models benefit today.

## Checked through llamastash

Isolated daemon built from the working tree, Llama-3.2-1B on CPU,
`proxy.idle_ttl_secs: 15`, `slot_save.enabled: true`:

- Turn 1 processed 8989 tokens. The idle sweep saved slot 3 (9002 tokens,
  282 MiB). Turn 2 after the reload: 9002 cached, 17 processed. The server log
  shows it picked the restored slot itself: `selected slot by LCP similarity,
  f_sim_best = 0.998`.
- The same flow addressed at `<model>@<preset>` (a 16384-token preset): 3054
  cached, 16 processed, under its own key.
- With `slot_save` off, turn 2 after a reload gets 0 cached.
- Qwen3.5-9B: the probe logged `a restored slot is not reused` and nothing was
  written.

## Not measured

- A large model at 100,000 tokens. File size grows with layer count and KV
  width, so expect several times the 1B figures.
- A slow disk.

## Reproduce

```sh
scripts/bench/slot_save_spike.py --model <gguf> --slot-dir <dir on a real disk> \
  --tokens 100000 --ctx 131072 --ngl 99 --out result.json
```
