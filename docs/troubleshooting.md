# Troubleshooting

Quick reference for the common ways LlamaStash can refuse to do what you want, with concrete remediation steps.

## `llama-server` not on `PATH`

**Symptom:** `llamastash start <ref>` exits `70` (`BINARY_NOT_FOUND`); the message names both the `--llama-server` flag and the `LLAMASTASH_LLAMA_SERVER` env var.

**Fix:** install llama.cpp's server build, then either put it on your `PATH`, set `LLAMASTASH_LLAMA_SERVER=/abs/path/to/llama-server`, or pass `--llama-server /abs/path/to/llama-server`. If `which llama-server` returns multiple hits (e.g. `llama-server-cuda` + `llama-server`), LlamaStash logs them and uses the first; pin a specific one via flag/env to avoid the surprise.

## GPU not detected

**Symptom:** `llamastash status --json | jq .gpu` returns `"CpuOnly"` even though you have a GPU. Memory estimates show only RAM, not VRAM.

**Fixes per backend:**

- **NVIDIA:** confirm `nvidia-smi` works. LlamaStash uses `nvml-wrapper`; if NVML isn't installed (driver-only install), the daemon falls back to CPU-only. Install the NVML library that ships with your CUDA toolkit.
- **AMD:** on Linux, LlamaStash reads `/sys/class/drm/card*/device/mem_info_*` (a stable kernel interface) and falls back to `rocm-smi --showmeminfo vram gtt --json`. Make sure the `amdgpu` driver is bound; if sysfs is unreadable, keep `rocm-smi` on `PATH`. `doctor` surfaces a probe failure rather than silently degrading to CPU-only.
- **Apple Silicon:** LlamaStash parses `system_profiler SPDisplaysDataType -json`. If this is empty, the macOS install is unusual — try the command manually and file an issue with the output.
- **Intel macOS:** there is no Metal support to detect; LlamaStash falls back to CPU-only and that's correct.

## `doctor` reports `memory_drift` or `gtt_hint`

**`memory_drift`:** the detected GPU memory pool changed size since the last baseline — growth is informational (e.g. you raised the GTT ceiling), shrinkage is a warning (a model that used to fit may not). `doctor` re-stamps the baseline after the finding fires, so it is one-shot; the previous size stays in the finding text. No action required.

**`gtt_hint` (Linux AMD APUs):** the GPU's shared GTT pool is sized at the amdgpu default (~half of system RAM), so a large model may spill to CPU sooner than the hardware can actually hold. To let `llama-server` use more system RAM as GPU memory, raise the GTT ceiling via kernel parameters — e.g. `amdgpu.gttsize=<MiB> ttm.pages_limit=<pages>`. Do **not** set `amd_iommu=off`; it breaks Thunderbolt/USB4 docks and is not needed.

## Stale daemon handshake file

**Symptom:** `llamastash list` exits `65` (`DaemonUnreachable`) even though `daemon.pid` is present and locked. The recorded daemon is dead but the handshake file (`runtime.json`) didn't get cleaned up.

**Fix:** `daemon stop --force` falls back to a PID-targeted graceful-then-kill that also clears the handshake. If that's unreachable too, remove the handshake + lockfile manually:

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

**Cause:** `daemon.idle_timeout_secs` is set above `0`. The daemon then shuts itself down once it has had no running models and no attached client for that long. A managed multiplexer's shared umbrella process doesn't count as a running model, so a host running one still idles out.

**Fix:** set `daemon.idle_timeout_secs: 0` to disable the timer. Nothing is lost either way — the daemon respawns on demand the next time a client attaches.

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

## My DeepSeek-V4 model launched on llama.cpp, not ds4

**This is expected in several cases** — ds4 is preferred, not required, and a current llama.cpp (**b9840+**) runs DeepSeek-V4 too, so an auto launch never refuses. Walk the checklist:

- **ds4-server not found.** ds4 is default-on only when the binary resolves. Check `llamastash status --json | jq '.backends[] | select(.id=="ds4")'` — `installed: false` means no `ds4-server` on `PATH` and no valid `backend.ds4.servers`. See the [ds4 backend](usage.md#ds4-backend) setup.
- **ds4 force-disabled.** `backend.ds4.enabled: false` in config turns it off even when the binary is present.
- **The GGUF isn't ds4-compatible.** A generic third-party `deepseek4` quant (K-quants on attention tensors, Q6_K experts) fails ds4's quant contract and stays a llama.cpp model. `llamastash list --json | jq '.models[] | {name, backend}'` badges `ds4` only on files that would actually route there.
- **Embedding / rerank mode.** `--mode embedding` or `--mode rerank` routes a compatible model to llama.cpp — ds4 serves chat/completions only.

**Force it:** `llamastash start <model> --backend ds4` bypasses the predicate (ds4-server surfaces its own error if the file is a genuine mismatch).

## DeepSeek-V4 fails on llama.cpp: `unknown model architecture: 'deepseek4'`

**Symptom:** a DeepSeek-V4 GGUF routed to llama.cpp — no `ds4-server` installed, or you passed `--backend llamacpp` — dies at load with `error loading model: unknown model architecture: 'deepseek4'`. It fails in milliseconds, before any tensor loads.

**Cause:** your `llama-server` predates DeepSeek-V4 support. It landed in llama.cpp **b9840** ([ggml-org/llama.cpp#24162](https://github.com/ggml-org/llama.cpp/pull/24162), merged 2026-06-29); older builds don't know the `deepseek4` architecture and reject the file outright. This is the failure mode behind the "fall back to llama.cpp, never a refusal" caveat — the fallback only degrades gracefully on a b9840+ build.

**Fix:** update `llama-server` to **b9840 or newer** (a GitHub release binary, `brew upgrade llama.cpp`, or a source build from that merge onward) and point `backend.llamacpp.servers` at it. Confirm the resolved binary and its build:

```bash
llamastash status --json | jq -r '.daemon.server_path'
"$(llamastash status --json | jq -r '.daemon.server_path')" --version   # want: version >= 9840
```

Or install `ds4-server` so compatible GGUFs route to ds4 instead. (On a b9840+ llama.cpp, Flash Attention is currently auto-disabled for the deepseek4 graph — the model still loads and runs.)

## ds4 model out-of-memories at load

**Symptom:** a DeepSeek-V4 launch is admitted, then the backend dies allocating VRAM/RAM. These GGUFs are 81–300+ GB; the practical floor is ~128 GB (CUDA/ROCm) / ~96 GB (Metal).

**Fix:** set the **`ssd_streaming` native knob** to stream weights from disk instead of requiring full residency. This is the one launch where the pre-spawn admission gate is deliberately skipped. Two caveats:

- The bypass keys on the **native knob only**. An extras-spelled `-- --ssd-streaming` reaches ds4-server but still hits LlamaStash's admission gate first, so the launch can be refused before spawn. Use the native knob (launch picker / preset), not the extras tail.
- Every deepseek4 launch also prints **"KV demand not modeled for deepseek4"** — the admission estimate omits the KV term for this arch, so it can admit a launch that then OOMs at a large context. Watch your memory headroom and lower `--ctx` if a load stalls.

(Verified: an 86 GB Flash IQ2XXS reached Ready via `ssd_streaming` on a 121 GB box, and OOMed without it.)

## ds4 split PRO half-file refused

**Symptom:** launching `DeepSeek-V4-Pro-Q4K-Layers00-30.gguf` or `…-Layers-31-output.gguf` is refused before spawn with "ds4 distributed mode unsupported".

**This is the design.** Those two files are one split PRO model that ds4 runs only in distributed mode, which LlamaStash does not support — each half is unloadable on its own by either engine. Use a **single-file** DeepSeek-V4 GGUF instead (the `…-Pro-IQ2XXS-…-Instruct` and Flash quants are single-file). `--backend ds4` bypasses the guard if you want ds4-server to surface its own error.

## ds4 `/v1/models` lists two models when one is running

**Symptom:** `curl http://127.0.0.1:<port>/v1/models` against a ds4 backend returns **two** entries — `deepseek-v4-flash` and `deepseek-v4-pro` — even though only one model is loaded.

**This is ds4-server behavior, not a LlamaStash bug.** ds4-server advertises a fixed two-entry menu on `/v1/models` regardless of which GGUF is resident; it is not a report of what is loaded. `/v1/chat/completions` serves the one loaded model and **echoes back the `model` name you sent** (no fixed-alias rewrite). You normally never see any of this through LlamaStash: the proxy publishes your *own* catalog by file name on its `/v1/models`, and forwards your request model to the backend, which echoes it — so you request and get back the same name. The two aliases only surface if you curl ds4-server's port directly.

## ds4 embeddings / rerank request fails

**Symptom:** a `POST /v1/embeddings` or `/v1/rerank` request that resolves to a running ds4 model returns a JSON error ("the ds4 backend serves chat/completions only, not embeddings or rerank").

**This is the design.** ds4-server has no embeddings/rerank endpoints. Launch the model on llama.cpp for those modes — a plain `--mode embedding` / `--mode rerank` launch of a ds4-compatible GGUF already routes to llama.cpp automatically. Reserve ds4 for chat/completions.

## `--mtp on` didn't enable MTP (or a launch failed with "context type MTP requested")

**Symptom:** you passed `--mtp on` but the launch warns "MTP forced on … but this model is not MTP-capable — skipping", or a hand-written `--spec-type draft-mtp` in the `-- <extras>` tail crashes the launch with `llama_init_from_model: context type MTP requested but model doesn't contain MTP layers`.

**This is the gate working (or the lack of one).** MTP only works on a model that ships a draft head — an embedded one (`{arch}.nextn_predict_layers > 0`) or a separate `mtp-*.gguf` sibling. llamastash checks first: `--mtp on` on a non-capable model warns and skips rather than emitting the flag and bricking the launch. If you bypass llamastash and pass `--spec-type draft-mtp` yourself in `extras`, llama-server has no such guard and fails to load. Fix: use `--mtp auto` (the default) so llamastash only enables MTP when the model can actually do it, and let `pull` fetch the `mtp-*.gguf` head for separate-head models (`--all-companions` if the default one-per-kind missed it).

## Codex / Responses-API client can't reach a ds4 model

**Symptom:** a client that speaks only the OpenAI Responses API (`POST /v1/responses`) — e.g. recent Codex CLI — can't drive a ds4 model through the proxy.

**Known gap.** ds4-server speaks `/v1/responses`, but the LlamaStash proxy does not route it yet (tracked in `TODO.md`). Use a Chat Completions (`/v1/chat/completions`) or Anthropic Messages (`/v1/messages`) client against the proxy for now.

## `state.json` quarantined after downgrading LlamaStash

**Symptom:** after running a newer LlamaStash that launched a ds4 model and then reverting to an older binary, the daemon quarantines `state.json` as `state.json.broken-<ts>` and boots with defaults.

**This is expected pre-release.** The ds4 work added a `resolved_backend` tag on last-params **and running-snapshot** rows and a `"ds4"` backend value the older binary's state schema doesn't understand, so it rejects the file rather than misreading it. LlamaStash keeps no backward-compatibility guarantees before the first stable release. Favorites / last-params / the running snapshot reset for that boot; named presets live in `config.yaml` and survive. Don't hop between old and new binaries against one state dir.

## HuggingFace pull

`llamastash pull <owner/repo[:filename.gguf]>` downloads a GGUF into the HuggingFace cache layout the scanner already reads, so the model shows up in `list` / the TUI right after. The TUI's `d` HuggingFace dialog is the interactive face of the same worker. If a download stalls, check network / egress and that the repo + filename resolve on huggingface.co; a failed pull exits `69` (`PULL_FAILED`). The per-file cap is 512 GiB (raised for ds4's single-file DeepSeek-V4 GGUFs).
