# Batch 1 - Lemonade, FastFlowLM (NPU) and whisper.cpp as generic servers

**Host:** `deepu-flowz13-arch`, ASUS ROG Flow Z13 GZ302EA, AMD Ryzen AI MAX+ 395 (Strix Halo, XDNA2 NPU, 8 columns, 58 TOPS).
**Binary under test:** `target/debug/llamastash` from `main` at `68f5e44d` (v0.7.0).
**Engine versions (all real binaries, no fixtures):**

| Component | Version tested | Latest upstream on 2026-10-17 | Install path |
|---|---|---|---|
| `lemond` (Lemonade Server) | 2026.42.1 | v2026.42.1, released 2026-10-14 (same) | AUR `lemonade-server` 2026.42.1-1, `/usr/bin/lemond` |
| Lemonade `llamacpp:rocm` backend | b11395 | - | `lemonade backends install llamacpp:rocm` (pulls ~1.7 GB of ROCm 10.0.0 wheels into `~/.cache/lemonade/bin`) |
| Lemonade `whispercpp:cpu` backend | v1.8.4 | - | `lemonade backends install whispercpp:cpu`, gives `~/.cache/lemonade/bin/whispercpp/cpu/whisper-server` |
| FastFlowLM (`flm`) | v1.0.9 | v1.0.9, released 2026-10-15 (same) | portable `fastflowlm_1.0.9_linux.tar.gz` (48 MB) into `~/.local/opt/fastflowlm`. The previous source build at `/opt/fastflowlm` (2026-06-09) is dead: `libavformat.so.62` is gone since Arch moved to ffmpeg 8 |
| NPU stack | kernel 7.2.10-arch1-1, in-tree `amdxdna` 0.10, FW 1.1.2.65, XRT 2.21.75 + `xrt-plugin-amdxdna`, memlock unlimited, platform `aie2p` | - | `flm validate` passes |

**Models pulled for the runs:** `unsloth/Qwen3-0.6B-GGUF` (Lemonade, 0.38 GB), `qwen3:0.6b` (FLM `Qwen3-0.6B-NPU2`, 652 MB), `whisper-v3:turbo` (FLM `Whisper-V3-Turbo-NPU2`, 1.0 GB), `ggml-tiny.bin` (whisper.cpp).

All LlamaStash runs used isolated daemons (`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` under `~/.local/state/ls-batch1`, proxy port 0 for an ephemeral bind in the generic run, 11436 in the Lemonade run). The installed `llamastash` and the user's live daemon were never used.

## Verdict

The Lemonade backend can be dropped, and the multiplexer concept with it. Everything the Lemonade backend does at request time is reproducible with one generic entry plus one `proxy.aliases` line per model, and that path was driven with real traffic. Against the current `lemond` the dedicated path is broken in four independent ways (below), and the thing Lemonade is nominally here for, the NPU, is not reachable through it on Linux at all: the Linux registry has 179 models and zero of them use the `flm` recipe. NPU inference works today by running `flm` directly as a generic entry.

What the dedicated backend uniquely buys, and what it costs:

| Dedicated Lemonade backend | Generic entry |
|---|---|
| Model rows come from `lemond` itself, so its registry appears in `list` | One row per entry; a model the engine owns is addressed with `proxy.aliases` |
| `stop <model>` unloads one model in 0.12 s, the umbrella keeps running | `stop` kills the umbrella (4.7 s here) and every model in it |
| Context is sent as the `ctx_size` field of `POST /api/v1/load` | Context can only go where argv goes, which is nowhere for `lemond` |
| Device set comes from `/api/v1/system-info` | Device set comes from the entry's own `arch` / `params` fields |
| N models, one shared umbrella | N launches of one entry means N umbrellas (~38 MB RSS each idle, verified with two) |

Of those four, only the per-model unload is both real and unreplaceable, and it buys little on a `lemond` configured with `max_loaded_models: 1`. Removal scope and the follow-up bookkeeping are in [`docs/plans/2026-10-17-002-refactor-drop-lemonade-backend-plan.md`](../plans/2026-10-17-002-refactor-drop-lemonade-backend-plan.md).

Two generic-backend gaps came out of this, both small: the proxy has no route for `/v1/audio/transcriptions`, and the usage tap does not read FLM's speed fields. See § Generic backend gaps.

## A. `lemond` 2026.42.1, probed directly

`GET /live` 200. `GET /health`, `GET /api/v1/health`, `GET /api/v1/models`, `GET /api/v1/system-info`, `POST /api/v1/load`, `POST /api/v1/unload`, and the OpenAI surface `/v1/models`, `/v1/chat/completions` all answered as documented. Notes that matter to us:

- `POST /api/v1/models/load` is 404. The path is `/api/v1/load`, and the body key is `model_name`, not `model`. LlamaStash sends the right shape.
- `POST /api/v1/load {"model_name":"Qwen3-0.6B-GGUF","ctx_size":8192}` returned `{"status":"success"}` in 1.3 s and `health` then showed the model with `launch_command` from `~/.cache/lemonade/bin/llamacpp/rocm-stable/llama-b11394/llama-server ... --ctx-size 8192 --port 8001`. That llama-server child is LlamaStash-visible as an `external` launch row, not as a managed one.
- Registry rows carry `id`, `labels`, `size`, `recipe`, `downloaded`, `context_length`, `max_context_window`, `update_available`. The `ModelEntry` fields LlamaStash parses (`id`, `labels`, `size`, `recipe`) are all present.
- An unknown name on `/v1/chat/completions` gets `model_not_found` plus a hint: `GET /api/v1/models?show_all=true` lists all 179 registry models. Recipe counts on this box: llamacpp 92, llamacpp-hrx 28, thenoise 12, sd-cpp 12, vllm 11, whispercpp 6, collection.omni 4, openmoss 4, moonshine 3, onnxruntime 2, and one each of acestep, ds4, trellis, thinksound, kokoro. **`flm`: 0.**
- Registry downloads land in the shared HF hub (`/mnt/work/huggingface/hub/models--unsloth--Qwen3-0.6B-GGUF`), which is the same tree LlamaStash scans. So the same weights produce two catalog rows, one registry and one GGUF.
- `/api/v1/system-info` `devices` reports the gfx1151 GPU (124 GiB virtual, 0.5 GiB carved) and the XDNA2 NPU with `power_mode` and utilization. `recipes` gives the HTTP equivalent of `lemonade recipes`, and pins `flm` at **v1.0.7** while upstream is v1.0.9.
- `lemond` takes about **6 s** to exit after SIGTERM with nothing loaded (measured, single process). Its llama-server children go with it.

## B. FastFlowLM as an engine

`flm serve [tag] --host H -p P [--ctx-len N] [--pmode performance] [--cors 0] [--asr 1] [--embed 1]`. Default port 52625 (`flm port`), the tag is optional for an ASR-only or embedding-only server, and the model loads before the socket binds. The `flm` in the portable tarball is a shell wrapper that sets `LD_LIBRARY_PATH` and `exec`s `flm-real`, so it stays in the foreground and is safe to supervise.

Endpoints, all verified against the running server:

| Path | Result |
|---|---|
| `/v1/models` | 200, but it lists FLM's whole model list (36 tags), not what is downloaded |
| `/v1/chat/completions` | 200, streaming works, and a request for another tag switches models (downloading if needed). A bad tag gives `{"error":"Failed to load nope:1b model!"}` |
| `/v1/completions` | 200 |
| `/v1/audio/transcriptions` | 200 with `--asr 1`: multipart `file=@test.wav` gave `{"model":"","text":" Thank you."}` for a 2 s sine (that is the expected whisper hallucination on non-speech). `whisper-v3:turbo` runs on the NPU |
| `/v1/version` | 200 `{"version":"1.0.9"}`, and it is the only cheap readiness probe: there is no `/health`, no `/healthz`, no `/props` |
| `/api/tags`, `/api/chat`, `/api/generate`, `/api/ps`, `/api/pull`, `/api/show`, `/api/version`, `/api/npu/status` | FLM also speaks the Ollama shapes on the same port; `/api/npu/status` gives `{"npu_available":true,...}` |

Throughputs for `qwen3:0.6b` on the NPU came back inside `usage`: `decoding_speed_tps` 82.9, `prefill_speed_tps` 21.1, plus `kv_token_occupancy_rate_percentage`. One NPU model per type (asr, llm, embedding) per process, and separate processes on separate ports per type is the documented pattern; two `flm` processes did run side by side here.

## C. whisper.cpp as an engine

`whisper-server` v1.8.4 (the CPU build Lemonade installs). **It has no OpenAI surface at all**: `/v1/audio/transcriptions`, `/v1/audio/...`, `/v1/models` and `/props` are 404 from its static handler. What it does have:

- `GET /health` 200 `{"status":"ok"}`, a good readiness path.
- `POST /inference`, multipart `file=@x.wav`, returned 200 `{"text":" [Bell]\n"}` on the same 2 s sine.
- `--request-path /v1/audio` prefixes all of its routes, so `/v1/audio/health` and `POST /v1/audio/inference` worked with that flag. It renames, it does not translate: the request and response shapes stay whisper.cpp's own.
- The binary needs `LD_LIBRARY_PATH` pointed at its own directory (bundled `libwhisper.so.1`).
- Its weights are `ggml-*.bin` in the HF hub, which LlamaStash's scanner does not list, so a whisper entry can only carry its own model path.

## D. All four entries under the generic backend

Entries used (`lemonade` disabled so the generic path stood alone):

```yaml
backend:
  generic:
    servers:
      - name: lemonade
        binary: /usr/bin/lemond
        args: ["--port", "{port}", "--host", "{host}", "--no-broadcast", "--log-file", "disabled"]
        ready: /live
        ready_timeout_secs: 90
        stop_grace_secs: 20
        memory_gib: 1

      - name: "qwen3:0.6b"          # the row id must be the engine's own tag, see below
        binary: ~/.local/opt/fastflowlm/flm
        args: [serve, "{name}", "--host", "{host}", "-p", "{port}", "--cors", "0"]
        ready: /v1/version
        memory_gib: 2
        modes: [chat]
        knobs:
          - {flag: --ctx-len, id: ctx-len, ctx: true, default: "32768"}
          - {flag: --pmode, default: performance}

      - name: whisper-npu           # FLM ASR-only server, NPU
        binary: ~/.local/opt/fastflowlm/flm
        args: [serve, "--asr", "1", "--host", "{host}", "-p", "{port}", "--cors", "0"]
        ready: /v1/version
        memory_gib: 2

      - name: whisper-tiny          # whisper.cpp on CPU
        binary: ~/.cache/lemonade/bin/whispercpp/cpu/whisper-server
        args: ["-m", "/mnt/work/huggingface/hub/models--ggerganov--whisper.cpp/snapshots/<rev>/ggml-tiny.bin",
               "--host", "{host}", "--port", "{port}"]
        env:
          LD_LIBRARY_PATH: /home/deepu/.cache/lemonade/bin/whispercpp/cpu
        ready: /health
        memory_gib: 1

proxy:
  aliases:
    Qwen3-0.6B-GGUF: lemonade
```

Results:

| # | Check | Result |
|---|---|---|
| 1 | Rows publish for all four entries | pass. An entry without `model` shows `?` for Arch/Params/Quant/Ctx unless `arch`/`params`/`quant`/`ctx` are set, and Size comes from `memory_gib` |
| 2 | `start` for each entry | pass for all four. FLM loaded its model in about 1.9 s inside the readiness wait |
| 3 | Chat through the proxy, FLM row | pass, 0.66 s round trip. The row id has to equal the engine's own tag, because the body is forwarded unchanged: with the entry named `flm-qwen3-06b` FLM answered `{"error":"Failed to load flm-qwen3-06b model!"}`, and renaming it to `qwen3:0.6b` fixed it. `:` and `.` are legal in an entry name, so `name` + `{name}` in argv keeps the tag written once |
| 4 | Chat through the proxy, Lemonade row | fails bare (`model_not_found` at lemond, since `lemonade` is not a Lemonade model), passes with `proxy.aliases: Qwen3-0.6B-GGUF: lemonade`: the alias string survives into the forwarded body, lemond loads on demand, 1.9 s cold including load, 103 t/s. Auto-start through the alias is recorded as `alias, auto-start` in `requests` |
| 5 | `--ctx` reaches the engine | pass for FLM through the `ctx: true` knob: the child argv carried `--ctx-len 8192`. No equivalent exists for `lemond`, whose context is a load-body field |
| 6 | Two launches of the Lemonade entry | pass (`lemonade@second`), two umbrellas at 39.4 MB and 37.1 MB RSS. Nothing is shared, so per-model launches of a multiplexer engine multiply the engine |
| 7 | `stop lemonade` | 4.7 s, `lemond` exited inside the declared `stop_grace_secs: 20`. Its llama-server child went with it, no orphan |
| 8 | `env` values with a leading `~` | fail. `LD_LIBRARY_PATH: ~/.cache/...` gave `exit code 127`, `libwhisper.so.1: cannot open shared object file`; the absolute path worked. Only `binary` expands `~/` (`GenericServer::binary_path`) |
| 9 | `/v1/audio/transcriptions` through the proxy | **404 `no such route`**. `GENERATION_ROUTES` in [`src/proxy/router.rs`](../../src/proxy/router.rs) lists 8 paths and audio is not one of them, so an ASR engine can be launched and supervised but not served through the proxy. Direct to the launch port it works |
| 10 | Server-reported speed for FLM | missing. `requests` showed `~18.1` tok/s (the estimate) for the FLM row against `138.1` for the Lemonade row. The usage tap reads `timings.predicted_per_second` and `metrics.tokens_per_second`; FLM reports `usage.decoding_speed_tps` |

## E. The dedicated Lemonade backend, baselined on 2026.42.1

Working: the umbrella is supervised and reported (`backend lemonade: installed [cpu|rocm] umbrella=running`), and per-model lifecycle against it is real and fast. `stop L1` unloaded one model in 0.12 s and left the umbrella ready.

Broken, each reproduced:

| # | Symptom | Detail |
|---|---|---|
| 1 | **No Lemonade rows for the first minutes after a daemon start** | The boot scan runs before the umbrella is ready, and `lemonade discovery: list_models failed (umbrella down?)` is logged once at debug. The 4 registry rows appeared only at the next rescan, either a watcher event or the 5-minute periodic backstop (`discovery: periodic rescan tick` at 09:24:08 for a 09:19:08 start). Nothing re-scans when the umbrella becomes ready, so `list` shows no Lemonade models for up to 5 minutes |
| 2 | **Registry rows are not addressable** | `start lemonade://Qwen3-0.6B-GGUF` is refused: `no model matches lemonade://Qwen3-0.6B-GGUF (20 known)`. By name it collides with the same-repo local GGUF: `Qwen3-0.6B-GGUF matches 2 models: Qwen3-0.6B-Q4_0.gguf, Qwen3-0.6B-Q4_0.gguf`, and the message prints the same display name twice. Since Lemonade and LlamaStash share the HF hub, that collision is the normal case for any model pulled by both |
| 3 | **A failed load still reports success** | Forcing a disk GGUF onto Lemonade (`start Qwen3-0.6B-Q4_0.gguf --backend lemonade`) printed `✓ started ... launch_id=L8` while the daemon logged `lemonade: preload of <path> failed: lemond returned HTTP 404`, `lemond /api/v1/health` showed nothing loaded, and the launch then vanished from `status`. `lemond` loads registry names, not paths |
| 4 | **stop then start inside ~6 s fails** | `start` after `stop --all` gave `lemonade umbrella failed to start: 127.0.0.1:13305 is already in use; cannot supervise a managed umbrella on it` while nothing was listening. `lemond` needs about 6 s to exit and `EXITED_PORT_WAIT` is 5 s, and the fresh-start path refuses outright on `UmbrellaPortState::Remnants` instead of waiting. It cleared on its own within about a minute (the kernel fin-timeout the code comment already names) |

## Generic backend gaps to close

1. **No audio route.** `/v1/audio/transcriptions` is not forwarded, so FLM's NPU Whisper and any whisper.cpp entry are launch-managed only: readiness, port, logs and stop work, and the client must talk to the launch port. Adding the route plus a `transcription` mode (there is `chat`, `embedding`, `rerank` today, and Lemonade's `transcription` label already maps to `ModeHint::Unknown`) is what would make an ASR engine a first-class row.
2. **FLM speed.** Reading `usage.decoding_speed_tps` as another `*_tps` source in [`src/proxy/usage_tap.rs`](../../src/proxy/usage_tap.rs) would give FLM rows a reported decode rate instead of the estimate.
3. **`env` does not expand `~`.** Either expand it like `binary` or say plainly in `docs/usage.md` that env values are literal.
