# Lemonade (`lemond`) setup

LlamaStash has no dedicated Lemonade backend. Run
[Lemonade](https://github.com/lemonade-sdk/lemonade)'s `lemond` as a
**generic server entry** — one `backend.generic.servers` block plus one
`proxy.aliases` line per model you want to name. Recipe:
**[Running Lemonade as a generic server](usage.md#running-lemonade-as-a-generic-server)**.

A dedicated backend existed up to 0.8.0 and was removed: measured against
`lemond` 2026.42.1 it had four reproducible defects, and the generic entry
served real traffic in its place. Findings and decision:
[`docs/testing/2026-10-17-batch1-lemonade-flm-whisper.md`](testing/2026-10-17-batch1-lemonade-flm-whisper.md),
[`docs/plans/2026-10-17-002-refactor-drop-lemonade-backend-plan.md`](plans/2026-10-17-002-refactor-drop-lemonade-backend-plan.md).

## 1. Install Lemonade

Follow Lemonade's own instructions for your platform:
<https://github.com/lemonade-sdk/lemonade>. LlamaStash never installs it.

Install the engines you want `lemond` to serve (e.g. `lemonade backends install
llamacpp:rocm`). For **NPU** inference on AMD Ryzen AI you also need AMD's NPU
system stack — XRT, the NPU firmware, and `flm` — per AMD's documentation.

Verify it works on its own before wiring it into LlamaStash:

```sh
lemond            # starts the umbrella (default port 13305)
# in another shell:
curl http://127.0.0.1:13305/api/v1/models
```

Then **stop that manual `lemond`** — the generic entry launches its own copy on
the port LlamaStash reserves, and a second listener on the same port blocks it.

## 2. Declare it as a generic entry

```yaml
backend:
  generic:
    servers:
      - name: lemonade
        binary: /usr/bin/lemond            # absolute path, or on PATH
        args: ["--port", "{port}", "--host", "{host}", "--no-broadcast", "--log-file", "disabled"]
        ready: /live                       # 200 once the umbrella answers
        ready_timeout_secs: 90
        stop_grace_secs: 20                # lemond takes ~6 s to exit
proxy:
  aliases:
    Qwen3-0.6B-GGUF: lemonade             # one line per model clients name
```

Full recipe with the measured numbers and the limits:
[usage.md](usage.md#running-lemonade-as-a-generic-server). Model names come
from the running entry's own port, which `llamastash status` shows:
`curl http://127.0.0.1:<launch-port>/api/v1/models?show_all=true`. Write the
alias key in exactly the case `lemond` uses (`Qwen3-0.6B-GGUF`, not
`qwen3-0.6b-gguf`): `lemond` matches names case-sensitively, while
`/v1/models` lists an alias lowercased, so a client that copies the listed id
gets a 404.

## 3. Verify

```sh
llamastash status        # the generic entry appears under servers/backends
llamastash start lemonade
llamastash requests Qwen3-0.6B-GGUF     # after a chat through the alias
```

## 4. Troubleshooting

- **`404 model_not_found` naming no alias** — the entry is not up. `llamastash
  start lemonade`, then check `llamastash logs lemonade`.
- **A model name 404s through the alias** — `lemond` only answers names it
  has; check the `show_all=true` list and add the exact name to
  `proxy.aliases`.
- **No NPU acceleration** — `lemond` falls back to CPU/GPU when AMD's NPU
  stack (XRT / firmware / `flm`) isn't installed. That stack is AMD's to
  install, not LlamaStash's.
- **`FLM NPU validation failed: Memlock limits are too low`** — `lemond`
  inherits the daemon's resource limits and `flm` needs to lock model memory.
  Many non-login contexts (systemd user services, agent sandboxes) cap
  `memlock` at 8 MB even when `/etc/security/limits.conf` says `unlimited`.
  Check `ulimit -l` in the shell that starts the daemon; start it from a real
  login shell, or raise `DefaultLimitMEMLOCK` for `user@.service`.
- **Running an AMD NPU engine without Lemonade** — `flm` also works as its own
  generic entry, one process per model:
  [FastFlowLM on an AMD NPU](usage.md#running-an-amd-npu-engine-fastflowlm-as-a-generic-server).
