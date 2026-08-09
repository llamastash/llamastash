# AGENTS.md

This file provides project-level guidance to coding agents (Claude Code, OpenCode, Codex, Copilot CLI) working in this repository. Treat it as authoritative alongside `CONTRIBUTING.md`; on conflict, prefer this file's specifics.

## Source of truth

The implementation plan is the canonical design document:

- `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md` — v1 architecture, security contract, the nine Implementation Units (1: scaffold, 2: daemon/IPC, 3: GGUF, 4: discovery, 5: launch/supervisor, 6: TUI shell, 7: right-pane tabs, 8: non-interactive CLI, 9: release scaffolding), and what is explicitly out of v1.
- `docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md` — v2 plan covering R48–R80: init wizard, doctor diagnostic, `pull` MVP, recommender, fetch contract, snapshot bundling.
- `docs/brainstorms/llamatui-requirements.md` — origin requirements (R1–R46) that the v1 plan traces to.
- `docs/brainstorms/2026-05-18-init-wizard-requirements.md` — origin requirements (R48–R80) for v2.
- `docs/spikes/2026-05-19-*.md` — pre-implementation spike findings that anchor v2's design (hf-hub injection, GH Releases asset contract, brew Linux bottle, VRAM overhead).
- `docs/architecture.md` — stable user-facing summary of what's actually in the binary.

Before any non-trivial change, identify which Implementation Unit it falls under. PR descriptions should cite the unit; commit subjects often use `feat(unit5):` / `fix(unit3):` style.

## TODO tracking

`TODO.md` at the repo root is the single index of outstanding work. Any time
you add a TODO somewhere — a `TODO(...)` / `FIXME` comment in code, an
unchecked `- [ ]` in a plan or doc, a `todo:` frontmatter field on a spike,
or a deferred follow-up surfaced during review — also add a one-line entry
in `TODO.md` that links back to the source location. When you complete a
TODO, strike it from both places in the same change. The goal is that
`TODO.md` alone tells you everything still open without grep-walking the
tree.

## Docs stay in sync with code

Docs and code ship together. After any change that alters user-visible
behavior, the CLI / IPC surface, configuration shape, install paths, exit
codes, dependencies, scope boundaries, or architecture, update the affected
docs in the **same change** (same commit, same PR). Treat a PR that ships
code without the matching doc update as incomplete.
Agents working on this app must always keep core docs in sync (README,
AGENTS.md, feature docs, CHANGELOG, usage docs, install docs, and adjacent
user/developer docs touched by the change).

Files to review for drift on every change — skip the ones a change doesn't
touch, but check before assuming:

- `README.md` — install, quickstart, screenshots, feature list, exit-code
  table when present.
- `AGENTS.md` (this file) — scope boundaries, exit-code table, CLI agent
  surface, `status` IPC fields, build/test/lint, common gotchas.
- `INSTALL.md` — installer paths, prerequisites, and installation flows
  across supported environments.
- feature docs (`docs/brainstorms/*requirements*.md`, `docs/plans/*.md`,
  and feature-focused sections in `README.md` / `docs/usage.md`) — keep
  feature scope, status, and user-facing behavior aligned with shipped code.
- `CHANGELOG.md` — noteworthy user-visible changes land an entry under
  `[Unreleased]` (or the active release section). Internal-only refactors
  can be omitted. Entries must be **short, human-scannable one-liners** —
  not every small change earns a bullet, and bullets must not carry
  implementation detail or noise. Bundle related small changes into a
  single entry where it reads better, and link to the PR / commit (e.g.
  `(#123)` or short SHA) for anyone who wants the full story.
- `CONTRIBUTING.md` — workflow / contribution rules when they shift.
- `SECURITY.md` — only when the threat model or hardening surface shifts.
- `docs/architecture.md` — when modules, the IPC shape, lifecycle states,
  or the data-flow diagram change.
- `docs/usage.md` — CLI subcommands, flags, JSON output shapes,
  configuration keys, keybindings.
- `docs/troubleshooting.md` — new failure modes / error messages that an
  end user might hit.
- `docs/plans/*.md` — tick the corresponding unit checkbox `[ ]` → `[x]`
  when the work lands; never invent retro-plans, but do keep checkboxes
  accurate.
- `config.example.yaml` — when a config key is added, removed, renamed,
  or its default changes.
- `Cargo.toml` — keywords / categories / description on any feature that
  changes the binary's positioning.
- `TODO.md` — per the section above.

If a change makes an existing doc statement wrong, fix or remove the
statement; don't leave the contradiction for the next reader. If you
introduce a new user-facing concept that none of the above docs cover yet,
pick the doc closest in scope and add a section there rather than spawning
a new file.

## Scope boundaries

The v1 contract — these are deliberate omissions, not gaps:

- **Loopback-only, same-UID.** The daemon binds two loopback TCP listeners on `127.0.0.1`: a JSON-RPC control plane on `:11436` (bearer-token authed; the token + URL land in `$XDG_STATE_HOME/llamastash/runtime.json`, mode `0600`) and the OpenAI-compat proxy (see next bullet). Neither listener accepts LAN traffic in v0.0.2. `--host` / `--listen` / `--bind` / `--api-key` / `--ssl-*` are refused if passed via `advanced[]` to `start_model`, and `LLAMA_ARG_*` env vars are stripped before spawn.
- **OpenAI-compat proxy carved out of the v1 R34 deferral.** A loopback HTTP/1.1 listener is enabled by default. In normal mode it prefers `127.0.0.1:11435` so a local Ollama daemon on `11434` can coexist; in Ollama-compat mode it prefers `127.0.0.1:11434`. It speaks `/health`, `/v1/models`, `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/rerank`, `/v1/responses` (+ `/v1/responses/input_tokens`) so OpenAI-compatible agents (OpenCode, Pi, OpenAI SDKs) attach via one stable URL. It also forwards the **Anthropic Messages API** — `/v1/messages` + `/v1/messages/count_tokens` — to llama-server's native endpoints (no body translation; llama-server converts internally), so Claude Code and other Anthropic-shape clients attach via `ANTHROPIC_BASE_URL`. Anthropic clients authenticate with the `x-api-key` header (accepted alongside `Bearer` and browser `Basic` in `ProxyAuth::check`); tool calling on either surface needs the backend launched with `--jinja`, emitted by default via the `backend.llamacpp.jinja` config key (see "jinja default" below). Same-machine threat model on the loopback default — no auth, no TLS, no peercred (the listener is plain loopback HTTP, not the IPC socket); LAN binding + bearer key are opt-in. The rest of R34 — MCP, idle eviction, fallback tuning, and TLS for the LAN-exposed proxy — stays deferred. See `docs/plans/2026-05-21-001-feat-proxy-router-plan.md`.
- **Web-UI surface (`/ui`) on the same proxy listener.** `GET /ui/` serves the running model's stock llama.cpp web UI through the proxy on one port-stable origin (`http://127.0.0.1:11435/ui/`), so users stop hunting the ephemeral backend port. Backend selection: cookie pin (`ls_ui_target=<launch_id>`) → the single running model → a llamastash chooser page (`>1` running) → a "no model running" page (zero). `GET /ui` 302s to `/ui/`; every `/ui/...` request strips the prefix and reverse-proxies to the chosen backend. Switching once pinned: `/ui/switch` always re-shows the chooser (marking the active model), or `/ui/?target=<launch_id>` re-pins directly. `/ui*` rides the same auth gate as the data plane, and `ProxyAuth` additionally accepts **HTTP Basic** (`base64(user:<key>)`, key as the password) so a browser authenticates over LAN — a `/ui` 401 carries `WWW-Authenticate: Basic`; `Bearer` stays the API path. Running-only (no auto-start from the chooser), single shared history, no iframe/custom UI — see `docs/plans/2026-06-15-001-feat-proxy-ui-surface-plan.md`.
- **jinja default.** `backend.llamacpp.jinja` (factory `true`) makes `compose` emit `--jinja` on every launch — it's what enables tool calling on both the OpenAI `/v1/chat/completions` and Anthropic `/v1/messages` surfaces. Config-only (no env override; the `"1"`-truthy env contract can't express "force off"). The value is seeded into the generic `backend_knobs` map each launch by the llama.cpp backend's `seed_launch_knobs` hook; `compose` reads `backend_knobs["jinja"]` and ORs it with the reasoning toggle (`backend_knobs["jinja"] || params.reasoning`), which still forces `--jinja` on regardless — so `jinja: false` only suppresses it on non-reasoning launches. Resolved value surfaces in the `status` running-params row under `params.backend_knobs` (there is no longer a top-level `jinja` row).
- **Config presets are the writable source of truth.** Named launch presets live in `config.yaml` under a `presets:` key, **not** `state.json`. The daemon loads them into an in-memory store (`daemon::preset_store`) at start and write-through to `config.yaml` on `presets save` / `delete` — comment-safe, via the shared `config::yaml_edit` primitive (`yamlpath` span-locate + `yamlpatch`, behind `config::presets_writer`): only the touched node changes, written in block style (multi-line, unquoted keys). App writes are live without a restart; hand-edits need a daemon restart. A typed knob delegated to `--fit` is the bare token `auto` (e.g. `n_gpu_layers: auto`), **not** `{auto:true}`; `auto` is reserved, so a literal `auto` value uses the `{value: auto}` escape — this encoding is identical in `config.yaml`, `--json`, and `state.json`. A `presets:` key is classified per-resolution against the live catalog: names a discovered model (basename, path fallback) → per-model; else read as a GGUF `general.architecture` id → that arch. A model's effective set = per-model ∪ arch (model wins on a name collision); `default` resolves the same way and is **config-only** (hand-edited; there is no set-default op). The default **does** auto-apply: it is the model's standing launch config, resolved server-side in `compose_and_spawn` as the `LayerLabel::PresetDefault` resolver layer (between `User` and `LastUsed`) on any **no-selection** launch — a plain `start`, or a proxy auto-start. `default: <name>` applies that preset; `default: auto` launches pure fit (skips `PresetDefault` **and** `LastUsed`); unset keeps last_params as the implicit default. An explicit `--preset <name>` / TUI selection still flattens client-side into the `User` layer (the default layer is skipped then); `--preset auto` is the clean per-launch "inherit nothing" gesture. Which path applies is driven by a `selection` field on `start_model` (`default` | `explicit` | `auto`; absent ⇒ `default`, what the proxy's `StartParams::default()` sends). Extras follow the same selection rule (whole-list, no per-flag merge): explicit inline extras verbatim, else a no-selection launch inherits the default-preset's (or last_params') extras, else none — this **supersedes the PR #49 origin gate**. `presets_list/show/save/delete` are config-backed; `presets_all` returns the raw map (the TUI resolves effective sets client-side). `status` model rows carry `preset_count` + `default`. **No `export`, no `presets_set_default`, no TUI list/delete** (TUI only *saves* via `Ctrl+P` — always from the Settings pane, but only from a running row in the Models list — and *selects* via the settings cycle row; the cycle marks whichever stop is the default with `(default)` and opens on it). CLI/TUI write per-model keys only; arch presets are hand-authored. Presets carry no `port`. See `docs/plans/2026-06-30-001-feat-default-preset-resolver-layer-plan.md` (and the original `docs/plans/2026-06-22-001-feat-config-presets-per-model-plan.md`).
- **`llamastash pull`** graduated in v2 from the v1 `unimplemented!` shim. MVP shape: `llamastash pull <owner/repo[:filename.gguf]>` — downloads via the [`hf-hub`](https://crates.io/crates/hf-hub) crate (0.5 line, resolves the same `reqwest 0.12` we pin elsewhere) into the canonical HF cache layout that discovery already scans. The TUI's `d` HuggingFace pull dialog is the interactive face of this primitive; the CLI `llamastash pull <slug>` stays the only non-TUI browse surface (the dialog is TUI-only, no HTTP / MCP equivalent in v2). The `favorites_json` / `CatalogRow::name` JSON shapes stay byte-stable. (`list --json` is **not** byte-stable across 0.1.0 — it was unified onto the single `list_models` wire shape; see the "one catalog-row serializer" note below.)
- **`llamastash init` / `llamastash doctor`** are v2 surfaces. Init is the first-run wizard + maintenance tool; doctor is the read-only diagnostic. Both honor `--json` per the v2 plan §"init/doctor mode/flag decision matrix". `init` is **interactive by default**; agents that need non-interactive runs pass `--recommended` (`--yes` remains a hidden alias with identical behavior, and both flags can be combined) and may pre-answer individual prompts with `--install`, `--model`, and `--config-step`.
- **ds4 (DwarfStar) backend — experimental; direct, process-per-model, DeepSeek-V4-only.** (New and lightly road-tested — validated on a single Strix Halo / ROCm host; behaviour and config may change. llama.cpp stays the stable default and runs deepseek4 too on a current build, so ds4 is never required.) A third backend (`ds4::Ds4Backend`) that runs the `ds4-server` binary for antirez's DeepSeek-V4 Flash/PRO GGUFs. **Default-on when the binary resolves.** Config is the `backend.ds4:` block: `backend.ds4.servers` (`[{binary}]` path to `ds4-server`, else `PATH`) and `backend.ds4.enabled` (tri-state — unset = auto/on-when-found, `false` = force off, `true` = force on); `--ds4` daemon flag / `LLAMASTASH_DS4=1` also force-on (OR-merge, carried through the detached re-exec like Lemonade). Zero footprint when the binary is absent (byte-stable argv/wire for llama.cpp + Lemonade). **Routing keys on `ds4::ds4_compatible(header)`** — a header-level predicate: arch `deepseek4` **and** the per-tensor-role quant contract (routed experts `ffn_*_exps` ∈ {IQ2_XXS, Q2_K, Q4_K}, every other tensor ∈ {F32, F16, Q8_0, I32}). A compatible GGUF auto-routes to ds4 when ds4 is available and the mode is chat/completions; otherwise it **falls back to llama.cpp — never a refusal** (a **b9840+** llama.cpp runs `deepseek4` too — [ggml-org/llama.cpp#24162](https://github.com/ggml-org/llama.cpp/pull/24162), merged 2026-06-29, first-hand load-tested; an older `llama-server` fails the fallback with `unknown model architecture: 'deepseek4'`). This **amends R13**: "disk GGUFs always bind llama.cpp" gains its first exception. `--backend ds4` / `--backend llamacpp` override in both directions; `--mode embedding|rerank` on a compatible model routes to llama.cpp (ds4 serves no embeddings/rerank). One pre-spawn refusal survives: the split PRO half-files (`…-Layers00-30` / `…-Layers-31-output`, `is_ds4_split_half`) — "ds4 distributed mode unsupported". **Native knobs: 14** (`power`, `tokens`, `threads`, `kv_disk_dir`, `kv_disk_space_mb`, `ssd_streaming` + its tuning family `ssd_streaming_cache_experts`/`ssd_streaming_preload_experts`/`ssd_streaming_cold`, the bools `warm_weights`/`quality`, plus the MTP trio `mtp`/`mtp_draft`/`mtp_margin` → `--mtp`/`--mtp-draft`/`--mtp-margin`); Typed IR is `Ctx` only (`--ctx`); the long tail of ds4-server flags rides `extras`. ds4-server's flag set is **build-dependent** (DwarfStar moves fast) — `--quality` and `--mtp`/`--mtp-draft`/`--mtp-margin` were absent from the 2026-06-17 build but parse as of 2026-07-10, so the earlier "ds4-server rejects them, dropped" is stale; verify against the live `ds4-server --help` (+ its `runtime`/`steering`/`kv-cache`/`distributed` topic pages) before assuming a flag is typed vs extras vs unsupported. ds4 extends the loopback/credential denylist with `--cors` and `--dist-` (`DS4_FORBIDDEN_EXTRA_HEADS`). **Readiness** = `GET /v1/models` → 200 **and** a body advertising a ds4 alias (`deepseek-v4-flash` / `deepseek-v4-pro`, tolerant of `deepseek-v4-*`) — ds4 loads weights before binding, so 200 means resident, and the alias guards the multi-minute unbound-port window. **`/v1/models` menu (verified against real `ds4-server`, not the fixture):** `/v1/models` advertises a **static two-entry list** [`deepseek-v4-flash`, `deepseek-v4-pro`] regardless of the loaded model (a direct curl shows two rows with one running); `/v1/chat/completions` **echoes back the request `model`** — there is no fixed-alias rewrite. The proxy relists your catalog by file name and forwards the request model verbatim, so clients never see the aliases. The right pane shows a plain ` ds4 ` backend chip (no "serves as" disclosure — there is no model→alias remap to surface). Do not trust `fake_ds4_server` for this: it echoes a fixed alias in chat, which the real binary does not. **Admission:** the `ssd_streaming` **native knob** (not an extras `--ssd-streaming`) skips the hard OOM refusal (below-RAM-floor streaming). deepseek4 KV is modeled from the header (`backend::ds4::ds4_kv_bytes` (the `Backend::kv_bytes` hook): per-layer `attention.compress_ratios` + `attention.key_length`), which tracks ds4's two-tier compressed cache (~0.5 GiB at 16k ctx, ~11 GiB at 1M for Flash) instead of the naive GQA figure the generic path would emit (`head_count_kv=1 × key_length × full ctx` ≈ 86 GiB at 1M). The admission gate still under-projects ds4's *full* runtime residency (the expert working set beyond raw weights isn't modeled), so ds4's `resolve_native_knobs` hook (invoked generically by `compose_and_spawn` over every backend — no ds4-specific branch in the launch path) **auto-enables `ssd_streaming`** when the ds4 resident estimate (`ds4_resident_estimate` = ~1.25× weights) exceeds effective free memory — the below-floor launch loads from disk instead of OOM-killing mid-load (ds4-server sets its own `oom_score_adj=1000`). This is the uniform Auto-knob behavior (an unset knob resolves from live host context), not a special case. An explicit `ssd_streaming: true/false` is respected (skips auto); the auto-enabled value is **not** frozen into `last_params` (it re-evaluates from live free memory each launch, so it never sticks the OOM gate off after RAM frees up). Dropped typed knobs also warn. **TUI ds4 surfacing:** the right-pane header shows a `ds4` badge under the model path on ds4-routed rows (via `render_header_badge` in the layout gap slot) — a *selected* row keys on the `list_models` prediction, a *running* row on the launch's actual `resolved_backend` — and the running-launch view renders the ds4 native knobs (from the live `status` `params.backend_knobs`) instead of the llama.cpp typed set. Badge / knob-panel on a running row both key on the real backend, so a compatible file launched `--backend llamacpp` shows its llama.cpp knobs, not a false ds4 view. The ctx quick-pick ladder extends to `MAX_CTX_TOKENS` (1 Mi) and is gated per model to its native window (`LaunchPickerState::ctx_presets`). **Adoption** dispatches on the launch's recorded `resolved_backend` tag (stamped at spawn on both the running-snapshot and last-params rows), so a renamed/wrapped `ds4-server` binary still re-adopts; a ds4 candidate then matches the alias set AND cross-checks the process argv `-m` against the recorded path (PID-reuse guard). The external sweep learns the `ds4-server` marker. **`/ui`** never auto-pins a ds4 row (it serves no web UI) — the chooser lists it non-selectable ("no web UI"); the "no model running" page stays reserved for zero running models. **kv-disk:** `--kv-disk-dir` is ds4's own persistent, reuse-across-restarts cache — llamastash never subdir-mangles or cleans it, and it holds conversation-derived state under ds4's own umask at the user-typed path, so docs recommend a **private, user-owned directory**. `status.backends` gains a ds4 row `{id:"ds4", lifecycle, installed, enabled, accelerators, binary}`; each running model row also carries `backend` (the resolved backend) + `params.backend_knobs`. The proxy's embeddings/rerank refusal covers **both** a running ds4 model and a not-running compatible model that would auto-start on ds4 (before the multi-minute load). `doctor` gains two info-tier advisories, both honoring the `LLAMASTASH_DS4` env force: `ds4_unavailable` (binary absent + a compatible model present — they still run on llama.cpp; `fix_hint` = clone/`make` recipe + `backend.ds4.servers` + `docs/usage.md#ds4-backend`, and only this case scans discovery) and `ds4_disabled` (binary installed but `backend.ds4.enabled: false` / no force — `fix_hint` = re-enable, no scan). Additive finding ids, so `doctor` `schema_version` stays **2**. The `pull` per-file cap was raised 64→512 GiB for ds4's 81–465 GB single-file GGUFs. **Deferred (not gaps):** distributed/split-GGUF PRO mode, embeddings/rerank on ds4, ds4 in `init`, recommender/benchmark integration, MTP is wired (the `mtp`/`mtp_draft`/`mtp_margin` native knobs above; auto-pairs a `mtp-*.gguf` sidecar found next to the model — see the MTP scope bullet below). The OpenAI `/v1/responses` (+ `/v1/responses/input_tokens`) surface **is** now proxied (both backends speak it). See `docs/plans/2026-07-10-001-feat-ds4-backend-plan.md`.
- **Server abstraction — per-backend `servers: []` + two-level server→device launch.** (BREAKING, 0.1.0.) A **server** = one build/binary of a backend (llama.cpp's ROCm build, its Vulkan build, `ds4-server`, `lemond`); a backend has 1..N servers. Config is a per-backend `servers: [{binary, name?}]` array (replaces llama.cpp's `binary`/`additional_binaries` and ds4/lemonade's single `binary`; first entry = default, and the `--llama-server` / `LLAMASTASH_LLAMA_SERVER` target). **No dedup across builds** — every configured binary is its own selectable server (the flat `--device` device *catalog* still dedups selectors within/across builds; host GPU detection keeps its own PCI dedup, untouched). Neutral `Device` / `Server` / `ServerConfig` / `ServerSpec` types live in `crate::backend` (`server.rs`); the boot builds the catalog generically over `Backends::all()` via three trait hooks — `configured_servers` (enumerate a backend's servers), `probe_devices` (one binary's `--list-devices`, llama.cpp-only), `launch_priority` (ds4 `20` > llamacpp `10` > lemonade `0`; orders the server knob, `supported_backends`, and the no-selection default). Server ids auto-derive with a plain hyphen separator: `<backend>-<gpu_backend>` (unique, e.g. `llamacpp-rocm`) → `<backend>-<binary-dir>` (colliding gpu tag) → `-N`, overridable per-server `name:`. A **device-less** server (ds4 / lemonade / a CPU-only build — no `--list-devices` probe, so no detectable compute type) gets the **bare backend id** (`ds4`, `lemonade`), disambiguated `-N` on collision; ds4's compute type (Metal / CUDA / ROCm — no Vulkan build exists) is labelled via an explicit `name:` (`ds4-rocm`). **Launch:** `LaunchParams.server: Option<ServerId>` + `start --server <id>` picks the binary and (when `--backend` is unset) the backend; the selector→binary lookup keys on the owning server; the pick persists in `last_params`. `DiscoveredModel.routed_backend` became `supported_backends: Vec<String>` (priority-ordered) — the `list`/`list --json` "backend" column + right-pane badge render all (`ds4|llamacpp`), and `resolve_backend_for_launch` walks the list. `status` grew a `servers` array (`{id, backend_id, binary, name, devices}`, mirrored into CLI `status --json`) that **replaces** the old flat `device_catalog` (removed — every launch-device surface reads `servers` now); the TUI daemon-pane `server` row groups by backend (`…/build-hip/bin/llama-server (rocm|vulkan) · …/ds4-server (ds4)`). `split_mode` gained `tensor`; the picker's `main_gpu` ring sizes to the real device count (was hardcoded `0-3`). **TUI server knob:** the launch picker shows a `server` row under `preset` when a model has >1 compatible server; cycling it re-scopes the Device row + multi-GPU gating to that server's devices and, on a cross-backend switch (deepseek4 ds4 → llamacpp), swaps the knob set; the pick rides `LaunchParams.server` and reopens from `last_params` (`last_params_list` projects `server`). The picker's device list + multi-GPU gate scope to the **selected** server, not a flat host catalog. **Multi-device inline toggle:** the Device row is a checkbox list scoped to the selected server — `←/→` (`walk_device_cursor`) walk a cursor, `Space` (the `toggle_device` binding, `LaunchPickerState::toggle_focused_device`) toggles the cursor GPU in/out; the value serializes catalog-ordered (`ROCm0,ROCm1`), and selecting all N or clearing the last one normalizes to unset (llama.cpp default = all GPUs). `LLAMASTASH_DEBUG_FAKE_GPUS=N` (debug builds) now fans out the **launch** device catalog too (`server::debug_fake_multi_gpu`, sibling of the host-metrics one), so this is exercisable on a single-GPU host. **Right-pane badge** renders **all** of a selected model's `supported_backends` (` ds4  llamacpp `, no longer hiding llama.cpp when a second engine also serves it); every model with a resolved backend carries a chip, including plain llama.cpp-only rows. **`doctor` server advisory:** additive config-only findings via the new config-only `Backend::config_servers` hook (`config_server_catalog` / `missing_configured_servers`) — `server_binary_missing` (Warning) + `servers_configured` (Info summary); schema stays **2**. **Deferred (not gaps):** cross-server physical-GPU dedupe (no PCI id from `--list-devices`). See `docs/plans/2026-07-16-001-feat-server-abstraction-plan.md`.
- **MTP (multi-token prediction) speculative decoding — llama.cpp + ds4.** Auto-detected and enabled by default. Two capability signals resolve at **discovery scan**: an *embedded* head (`ModelMetadata.mtp` from `{arch}.nextn_predict_layers > 0` — Qwen3.5/3.6, GLM-4.x, DeepSeek) and a *separate* head (`DiscoveredModel.mtp_head` via `scanner::find_mtp_head`, a `mtp-*.gguf` sibling — Gemma-4 shape). `mtp-*.gguf` heads are excluded from the launchable catalog like mmproj (`is_mtp_companion`). **Enable is a launch-only tri-state** `MtpEnable { Auto, On, Off }` (`--mtp auto|on|off`) — **no `config.yaml` entry**; persisted in `last_params`/presets like any launch choice. `compose_and_spawn` resolves it against real capability into a transient `LaunchParams.mtp_directive` (`resolve_mtp_directive`), and llama.cpp's `compose` (now in `backend/llama_cpp/compose.rs`) emits `--spec-type draft-mtp` (+ `--model-draft <head>` for a separate head, + `--spec-draft-n-max N` from the neutral `mtp_draft_n` knob) **before `--fit-ctx`** so `--fit` is MTP-aware. The per-step draft count is the **neutral `LaunchParams.mtp_draft_n`** (`--mtp-draft-n`, byte-stable persisted key `mtp_draft_n`) — llama.cpp maps it to `--spec-draft-n-max`, ds4 to its `mtp_draft` native knob (both in `resolve_native_knobs`), so the one flag works on either backend and no backend-specific scalar rides the neutral IR. **Hard gate (KD1):** the flag is emitted only when genuinely capable — emitting it on a non-MTP model is a hard server launch failure, so a force-on (`--mtp on`) against a non-capable model **warns and skips**; a non-chat launch (embedding/rerank) never speculates. **KD3** — a user already hand-driving speculation through `extras` **defers entirely** — is a backend predicate (`Backend::speculation_set_in_extras`, default off; llama.cpp matches `--spec-type` via the neutral `extras_have_flag`), asked before `resolve_mtp_directive` so the generic path names no flag. **DeepSeek-V4 MTP is ds4-only** — ds4's `--mtp`/`--mtp-draft`/`--mtp-margin` native knobs, auto-pairing a sidecar via `Ds4Backend::resolve_native_knobs` (gated on `MtpEnable != Off`); antirez's sidecar is never fed to llama.cpp's `--spec-type`. The MTP path **names no backend** (KD9): llama.cpp flags + its log-format parse live in `backend/llama_cpp/` (`compose` + `telemetry`), ds4's in `backend/ds4/`, detection is generic/header-keyed. **Surfaces:** a `↯` capability glyph in the TUI (`discovery::MTP_LEGEND`, help-overlay legend), a generic `PickerField::Mtp` launch-picker cycle row (auto/on/off) shown only for MTP-capable models (any backend), and `status` per-model `params.mtp` `{enable, active, acceptance, draft_accepted, draft_generated}` — `active` (`Backend::mtp_active`) and `acceptance` (`Backend::draft_acceptance`, only read for an active launch) come from the **owning backend**, not the generic directive: llama.cpp reports active off the emitted `--spec-type` and parses its own `draft acceptance = …` log line; ds4 reports active off its paired `--mtp` head and publishes **no** acceptance figure — verified against the ds4 sources at upstream HEAD (2026-07-28): its MTP counters are all debug-env-gated and per-decode-step, and the one cumulative `accept_rate` belongs to DSpark and prints at session close, so `acceptance` stays null by design. **`ssd_streaming` and a paired `mtp` head are mutually exclusive in ds4-server** (it exits *after* loading the full model), so `mtp_stream_conflict` reconciles them pre-spawn: an auto-paired head yields to streaming, auto-streaming yields to a user-set head, and two explicit choices are refused via `NativeKnobResolution::refusal` (the generic pre-spawn refusal channel, checked in `compose_and_spawn`). **`pull` companion fetch:** `download_repo` now also grabs mmproj / MTP-head siblings (one per kind by default — `--no-companions` / `--all-companions`; same-repo name-pattern only), closing the standing gap where `pull` never fetched mmproj. See `docs/plans/2026-07-14-001-feat-mtp-speculative-decoding-plan.md` and `docs/usage.md#mtp-speculative-decoding`.
- **CLI color policy.** Every human-readable output uses ANSI colors when stdout is a TTY, `NO_COLOR` is unset, and `--no-colors` was not passed. Any one of those three conditions silences color. `--json` output is byte-stable regardless of the color policy. Padded report tables (`list`, `status`, `presets list`, `favorites list`, `last-params`, `daemon status`) are TTY-gated by the same three off-conditions: when piped or color-disabled, every command emits the same `\t`-separated rows as before so `awk -F\t` / `column -t` pipelines keep working. Agents pin against `--json`, not the TTY rendering.
- **Single binary, three roles.** The TUI, CLI, and daemon are all `llamastash`. Daemon spawns on demand when TUI/CLI attach and find the socket missing.
- **Catppuccin Macchiato is the default theme.** Five themes ship total (Macchiato, Latte, Gruvbox Dark, Solarized Dark, Monochrome). Themes are hard-coded palettes; no dynamic loading.

## Dev commands: `make` / `cargo`, never global `llamastash`

When handing the user (or another agent) commands to run **for a development
task**, always give `make <target>` or `cargo …` forms, never a bare global
`llamastash <args>`:

- Prefer `cargo run -- <args>` (e.g. `cargo run -- daemon start --lemonade`) or a
  `make` target (`make run`, `make test`, `make lint`, `make doc`, `make render`).
- For a stable binary path across many client calls, use `cargo build` then
  `./target/debug/llamastash <args>` (still the working-tree build, not the
  installed one). Isolate side-by-side daemons with `LLAMASTASH_STATE_DIR` and a
  non-default `--proxy-port` so you never touch the user's real daemon.
- Never tell the user to run a bare `llamastash <args>`: that resolves to
  whatever is installed on `PATH`, not their working tree, so it will not reflect
  the change under test.

Reserve a bare global `llamastash` only for genuine LLM-management work the user
is actually doing with the tool (serving / managing real models), not for
exercising or verifying code changes.

## Build, test, lint

```bash
cargo build                                                # release: cargo build --release
cargo test --features test-fixtures                        # full suite — required for CI parity
cargo test --features test-fixtures --test <name>          # one integration binary
cargo test --features test-fixtures <substring>            # filter by test name
cargo fmt --all -- --check
cargo clippy --all-targets --features test-fixtures -- -D warnings
make audit                                                 # maintainer audit bundle: Outputs to `target/audit`. Tests + release build + deps/security/unsafe/coverage artifacts
make audit-summary                                         # headline summary from `target/audit`
```

Look at the `Makefile` for more commands, including `make uat-*` for the manual UAT runs.

`--features test-fixtures` is required for the integration suite. It enables:

- the `fake_llama_server` binary (`tests/fixtures/fake_llama_server.rs`) that integration tests spawn instead of a real `llama-server` — answers `/health`, `/v1/models`, streaming `/v1/chat/completions`, `/v1/embeddings`, `/v1/rerank`, with deliberate failure-injection markers in request bodies.
- the `_test_sleep` IPC method used by drain-timeout tests (never exposed in release builds because the feature is opt-in and not in the default set).
- `src/gguf/test_fixtures` (`FixtureBuilder`, `build_minimal_gguf`).

`--features uat` enables the maintainer-only `llamastash uat` subcommand (hidden from `--help`; the release binary on crates.io and Homebrew bottles never ships it). The orchestrator drives a 5-step real-hardware lifecycle and emits a structured JSON report — see [`docs/testing/hardware-uat.md`](docs/testing/hardware-uat.md) for setup and run instructions. The release workflow audits that `--features uat` is never enabled in shipped binaries, while its pre-build `release-gate` job runs cold CPU-only UAT on Linux and macos.

Two-space indentation is enforced by `rustfmt.toml`. Clippy denies `shadow_unrelated` crate-wide; rename rather than reuse `let` bindings inside the same scope.

## End-to-end CLI validation (required for user-visible changes)

A passing `cargo test` is necessary but **not sufficient**. After any change to a CLI subcommand, IPC response shape, daemon lifecycle, TUI panel, or anything else a user would notice, **actually run the CLI** against a real daemon and verify the behavior with your own eyes. Test suites can pass while the binary is broken — stale daemons, missed env vars, deferred restarts, schema drift between client and server all hide behind green CI.

Minimum E2E loop after any user-facing change:

```bash
cargo build --bin llamastash
# If a daemon is already running from an older binary, kill it first —
# the running process is using a deleted binary and won't pick up your
# changes until restarted:
target/debug/llamastash daemon stop                     # or: --force when stale
target/debug/llamastash daemon start                    # backgrounds + writes runtime.json
target/debug/llamastash daemon status --json | jq .     # shape sanity-check
target/debug/llamastash list                            # the change you just made
target/debug/llamastash status --json | jq .daemon      # confirm new IPC fields
target/debug/llamastash                                 # TUI: pan through every visible panel
```

For TUI changes specifically, **launch the TUI and look at the panel you touched** — golden snapshots catch byte-exact regressions but not "the field is empty in real life because the running daemon doesn't surface it yet." A fresh daemon restart is part of the validation.

Agents (no interactive terminal) can drive the TUI in a pty. Two drivers live
under `scripts/tui/` (both render the live screen as plain text via `pyte`;
both inherit `LLAMASTASH_*` env vars, so pair them with an isolated state dir).
See `scripts/tui/README.md` for the full contract — when to use which:

- **`scripts/tui/tui_drive.py`** — quick, throwaway inspection. JSON-on-argv,
  zero deps beyond `pyte`, prints each screen to stdout. No assertions, no exit
  code. Use it to *look* at a flow. Example — stage the launch picker on a
  filtered row and read the staged form:

  ```bash
  python3 scripts/tui/tui_drive.py '[["", 4, "boot"], ["/gemma|<enter>", 2, "staged"]]'
  ```

- **`scripts/tui/harness.py`** — repeatable UAT / regression checks. A
  line-based program file with `expect:`/`refute:` assertions, PASS/FAIL
  accounting, a non-zero exit code for CI, and persisted `snap:` screenshots.
  Use it to *gate* on a flow. Needs `pexpect` on top of `pyte`; it also answers
  crossterm's `ESC[6n` so the TUI can't abort mid-init:

  ```bash
  python3 scripts/tui/harness.py scripts/tui/example.prog /tmp/ls-tui-out
  ```

One-frame renders without key input are cheaper via the built-in
`llamastash --render --render-size 160x45` (`make render` renders all sizes).

When E2E surfaces a regression the test suite missed (stale daemon, missing IPC field, wrong port, etc.), add a regression test before fixing — that's the gap the suite needs covered.

## Running the daemon locally

```bash
cargo run -- daemon start                # foreground; logs to terminal, Ctrl-C to stop
cargo run -- list                        # in another terminal
cargo run                                # opens the TUI against the same daemon
cargo run -- daemon stop
```

Attach surface: clients read `$XDG_STATE_HOME/llamastash/runtime.json` (mode `0600`) to discover the daemon's control-plane URL + bearer token. The file is `{"schema_version":1,"ipc_url":"http://127.0.0.1:<port>","ipc_token":"<bearer>","started_at_unix":<ts>,"daemon_pid":<pid>}` — the URL/token live under `ipc_url` / `ipc_token`. Note that under a `LLAMASTASH_STATE_DIR` override the file sits directly in that dir (no `llamastash/` subdir). Override the state directory with `LLAMASTASH_STATE_DIR=/path` for side-by-side daemons, or use `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` (both required together) for clients that don't want to read runtime.json. If wedged, deleting `runtime.json` and `daemon.pid` in the state dir is safe — next `daemon start` rebinds clean.

For full path isolation (e.g. integration tests, the maintainer UAT command, side-by-side daemon experiments), pair `LLAMASTASH_STATE_DIR` with `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR`, and `HF_HOME` so state, config, cache/logs, and the HF cache all redirect together. Each variable is a verbatim override; empty values are treated as unset. See `docs/usage.md §Environment variables`.

## Architecture in one breath

```
TUI / CLI ──HTTP+Bearer──► Control plane (127.0.0.1:11436, loopback, bearer token)
OpenCode / Pi / SDK ──HTTP──► Proxy listener (127.0.0.1:11434, loopback, no auth)
                          │
                          ├── Discovery (scan + watch + caches)
                          ├── GGUF parser (metadata + identity)
                          ├── Process supervisor (spawn / probe / stop)
                          ├── Resource monitor (RAM/VRAM/CPU)
                          └── Persisted state (favorites / presets / running)
```

- **Wire format.** JSON-RPC 2.0 envelopes carried in `POST /rpc` request/response bodies. `src/ipc/methods.rs` is the dispatch table; `src/daemon/control_plane.rs` is the hyper service in front of it.
- **Model lifecycle.** `Launching → Loading → Ready → Stopping → Stopped`, plus `Error{cause}`. Transitions are guarded — once Stopping or Error, the model never moves out. The supervisor health-probes `/health` every 500 ms during Loading. See `src/daemon/supervisor.rs`.
- **Process survival.** `llama-server` children get their own session via `setsid`, so they outlive the daemon. On daemon restart, an orphan sweep re-adopts each entry in `state.running` only after three-factor confirmation: PID alive, recorded port answering, and `/v1/models` advertising the recorded model. The identity match accepts either the full canonical path (older llama-server echoed the `-m` value) or a bare basename (b9245+ reports only the file name as `id`); a *differing* full-path id is still rejected as the PID-reuse guard.
- **Model identity.** `(canonical absolute path, BLAKE3 of header bytes)`. Renames survive; symlinks dedupe to target; split GGUFs collapse to shard 1.
- **Persistence.** `$XDG_STATE_HOME/llamastash/state.json` (durable user state: favorites, last-params, running snapshot) plus `runtime.json` (per-instance URL + bearer token, deleted on shutdown). Both written via the shared `util::atomic_write::write_secure` path (`*.tmp.<rand>` + `fsync` + atomic rename + parent dir `fsync`), mode `0600` on Unix. Parse failure on `state.json` → `state.json.broken-<ts>` quarantine, boot with defaults. **Named presets live in `config.yaml`, not `state.json`** — see the "Config presets" scope bullet. `config.yaml` is read/deserialized with `yaml_serde` (the maintained serde_yaml fork; the archived `serde_yaml` is not a dependency), and **every** `config.yaml` write — the presets writer and the init/cli `config::writer::merge_and_write` — goes through the one comment-safe `config::yaml_edit` primitive, so hand-written comments survive a save. A **symlinked** `config.yaml` (dotfiles repo) is followed to its target and written there (`config::writer::preflight` resolves the link; the link is preserved) — config-only, `state.json` keeps its non-following write.

## Adding a backend (keep it leak-free)

A new inference backend must be **one new file plus the minimum central wiring**, and removing one must be **deleting the file plus removing that wiring**. All backend-specific behavior lives behind the `Backend` trait (`src/backend/mod.rs`); the rest of the tree (discovery, proxy, TUI, status, doctor, `gguf/memory`, CLI) reaches backends only through trait methods + `Backends::all()` / the registry helpers (`routed_backend_for`, `refusal_for_auto_launch`, `is_infra_launch`). **No backend id-string or name may appear — in code *or comments* — outside the canonical locations below.**

**Canonical locations (the only places a backend may be named):**

1. The backend's own module — `src/backend/<name>/` (or `src/backend/<name>.rs`). **All** its logic lives here, behind trait methods: argv/launch translation + `resolve_launch_binary`, identity, capabilities, native knobs, config-derived launch knobs (`seed_launch_knobs` projects a backend's own config into the generic `backend_knobs` map, read back by `admission_ctx_floor` + `readiness_fit_gate`), the routing predicate (`auto_routes`), mode support (`serves_mode`), web-UI opt-in (`serves_web_ui`, default off), refusals (`refuses`), KV model (`kv_bytes`), the process marker (`process_marker`), availability (`available` + `installed`/`status_enabled`/`binary_path`/`status_accelerators`/`status_extra`), and — for a managed multiplexer — the `start` + `stop` overrides, `umbrella_launch_id` (the sole delegation-shape method the generic tree reads), and `supervise_at_boot` (bring the shared umbrella up at daemon boot; default no-op). Everything delegation-specific (the umbrella-unload API call, "what's resident") stays **private to the backend module**, called only from that backend's own `start`/`stop` — the trait exposes no `unload_delegated` / `resident_delegated_model`. A process-per-model backend leaves `start` / `stop` on their defaults (supervised spawn / SIGTERM), overrides no `supervise_at_boot`, and never touches lifecycle plumbing.
2. `src/backend/mod.rs` — a `pub mod <id>;` in the submodule list at the top, then in the block a `use <id>::{<Id>Backend, <Id>Config};`, a `Backends` enum variant, one arm in the `for_each_backend!` macro, one line in `Backends::all()`, and (only if the backend has a `backend.<id>:` config block) a field on `BackendConfig`.
3. Config — each backend owns its typed config struct in its **own module** (e.g. `LlamaCppConfig` in `src/backend/llama_cpp/`, `LemonadeConfig` / `Ds4Config` in theirs; all re-exported from `crate::config` for path stability). `BackendConfig` in `src/backend/mod.rs` aggregates them under the top-level `backend:` map, carried on `Config.backend`. A backend adds a field to `BackendConfig` **only if** it has a `backend.<id>:` block; it then reads its own sub-config via `ctx.backend.<id>` and its force flag via the id-keyed `ctx.backend_force` map. The YAML surface stays typed; no generic consumer names the backend elsewhere. Config-derived llama.cpp launch knobs (`jinja`/`strict_fit`/`fit_ctx_floor`) are **not** fields on `LaunchParams`/`LaunchEnv`: `seed_launch_knobs` projects them into the generic `backend_knobs` map each launch, read back only by the backend's own `compose`/`admission_ctx_floor`/`readiness_fit_gate` hooks — so no llama.cpp knob name leaks into the neutral launch IR.

`BackendChoice` (`src/launch/params.rs`) is **not** a per-backend location: it is `Auto | Explicit(String)`, so an id is just data. `--backend <id>` validates against the registry (`cli_args::parse_backend_id`), the daemon parses it via `BackendChoice::from_id`, and the wire form is the bare id string (custom serde, byte-stable). Adding a backend needs no `BackendChoice` / CLI edit.

**How the generic tree stays backend-agnostic (a new backend needs no edit in any of these):**

- **Discovery routing / `list` badge** — `auto_routes` + `routed_backend_for` record the claiming backend id on `DiscoveredModel.routed_backend`; the badge echoes it.
- **Launch selection + binary pick** — `resolve_backend_for_launch` iterates the registry (`auto_routes` + `available` + `serves_mode`, else the identity rule); the executable/port pick is `resolve_launch_binary`.
- **Launch / stop execution (process vs umbrella)** — `compose_and_spawn` does the backend-agnostic prep (validate → identity/arch → port reserve → layered knobs → native-knob auto-resolve) into a `LaunchExec`, then calls `Backend::start`; `stop_model` resolves the owner via `backend_for_launch` and calls `Backend::stop`. The defaults run the supervised child-process spawn / SIGTERM (`spawn_supervised` / `stop_supervised`); a managed-multiplexer overrides both to ensure/tear-down its umbrella + delegate/unload the model. **The caller never branches on process-vs-umbrella lifecycle for start or stop.**
- **Pre-spawn refusal** — `refusal_for_auto_launch` asks every backend via `refuses`.
- **Memory** — `gguf::memory::kv_bytes` consults `Backend::kv_bytes` over the registry (header-keyed, so it applies even on a fallback).
- **`status.backends` + `list` badge availability** — generic `Backends::all()` loops over the status hooks.
- **Orphan sweep** — `external_process_markers` / `adopted_process_name` come from `process_marker`.
- **Infra (umbrella) launches** — the daemon brings the umbrella up at boot by looping `Backends::all()` and calling `supervise_at_boot` (each backend self-gates on `available`; a process-per-model backend does nothing) — no backend named in `daemon::run_foreground`. The running-launch walkers (proxy routing, `/ui`, `status`) skip via `is_infra_launch` (`umbrella_launch_id`); idle eviction resolves the owner via `umbrella_owner` and `stop`s the delegated models it serves (which unloads them from the umbrella, keeping it up) — no delegation vocabulary in the sweep; the TUI shared-marker + tab-gating key on `is_managed_multiplexer`.
- **Proxy** — the embed/rerank refusal keys on `serves_mode`; `/ui` on `serves_web_ui` (default off — only a backend that genuinely serves a browser UI opts in).
- **TUI display** — the header badge is a generic id chip; the running native-knob view uses `native_knobs_for`; `DEFAULT_BACKEND_ID` / `default_backend` / `BackendChoice::from_id` cover the "default backend" / id→choice sites.

**Rule of thumb:** if you're about to write `== "<backend>"`, `resolve_<backend>_binary`, `<Backend>::new()`, or name a backend in code or a comment *outside* the two locations above, add or call a trait method / registry helper instead. (User-facing *strings* — an error message naming the actual resolved backend — are fine; derive them from `backend.id()`, don't hard-code them.) Neutrality guards: `routed_backend_for` with a synthetic id, and `backends_forward_defaulted_methods_to_variants`.

## CLI agent surface (Units 8 + 10/13)

Every non-interactive read-and-mutation command supports `--json` and emits a wrapped object: `{"models":[…]}`, `{"favorites":[…]}`, `{"presets":[…]}`, `{"last_params":[…]}`, `{"stopped":[…],"count":N}`, `{"steps_ran":[…],"install":{…},"model":{…},"config":{…},"smoke":{…},"hardware":{…}}` for `init`, `{"schema_version":2,"findings":[{"id":…,"severity":…,"message":…,"fix_hint":…,"safe_to_log":true}],"baseline":{…},"hardware":{…}}` for `doctor` (schema `2` added the `hardware` section and the `memory_drift` / `gtt_hint` finding ids; the additive `ds4_unavailable` / `ds4_disabled` finding ids landed with the ds4 backend, and `server_binary_missing` / `servers_configured` (the configured-server advisory) with the server abstraction — all additive, so none bump the schema; readers refuse only versions above their max). `llamastash config` is interactive: it opens the active config file in `$EDITOR`; `llamastash config bindings` prints every effective keybinding as YAML for transfer between config files. Neither form has JSON output. Stable shapes for agent consumption. Exit codes follow `<sysexits.h>` numerically but with project-specific meanings — pin against the table in `src/cli/exit_codes.rs`, not the libc constants. `stop --all` in a non-TTY context refuses without `--yes`. The IPC `capabilities` method enumerates supported methods so clients can feature-detect.

**One catalog-row serializer (0.1.0 breaking).** `list --json`'s `models[]` rows are now **byte-identical to the IPC `list_models` rows** — a single `CatalogRow` serde impl (`src/launch/resolve.rs`) is the only definition of the wire shape; the daemon serializes it, the CLI and TUI deserialize it (no second flat serializer). So GGUF-derived fields moved **under a nested `metadata` block** (`arch` / `quant` / `native_ctx` / `mode_hint` / `parameter_label` / `weights_bytes` / `total_parameters` / `tokenizer_kind` / `has_chat_template` / `has_reasoning_hint` — the old flat top-level `arch` / `native_ctx` / etc. are **gone**), and `list --json` additively gained the `mtp` (`{embedded_layers, separate_head}`) and `multimodal` (`{vision, audio}`) capability blocks it previously dropped. Top-level keys stay `name` / `path` / `parent` / `source` / `backend` / `supported_backends` / `split_siblings` / `parse_error` / `display_label` (+ `model_id` only when set, + the CLI-only `status` object for a running row). The deprecated `metadata.reasoning_hint` alias is dropped — read `has_reasoning_hint`. `show --json` shares the same row shape: its envelope **is** the serialized `CatalogRow` with the show-only `size` / `arch_defaults` / `last_params` / `running` sections layered on top (`src/cli/show.rs::assemble_envelope`) — never a second hand-built projection. The human `list` table mirrors the TUI Models list columns: `NAME ARCH PARAMS QUANT CTX SIZE MODE [BACKEND] STATUS [DEVICE]`, with DEVICE gated on the same "some single server offers >1 device" rule as the TUI (`cli::resolve::multi_device`) and the TUI's `all`-for-running-default-launch cell semantics.

### Exit-code table

| Code | Constant               | Meaning                                        |
| ---- | ---------------------- | ---------------------------------------------- |
| 0    | `SUCCESS`              | Success                                        |
| 64   | `USAGE`                | Bad CLI usage (clap rejection)                 |
| 65   | `DAEMON_UNREACHABLE`   | Daemon socket missing / timeout                |
| 66   | `MODEL_NOT_FOUND`      | Model reference matched zero or multiple       |
| 67   | `LAUNCH_FAILED`        | `start_model` accepted but supervisor failed   |
| 68   | `STOP_FAILED`          | `stop_model` / `stop_all` returned an error    |
| 69   | `PULL_FAILED`          | Standalone `llamastash pull` failed            |
| 70   | `BINARY_NOT_FOUND`     | `llama-server` not on PATH / config            |
| 71   | `UNKNOWN`              | Catch-all (anyhow bubble-up)                   |
| 72   | `INIT_ABORTED`         | Init pre-smoke abort (integrity / daemon stop) |
| 73   | `INIT_DOWNLOAD_FAILED` | Init's model-download step failed              |
| 74   | `INIT_SMOKE_FAILED`    | Init reached smoke but probe didn't pass       |

`llamastash uat` (maintainer-only, `--features uat`) emits a parallel
set of synthetic codes inside its JSON report's
`failure_summary.exit_code` — `10` (preflight backend mismatch), `11`
/ `12` / `13` (smoke HTTP / parse / status), `124` (timeout), `130`
(SIGINT). Full table in [`docs/testing/hardware-uat.md`](docs/testing/hardware-uat.md) §UAT synthetic exit codes.

### `status` IPC fields (kdash-style dashboard wiring)

The `status` method response carries the following top-level objects beyond the legacy `models` / `external` / `gpu` shapes:

- `host` — always an object (no `null`). Populated by the daemon's host-metrics sampler (`daemon.metrics_interval_secs`, factory 1 Hz). Fields: `cpu_pct` (f32, 0..=100 mean across cores), `ram_used_bytes` / `ram_total_bytes` (u64), `gpu_util_pct` / `gpu_mem_used_bytes` / `gpu_mem_total_bytes` / `gpu_temp_c` (each `Option`, omitted on backends that don't surface them), `gpu_backend` (string), `gpu_device_count` (u32), `gpu_devices` (`Option<[…]>`, present only on multi-GPU/multi-backend hosts), `unified` (bool — GPU shares one physical pool with the CPU: Apple Silicon, an AMD/Intel UMA APU, or an NVIDIA coherent-UMA part such as GB10), `uma_shared_total_bytes` / `uma_shared_used_bytes` (`Option`, the system-RAM-backed portion of a UMA pool — AMD GTT), and `uma_class_source` (`Option`, how the unified-vs-discrete verdict was reached: `"explicit_dxgi_uma"`, `"nvml_no_framebuffer"`, `"carve_signature"`, or `"discrete"`; `null` on Apple Metal and non-classifying backends).
  - `gpu_backend` values: `"cpu_only"`, `"nvidia"`, `"amd"`, `"apple_metal"`, `"unknown"` (Vulkan-only fallback), `"multi"` (two or more backends each found a device), or the sentinel `"unsampled"` returned in the brief window between daemon start and the sampler's first tick. Clients gating UI on backend kind should treat `"unsampled"` as "not yet known", not as a real reading.
  - `gpu_devices` — when two or more GPUs are visible, one row per device: `{selector, backend, name, total_memory_bytes, used_memory_bytes?, utilization_pct?, temperature_c?}` (`?` = omitted when the vendor tool doesn't surface it). `selector` is a backend-prefixed *display* label (`Nvidia0`, `Amd0`), not a `--device` value — launch selection draws from a separate `llama-server --list-devices` catalog. Lets a dashboard render per-card stats instead of one aggregate row; omitted on single-GPU hosts.
- `daemon.build` — semver string from `CARGO_PKG_VERSION`; matches `--version`.
- `daemon.server_path` — absolute path to the `llama-server` binary the daemon resolved at startup. `null` when unset.
- Per-model rows in `models[]` carry `latest_rss_bytes: Option<u64>` and `latest_cpu_pct: Option<f32>` from the per-launch resource sampler. Both are `None` until one tick (~1 s) after launch. Lemonade **delegated** rows carry the shared umbrella process's reading (they all run inside it), not a per-model figure — the TUI flags these with a `*` shared-marker; the umbrella's own row is hidden from the TUI running list (kept in `status`/CLI).
- Per-model rows in `models[]` also carry a config-preset hint: `preset_count: u32` (how many presets the model resolves, per-model ∪ arch) and `default: Option<String>` (the config-only default preset name, or `null`). The full set lives in `presets_list`. Both mirror byte-for-byte into `cli/output.rs::status_json`.
- Per-model rows in `models[]` carry `backend: String` — the backend the launch actually resolved to (`llamacpp` / `ds4` / `lemonade`), stamped on the running snapshot at spawn (honest even for a compatible file launched `--backend llamacpp`). `params.backend_knobs` carries the native (ds4) knobs the launch dispatched with, and `params.server` carries the server (build/binary) id the launch picked (`null` when it took the backend's default) — the TUI running view renders it as a read-only `server` row. All mirror into `cli/output.rs::status_json` (`backend` at the row root; `backend_knobs` / `server` inside `params`).
- `proxy` — `{enabled: bool, listen: Option<String>, status: "disabled" | "listening" | "port_in_use" | "unbound", bind_error: Option<String>, ui_url: Option<String>}`. `listen` is the attempted address (`"127.0.0.1:<port>"`) on every state except `disabled`, where it is `null`. `bind_error` is non-null only on `unbound` (unexpected bind failure beyond port-in-use). `ui_url` is the port-stable web-UI origin (`"http://<listen>/ui/"`), non-null **only** when `status: "listening"` (the only state that serves `/ui`); the CLI `status` renders it as a `web ui` row.

`status.gpu` is **live**: when the host-metrics sampler is attached, it reflects the freshest GPU probe. Late driver loads / hotplug changes propagate within one `gpu.reprobe_interval_secs` period (default 60 s) rather than staying pinned to the boot snapshot — setting that key to `0` opts out and pins `status.gpu` to the boot reading.

All of these fields land in the CLI's `status --json` output too (`src/cli/output.rs::status_json`), so agents that consume the CLI surface get the same view as raw IPC clients.

## Conventions

- Less jargons and more straightforward facts and numbers. Keep jargon only if it helps marketing or understanding.
- Until first release is made, do not add code/docs etc for backward compatibility/legacy etc.
- Prefer using commands from the Makefile for common tasks (`make build`, `make test`, `make lint`) to internalize the standard flags and avoid mistakes like forgetting `--features test-fixtures` on tests.
- Conventional-commit prefixes: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`. Unit-scoped variants are common (`feat(unit8): …`).
- Inline `#[cfg(test)] mod tests` per file is the default; integration tests under `tests/` for daemon-spawning scenarios.
- Comments explain **why**, not **what** — keep them concise and clear, and add one only when it carries value the code itself can't show. Don't narrate or restate the logic in prose; a comment that just paraphrases the next line is noise — delete it. No multi-paragraph doc blocks unless the constraint is genuinely non-obvious. Don't reference task IDs or PR numbers in comments — those rot.
- No `#[allow(...)]` without a one-line reason.
- **Keybinding labels are never hardcoded in UI.** Help bars, footers, hints, popup affordances ("Press `q` to quit", `[Ctrl+S] save`, etc.) must derive their key text from the active `KeyMap` (`src/tui/keybindings.rs` — `Binding::label` / `Binding::description`), never from inline string literals. The keymap is the single source of truth so that user overrides from the config file are reflected everywhere the binding is surfaced. When adding a new action: add it to the `Action` enum and the appropriate `*_BINDINGS` slice with a `label`/`description`, then look that binding up at render time (e.g. via `KeyMap::bindings_for(Focus)` or a focused helper) — do not duplicate the literal key string in the widget.
- **TUI glyphs are single-cell, text-presentation BMP symbols.** Capability / status / title glyphs (e.g. `◌ ◐ ○ ● ◉ ▲` in `src/tui/glyphs.rs`, `♪` audio, `↯` MTP in `discovery::MTP_LEGEND` / `Multimodal::LEGEND`) must be monochrome **text-presentation** codepoints that occupy exactly one terminal cell. Do **not** use emoji-presentation codepoints (e.g. `⚡` U+26A1, or anything in an emoji block / with a default emoji variation selector) — terminals render them double-width and colored, which breaks column alignment in the panes. When adding a glyph, pick from the geometric / arrow / symbol text ranges already in use and eyeball it against the neighbouring glyphs (`--render`) before committing.

## Protected artifacts

Do not flag these for deletion or `.gitignore` during reviews — they are part of the engineering record:

- `docs/brainstorms/*` — origin requirements.
- `docs/plans/*.md` — implementation plans (living docs with progress checkboxes).
- `docs/solutions/*.md` — solution memos when present.
- `docs/benchmarks/*` — methodology doc, results pages, and the raw per-host run JSONs under `runs/` and `overhead/`. These are the published evidence behind the README's positioning claims; deleting or rewriting prior dated pages destroys the reproducibility contract documented in `docs/benchmarks/methodology.md`.
- `.context/compound-engineering/ce-review/*` — multi-agent review run artifacts.

## Built-in defaults table maintenance

The static `(architecture, gpu_backend) → TypedKnobs` defaults table
lives in `src/launch/defaults_table.rs`. When `data/benchmark-snapshot.json`
adds a new recommender pick, audit the table coverage:

- The table no longer pins `n_gpu_layers` on any (arch, backend):
  offload placement is delegated to llama-server's `--fit` (a layer-less
  `n_gpu_layers` is seeded `Auto` by the resolver and emits no `-ngl`).
  Architectures missing from `COVERED_ARCHS` fall through to the empty
  `*` row.
- `FLASH_ATTN_ELIGIBLE` is opt-in only — extend it once measurement
  confirms a new architecture supports flash-attn cleanly on NVIDIA
  / Apple Metal. AMD/HIP flash-attn coverage stays uneven; leave to
  user override via `config.yaml arch_defaults`.
- Folklore-only flags (`mlock`, `no_mmap`, KV-cache quant types) stay
  unset at the table level until measurement supports them.

A TODO entry tracks the AMD/HIP `no_mmap` measurement follow-up.

## Common gotchas

- The CLI/TUI/daemon are one binary. `cargo run -- daemon start` and `cargo run` (TUI) attach to the same daemon via `runtime.json` (URL + bearer token) under the state dir — running two `cargo run` invocations in parallel without distinct `LLAMASTASH_STATE_DIR` will both attach to the same daemon.
- Integration tests bind to a temp dir per test (`unique_temp_dir(label)`); never share `state_dir` between tests, or they'll race the lockfile.
- `cargo build` (without `--features test-fixtures`) intentionally omits `fake_llama_server` and `_test_sleep`. CI runs both with and without the feature to catch accidental dependencies on test-only surface.
- `cargo install` artifacts deliberately exclude `src/gguf/test_fixtures` and the `_test_sleep` IPC method via feature gating — don't move them out from behind `#[cfg(any(test, feature = "test-fixtures"))]`.
- Release pipeline runs `publish-homebrew`, `publish-site`, and `publish-cargo` in parallel after the upstream build matrix; a single-job failure leaves channels diverged. Recovery is to re-run the failed job from the Actions UI (or `gh run rerun --failed <run-id>`). Pre-release tags (`vX.Y.Z-<suffix>`) skip all three downstream jobs by design so dry runs never write to external repos.
- `LLAMASTASH_BENCH_DISABLE_DEFAULTS=1` is a bench-internal escape hatch read by `src/launch/params.rs::resolve_layered`. When set, the resolver collapses to "User-labeled layers only" — preset/last-used/yaml-arch/built-in-arch defaults all skip. `scripts/bench/` sets it so `llamastash start` produces byte-identical argv to raw `llama-server` for Suite-A overhead comparison. Never set this in production runs; it disables the auto-tuning the launcher exists to do.

## Release & distribution

- Steady-state contract: `git tag vX.Y.Z && git push --tags` triggers `.github/workflows/release.yml`, which first runs `release-gate` (tests + cold CPU-only UAT on Linux and macos), then builds 4 target tarballs, uploads release assets, pushes the new Homebrew formula to `llamastash/homebrew-llamastash`, pushes the verified `install.sh` mirror to `llamastash/llamastash.github.io`, and publishes to crates.io. The full pipeline takes ~10-15 minutes on cold caches plus the pre-build validation time.
- First-time setup (creating org repos, minting tokens, configuring Pages) lives in [`docs/runbooks/release-0.0.1-bootstrap.md`](docs/runbooks/release-0.0.1-bootstrap.md) — every step has a `gh` CLI primitive.
- Pre-tag CI guards in `release-readiness` (ci.yml) catch most release-breaking PRs before tag time: `cargo publish --dry-run --locked`, crates.io name-availability against a publisher allowlist, CHANGELOG `[Unreleased]` header presence, Cargo.toml ↔ CHANGELOG version alignment, packager.py unit tests.
- Action SHA-pinning policy: trust-critical actions in release.yml (those alongside secrets) are pinned to commit SHAs; first-party `actions/*` are tag-pinned. Updates flow through Dependabot PRs (`.github/dependabot.yml`).
