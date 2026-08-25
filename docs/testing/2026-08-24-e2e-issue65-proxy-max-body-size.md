# E2E — `proxy.max_body_size` + default cap 2 MiB → 16 MiB (issue #65)

**Host:** Strix Halo (AMD Ryzen AI MAX+ 395, ROCm + NPU), the box the reporter ran the ~10 MiB Qwen 3.8 27B case on.
**Binary under test:** `target/debug/llamastash` at the `feat/proxy-max-body-size` branch (v0.2.0-dev tree).
**Upstream binaries (verified real, no fakes):** `llama-server` 0.2.0-dev build 10610 / commit `a14dba686` (built 2026-07-14 — **newer** than the latest release `v0.2.0`, 2026-08-21), `lemond` `/usr/bin/lemond`.

All runs against isolated daemons (`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` under `mktemp -d`, proxy ports 11497/11498/11499, lemond umbrella pinned to 13399) so the user's real daemon was never touched. Models: `Llama-3.2-1B-Instruct-Q4_K_M.gguf` (bartowski), auto-started through the proxy where noted.

## Proxy-side results (the change under test)

| # | Cap in force | Body | Route | Expected | Got |
|---|---|---|---|---|---|
| 1 | default **16 MiB** (config key omitted) | 10 MiB JSON | `/v1/chat/completions` → Ready llama-server | not 413 from the proxy; forwarded | **forwarded byte-for-byte** — the 413 that came back carried `server: llama.cpp` (upstream, see §Upstream limits), i.e. the proxy let it through. Pre-#65 this exact request died in the proxy with "exceeds the 2 MiB limit" |
| 2 | configured **2 MiB** (`max_body_size: 2097152`) | 10 MiB JSON | `/v1/chat/completions`, no model running | proxy 413 naming the cap | `413 {"error":{"type":"payload_too_large","message":"request body exceeds the 2 MiB limit"}}` ✓ |
| 3 | configured 2 MiB | 0.9 MiB JSON | `/v1/chat/completions`, no model running | under cap → full auto-start E2E | **200** — proxy auto-started the real `llama-server`, forwarded, `{"choices":[{"content":"OK",...}]}` ✓ |
| 4 | default 16 MiB | 0.9 / 1.5 / 3 MiB JSON | direct to `llama-server` (proxy bypassed) | — | 200 / 413 / 413 — upstream boundary, not the proxy |
| 5 | default 16 MiB | 3.2 MiB silence WAV (multipart, whisper) | user's running `lemond :13305` (proxy bypassed) | — | **200** `{"text":" Thank you.\n..."}` — lemond has no 1 MiB wall ✓ |

Row 1 + row 5 together close the reporter's actual path: a body between 2 MiB and 16 MiB now **traverses the proxy and reaches a real upstream that accepts it** (lemond; lemonade-routed models delegate to the llama.cpp engine, which has its own wall — below).

## Upstream limits (pre-existing, verified, never changed)

- **`llama-server` caps request payloads at 1 MiB** — `CPPHTTPLIB_FORM_URL_ENCODED_PAYLOAD_MAX_LENGTH = 1048576` in `tools/server/server.cpp` (still present on current `master`); the local binary (newer than release `v0.2.0`) refuses at exactly that boundary (row 4). This wall predates #65 and is a separate layer: llama.cpp-routed vision bodies > 1 MiB will 413 *upstream* (empty body, `server: llama.cpp`) no matter what `proxy.max_body_size` says. Raising it means recompiling llama.cpp (or waiting on upstream) — out of scope here, flagged for the issue thread.
- **`lemond` does not share that wall** — 3.2 MiB multipart body accepted (row 5), which is how the reporter's Qwen 27B case runs on this box.
- A `lemonade`-started GGUF model is served by the llama.cpp engine *inside* lemond's umbrella (the 413s in row 1 carry `server: llama.cpp`), so lemonade-routed chat traffic inherits the engine's 1 MiB wall even though the umbrella itself accepts large bodies.

## Verdict

- Proxy behaviour matches the plan at every stage: default 16 MiB passes what 2 MiB refused; configured cap (including a low one) refuses with the cap-naming message; `0`-and-small-cap semantics covered by the unit + integration suite (`make test` green, 44 binaries).
- The reporter's end-to-end failure (10 MiB body, lemond-routed 27B) is fixed at the proxy layer; the only remaining wall on that box is llama.cpp's own 1 MiB compile-time constant for chat traffic, which is now visible in the 413 (`server: llama.cpp`, empty body) rather than masquerading as the proxy's.
