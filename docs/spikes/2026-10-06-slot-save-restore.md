---
title: "spike: llama-server slot save and restore against reprocessing"
date: 2026-10-06
status: done
unblocks: ["R14 batch 4: keep a model's prompt cache across an unload"]
todo: "Remeasure on a large GPU-offloaded model. This run used a 1B model on CPU."
---

# Question

When the idle sweep or make-room stops a llama.cpp launch, the next request
reprocesses the whole prompt. Is writing the slot's KV cache to disk and reading
it back faster than that?

# Setup

- llama-server build 11457 (commit `5ad1c5da0`, 2026-10-06). The latest upstream
  release that day was `b11456`, so this build is one commit ahead of it.
- Model: `bartowski/Llama-3.2-1B-Instruct-GGUF`, Q4_K_M, 770 MB.
- `-ngl 0 -t 16 -c 110000 -np 1 --cache-ram 0`, CPU only. A large model was not
  used because another engine held the GPU during the run.
- Host: AMD Strix Halo (32 threads), ext4 on NVMe.
- Prompt: 100,000 tokens of random common words, sent as a token array to
  `/completion` with `n_predict: 1`.
- Script: `scripts/bench/slot_cache_spike.py`.

# Numbers

| Step | Time | Notes |
|---|---:|---|
| Process the 100,000-token prompt | 81.8 s | 1,222 tokens/s |
| `POST /slots/0?action=save` | 2.16 s | 3,278,400,436 bytes written, 1.5 GB/s |
| Server restart to `/health` 200 | 1.6 s | weights in page cache |
| `POST /slots/0?action=restore` | 0.34 s | file still in page cache |
| Same prompt after restore | 0.22 s | 99,999 tokens from cache, 1 processed |

Save plus restore is 2.5 s against 81.8 s of reprocessing, about 33 times faster.

File size is 32.8 KB per token for this model (65,568,436 bytes at 2,000 tokens,
3,278,400,436 at 100,000), so it grows in a straight line with the prompt.

# What the run does not show

- **Large models.** The file is the KV cache, so its size per token scales with
  layer count and KV width. A model with a much larger KV cache will write tens
  of GB at 100k tokens. Not measured here.
- **A cold restore.** The restore read the file from page cache. Reading 3.3 GB
  from a cold NVMe adds a few seconds; not measured.
- **GPU offload.** With the KV cache on a GPU, save and restore also copy it
  across. Not measured.

# Behavior found on the way

Checked against the server source at the commit above and on the live binary.

- `--slot-save-path` must name an existing directory or the server refuses to
  start (`common/arg.cpp`).
- The filename in the request body is checked with `fs_validate_filename` and
  joined to that directory, so a request cannot write outside it.
- A slot id past the last slot is **not** an error. `get_slot_by_id` takes the id
  modulo the slot count, so `POST /slots/7?action=save` on a 4-slot server saves
  slot 3. A caller has to read the ids from `GET /slots`.
- `GET /slots` reports `n_prompt_tokens` and `is_processing` per slot, enough to
  skip empty and busy slots.
- A save or restore on a busy slot is deferred until the slot is idle.
- Restore fails with HTTP 400 when the saved prompt does not fit the slot's
  context, and clears the slot.
- The server picks a slot for a request by the longest common prefix between
  the request and each slot's cached tokens (`--slot-prompt-similarity`), so a
  slot restored under its old id is found by the returning conversation with no
  help from the caller.

# Decision

Go. Decisions taken for the implementation:

- **When to save:** only right before the idle sweep or make-room stops a
  launch. A user `stop` and a daemon shutdown save nothing.
- **Where:** `<cache_dir>/slot-cache/<key>/slot-<id>.bin`, where the key hashes
  the model path, size and mtime.
- **Cap and cleanup:** a restore deletes the files it read, so one model holds
  at most one save. The total across models is capped
  (`backend.llamacpp.slot_cache.max_gb`, default 20), oldest file first.
- **Matching:** each file goes back to the slot id it came from; llama-server's
  prefix matching does the rest.
- **Default:** off, because of the file sizes on large models.

End-to-end check on an isolated daemon with the same model at a 9,952-token
prompt: after an idle eviction the next turn of the conversation reported
`cache_n: 9952, prompt_n: 14` and returned in 1.39 s including the model reload.
The first turn had taken 5.55 s with `prompt_n: 9951`.
