---
title: "refactor: drop the Lemonade backend and the managed-multiplexer concept"
type: refactor
status: proposed
date: 2026-10-17
origin: "TODO Next Batch 1, decided from [`docs/testing/2026-10-17-batch1-lemonade-flm-whisper.md`](../testing/2026-10-17-batch1-lemonade-flm-whisper.md)"
---

# refactor: drop the Lemonade backend and the managed-multiplexer concept

**Status:** decision recorded 2026-10-17, implementation not started.

## Decision

Remove the Lemonade backend (`src/backend/lemonade/`) and document Lemonade as a
generic server entry. Retire the managed-multiplexer shape with it: one umbrella
process supervised once per daemon, per-model load and unload delegated to its
HTTP API, registry rows projected into the catalog. Nothing else uses that shape,
and the engine it was built for does not need it.

Measured basis, all of it in the test write-up:

- The generic path serves real traffic for both engines. Lemonade through one
  entry plus `proxy.aliases`, 1.9 s cold including the engine's own model load,
  103 t/s. FastFlowLM through one entry named after its own model tag, 0.66 s,
  82.9 t/s on the NPU.
- Against `lemond` 2026.42.1, the current upstream release, the dedicated path
  has four reproducible defects: registry rows missing for up to 5 minutes after
  a daemon start; registry rows not addressable by path and ambiguous by name
  whenever the same repo is also on disk; a failed `POST /api/v1/load` reported
  as a successful start; `stop` then `start` inside `lemond`'s ~6 s exit window
  refused on a port nothing is listening on.
- The NPU, which is the reason Lemonade is enabled by default here, is not
  reachable through Lemonade on Linux: 179 registry models, none with the `flm`
  recipe, and Lemonade pins `flm` at v1.0.7 while upstream is v1.0.9. `flm` run
  directly works, including Whisper ASR on the NPU.

What is given up: catalog rows for the engine's own registry, per-model unload
against a shared umbrella (0.12 s, versus killing the umbrella in 4.7 s), and
context as a JSON load field instead of argv. On the test host `lemond` runs with
`max_loaded_models: 1`, so the shared-umbrella saving is one process's idle RSS
(about 38 MB) against a model that is resident either way.

## Scope

Removal, per the backend no-leak rule, is one module plus the central wiring:

1. `src/backend/lemonade/` (5 files, about 2.1k lines with tests) and every
   reference to it: the `Backends` variant, the `for_each_backend!` arm,
   `Backends::all()`, `BackendConfig.lemonade`, `LemonadeConfig`,
   `LEMONADE_BACKEND_ID`, `LEMONADE_PATH_SCHEME`, `--lemonade` and
   `LLAMASTASH_LEMONADE`, the `lemonade://` path scheme and its parsing in
   `util::paths`, `ipc/status.rs` and `ipc/methods.rs`, and the `lemonade` arm of
   `--backend`.
2. `tests/lemonade_route_test.rs`, the `fake_lemond` fixture and its
   `--features test-fixtures` wiring, and the lemonade expectations in the
   `status`, route and TUI tests.
3. Bookkeeping that has to move in the same commit: `docs/lemonade-setup.md`
   (deleted, its content replaced by a generic-entry recipe in `docs/usage.md`),
   the Backends list and umbrella paragraphs in `docs/architecture.md`, the
   `--backend` list and the backend table in `docs/usage.md`,
   `config.example.yaml`'s `backend.lemonade:` block, `README.md` and
   `FEATURES.md`, the Lemonade rows in `docs/troubleshooting.md`, and
   `docs/testing/hardware-uat.md`. Strike the open Lemonade TODO items it
   retires.
4. `CHANGELOG.md`: one line under `[Unreleased]`, breaking. No compat window:
   pre-release rule, and `backend.lemonade:` in an existing `config.yaml` must
   then be refused with a message naming the replacement (one `deny_unknown_fields`
   error is not that message, so this needs an explicit check).

Deliberate omissions: no `lemonade://` alias window, no migration of
`state.json` entries keyed on `lemonade://<name>`, no `lemonade` left in
`--backend` as a deprecated spelling.

## Replacement recipe to ship in its place

The generic entry that replaces it, verified on the test host:

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

proxy:
  aliases:
    Qwen3-0.6B-GGUF: lemonade
```

`stop_grace_secs` is load-bearing: `lemond` takes about 6 s to exit on SIGTERM,
over the 5 s default grace. One alias per Lemonade model name a client will use;
`GET /api/v1/models` on the umbrella lists what is downloadable, and the body's
`model` string is what reaches `lemond`, so the alias name has to be the
engine's own model name.

Same shape for FastFlowLM on the NPU (documented in `docs/usage.md` with
whisper.cpp): the entry `name` is the `flm` model tag, since that string is what
the engine expects in `body.model`, and `ready: /v1/version` because FLM serves
no `/health`.

## Follow-ups this decision does not settle

- The proxy has no `/v1/audio/transcriptions` route and no `transcription` mode,
  so an ASR engine is launch-managed but not servable through the proxy. Open
  question whether to add both.
- The usage tap has no source for FLM's `usage.decoding_speed_tps`.
- `GenericServer` env values do not expand `~/`, unlike `binary`.
- Two Lemonade-specific defects are worth fixing even if the backend goes: a
  managed launch should not report success when the engine refused the load, and
  a port held by teardown remnants should be waited out rather than refused.
