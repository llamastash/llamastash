# E2E UAT Findings — llamastash @ main `d043f99` (v0.0.6 + Unreleased)

Run date: 2026-07-19. Isolated sandbox (own `LLAMASTASH_STATE_DIR`, `HF_HOME` pointed at the user's real HuggingFace cache, no user daemon / no real model served by the user). Binary built from `main`.

## Environment
- Host: AMD Ryzen AI Max+ 395 / Radeon 8060S (ROCm, unified 126976 MiB). RAM ~130 GB.
- llama-server: b10011 (bf2c86ddc) — supports `--fit`, `--spec-type draft-mtp`, `--model-draft`.
- Backends: llamacpp (rocm, installed) + lemonade (`lemond` umbrella, auto-started at boot) + ds4 (`ds4-server` absent → unavailable).
- Servers (per `status.servers`): `llamacpp-rocm`, `lemonade`.
- Catalog: **20** models (`list --json`); proxy `/v1/models` advertises **17** (see F-06). chat / embed / rerank / vision / ds4-compatible present. **No lemonade-routed model** (the `qwen3.5-4b-FLM` fixture is gone) → lemonade routing not exercisable.
- No MTP-capable model in catalog (every row `mtp:null`, no `mtp-*.gguf` sibling) → MTP path not exercisable here.
- Two co-resident daemons during the run: a pre-existing correct-repo daemon on IPC `11436` (proxy `11435`), and the isolated UAT daemon on IPC `48134` (proxy also `11435` — shared/collided; both scan the same `HF_HOME`). All UAT calls below target the isolated daemon via `runtime.json`.

## Findings

### F-01 (medium) — CLI `list --json` diverges from IPC `list_models` shape
CHANGELOG (Unreleased, "one catalog-row serializer, byte-identical") claims `list --json` rows are byte-identical to IPC `list_models`. On `main` they are not.
- IPC `list_models` row keys: `backend, display_label, metadata, multimodal, parent, parse_error, path, source, split_siblings, supported_backends` — `metadata` is **nested** (`arch/quant/native_ctx/mode_hint/parameter_label/weights_bytes/tokenizer_kind/has_chat_template/has_reasoning_hint`), plus top-level `multimodal` and `split_siblings`.
- CLI `list --json` row keys: `arch, backend, display_label, mode_hint, name, native_ctx, parameter_label, parent, parse_error, path, quant, source, status, supported_backends, weights_bytes` — **flat**, with `arch/quant/native_ctx` at root, **no `metadata` nesting, no `multimodal`, no `split_siblings`** (though it does carry a CLI-only `status` object on running rows).

So the new capability blocks (`multimodal`, `split_siblings`) and the `metadata` nesting are absent from the CLI / agent `--json` surface. The "byte-identical" contract is broken.
Repro: `llamastash list --json | jq '.models[0] | keys'` vs IPC `list_models` row keys (above). Status: OPEN.

### F-02 (low) — `status --json` running rows have `name: null`
Running launch rows carry `name: null`; identity is via `model_path` / `launch_id`. Catalog `name` is not surfaced on running rows. Likely by-design (note only). Status: OPEN.

### F-03 (medium) — `start --server <invalid>` is not rejected
`llamastash start <model> --server nope` (and `--server definitely-not-real`) does **not** error. It launches, records `params.server: "nope"` verbatim, and silently uses the default server binary. Per the server-abstraction design, an unknown server id should be a usage / binary-not-found error (64 or 70), not a silent fallback with a bogus recorded value. Status: OPEN.

### F-05 (low) — `pull` missing the documented companion-fetch flags
AGENTS.md describes v2 `pull` companion fetch (`--no-companions` / `--all-companions`, mmproj / MTP-head siblings). `llamastash pull --help` on `main` shows only the MVP shape (`owner/repo[:filename.gguf]` positional, `--json`, `--no-color`) — the companion flags are absent from the binary. The `:filename.gguf` pin is positional syntax, not a `--filename` flag. Either the feature is unshipped or the docs lead. Status: OPEN (docs/impl gap).

### F-06 (low) — `list` count (20) != proxy `/v1/models` count (17)
`list --json` reports 20 catalog models; the OpenAI-compat proxy `/v1/models` advertises 17. The two surfaces disagree on model count. Likely the proxy applies a serve-filter (e.g. excludes models it can't currently serve, or a subset rule) that `list` does not. Agents pinning against `/v1/models` vs `list` will see different totals. Status: OPEN.

### F-07 (info) — ambiguous-ref proxy envelope (11.2) not separately curl-exercised
`ambiguous_model` 400 asserted via contract/code; this pass did not curl an ambiguous proxy request headlessly (prior session verified it). Low risk — the error path is shared with the 404/400 envelope logic that WAS exercised (11.1/11.3/11.6). Status: OPEN (verification gap).

### F-08 (low) — config `backend.llamacpp.servers[].name` ignored for server id
Config `backend.llamacpp.servers: [{binary, name: my-rocm}]` overrides the **binary** (correct path used) but the `name:` field does NOT override the auto-derived server `id` (`llamacpp-rocm` stays). May be intended (id auto-derive wins) or a gap. Per AGENTS.md the `name:` is meant to be overridable. Status: OPEN (clarify intent).

### F-09 (medium) — breaking config rename gives no signal
In 0.1.0, all backend config regroups under `backend:`, `servers: []` replaces `binary`/`additional_binaries`, and `state.json` stops carrying presets. The top-level `Config` is **not** `deny_unknown_fields`, so old top-level keys (`jinja`, `llama_server_path`, `lemonade:`) are **silently ignored** — the daemon still boots, no `config error:`. A user upgrading from pre-1.0 loses their settings with zero warning. The breaking rename produces no diagnostic. (Contrast: the per-backend `backend.<id>` structs ARE strict — unknown sub-keys error at 64.) Status: OPEN.

### F-10 (low) — `--llama-server` flag does not override a config `servers:` entry
`LLAMASTASH_LLAMA_SERVER` env works **only** when no `backend.llamacpp.servers:` is set in config (then `servers[0].binary` = env path). With a config `servers:` block, the `--llama-server` flag is ignored, and a config entry pointing at an invalid binary silently yields `servers[].binary: null` (no error). The documented "flag targets servers[0]" contract is partially broken. Status: OPEN.

### F-11 (medium) — `list` table missing MODE and DEVICE columns
The plan (§16.10) asserts the `list` table has a `MODE` column (before `BACKEND`) and a `DEVICE` column (`all`/dash). The actual header is `NAME ARCH PARAMS QUANT CTX SIZE BACKEND STATUS` — **no MODE, no DEVICE column** (verified on both 1-GPU and fake-2-GPU daemons). Either the columns were never implemented or the plan is aspirational. Status: OPEN.

### F-12 (medium) — `backend.llamacpp.jinja: false` not honored
`backend.llamacpp.jinja` factory-defaults to `true` (per AGENTS.md) and is expected to be suppressible to `false` on non-reasoning launches (reasoning still forces `--jinja`). Confirmed via spawned llama-server `ps` cmdline: with `jinja: false`, a non-reasoning `--backend llamacpp` launch STILL emits `--jinja`; a default (no-key) launch also emits `--jinja`. The disable path is broken — `--jinja` is effectively always on. Status: OPEN.

### F-13 (high) — LAN keyless proxy bind not refused
Per §24.6, `daemon start --proxy-host 0.0.0.0` with no `proxy.api_key` and no `--insecure-no-auth` should refuse to bind (fail-closed). Instead the proxy bound to `0.0.0.0:11435` and `status.proxy.status="listening"` (auth reports "enforced" but no key exists). The proxy is LAN-exposed without authentication. This is the highest-severity finding — a user running with `--proxy-host 0.0.0.0` (or a future default) leaks the model API to the LAN with no key. Status: OPEN.

### Minor — `scripts/tui/presets.prog` golden program is stale
The Ctrl+P save + overwrite dialog **works** (validated via harness: `save preset`, `name this preset`, `saved preset`, `already exists`, `overwrite` all PASS). But the program's `expect: 'Launch settings'` and `iexpect: 'last used'` assertions fail — the current picker UI does not render those exact strings (the preset cycle row shows `auto`/`last used` differently, and there is no `Launch settings` header). Test-script staleness, not an app regression. Worth refreshing the golden program. Status: OPEN (test-only).

## Validated (passed)
- **list / status**: catalog (20) + host/daemon/backends/proxy fields; `params.ctx`, `params.server`, `params.backend_knobs`, `preset_count`, `backend`, `status.proxy` (webui URL); host sampler populates after 1 tick (cpu_pct/ram/gpu/uma).
- **start**: by name / path; `--server llamacpp-rocm`; `--ctx auto` (fit delegation); `--ctx 8192`; `--wait` (reports `resolved_ctx`, e.g. 131072); `--port 41250`; `--cache-type-k fp4` (accepted); `--flash-attn auto`; `--device 0`; `--backend ds4` on incompatible model → exit 67 with clear message, no model load; `--preset fast` (explicit, applies `(preset: fast)`); `--preset auto`; **default-preset auto-apply** (config `presets:<model>.default` applied on bare `start`, verified `ctx:2048`); embedding/rerank modes Ready.
- **stop**: by launch-id; by name substring; `--all -y` (`{count,stopped}`); `stop --all` (no `--yes`) → exit 64 refusal; `stop <nonexistent>` → exit 66; `stop` on a non-running ref → exit 66 (matches running launches only).
- **presets** (config.yaml-backed): `list` / `show` (reports `is_default`) / `save` (incl. `--preset fast` flow) / `delete`; Ctrl+P save + overwrite dialog in TUI.
- **favorites**: add / remove / list; **stale favorite filtered** (favorite temp model → 1, delete file + restart → 0); **preset + favorite survive daemon restart**.
- **show** (`<model>`, human): path/parent/source/backend + metadata block + size + running (state/port/resolved_ctx) — correct; `show --json` nested shape valid.
- **logs**: resolves by launch-id and by name substring (F-04 fix confirmed in prior session).
- **proxy (adversarial)**: `/health` 200; `/v1/models` 17 ids (see F-06); `/v1/chat/completions` 200 by basename + stream SSE; `/v1/messages` (Anthropic) 200 + `/v1/messages/count_tokens` 200; `/v1/responses` + `/v1/responses/input_tokens` (0.0.6) 200; `/v1/rerank` + `/v1/embeddings` auto-start 200; 404 `model_not_found` (`{error:{type,message}}`); 400 `model_required`/malformed; control-plane no-bearer → 401; `/ui` chooser 200 (real llama.cpp web UI with `--compressed`); `/ui?target=<launch_id>` pins (302); `/ui/switch` re-shows chooser.
- **TUI `--render`**: all sizes 80×24 / 120×30 / 160×45 / 200×60 (no panic); Host panel (CPU/RAM*/GPU/VRAM + `backend AMD · 1 GPU`); Daemon panel (port/pid/server/proxy/models/running); `Models [N]`; `▶ Running` ready row; theme via `config.yaml` (`theme: latte|monochrome|…`) renders; `ascii_glyphs: true` switches main glyph set to ASCII (footer arrow hints `↹/⇧↹` remain Unicode — minor).
- **TUI harness**: daemon-pane `server` row shows `…/llama-server (rocm) · /usr/bin/lemond (lemonade)`; theme cycle (`t`) through all 5 themes; presets Ctrl+P save + overwrite.
- **init**: `init --only config --recommended --json` → valid summary (`steps_ran:[detect,config,smoke,handoff]`, `config.path`); `init --offline --recommended` → refuses download with clear hint (exit 73); `init` HF model-search flow (`drive_init_search.py`) reaches picker, searches, returns to Skip (no download); non-TTY no-consent → exit 72, `--recommended` → non-interactive.
- **recommend --json**: returns a structured object (`recommendations`, `model`, `hardware`, `config`, `install`, `offline`, `steps_ran`, `steps_skipped`); `recommend --offline` → exit 72.
- **pull**: `--help` (MVP shape); invalid slug → exit 69 with clear message.
- **config bindings**: prints full keybinding YAML, including `cycle_pane_ratio: alt+l` (Alt+L pane split), `cycle_theme: t`, `cycle_theme_prev: T`, `delete_model: ctrl+d`.
- **config ($EDITOR)**: `EDITOR=true llamastash config` → exit 0 (opens editor).
- **ds4**: `list` shows `DeepSeek-V4-Flash` `supported_backends: ["ds4","llamacpp"]`; `doctor --json` `schema_version:2` with `ds4_unavailable` (info) advisory; `start <model> --backend ds4` when binary absent → exit 67.
- **server abstraction config**: `backend.ds4.enabled: false` honored (`status.backends.ds4.enabled=false`); `backend.llamacpp.servers` binary override honored (see F-08).

## Not exercisable in this environment
- **MTP speculative decoding**: no MTP-capable model in catalog and `--mtp` is not present on `main` `start` (it lives on the `feat/mtp-speculative-decoding` branch). TUI MTP cycle row only appears for capable models. (§21 skipped ⏭️)
- **lemonade routing** (§7.3 / §16.3): no model in the current catalog routes to `lemonade` (`supported_backends` contains `lemonade` for 0 models; the earlier `qwen3.5-4b-FLM` fixture is gone). Lemonade umbrella + `status.backends.lemonade` (installed) otherwise validated. Re-test when a lemonade-routed fixture is available.
- **ds4 auto-route + `/ui` no-pin** (§17.4 / §17.5): `ds4-server` binary absent in sandbox; auto-route and the non-selectable `/ui` chooser marker are not exercisable without the binary. Behavior specified in AGENTS.md and validated by code review.
- **`pull` real download / companion fetch**: avoided (network-heavy, large files); only the CLI contract + error path exercised.
- **Alt+L pane split**: key is bound (confirmed in `config bindings`); driving it headlessly produced no panic and shifted pane focus/footer, but the visual ratio change is not verifiable from headless text (harness lacks `alt+l` key vocab).
- **§13.9 `start --render`**: `start` has no `--render` flag (that flag belongs to the TUI entry); the cliclack launch picker is interactive and not headless-assertable. Picker behavior validated in prior harness sessions.

## Resolution (2026-07-19 follow-up)

Triaged with a code read of each surface plus a fresh, single-daemon E2E re-run in an isolated sandbox (own state/config/cache, non-default proxy port). Several findings turned out to be sandbox artifacts (two co-resident daemons on the same proxy port, scan-timing variance, a stale daemon holding old config in memory) rather than code defects.

| ID | Disposition | Notes |
| --- | --- | --- |
| **F-01** | **Fixed** (2026-08-09) | `list --json` is now byte-identical to IPC `list_models` (one shared `CatalogRow` serializer, `9d0ddef` — landed after this UAT), and `show --json` builds its envelope from the same serialized row, so `multimodal` / `mtp` / `split_siblings` / `supported_backends` surface on both (breaking shape change, rides the 0.1.0 wave). The §25.3 half was never broken — see the row below. |
| **F-02** | By design | Running rows key on `model_path` / `launch_id`; catalog `name` is a discovery attribute, not a launch one. No change. |
| **F-03** | **Fixed** | `start --server <unknown>` now errors (exit 64, lists valid ids) before reserving any resources, instead of silently using the default binary. Daemon-side validation + CLI `InvalidParams`→`USAGE` mapping + unit test. Re-verified E2E: `--server nope` → exit 64; `--server llamacpp-rocm` → launches. |
| **F-05** | Not a defect | AGENTS.md never documented `--no-companions` / `--all-companions` (the strings live only in the UAT docs). Companion-fetch is deliberately deferred and already labeled so in `TODO.md` / the MTP plan. No docs lead to correct. |
| **F-06** | Sandbox artifact | The proxy applies no serve-filter (verified by code + `tests/proxy_models.rs` parity test). Re-run on a **single** daemon: `list` == `/v1/models` == 17. The 20-vs-17 was cross-daemon scan-timing variance, not a proxy divergence. |
| **F-07** | Verification gap only | Error path shared with the exercised 404/400 envelopes; left as-is. |
| **F-08** | Not a defect; test strengthened | The `name:` override **is** honored — the id becomes `llamacpp-<name>` (the backend prefix is intentional per AGENTS.md). The unit test was too weak (used `name: "rocm"`, which collides with the gpu tag); rewritten to use a distinctive name and assert both `id` and `name`. |
| **F-09** | **Parked for decision** | Top-level `Config` intentionally lacks `deny_unknown_fields` (documented forward-compat). Fixing needs a design call — a non-breaking soft-warn pass vs a hard `deny_unknown_fields`. |
| **F-10** | **Parked for decision** | The `--llama-server` / env precedence contract is actually honored (flag/env win over a config `servers:` block). The genuine gap is narrower: an invalid **explicit** binary path fails soft (log-only warning, null server surface) instead of surfacing a `config error:`. Fixing is a fail-soft→fail-fast boot-behavior change that must distinguish "explicit bad path" from "nothing configured" (ds4/lemonade-only hosts). |
| **F-11** | **Fixed** (2026-08-09) | The `llamastash list` table now mirrors the TUI Models list: `NAME ARCH PARAMS QUANT CTX SIZE MODE [BACKEND] STATUS [DEVICE]`, with DEVICE gated on multi-device (same rule as the TUI) and the same `all` / explicit-selector / placeholder cell semantics. Breaking TSV shape change, called out in the CHANGELOG. |
| **F-12** | Stale-daemon artifact | The config→`seed_launch_knobs`→`compose` chain is correct (unit-tested). Re-verified E2E on a **fresh** daemon: default config → `--jinja` present; `backend.llamacpp.jinja: false` non-reasoning launch → `--jinja` **absent**. The UAT observation was a daemon still holding `jinja: true` in memory (config is read once at boot; no hot-reload). |
| **F-13** | Not a vulnerability; backstop hardened | The fail-closed guard already exists. `--proxy-host 0.0.0.0` with no key **auto-provisions** a key, persists it, prints it once, and binds **with** auth — re-verified E2E: unauth curl → 401, auth curl → 200, `status.proxy.auth: "enforced"`. `--insecure-no-auth` remains the loud, intentional keyless opt-out (→ 200). The UAT observation was the two-daemon/shared-port collision noted in the Environment. One real (latent) gap was closed: the backstop now treats a blank/whitespace key as absent (defense-in-depth). The UAT plan §24.6 asserted the pre-2026-06-09 hard-refuse spec; it should assert auto-provision-then-bind. |
| **Minor** | Not stale | `scripts/tui/presets.prog` passes **10/10** against `main` (harness re-run), including `Launch settings` and `last used`. The UAT failure was an environment artifact (running-model view / stale daemon). No change. |

### Parked for maintainer input

- **F-09** — soft-warn on unknown top-level config keys vs `deny_unknown_fields` (forward-compat tradeoff).
- **F-10** — surface an invalid **explicit** `llama-server` / `servers[].binary` path as a hard `config error:` (fail-fast) instead of a silent log warning.
- ~~**F-01 / §25.2 / §25.3** — expand multimodal surfacing to the CLI (`list --json`, `show --json`) and the TUI vision glyph (additive, agent-facing).~~ **Done 2026-08-09:** `list --json` was unified onto the shared `CatalogRow` wire shape (`9d0ddef`); `show --json` now derives from the same serialized row. The §25.3 TUI vision glyph was never broken — verified 2026-08-09: the right-pane header renders `◉` / `♪` / `↯` for the focused model; the UAT's "glyph count = 0" was a `--render` frame focusing row 0, not the vision model.
- ~~**F-11 (CLI parity)** — add MODE/DEVICE columns to the `llamastash list` table to match the TUI.~~ **Done 2026-08-09** (see the F-11 row above).
