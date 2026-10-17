# Security Policy

## Supported versions

LlamaStash is pre-1.0. Only the `main` branch is supported. Security fixes will land on `main` and ship in the next tagged release.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security reports. Instead, email the maintainer at `d4udts@gmail.com` with:

- a description of the issue,
- reproduction steps,
- the version (`llamastash --version`) and platform,
- any proof-of-concept code or scripts.

You can expect an acknowledgement within a few business days. If you don't hear back, please follow up — email gets dropped.

## Threat model summary

LlamaStash is **loopback-only by default**, with an opt-in LAN mode
for the proxy data plane (behind a bearer key). The daemon binds two
TCP listeners, both protected by file-system permissions on the
daemon's state directory:

- **Control plane** (JSON-RPC for the CLI / TUI): bound on
  `127.0.0.1:48134` (with a small port-scan window if the slot is
  taken; deliberately above IANA's registered range and outside the
  `1143x` proxy family). **Always loopback** — there is no host knob.
  Every request except `GET /health` carries a `Bearer` token
  validated in constant time. The token is 32 bytes of OS randomness,
  generated fresh on each daemon start, and written to
  `$XDG_STATE_HOME/llamastash/runtime.json` (mode `0600`). Same-UID
  trust: any process with read access to `runtime.json` can attach.
- **OpenAI-compat proxy** (data plane for OpenCode / Pi / OpenAI
  clients): bound on `127.0.0.1:11434` or `11435` by default. This is
  the **only** listener that can be exposed to the LAN, via
  `proxy.host` / `--proxy-host` (e.g. `0.0.0.0`). A non-loopback bind
  **requires** a bearer key (`proxy.api_key`, sent as `Authorization:
  Bearer`): llamastash auto-generates and persists one on first use,
  and the daemon refuses to bind a routable address with no key unless
  the operator passes `--insecure-no-auth`. The key is validated in
  constant time and never logged. TLS is not yet implemented, so LAN
  mode is plaintext HTTP — keep it on a trusted network or front it
  with a TLS-terminating reverse proxy.
- Backend children (`llama-server`, `vllm`, `sglang`) **always** listen on
  `127.0.0.1` only. The proxy reaches them over loopback; the `extras`
  denylist (`--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`,
  plus per-backend additions like vLLM's `--allowed-origins`) blocks any attempt to rebind them
  or weaken the browser same-origin posture. LAN exposure is the
  proxy's job alone — models are never put on the network. A
  `backend.generic` entry is the exception: LlamaStash passes `{host}`
  (always `127.0.0.1`) but can't check what a foreign binary binds, so
  binding loopback is the entry author's job.
- The daemon does **not** persist or transmit telemetry.

Issues we treat as in scope:

- Any path that lets a non-owner (different UID) read `runtime.json`
  or otherwise attach to the control plane.
- Any path that lets a remote attacker reach the **control plane** or a
  **backend child** LlamaStash launched (`llama-server`, `vllm`)
  (e.g., accidentally binding either to `0.0.0.0`) — both must stay
  loopback always.
- Any **auth bypass** on the LAN-exposed proxy: reaching the proxy's
  data routes without a valid bearer key when one is configured, or the
  daemon binding a non-loopback proxy without a key when
  `--insecure-no-auth` was not set. (A reachable proxy is *expected*
  once the operator opts into LAN mode with auth — that is not a
  vulnerability; an auth bypass is.)
- Token / key leakage via logs, env-var dumps, or error messages.
- Memory-safety, deserialization, or path-traversal bugs in the
  daemon, CLI, or HTTP handlers.
- Lockfile / state-file races that allow privilege confusion between
  users.

Out of scope:

- Backend-engine upstream behaviour — `llama.cpp` / `llama-server`,
  vLLM, SGLang, and any server you run through the
  generic backend (please report there).
- Local denial-of-service by a malicious user against their own daemon.
- Issues that require the attacker to already have shell access as
  the daemon owner.
