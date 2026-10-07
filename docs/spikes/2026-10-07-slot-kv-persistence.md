---
title: "spike: llama.cpp slot KV save/restore against prompt reprocessing"
date: 2026-10-07
status: complete
unblocks: ["Batch 4 (R14): keep a model's prompt cache across an unload"]
todo: "Re-run `scripts/bench/measure-slot-kv.py` when ggml-org/llama.cpp#28194 closes, and after any llama.cpp bump that touches the state IO path or the KV layout; bytes per token tracks the layout, so a quant/KV-dtype change moves it."
---

# Finding

**The save works; the restore does not, on the model class tested.** llama-server writes a slot's KV state fast and small enough to be worth keeping, but reading it back gives a returning request no reused tokens on a hybrid/recurrent model. A 120,000-token prompt that took 266 s cold still took 268 s after `action=restore`, and `n_restored` reported all 120,000 tokens. This is a known upstream bug, [ggml-org/llama.cpp#28194](https://github.com/ggml-org/llama.cpp/issues/28194) (open since 2026-09-01), which reports the same split on its own measurements: 8007 of 8011 tokens reused (99.97%) on a pure-attention model against 0 on a `qwen35` hybrid, and says the reuse gate has never interoperated with the save format rather than being a regression.

So the feature ships behind `backend.llamacpp.slot_cache` (default `false`), built and tested, ready to turn on when upstream lands the fix or when you serve a pure-attention model. See [`src/backend/llama_cpp/slot_cache.rs`](../../src/backend/llama_cpp/slot_cache.rs).

Host: Strix Halo (gfx1151), 124 GiB unified memory, llama.cpp `0.6.0-dev (build 11457, commit 5ad1c5da0)`, `Qwen3.5-4B-Q4_K_M.gguf` (2.7 GiB, `general.architecture = qwen35`, a Gated DeltaNet hybrid named in #28194), `-ngl 99`. `-c 40960` for the 6k/24k rows, `-c 131072` for the 120k row (`n_slots = 4`, `n_ctx_slot = 131072`, `kv_unified = true`). Raw `llama-server` on an isolated port, not through llamastash, so the numbers carry no launcher overhead.

### What a save costs

| Prompt tokens | Reprocess, cold | Save | Save file |
|---:|---:|---:|---:|
| 6,001 | 4.67 s | 0.14 s | 249.5 MB |
| 24,000 | 24.9 s | 0.47 s | 839.8 MB |
| 120,000 | 266.2 s | 0.73 s | 3.99 GB |

Files run 33-42 KB per saved token; the ratio drifts down as the prompt grows, so budget the cap on the high end. The 120k write ran at 5.5 GB/s with the page cache warm (that row was written after two smaller ones into the same directory); the 24k write ran at 1.8 GB/s. Reprocess is the server's own `prompt eval time` for a `cache_prompt: true` request against an empty slot, and it degrades with context: 1,286 tok/s at 6k against 451 tok/s at 120k.

### What a restore buys: nothing here

Every measurement of a request after a restore reprocessed the whole prompt.

| Case | First request after restore |
|---|---|
| 6k, fresh server, restore before any request | 5.90 s / 6,031 tokens |
| 6k, restore into the slot that already held the prompt in-process | 6.53 s |
| 6k, control: same prompt twice in one process, no restore involved | 0.059 s |
| 120k, fresh server, restore before any request | 268.1 s / 120,000 tokens |

The control row is the one that makes this a finding instead of a measurement error: prefix reuse works fine inside a process (59 ms), so the harness, the prompt, and the routing are all healthy. Only the file round-trip fails to transfer anything. Each failing request was routed by the engine's own longest-common-prefix match (`selected slot by LCP similarity, f_sim_best = 1.000, f_keep = 1.000`) to the slot that supposedly held the state, then computed 0.96-2.23 ms per token from token zero.

The third row carries a second warning: a restore into a slot that already held that prompt turned a 59 ms request into a 6.5 s one, so on this model class a restore does not merely fail to help, it throws away reuse the live slot had. llamastash only restores into a launch that has served nothing yet, which is why no shipped path can hit that case.

### Why, from the source at that commit

`tools/server/server-context.cpp:3444` sets `n_past` to the common prefix between the saved prompt tokens (which `action=restore` does populate) and the incoming ones. Then at `:3580-3615`, if `llama_memory_seq_pos_min` says the slot's memory starts at or past the threshold `pos_next - n_swa`, the server looks for an in-process context checkpoint covering that position and, finding none, runs the `do_reset` branch: `pos_next = 0; n_past = 0`. Checkpoints live in `slot.prompt.checkpoints` (`server-task.h:609`), are created during generation (`-ctxcp`, default 32, `-cms` default 8192), and are never written into or read from a save file. On a pure-attention model `n_swa` is 0, that gate does not fire, and the restored cells are used; on a hybrid or sliding-window model `n_swa > 0` puts the gate in the way and the restore is decorative. Upstream files the same diagnosis in #28194. The `forcing full prompt re-processing due to lack of cache data` line in that branch is trace-level (`SLT_TRC`), so a default-verbosity server log never shows it; `-lv 6` does.

Related open issues in the same area: #28276 (a restored slot blocks prompt-cache lookup), #28619 (the draft model's state is not persisted), #27148 (`--cache-ram` restoring an unrelated conversation), #29678 (ask for automatic save/restore by session id).

## How the first read of this was wrong

The first pass concluded the opposite and wrote it into this page: "100× to 300× cheaper, go", with 6k and 24k rows showing 34-43 ms after a restore. Two harness bugs produced that.

- A restart that did not restart. The harness waits on `/health` at a fixed port, and a leftover `llama-server` from an earlier phase was still bound to it, so the "new" process failed to bind, `/health` answered from the old one, and the request hit a prompt that process already had in cache. Same-process reuse is 59 ms, which is exactly the shape of the false numbers.
- `spawn()` wiped the save directory before each start, so some restores read a missing file (`error loading sequence state file: ... No such file or directory`) and were scored as instant successes because the code ignored the error path.

The fix in the committed harness ([`scripts/bench/measure-slot-kv.py`](../../scripts/bench/measure-slot-kv.py)): one fresh process per prompt size, the workdir wiped once up front, restore errors surfaced as row values instead of swallowed, and a uuid prefix per prompt so two sizes built from the same filler paragraph cannot share a prefix and read each other's cache. If you re-run anything here, check that the PID on the port is the one you just started.

## Engine behaviour worth knowing

- `--slot-save-path PATH` must point at an **existing** directory; the server refuses to start otherwise. `slot_cache::create_save_dir` makes the per-launch directory before the child spawns.
- The endpoint is on by default (`--slots, --no-slots  expose slots monitoring endpoint (default: enabled)`), so `--slot-save-path` alone is enough. `POST /slots/{id}?action=save` takes `{"filename": ...}` relative to that directory and answers `n_saved` (tokens) and `n_written` (bytes); `action=restore` answers `n_restored`. Both answer 2xx whether or not anything usable moved.
- An **idle slot saves a 624-byte stub with `n_saved: 0`** and no error. Anything that reads file size or trusts the call to have succeeded will believe an empty conversation was saved; `save` skips those.
- Restore validates the model architecture and free KV cells, nothing else. A mismatched file answers `400 Unable to restore slot: No available space in KV cache or invalid slot save file`, with `state_read_meta: failed to find N available cells in kv cache` (file larger than the slot's free cells) or `llama_state_seq_load_file: failed to restore kv cache` (same model shape assumed, different layout: a file saved from a `n_slots=2, n_ctx_slot=8192, kv_unified=false` server rejected by a `n_slots=4, n_ctx_slot=40960, kv_unified=true` one) in the log. A different model file was not tested because llamastash refuses to guard on the engine's silence: every pair carries a fingerprint of the model path, size, mtime, serving mode and engine-resolved ctx, and a pair that does not match is dropped unread.
- Slot choice is the engine's, unchanged by a restore: `slot get_availabl: selected slot by LCP similarity` routes a request to the slot whose cached prompt shares the longest prefix. Restoring each slot id is therefore enough to give a returning conversation its cache back once #28194 is fixed; a conversation that lands elsewhere reprocesses and never reads a stranger's state.
- Restores compete for the same KV pool as live requests: a second restore that does not fit the remaining cells fails with the same 400, cleanly, leaving the slot usable.

## Decisions taken from this

- **Ship it off by default.** `backend.llamacpp.slot_cache: false`. On the model family tested, turning it on writes tens of gigabytes per long conversation and returns nothing, so the honest default is off. Anyone serving a pure-attention model can set it to `true` today and get the win upstream measured.
- **Save on an eviction only.** `proxy::eviction::stop_launch` (idle sweep and make-room) saves; a manual `stop` means the conversation is over. Reload happens in the supervisor between `/health` and `Ready`.
- **One directory per launch** under `<cache_dir>/slots/<pid>-<clock>`, so a save can never be read by a launch of a different model or a different config without passing the fingerprint.
- **Cap by size, on every save** (`backend.llamacpp.slot_cache_max_mib`, default 8192 MiB), oldest pair first and never the pair just written, plus a boot pass that drops pairs older than 7 days, engine temps, and halves of broken pairs. At the measured ~40 KB/token the default cap holds roughly one 120k-token slot or about 200k tokens spread across slots.
- **Feature off unless the binary advertises the flag.** One `--help` probe at boot per binary, the same subprocess that settles the `--load-mode` dialect, decides `BuildCaps.slot_save`. A false negative loses a speedup; emitting a flag the engine rejects loses the launch.
- `--slot-save-path` in user `extras` is refused and stripped, the same way the launcher owns other heads it writes itself: a copy pointing at another directory would write saves that nothing fingerprints.

## The shipped path, end to end, on a pure-attention model

The table above rests the "works on that class" half on upstream because every row here ran a `qwen35` hybrid. The shipped wiring was then driven through the real daemon and a real `llama-server` (same b11457 binary, `Llama-3.2-1B-Instruct-Q4_K_M`, `backend.llamacpp.slot_cache: true`, `proxy.idle_ttl_secs: 20`, isolated state/config/cache dirs):

- the launch carried `--slot-save-path <cache_dir>/slots/<pid>-<clock>` in argv;
- a 1,595-token prompt answered `cached_tokens: 0`;
- the idle sweep stopped the launch and saved one slot: 52,520,404 bytes, 33 KB per token, in line with the ratio above;
- the next request relaunched the model and answered `cached_tokens: 1594` of 1595, having restored in under 0.1 s;
- the save pair was gone afterwards, and a manual `stop` saved nothing.

So on a pure-attention model the round trip does transfer the cache, across a real process boundary rather than a restart the harness performed, and #28194 is the only thing standing between the hybrid models and the same result.

## Not measured here

Cold-vs-page-cache weight reads, which the plan's step 1 also asks for, belong to the `warm <model>` item and its own loaded-model session; nothing here speaks to it. Multi-turn conversations (a save taken mid-generation, several slots with different prompts in one launch) were not exercised — the numbers come from one prompt per slot. `docs/testing/hardware-uat.md` covers the llamastash-side check.

## Repro

`scripts/bench/measure-slot-kv.py --model ~/models/Qwen3.5-4B-Q4_K_M.gguf --sizes 6000,24000,120000 --ctx 131072` reproduces the save column and the post-restore reprocess. It prints one JSON row per size; a row whose `after_restore_ms` is near its `reprocess_s` is #28194 reproducing.
