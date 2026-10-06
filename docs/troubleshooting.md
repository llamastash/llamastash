# Troubleshooting

Quick reference for the common ways LlamaStash can refuse to do what you want, with concrete remediation steps.

## `llama-server` not on `PATH`

**Symptom:** `llamastash start <ref>` exits `70` (`BINARY_NOT_FOUND`); the message names both the `--llama-server` flag and the `LLAMASTASH_LLAMA_SERVER` env var.

**Fix:** install llama.cpp's server build, then either put it on your `PATH`, set `LLAMASTASH_LLAMA_SERVER=/abs/path/to/llama-server`, or pass `--llama-server /abs/path/to/llama-server`. If `which llama-server` returns multiple hits (e.g. `llama-server-cuda` + `llama-server`), LlamaStash logs them and uses the first; pin a specific one via flag/env to avoid the surprise.

Both shapes of the binary work. llama.cpp's own installer (`llama.app`) ships a single `llama` / `llama.exe` that serves behind a subcommand instead of a standalone `llama-server`; LlamaStash searches `$PATH` for `llama-server` first, falls back to `llama`, and adds the `serve` subcommand when it launches the unified one. Flag and env var take either.

## GPU not detected

**Symptom:** `llamastash status --json | jq .gpu` returns `"CpuOnly"` even though you have a GPU. Memory estimates show only RAM, not VRAM.

**Fixes per backend:**

- **NVIDIA:** confirm `nvidia-smi` is on `PATH` and answers — LlamaStash probes `nvidia-smi --query-gpu=… --format=csv` (a subprocess, not the NVML library), so a working driver install is enough. Run `nvidia-smi` manually; if it fails or is missing, the daemon falls back to CPU-only. On coherent-UMA parts (GB10 / DGX Spark, Jetson) the memory columns read `[N/A]` — that's expected, and the GPU is sized from the system pool instead.
- **AMD:** on Linux, LlamaStash reads `/sys/class/drm/card*/device/mem_info_*` (a stable kernel interface) and falls back to `rocm-smi --showmeminfo vram gtt --json`. Make sure the `amdgpu` driver is bound; if sysfs is unreadable, keep `rocm-smi` on `PATH`. `doctor` surfaces a probe failure rather than silently degrading to CPU-only.
- **Apple Silicon:** LlamaStash parses `system_profiler SPDisplaysDataType -json`. If this is empty, the macOS install is unusual — try the command manually and file an issue with the output.
- **Intel macOS:** there is no Metal support to detect; LlamaStash falls back to CPU-only and that's correct.

## `doctor` reports `memory_drift` or `gtt_hint`

**`memory_drift`:** the detected GPU memory pool changed size since the last baseline — growth is informational (e.g. you raised the GTT ceiling), shrinkage is a warning (a model that used to fit may not). `doctor` re-stamps the baseline after the finding fires, so it is one-shot; the previous size stays in the finding text. No action required.

**`gtt_hint` (Linux AMD APUs):** the GPU's shared GTT pool is sized at the amdgpu default (~half of system RAM), so a large model may spill to CPU sooner than the hardware can actually hold. To let `llama-server` use more system RAM as GPU memory, raise the GTT ceiling via kernel parameters — e.g. `amdgpu.gttsize=<MiB> ttm.pages_limit=<pages>`. Do **not** set `amd_iommu=off`; it breaks Thunderbolt/USB4 docks and is not needed.

## Stale daemon handshake file

**Symptom:** `llamastash list` exits `65` (`DaemonUnreachable`) even though `daemon.pid` is present and locked. The recorded daemon is dead but the handshake file (`runtime.json`) didn't get cleaned up.

**Fix:** `daemon stop --force` falls back to a PID-targeted graceful-then-kill that also clears the handshake. The reverse state — `runtime.json` present, nothing holding the lock — is cleared on its own: `daemon stop` and `daemon restart` find no lock holder, delete the handshake, and report `daemon: not running`. If neither is reachable, remove the handshake + lockfile manually:

```bash
state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/llamastash"
rm -- "$state_dir/runtime.json" "$state_dir/daemon.pid"
llamastash daemon start
```

State-dir paths per platform:

- Linux: `$XDG_STATE_HOME/llamastash` (default `~/.local/state/llamastash`)
- macOS: `~/Library/Application Support/llamastash`
- Windows: `%APPDATA%\llamastash\data` (i.e. `C:\Users\<you>\AppData\Roaming\llamastash\data`)

## Stale PID lockfile after a crash

**Symptom:** `llamastash daemon start` reports `AlreadyRunning(pid)` but `ps -p <pid>` shows nothing.

**Fix:** llamastash validates the lockfile against `kill -0 pid` and clears stale entries. If it's still wedged, delete it:

```bash
rm -- "$XDG_STATE_HOME/llamastash/daemon.pid"
```

The state directory defaults to `~/.local/state/llamastash/` on Linux, `~/Library/Application Support/llamastash/` on macOS, and `%APPDATA%\llamastash\data\` on Windows.

## Port range exhausted

**Symptom:** `llamastash start ...` exits `67` with `port allocation failed: NoFreePort`.

**Fix:** widen the range in your config or pin a specific port:

```yaml
daemon:
  port_range:
    start: 41100
    end: 41500
```

```bash
llamastash start <ref> --port 41250
```

## Daemon refuses to start: port range inverted

**Symptom:** `llamastash daemon start` exits without starting, saying `daemon.port_range` is inverted or that `start` is 0.

**Cause:** `daemon.port_range.start` is above `end` (or is `0`), so no port can ever be allocated and every launch would fail. The daemon refuses at startup rather than coming up healthy and failing on your first `start`.

**Fix:** swap the two values, or drop back to the factory range:

```yaml
daemon:
  port_range:
    start: 41100
    end: 41300
```

## Wayland clipboard yank does nothing

**Symptom:** `y` / `Y` / `p` in the TUI flashes a toast but the system clipboard stays empty (Wayland sessions are the usual culprit).

**Fix:** LlamaStash uses `arboard` first, then falls back to `wl-copy`, `xclip`, and `xsel` (in that order). Install at least one fallback:

```bash
# Wayland
sudo apt install wl-clipboard
# X11
sudo apt install xclip
```

The toast prints the URL inline when every backend fails, so you can still paste manually.

## Daemon disconnect during `logs --follow`

**Symptom:** `LlamaStash logs <id> -f` exits `65` mid-stream.

**Fix:** the daemon was shut down or crashed. Restart it with `llamastash daemon start`. Running children survive daemon exit; you can re-attach to the same launch id once the daemon is back (orphan re-adoption verifies PID + port + `/v1/models` match).

## Daemon shuts down on its own when idle

**Symptom:** the daemon exits after you stop every model and close the TUI/CLI.

**Cause:** `daemon.idle_timeout_secs` is set above `0`. The daemon shuts itself down once nothing has needed it for that long — no live model, and no CLI, TUI, or proxy traffic. A managed multiplexer's shared umbrella doesn't count as a running model, so a host running one still idles out.

**Fix:** set `daemon.idle_timeout_secs: 0` to disable the timer.

The CLI and TUI respawn the daemon on the next attach, so for them this costs a restart, not your state. The proxy is the one thing that does not come back on its own: an agent pointed at `http://127.0.0.1:11435` gets a refused connection until something re-attaches. Leave the timer off if you keep agents pointed at the proxy across long idle gaps.

## GPU stays `cpu_only` after the driver loads

**Symptom:** you started the daemon before the GPU driver was ready (fresh boot, a container that got its device mapped later), and `status.gpu` still reports `cpu_only` long after.

**Cause:** `gpu.reprobe_interval_secs: 0` disables the periodic full re-probe, so the boot reading is the only one the daemon ever takes.

**Fix:** set it back to `60` (the default), or restart the daemon once the driver is up.

## "model already running" surprise

**Symptom:** the TUI launch picker shows a "model is already running on port N" line.

**This is the design.** v1 has no duplicate-prevention; a second launch creates a new instance on a different port. Stop the original first if you don't want two instances. The `--port` flag pins a specific port if you want to reuse one explicitly.

## `state.json` corruption after a SIGKILL

**Symptom:** daemon refuses to start; log says state-store parse failed.

**Fix:** llamastash quarantines a corrupt `state.json` as `state.json.broken-<ts>` and starts with defaults. You'll lose favorites, last-params, and the running snapshot for this restart — but the daemon will come up (named presets live in `config.yaml`, so they're unaffected). If you have a recent backup of `state.json`, restore it and try again.

## Proxy port already in use (`:11434`)

**Symptom:** `llamastash status --json | jq .proxy` shows `"status": "port_in_use"` (and `bind_error` is `null`). Agents pointed at `http://127.0.0.1:11434/v1` get connection-refused or hit Ollama instead of llamastash.

**This is the design.** The proxy refuses to auto-roam to a free port — the `:11434` default exists so OpenAI-client wrappers that hard-code Ollama's well-known port discover llamastash without reconfiguration; silently moving would break that contract. The most common cause is Ollama running on the same box.

**Fix (pick one):**

- Stop the conflicting listener and restart the daemon:

  ```bash
  lsof -i :11434                  # identify the owner
  systemctl --user stop ollama    # if Ollama is the culprit
  llamastash daemon stop && llamastash daemon start
  ```

- Move llamastash off the default port — CLI flag (one-shot) or config (persistent):

  ```bash
  llamastash daemon start --proxy-port 11500
  ```

  ```yaml
  proxy:
    port: 11500
  ```

  Agents then point at `http://127.0.0.1:11500/v1`. `--proxy-port 0` binds an ephemeral port; the actual address is reported via `llamastash status --json | jq .proxy.listen`.

## Agent reports "could not reach API" / connection refused on `:11434`

**Symptom:** an OpenAI-compatible client (OpenCode, Pi, etc.) configured against `http://127.0.0.1:11434/v1` reports connection-refused; `curl http://127.0.0.1:11434/v1/models` returns `curl: (7) Failed to connect`.

**Fix:** the proxy listener is owned by the daemon, so no daemon means no listener. Start it:

```bash
llamastash daemon start
llamastash status --json | jq .proxy
# expect: {"enabled": true, "listen": "127.0.0.1:11434",
#         "status": "listening", "bind_error": null}
```

If `status` is `"disabled"` instead of `"listening"`, your config has `proxy.enabled: false` — flip it back and restart the daemon. If `status` is `"port_in_use"`, see the previous section.

## Proxy returned a different model than I asked for

**Symptom:** an agent gets a plausible response but the answer style doesn't match the requested model; response headers carry `x-llamastash-served-by: <other-model>` and `x-llamastash-fallback-reason: launch_failed` or `family_mismatch`.

**This is the family-MRU fallback.** When the requested model's auto-start fails and another model is already `Ready`, the proxy substitutes it and stamps both headers so the substitution is auditable. `launch_failed` means an in-family pick (closest match was the same architecture); `family_mismatch` means cross-arch (no in-family Ready model existed).

**Fix:** look at the daemon log around the request timestamp for the underlying launch failure — usually a missing GGUF, `llama-server` ENOENT, port-range exhaustion, or a probe timeout. Start the intended model manually first to surface the real error:

```bash
llamastash start <model-name>
# or, for the full launch log:
llamastash logs <launch-id> -f
```

Once the underlying launch issue is fixed, the fallback path stops firing. To turn the fallback off entirely is tracked as a deferred decision in `TODO.md §R1` (`proxy.fallback: false`).

## Config rejected: unknown field ds4

**Symptom:** every command fails with ``backend: unknown field `ds4` `` and a note that the ds4 backend was removed.

**Cause:** ds4 was a dedicated backend up to 0.4.0. It now runs as a `backend.generic` entry, and the old `backend.ds4:` block no longer parses.

**Fix:** follow the migration steps in [Running ds4 as a generic server](usage.md#running-ds4-as-a-generic-server): replace the block with the generic entry, add `server: generic-ds4` to your ds4 presets, and restart the daemon.

## My DeepSeek-V4 model launched on llama.cpp, not ds4

**This is expected.** llama.cpp is the default server for every GGUF, and a generic entry never becomes the default. Pick ds4 per launch with `--server generic-ds4`, or pin it in a preset with `server: generic-ds4` and make that preset the model's `default:`. `llamastash list` shows `llamacpp|generic` on the rows the entry's `model:` matches; if a DeepSeek-V4 row shows only `llamacpp`, the glob doesn't match its name.

## DeepSeek-V4 fails on llama.cpp: `unknown model architecture: 'deepseek4'`

**Symptom:** a DeepSeek-V4 GGUF launched on llama.cpp dies at load with `error loading model: unknown model architecture: 'deepseek4'`. It fails in milliseconds, before any tensor loads.

**Cause:** your `llama-server` predates DeepSeek-V4 support. It landed in llama.cpp **b9840** ([ggml-org/llama.cpp#24162](https://github.com/ggml-org/llama.cpp/pull/24162), merged 2026-06-29); older builds don't know the `deepseek4` architecture and reject the file outright.

**Fix:** update `llama-server` to **b9840 or newer** (a GitHub release binary, `brew upgrade llama.cpp`, or a source build from that merge onward) and point `backend.llamacpp.servers` at it. Confirm the resolved binary and its build:

```bash
llamastash status --json | jq -r '.daemon.server_path'
"$(llamastash status --json | jq -r '.daemon.server_path')" --version   # want: version >= 9840
```

Or run the file on ds4 instead, through a [generic server entry](usage.md#running-ds4-as-a-generic-server). (On a b9840+ llama.cpp, Flash Attention is currently auto-disabled for the deepseek4 graph — the model still loads and runs.)

## A model shows `error: process exited unexpectedly`

**Symptom:** a model that was `ready` now shows `error` in `status` or the TUI, with `process exited unexpectedly (killed by signal 9)` or `(exit code N)` and the last lines of its log.

**Cause:** the model process died without being asked to stop. `killed by signal 9` is SIGKILL, which is what the kernel's OOM killer sends; check `journalctl -k | grep -i oom`. An exit code is the engine's own failure; the log lines show why.

**Fix:** the row stays until you stop it: `llamastash stop <model>` (or the stop key in the TUI, `Ctrl+s` by default). A new request to the proxy starts a fresh copy either way.

## `launch refused: needs N GiB but only M is free`

**Symptom:** `start` exits before spawning anything, naming the projected demand and the effective free memory.

**Fix:** free memory first — stop a resident model (`llamastash stop <ref>`), pin a smaller `--ctx`, or lower `backend.llamacpp.fit_ctx_floor`. The projection is weights + KV cache at the effective context + the backend's overhead band, so a smaller window is usually the cheapest lever.

If you know the projection is wrong for your setup, `llamastash start <model> --force` launches anyway and prints the refusal as a warning (also carried in `--json` as `warnings`). There is no safety net behind it: if the projection was right, the host runs out of memory and the kernel's OOM killer picks a victim, which may be a different process entirely.

Weights the engine streams from the mapping rather than holding resident are already subtracted — a 103.7 GiB `Qwen3.8-Flash-Next` is priced at about 77 GiB, because its 26.8 GiB per-layer embedding table never becomes resident. Pinning `-- --lazy-mode off` turns that streaming off, and the gate then prices the whole file.

## A reloaded model processes the whole prompt with `slot_save` on

**Symptom:** `backend.llamacpp.slot_save.enabled: true`, a model was unloaded while idle, and the next request still takes as long as a cold prompt.

**Cause:** look for `slot save:` lines in `<cache dir>/logs/llamastash.log`.

- `a restored slot is not reused`: the model is hybrid (`qwen35`: Qwen3.5, Qwen3.8) or sliding-window (`gemma4`). llama-server restores the file and then processes the prompt anyway ([llama.cpp#28194](https://github.com/ggml-org/llama.cpp/issues/28194)), so nothing is saved for it.
- `is over the size cap` or `not enough free disk`: raise `slot_save.max_gib` or free space under the cache dir.
- No line at eviction time: the slot held fewer than `slot_save.min_tokens` tokens, or the model was stopped by hand (only the idle sweep and make-room save).
- A `saved` line but no `restored` line: the model came back with different launch settings (another preset, another `--ctx`), which is a different file.

Also check that the conversation was the last one the model served. llama-server keeps only the most recent conversations in slots.

## ds4 model out-of-memories at load

**Symptom:** a DeepSeek-V4 launch on ds4 dies allocating memory. These GGUFs are 81-300+ GB; the practical floor is about 128 GB on CUDA/ROCm and 96 GB on Metal.

**Fix:** set `ssd-streaming: true` in the preset so `ds4-server` streams weights from disk. LlamaStash's admission check still sizes the launch from the GGUF, so a launch that doesn't fit is refused before spawn; add `start --force` for a streaming launch. Don't combine it with `mtp-model`: `ds4-server` refuses streaming with a draft head, after the full load. (Verified: an 86 GB Flash IQ2_XXS reached Ready with streaming on a 121 GB box, and ran out of memory without it.)

## ds4 split PRO half-file fails to load

**Symptom:** launching `DeepSeek-V4-Pro-Q4K-Layers00-30.gguf` or `*-Layers-31-output.gguf` fails in the engine.

**Cause:** those two files are one split PRO model that ds4 runs only in distributed mode, which LlamaStash doesn't support. Neither half loads on its own in any engine. Use a single-file DeepSeek-V4 GGUF (the `*-Pro-IQ2XXS-*-Instruct` and Flash quants are single-file).

## ds4-server's `/v1/models` lists two models when one is running

**Symptom:** `curl http://127.0.0.1:<port>/v1/models` against a ds4 launch returns **two** entries, `deepseek-v4-flash` and `deepseek-v4-pro`, even though only one model is loaded.

**This is ds4-server behavior, not a LlamaStash bug.** ds4-server advertises a fixed two-entry menu on `/v1/models` regardless of which GGUF is resident. `/v1/chat/completions` serves the loaded model and echoes back the `model` name you sent. Through the LlamaStash proxy you never see the menu: the proxy publishes your own catalog on its `/v1/models` and forwards your request model, which ds4 echoes back.

## Generic server fails an embeddings or rerank request

**Symptom:** a `POST /v1/embeddings` or `/v1/rerank` request for a model on a generic entry (ds4-server, Halogen, gufo) fails with the engine's own error, often after starting it.

**Fix:** most of these servers answer chat only. Add `modes: [chat]` to the entry in `config.yaml`; the proxy then refuses those requests with `400 unsupported_endpoint` without starting the model. Send embeddings to a model that serves them, such as a GGUF embedder on llama.cpp.

## `--mtp on` didn't enable MTP (or a launch failed with "context type MTP requested")

**Symptom:** you passed `--mtp on` but the launch warns "MTP forced on … but this model is not MTP-capable — skipping", or a hand-written `--spec-type draft-mtp` in the `-- <extras>` tail crashes the launch with `llama_init_from_model: context type MTP requested but model doesn't contain MTP layers`.

**This is the gate working (or the lack of one).** MTP only works on a model that ships a draft head — an embedded one (`{arch}.nextn_predict_layers > 0`) or a separate `mtp-*.gguf` sibling. llamastash checks first: `--mtp on` on a non-capable model warns and skips rather than emitting the flag and bricking the launch. If you bypass llamastash and pass `--spec-type draft-mtp` yourself in `extras`, llama-server has no such guard and fails to load. Fix: use `--mtp auto` (the default) so llamastash only enables MTP when the model can actually do it, and let `pull` fetch the `mtp-*.gguf` head for separate-head models (`--all-companions` if the default one-per-kind missed it).

## `state.json` quarantined after downgrading LlamaStash

**Symptom:** after running a newer LlamaStash and then reverting to an older binary, the daemon quarantines `state.json` as `state.json.broken-<ts>` and boots with defaults.

**This is expected pre-release.** A newer binary can write state fields or backend values an older binary's schema doesn't understand, so the older one rejects the file rather than misreading it. LlamaStash keeps no backward-compatibility guarantees before the first stable release. Favorites / last-params / the running snapshot reset for that boot; named presets live in `config.yaml` and survive. Don't hop between old and new binaries against one state dir.

## vLLM shows as not installed even though it runs

**Symptom:** `doctor` / the Daemon pane reports vLLM as unavailable, but `vllm serve` works from your shell.

**Detection is a filesystem check, never an exec.** vLLM builds its argument parser through a device probe, so even `vllm --version` can fail with `RuntimeError: Failed to infer device type` on a host with no usable accelerator — running it to detect it would be unreliable. LlamaStash only checks that a `vllm` launcher exists on `PATH` or at `backend.vllm.servers[].binary`. If yours lives in a venv that is not on the daemon's `PATH`, point `backend.vllm.servers` at the absolute path. A wrapper script works for the same reason.

## vLLM launch is OOM-killed on an APU

**Symptom:** the engine starts, then dies during KV-cache build; `dmesg` shows the OOM killer.

**On a unified-memory host GPU memory *is* system RAM,** and vLLM sizes its KV cache against the whole pool rather than the model. The launcher therefore sets `--kv-cache-memory-bytes` from live free memory when the host is unified and you set neither `kv_cache_memory_bytes` nor `gpu_memory_utilization`, keeping a reserve that covers the engine's own overhead (measured 5.4–6.7 GiB on a DGX Spark) as well as the OS, and passes a matching `--gpu-memory-utilization` so vLLM's startup check does not refuse the capped launch on a host with other tenants. If you set either knob yourself the auto-cap steps aside and the figure is yours to get right — `gpu_memory_utilization` in particular is a fraction *of the pool*, so even `0.15` reserved 15.1 GiB on a 0.5B model. Prefer the absolute `kv_cache_memory_bytes` cap on these hosts: your value is passed through untouched, and the launcher still derives the companion `--gpu-memory-utilization` from it so the cap is not refused by vLLM's startup check on a host with other tenants.

If the daemon has no memory reading yet (right after a restart), it treats the host as unified rather than leaving the launch uncapped. In that window there is no pool total to size the companion fraction against, so vLLM's own `0.92` startup check still applies and a first launch beside a busy tenant can be refused; start it again once the host has been sampled.

## Stopping a containerized vLLM leaves it running

**Symptom:** `stop` returns, but the vLLM process and its port are still held.

**The SIGKILL escalation does not cross the container boundary.** Where vLLM ships only as a container, `backend.vllm.servers[].binary` points at a thin host wrapper script; LlamaStash signals the wrapper, not the engine inside. A graceful stop works when the wrapper forwards signals — make sure it `exec`s the container runtime rather than backgrounding it, so the runtime inherits the wrapper's PID. Otherwise stop the container yourself. The native wheel has no such gap and is the documented default; see [vLLM setup](vllm-setup.md).

## vLLM sits in `starting` for minutes

**Symptom:** the row stays `starting` far longer than a llama.cpp model of the same size.

**Expected.** Weight load is quick, but engine init (memory profiling plus KV-cache build) took 10-27 s on a 0.5B and runs considerably longer on real models; the readiness deadline scales with model size. Setting `kv_cache_memory_bytes` skips vLLM's memory-profiling pass, which is the slowest part. If it never reaches Ready, check `logs` for the engine's own error — a repo whose architecture vLLM cannot serve fails here, since eligibility only checks that a repo is safetensors-and-no-GGUF.

## SGLang shows as not installed even though it runs

Same cause and fix as vLLM above: detection is a filesystem check for a `sglang` launcher on `PATH` or at `backend.sglang.servers[].binary`, never an exec. Point the config at the absolute path of the console script in its venv, or at a wrapper script.

## SGLang launch is refused with "cannot size the KV pool"

**Symptom:** `start` returns before spawn with a message naming `max-total-tokens`.

**The guard could not read the model's attention geometry.** SGLang has no byte-level KV cap, so on a unified-memory host the launcher converts the byte budget into `--max-total-tokens` using `num_hidden_layers`, `num_key_value_heads` (or `num_attention_heads`) and `head_dim` (or `hidden_size`) from the repo's `config.json`. A config missing those fields — or nested under a key other than `text_config` — cannot be priced, and guessing in the wrong direction is the OOM the guard exists to prevent. Set `max_total_tokens` (or `mem_fraction_static`) yourself for that repo; a preset is the natural place.

## SGLang rejects long requests after a launch

**Symptom:** the row is `ready`, the launch log carries a warning that the KV pool is below the requested context, and requests near `--ctx` fail.

**The token cap came out smaller than `--ctx`.** On a tight host the shared budget divided by a large model's per-token cost can be fewer tokens than the requested window; the launch proceeds with the warning rather than refusing. Raise `max_total_tokens` if the host can take it, or lower `--ctx`.

## Generic server returns 404 `model_not_found`

Some engines (gufo) answer only to the name they were started with. Pass `{name}` to the engine's served-name flag (`--served-model-name "{name}"`), and send the model's full id: a partial name the proxy resolves still reaches the engine as you typed it, since the proxy does not rewrite `model`.

## Generic entry edits don't show up

Entries and their knobs are read when a process starts. Run `llamastash daemon restart` and reopen the TUI after editing `backend.generic`. A daemon and a TUI started on different versions of the file disagree about which knobs exist.

## Generic launch leaves a container or process behind

llamastash sends one SIGTERM to the launch's process group and SIGKILLs after `stop_grace_secs`. A wrapper that runs `docker run` in the foreground and gets SIGKILLed leaves the container running. Use the wrapper shape in `docs/usage.md` § Generic backend (`docker run -d`, a `trap` that runs `docker stop`, `docker wait` in the foreground), set `stop_grace_secs` above your `docker stop -t`, and remove leftovers at wrapper start (`docker rm -f "llamastash-<engine>-$port"`). There is no orphan adoption for generic launches after a daemon crash.

## Generic server is reachable from the LAN

llamastash can't see what a foreign binary binds. Pass `{host}` (always `127.0.0.1`) to the engine's bind flag. For a container whose server binds `0.0.0.0` inside (Halogen 0.14.0's `all` mode does), publish the port on loopback only (`-p 127.0.0.1:$port:<inner>`) instead of `--network host`. Check with `ss -ltn | grep <port>`.

## HuggingFace pull

`llamastash pull <owner/repo[:filename.gguf]>` downloads a GGUF into the HuggingFace cache layout the scanner already reads, so the model shows up in `list` / the TUI right after. The TUI's `d` HuggingFace dialog is the interactive face of the same worker. If a download stalls, check network / egress and that the repo + filename resolve on huggingface.co; a failed pull exits `69` (`PULL_FAILED`). The per-file cap is 512 GiB (raised for the single-file DeepSeek-V4 GGUFs).
