# SGLang setup

SGLang serves **safetensors HuggingFace repos** — the non-GGUF half of your
model cache — the same rows the vLLM backend serves. It sits alongside
llama.cpp rather than competing for the same files: a GGUF still binds
llama.cpp (or ds4), and a safetensors repo binds a safetensors engine.

LlamaStash never installs SGLang. You supply the launcher; the backend is on by
default whenever a `sglang` is found, and contributes nothing when it isn't.

> **Experimental.** Validated against SGLang `0.5.18` on one DGX Spark (GB10,
> unified memory): the flag surface from `sglang serve --help`, and a real
> launch through `scripts/sglang/uat.sh` — the token cap the guard passed came
> back as the server's resolved pool, with host memory peaking at 22 GiB of
> 121 GiB where the fraction default would have claimed ~107 GiB. Behaviour
> and config may change.

## Install

A native install puts a real `sglang` console script on disk, which is what
LlamaStash spawns.

### NVIDIA / CUDA

```bash
python3 -m venv ~/.venvs/sglang
~/.venvs/sglang/bin/pip install "sglang[all]"
```

### Pointing LlamaStash at it

```yaml
backend:
  sglang:
    servers:
      - binary: /home/you/.venvs/sglang/bin/sglang
```

Check the install before wiring it up:

```bash
~/.venvs/sglang/bin/sglang serve --help | head -3
```

### Containers

The `lmsysorg/sglang` images work too, and are the route on a DGX Spark.
LlamaStash spawns a binary, so you need a wrapper script that `exec`s
`docker run` with `"$@"` appended, pointed at by `servers[0].binary`. The
image's `sglang` entrypoint accepts `serve --model-path …` exactly as the
native script does.

The container caveats are the same as for vLLM, and worth re-reading before
choosing this route:

- **Do not read `$HF_HOME` in the wrapper.** LlamaStash strips `HF_HOME`,
  `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` and `HF_ENDPOINT` from every backend
  child. Hardcode the cache path and bind-mount it at the same path inside and
  out, since LlamaStash passes an absolute host path as `--model-path`.
- **The supervised process is the docker client, not SGLang.** SIGTERM
  forwards; the SIGKILL escalation does not cross the container boundary.
- Pass `--network host` so the port LlamaStash reserved is the one SGLang
  binds, and `--gpus all` (or the device flags your runtime needs).

## Enabling and disabling

SGLang is on whenever the launcher resolves. The tri-state mirrors the other
detected backends:

| `backend.sglang.enabled` | Result |
|---|---|
| unset (default) | on when a `sglang` resolves, silent no-op when it doesn't |
| `true` | force on |
| `false` | force off even when present |

`daemon start --sglang` and `LLAMASTASH_SGLANG=1` force it on over an explicit
`false`. Check what the daemon decided:

```bash
llamastash status --json | jq '.backends[] | select(.id == "sglang")'
```

### When vLLM is installed too

Both engines claim the same safetensors rows. A repo then lists both in
`supported_backends`, and an `auto` launch picks vLLM — it has the higher
launch priority, being the longer-validated of the two. Launch on SGLang
explicitly:

```bash
llamastash start owner/repo --backend sglang
```

## Tuning

Context length is the shared `--ctx` knob and maps to `--context-length`.
Everything else SGLang-specific is a native knob, settable in the TUI launch
picker or saved in a preset:

| Knob | Flag |
|---|---|
| `mem_fraction_static` | `--mem-fraction-static` |
| `max_total_tokens` | `--max-total-tokens` |
| `enable_unified_memory` | `--enable-unified-memory` |
| `max_running_requests` | `--max-running-requests` |
| `quantization` | `--quantization` |
| `trust_remote_code` | `--trust-remote-code` |
| `tool_call_parser` | `--tool-call-parser` |
| `reasoning_parser` | `--reasoning-parser` |

The two parser knobs are closed sets, transcribed from 0.5.18's own argparse
choices; `auto` detects the parser from the chat template. `quantization`
shares its declaration with the vLLM knob of the same name (the registry
allows one shape per knob id), so a dashed method such as `auto-round` goes
through the extras tail: `-- --quantization auto-round`.

`--enable-unified-memory` is **not** a host unified-memory switch. Per 0.5.18's
own help it replaces the statically partitioned pools of a hybrid model
(full-attention KV plus SWA/Mamba state) with one buffer split dynamically.
It is exposed because hybrid models are common on the hosts this backend
targets; it plays no part in the guard below.

SGLang has several hundred flags; the rest ride the `-- <extras>` tail. A
handful are refused there because they would undo the loopback-only posture,
spawn processes the supervisor cannot reap, or bypass the HTTP server the
proxy forwards to: `--api-key` / `--admin-api-key` / `--enable-ssl-refresh`;
`--dist-init-addr` (`--nccl-init-addr`), `--nnodes`, `--node-rank`; `--dp-size`
(`--data-parallel-size`) and `--pp-size`; the `--disaggregation-*` family;
`--grpc-mode`, `--smg-grpc-mode`, `--grpc-port`, `--sidecar`,
`--sidecar-args`; `--fastapi-root-path`; `--file-storage-path`; plus the
shared `--host` / `--port` / `--ssl-*` denylist. `--tp-size` is allowed —
tensor parallel across local GPUs is in scope.

Two more are refused for reasons of our own. `--config` points SGLang at a
YAML file whose flags are spliced in ahead of the launcher's, so anything in
it would override the list above. `--served-model-name` is the launcher's:
readiness and the chat path match the name it passes, and a second one
(argparse keeps the last) would leave the launch waiting out its probe budget
for a name the server never advertises.

## Notes and limitations

- **Detection is a filesystem check, never an exec.** Same rule as vLLM: the
  launcher imports the engine before building its parser, so running it to
  detect it is unreliable on a host without a usable accelerator. LlamaStash
  only checks that the path exists — which is also why a wrapper script works.
- **Startup is slow.** Weight load, CUDA graph capture and warmup keep the
  server unready for a long window. Readiness waits for `/v1/models` to
  advertise the model, not just for the port to answer.
- **On unified-memory hosts, the KV pool is capped automatically — in
  tokens.** This is the sharpest edge here. SGLang has no byte-level cap:
  `--mem-fraction-static` is a fraction of the **whole pool**, which on a
  unified host (DGX Spark, Strix Halo) is system RAM, and the only
  deterministic bound is `--max-total-tokens`, denominated in tokens. So
  LlamaStash takes the same byte budget vLLM gets on these hosts (8 GiB, or
  less if weights plus an 8 GiB host reserve leave less) and divides it by
  what one token costs this model — `2 × layers × KV heads × head dim × dtype
  bytes`, read from the repo's `config.json` (multi-head latent attention and
  multimodal `text_config` shapes included). Hybrid models are priced as if
  every layer were full attention, which overestimates and so caps smaller
  than it could: the safe direction.

  When the geometry cannot be read the launch is **refused**, naming the
  override, rather than guessed: a guess in the wrong direction is the freeze.
  Set either `max_total_tokens` or `mem_fraction_static` yourself and the
  auto-cap steps aside. Both are volatile, so a preset run applies the value
  without every later bare `start` inheriting it. Discrete-GPU hosts are
  untouched; there the fraction applies to real VRAM.
- **The cap can be smaller than your `--ctx`.** A large model on a tight host
  may get a pool below the requested context; the launch goes ahead with a
  warning, and requests longer than the pool are rejected by SGLang. Raise
  `max_total_tokens` if you know the host can take it.
- **The model name is the repo id.** LlamaStash passes `--served-model-name`,
  so `/v1/models` advertises `owner/name`, not the cache path. SGLang takes
  one served name and does not validate the `model` field of a request
  (verified on 0.5.18: an unregistered name is served), so no aliases are
  registered and any name the proxy resolves reaches the model.
- **No GGUF on SGLang.** A GGUF binds llama.cpp (or ds4). SGLang claims
  safetensors repos only.
- **Single-host only.** Tensor parallel across local GPUs is exposed;
  multi-node, data parallel and prefill/decode disaggregation are out of scope.
- **The memory admission gate covers SGLang.** The pre-spawn refusal prices
  the weights on disk plus the pool the backend resolved for itself (the token
  cap times the per-token cost, or the fraction of free memory).
- **The daemon still needs a `llama-server` to launch anything.** The launch
  environment is built only when the default llama.cpp binary resolves, so a
  host with SGLang alone cannot launch; point `LLAMASTASH_LLAMA_SERVER` (or
  `server.binary`) at any `llama-server`. This predates the backend and
  affects vLLM the same way.
- **CORS is open and there is no switch.** SGLang's HTTP server allows every
  origin unconditionally (0.5.18) and exposes no flag to narrow it, so there
  is no `cors` config key here. The proxy relays the upstream CORS headers onto
  its stable port, so while an SGLang model is running, any page you visit can
  read completions off the loopback listener.
- **`resolved_ctx` comes from `/get_server_info`.** SGLang's `/v1/models`
  carries no context field; the backend reads `context_length` off the
  server-info endpoint instead. That field is `null` when no
  `--context-length` was passed, so a launch without `--ctx` shows no
  resolved window.
