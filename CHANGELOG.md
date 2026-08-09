# Changelog

All notable changes to LlamaStash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

## [Unreleased]

### Added

- `llamastash config` opens the active config file in `$EDITOR`.
- `llamastash config bindings` prints every effective keybinding as YAML for copying between configs.
- The launch picker's Device row uses the same `◀ ▶` single-stop style as other knobs — one GPU shown as `[x] ROCm0  ·  2 of 3`, `←/→` walk the cursor, `Space` toggles it (a high-priority `Space:choose` hint shows while the row is active), scoped to the selected server (unset = all GPUs); the read-only running view gains a `server` row showing which build served the model; the right-pane header badges every backend a model supports as separate chips (e.g. ` ds4 ` ` llamacpp `); the TUI model list's Backend column now renders after Mode and gains a Device column that reads `all` for a running all-GPU launch (the `llamastash list` CLI table keeps its leaner `NAME ARCH PARAMS QUANT CTX SIZE [BACKEND] STATUS` column set); and `doctor` gains a configured-servers advisory (warns on a missing `servers[].binary`, summarizes the rest).
- **MTP (multi-token prediction) speculative decoding** ([#51](https://github.com/llamastash/llamastash/issues/51)). llamastash auto-detects MTP-capable GGUFs (embedded nextn head, or a separate `mtp-*.gguf` drafter sibling) and enables it by default. llama.cpp launches with `--spec-type draft-mtp` (+ `--model-draft` for a separate head), fit-aware. Roughly a 2x decode speedup at high draft acceptance, and output-equivalent to non-speculative decoding. Control it per launch with `--mtp auto|on|off` (no config-file entry) and `--mtp-draft-n N`, or the TUI launch picker's `mtp` cycle row (shown only for capable models); forcing it on a non-capable model warns and skips rather than failing the launch. A `↯` glyph marks MTP-capable models in the TUI, and `status` reports each launch's MTP state + live draft-acceptance rate. ds4 gains the same via its `--mtp`/`--mtp-draft`/`--mtp-margin` native knobs, auto-pairing a sidecar found next to the model. See `docs/usage.md#mtp-speculative-decoding`.
- **`pull` fetches companion files.** A `pull` now also downloads the model's mmproj projector and MTP draft-head siblings from the same repo (one per kind by default, `--all-companions` for every variant, `--no-companions` to skip) — so a multimodal or speculative-decoding model arrives ready to launch instead of missing its companion.
- New `gpu.enable_vulkan_probe` config toggle (default `true`). When `false` the daemon skips the Vulkan fallback probe (`vulkaninfo -j` / `--summary`) — no subprocess spawns at startup or during the periodic hotplug re-probe. Useful when a native NVIDIA/AMD/Metal probe already covers all GPUs. Config-only, no CLI/env surface.
- New `gpu.reprobe_interval_secs` config key (default `60`). Controls how often (in seconds) the daemon re-runs the full vendor chain to catch GPU hotplug, late driver loads, or CpuOnly → detected transitions. `0` disables periodic re-probes entirely (initial probe still runs at daemon start). CpuOnly and Vulkan-only hosts skip re-probes even when `> 0` — they have no live metrics to refresh. Config-only, no CLI/env surface.
- New `daemon.idle_timeout_secs` config key (default `0`). When > 0, the daemon auto-shuts down after the specified seconds with no running models AND no active IPC connections. A background monitor checks every 5 s; once both conditions hold for the full duration, the shutdown token fires. Useful on personal desktops. Config-only, no CLI/env surface.
- New `daemon.probe_interval_secs` config key (default `1`). Controls the host-metrics live-sampler tick interval (CPU%, RAM, GPU util/temp/VRAM). Factory `1` (1 Hz). Raising this reduces the frequency of `nvidia-smi` / `rocm-smi` calls at the cost of less responsive dashboard numbers. Clamped to `1..=60`. Config-only, no CLI/env surface.

### Changed

- **BREAKING: per-backend `servers: []` arrays replace `binary` / `additional_binaries`.** Each backend's binary config is now a list of build/binary variants — `backend.llamacpp.servers: [{binary, name?}]` (first = default; each is a selectable "server"), and single-entry lists for `backend.ds4.servers` / `backend.lemonade.servers`. Every configured build is listed (no dedup across builds), so one install can offer CUDA / ROCm / Vulkan launches, each with a stable id (`llamacpp-rocm`, `llamacpp-vulkan`, or a `name:`-overridden `ds4-rocm`; a device-less engine like ds4/lemonade is the bare backend id). New `start --server <id>` picks a build (and its backend); the TUI launch picker gains a `server` row (under `preset`, shown when a model has >1 compatible build) that re-scopes the device list + multi-GPU gating to the picked build and reopens on the last-used one. `status` gains a `servers` array (id, backend, binary, devices) that **replaces** the old flat `device_catalog`, and the model list surfaces `supported_backends` (all backends that can serve a model, e.g. `ds4|llamacpp`). No migration (pre-1.0) — update `config.yaml` (see `config.example.yaml`); `--llama-server` / `LLAMASTASH_LLAMA_SERVER` now set the first server. See `docs/plans/2026-07-16-001-feat-server-abstraction-plan.md`.
- **BREAKING: all backend config is grouped under a `backend:` map.** The top-level `llama_server_path` / `llama_server_paths` / `jinja` / `strict_fit` / `fit_ctx_floor` keys and the separate `lemonade:` / `ds4:` blocks now live under `backend.{llamacpp,lemonade,ds4}`. No migration (pre-1.0) — update `config.yaml` to the new shape (see `config.example.yaml`). CLI flags and `LLAMASTASH_*` env overrides are unchanged. Internally the llama.cpp-specific launch knobs no longer leak into the generic launch structs: `status` running rows now surface `jinja` / `strict_fit` / `fit_ctx_floor` under `params.backend_knobs` rather than a top-level `jinja` field.
- **BREAKING: `state.json` no longer carries or imports presets.** Add any named presets you still need under `config.yaml`'s `presets:` key; it is now the only preset source.
- **BREAKING: `list --json` uses one catalog-row shape, identical to IPC `list_models`.** The CLI and daemon shared no serializer before, so `list --json` silently lagged the richer IPC shape. They now go through a single `CatalogRow` (serialize on the daemon, deserialize on the CLI/TUI). GGUF-derived fields moved under a nested `metadata` block (the flat top-level `arch` / `native_ctx` / `quant` / `mode_hint` / `parameter_label` / `weights_bytes` are gone), and `list --json` additively gained the `mtp` and `multimodal` capability blocks it used to drop. The deprecated `metadata.reasoning_hint` alias is dropped (use `has_reasoning_hint`). Update any `list --json` consumers: `.arch` → `.metadata.arch`, `.native_ctx` → `.metadata.native_ctx`, etc.

### Fixed

- NVIDIA coherent-UMA GPUs (GB10 / DGX Spark, Jetson) are no longer dropped from detection ([#59](https://github.com/llamastash/llamastash/issues/59)). These parts share one memory pool between CPU and GPU, so `nvidia-smi` reports the memory columns as `[N/A]`; the probe treated the unparseable size as a malformed row and discarded the device, leaving `doctor` to report `GPU  CPU only` on a machine with a GB10 in it. The row is kept now and its memory read from the system pool (available, not free — the page cache is reclaimable), surfacing as `uma_class_source: nvml_no_framebuffer`.
- The proxy no longer starts a second `llama-server` for a model that is already loading ([#56](https://github.com/llamastash/llamastash/issues/56)). A request landing inside the load window now waits for the in-flight launch, whether it came from the proxy, the CLI, the TUI, or a boot restore, instead of spawning a duplicate that held VRAM until the idle sweep.
- Proxy auto-start picks the launch mode from the endpoint that triggered it ([#56](https://github.com/llamastash/llamastash/issues/56)). A `/v1/rerank` call starts the model in rerank mode instead of inheriting the GGUF's `embedding` hint, which used to produce a server that answered 501 to the very request that started it. Recorded `last_params` mode is the next fallback, then the hint. A chat model is never forced into embedding or rerank mode.
- `start --server <id>` now rejects an unknown server id (exit 64, listing the valid ids) instead of silently falling back to the default binary and recording the bogus value in `params.server`.
- The launch picker's `main_gpu` row no longer offers a hardcoded `0-3` ring on hosts with fewer GPUs — it sizes to the actual device count. `split_mode` gains `tensor`.
- Lemonade models no longer appear in the `/ui` chooser as web-UI-capable — they serve no browser UI, so the chooser lists them non-selectable. (`22fc76f`)
- Stopping a busy Lemonade umbrella no longer times out mid-teardown and leaves a ghost running row; the stop client now waits out the grace window. (`22fc76f`)

### Security

- The proxy's fail-closed LAN backstop now treats a blank/whitespace `proxy.api_key` as no key (matching `ProxyAuth`), so a whitespace-only key can never leave the LAN listener keyless. Defense-in-depth — production paths already normalize the key before this point.

## [0.0.6] — 2026-07-13

### Added

- **ds4 (DwarfStar) backend — experimental** (new, validated on a single Strix Halo / ROCm host; behaviour and config may change — llama.cpp stays the stable default). For DeepSeek-V4 GGUFs — runs antirez's `ds4-server` for the Flash/PRO models, auto-routing a compatible GGUF to it when the binary resolves and falling back to llama.cpp otherwise (never a refusal — on a DeepSeek-V4-capable llama.cpp, b9840+). Default-on when found; enable/force via `[ds4]` config, `--ds4`, or `LLAMASTASH_DS4=1`. Six ds4-native launch knobs, a `ds4_unavailable` doctor advisory, and a `status.backends` ds4 row. When a ds4 model won't fit RAM the launcher now **auto-enables SSD streaming** so it loads from disk instead of OOM-killing mid-load. The TUI marks ds4-routed models with a `ds4` badge in the right pane and lists the ds4 native knobs (not the llama.cpp set) in the running view. `pull`'s per-file size cap is raised to 512 GiB to accommodate the multi-hundred-GB single-file DeepSeek-V4 GGUFs. See `docs/usage.md#ds4-backend`.
- Proxy forwards the OpenAI **Responses API** (`/v1/responses` + `/v1/responses/input_tokens`) — both the llama.cpp and ds4 backends speak it, so agents on the Responses surface attach through the one stable proxy URL.
- Context-length quick-picks extend to 1 Mi and are gated per model to its trained window — the ctx cycle never offers a context larger than the model supports (type a custom value for anything off the ladder).
- A model's `default:` preset is now its standing launch config and **auto-applies** — on a plain `start <model>` (no `--preset`) and on proxy auto-start, not just as a TUI cycle hint. Resolved server-side as a new precedence layer (`your flags > default preset > last-used > arch defaults > fit`). `default: auto` launches pure fit; `start --preset auto` is the clean per-launch "ignore last-used + default" gesture. The TUI cycle drops the separate `[default]` stop, marks whichever stop is the default with `(default)` and opens on it, and the preset row shows the available count (`preset (N)`). See `docs/plans/2026-06-30-001-feat-default-preset-resolver-layer-plan.md`.
- **`Alt+L` cycles the TUI left/right pane split.** Steps the Models-list width through the configurable `left_pane_ratios` slots (default `[65, 100, 50, 35, 0]`) in wide mode — `100` gives the list the whole width (all columns show), `0` hands it all to the right pane. Session-only; the pick resets to the first slot on restart.
- A **Params** column in the model list (TUI + `llamastash list`) showing the parameter count (`7B`, `235B`, `1.2T`), derived from the GGUF header. The label is now compact and accurate across the full range instead of bucketed.
- A **Backend** column in the model list (TUI + `llamastash list`) showing where a model routes — `llamacpp` / `lemonade` / `ds4` (the daemon's prediction for idle rows, the resolved backend for running ones). Shown only on multi-backend hosts, so a pure-llama.cpp user never sees a redundant all-`llamacpp` column.
- `status` surfaces the **web-UI URL** — a `web ui` row in the human output and a `ui_url` field in the proxy block (`--json` / IPC), so you don't have to reconstruct the port-stable `http://<proxy>/ui/` origin by hand. Present only while the proxy is listening.

### Changed

- **Lemonade is now default-on when the `lemond` binary resolves** (matching ds4), instead of opt-in/off-by-default. If `lemond` is on `PATH` (or `lemonade.binary` points at it) the daemon runs Lemonade discovery + umbrella unless `lemonade.enabled: false`; `--lemonade` / `LLAMASTASH_LEMONADE=1` still force it on. Zero footprint when the binary is absent. `lemonade.enabled` is now tri-state (unset = auto / `true` = force on / `false` = force off).
- The running-model header shows the resolved context window for every backend (`… · 16k ctx · …`), omitted when unknown.
- Lemonade running rows are tidier: a `lemonade` backend badge, the shared umbrella hidden from the running list (its RAM/CPU/port surface on the model rows behind a `*` shared-marker — the help Legend explains it), the synthetic `lemonade://` path row dropped, and deleting a registry model refused with guidance (it is managed by Lemonade, not a local file). Lemonade models now use the same `L#` launch id as every other backend (was `lemonade:<name>`), so `stop` / `logs` take one id scheme across llama.cpp / ds4 / Lemonade. Delegated model rows show `-` for PID (`null` in `--json`) since they have no process of their own; only the shared umbrella row carries a pid.

### Fixed

- `init`-generated dev-tool configs now authenticate against an auth-enforced proxy: they carry the resolved `proxy.api_key` (honoring the `LLAMASTASH_PROXY_API_KEY` override) instead of the `llamastash` stub, and OpenCode's `apiKey` moves inside `options` — where the `@ai-sdk/openai-compatible` SDK actually reads it, so a top-level one no longer gets silently ignored. `env.sh` / `claude-code.sh` tighten to `0o600` since they can now hold the real bearer token. The env override + blank-normalization now resolve through one shared `ProxyConfig::effective_api_key`, shared by the daemon and the init writers.
- Lemonade models can now be favorited (`f`) and show up in the TUI's `↺ Recent` section. Favoriting a `lemonade://` registry model no longer errors (there's no GGUF file to hash — it uses a synthetic id keyed on the catalog path), and a successful Lemonade launch records `last_params` like every other backend.
- Split-GGUF parameter counts now sum tensor elements across every shard, so a 2-shard 80B model reports ~80B instead of the shard-1-only ~56B in the `Params` column of `list` / `show` (an explicit `general.parameter_count` still wins). Small models (embedding-scale) now label in millions (`22.7M`) instead of showing a placeholder.
- `status.backends` accelerators are now accurate per backend: ds4 picks up the host GPU (e.g. `rocm`) from the live device catalog instead of reporting cpu-only, and lemonade reports what `lemond` actually has installed via a live `system-info` probe (e.g. `rocm, vulkan, npu`) instead of a static `cpu, npu` guess.
- deepseek4 KV cache is now modeled from the header (its two-tier compressed cache) instead of the naive per-head estimate, which over-counted ~8x at long context (~86 GiB vs the real ~11 GiB at 1M for Flash) and could spuriously refuse a launch. The "KV demand not modeled" advisory is dropped.
- TUI Settings knob rows no longer wrap when a `(model/server default)` source label doesn't fit the pane — they truncate on one line with `…`, so cycling presets or live updates don't make the form jump. The running view and the editable form now render through one shared path, so both show/hide/truncate these labels identically.
- Free-form `llama-server` flags (e.g. `--chat-template-file`, `--mmproj`) survive a proxy auto-start reload and a plain restart: a launch that doesn't pick params inherits its model's effective default (the default preset, else last-used). Use `--preset auto` to launch with nothing inherited. (#49, supersedes the earlier origin-gated behavior)
- Model-list columns render one consistent placeholder for empty/unknown values — `—` in the TUI, `?` in the CLI — instead of a mix of blank / `Unknown` / `unknown` (e.g. registry-served Lemonade models with no GGUF header).
- The TUI `c` (copy curl) targets the port-stable proxy when it is auth-free (falling back to the backend port when the proxy requires a key), so a pasted command survives relaunches and works for every backend; `u` still copies the raw backend URL.
- A Lemonade model whose `lemond` umbrella dies out-of-band (crash / external kill) no longer lingers as a green `ready` ghost in `status` and the TUI `▶ Running` group; the row now reflects the umbrella's real state (`stopped` / `error`), so it reads as dead and can be stopped.
- A Lemonade model with a dotted registry name (e.g. `qwen3.5-4b-FLM`) keeps its full name in every TUI state; it used to truncate to `qwen3` while loading / errored / stopping, when the name fell back to a `file_stem` that mistook `.5-4b-FLM` for a file extension. Name derivation now runs through one shared resolver (catalog label, else a scheme-aware path fallback) across the list, header, info, and `favorites list --json` surfaces.

### Security

- Bumped `anyhow` to 1.0.103 to patch RUSTSEC-2026-0190 (an `Error::downcast_mut()` unsoundness). Lockfile-only.

## [0.0.5] — 2026-06-25

### Added

- Named launch presets now live in `config.yaml` (a `presets:` key) — the single writable source. `presets save` / `delete` (CLI) and the new TUI `Ctrl+P` save dialog (the Settings form, or a running model's live knobs) write there comment-safely. A `presets:` key is per-model when it names a discovered model (basename, path fallback), otherwise an arch id applying to every model of that arch; a model's effective set is per-model ∪ arch (model wins), plus an optional config-only `default:` (one stop in the cycle; the CLI never auto-applies it). The TUI Settings form gains an always-shown top-of-form preset cycle (`last used → auto → [default] → named presets`) that rewrites the knobs live; it opens on `last used` (your pre-filled last params). Entries are written in readable block YAML — a knob set to `auto` delegates it to `--fit` (e.g. `n_gpu_layers: auto`; `{ value: auto }` escapes a literal `auto`). Existing `state.json` presets migrate into `config.yaml` once on upgrade. `status` model rows gain `preset_count` + `default`. See `config.example.yaml`.
- HuggingFace pull dialog search rows now show `params` (model size, e.g. `35B`) and `size` (approximate download size, e.g. `5.3G`) columns, fetched in the same search request. Sort (`o`) now also cycles through File size, Params, and Repo name (reordering the current page).
- `init` / `recommend` model picker gains a "Search HuggingFace by name…" option: prompts for a query, lists live results (params · size · downloads), and downloads the chosen repo.
- Opt-in ASCII glyph fallback for the TUI: `LLAMASTASH_ASCII=1` (or `ascii_glyphs: true` in `config.yaml`; env wins) renders status dots, severity markers, gauge bars, the logo banner, and box borders in 7-bit ASCII for terminals / fonts that show the Unicode set as tofu. Severity stays double-encoded (`!` warning vs `*` critical). Default rendering is unchanged.

### Changed

- TUI Host pane now shows a single `GPU*` row on multi-GPU machines (combined usage + hottest temp) instead of one row per card; the help legend explains the marker, and `status --json .host.gpu_devices` keeps the per-card breakdown.
- `--help` is now colorized on a TTY (styled section headers / flags), following the same color policy as the rest of the CLI — plain bytes when piped, `NO_COLOR` is set, or `--no-colors` is passed.
- `show`'s human output now matches the `status` / `presets` tables: shared section headers and aligned labels, with on-disk sizes routed through the canonical byte formatter.
- TUI: pressing `s` (Logs auto-scroll) or `r` (Chat reasoning collapse) now toasts the new state instead of toggling silently; the narrow title bar drops trailing brand segments (theme, then daemon, then version) instead of clipping mid-word; and the confirm popup tones its border by severity — red only for stop / kill / delete / cancel, the warning hue for neutral prompts.
- TUI: the HuggingFace pull dialog uses a shorter `HuggingFace — Search` title and moves its per-stage key hints into the title bar (no separate bottom strip), matching the help overlay; the help overlay itself drops the `j/k:scroll` title chip and tightens its padding.
- TUI: the `?` help overlay renders its keybindings across three columns with the glyph/marker legend full-width below them (no longer truncated inside a column), scrolls as one page, and gains an `↑↓:scroll` hint in the title.
- TUI: key hints render non-bold across the top bar, the confirm popup, and the help overlay; the top bar gains an always-on `↑↓:scroll` chip (order: `pull · help · panes · scroll · theme · quit`) and now drops chips by priority as the terminal narrows (keeping `pull` / `help` longest, `scroll` first to go) instead of hiding the whole strip — and since the top bar always carries scroll, the redundant per-pane `↑↓:scroll` bottom-strip copies are gone; and the right pane's tab strip dims to muted + first-letter-underlined (non-bold) when the pane is unfocused, matching the Models pane title.

### Fixed

- `init`'s "point at an existing binary" path now adopts a system-package-manager llama.cpp. It used to refuse any symlink whose target was owned by another UID — including the root-owned binaries every distro ships in `/usr/bin` — so a `/usr/bin/llama-server` symlink dead-ended. It now resolves the symlink and accepts a target owned by you or root (foreign-UID targets and group/world-writable dirs are still refused), and warns when the chosen file isn't a server binary (e.g. `llama-cli`). (#45)
- Config writes no longer strip your comments. The init wizard and `daemon` config persistence (proxy key, server path) used to re-serialise the whole `config.yaml`, discarding hand-written comments; every `config.yaml` write now goes through one comment-preserving path (the same one presets already used). Internally drops the archived `serde_yaml` for the maintained `yaml_serde` fork.
- A `config.yaml` symlinked into a dotfiles repo no longer errors on write. Config writes now follow the link to its target and update that (the link survives), instead of refusing. `state.json` keeps its non-following behavior.
- Proxy `/api/show` now preserves a model's embedding/rerank mode hint (it previously dropped it on the resolver path, so an auto-started embedding model could be composed as chat), and `/api/ps` no longer lists the internal Lemonade umbrella process as a model. Both surfaces now share one catalog projection / index with the rest of the proxy.
- `doctor`'s `snapshot_stale` no longer cries wolf. It now probes the latest remote snapshot (the one the recommender already prefers) before judging staleness, so it only fires when no fresher snapshot is actually reachable (`LLAMASTASH_OFFLINE` skips the probe). `init`/`recommend` also persist the *effective* snapshot date — the fresh remote, not the binary's bundled date — and the finding's message/fix-hint are corrected (the old "daily CI refresh will heal automatically" never moved the bundled date).
- Windows `init` no longer aborts with "no GH Releases asset matches this hardware" on single-GPU machines. A lone AMD/NVIDIA card that both the DXGI and Vulkan probes detect was double-counted and reported as `gpu_backend: multi`, which had no install route. The probe now collapses the cross-probe duplicate before classifying, so a single card reports its vendor (e.g. `amd`); the GH Releases router also handles genuine multi-GPU hosts (CUDA when any NVIDIA card is present, else the universal Vulkan build).
- TUI host pane VRAM gauge no longer understates a Windows UMA APU's pool. The reachable ceiling is now `min(pool_total, ram_total − non-GPU RAM use)` for both Linux `amdgpu` (GTT) and Windows DXGI UMA: it shows the full pool when RAM is free (matching `doctor` / `llama-server --list-devices`, e.g. `0.0/64G` instead of the old drifting `0.0/42G`) and only clamps toward free-RAM headroom under memory pressure. On Linux (pool ≈ total RAM) this is identical to the previous behavior.
- `init`'s interactive install picker no longer offers Homebrew on Windows, and on macOS/Linux offers it only when `brew` is actually on `PATH`. Previously the menu listed Homebrew unconditionally, dead-ending Windows users (and anyone without Homebrew) on a method their host can't run.

## [0.0.4] — 2026-06-16

### Added

- Anthropic Messages API through the proxy — `/v1/messages` + `/v1/messages/count_tokens` forward to llama-server's native endpoints, so Claude Code and other Anthropic-shape clients attach via `ANTHROPIC_BASE_URL` (key sent as `x-api-key`). New `jinja` config key (default `true`) emits `--jinja` on every launch for tool calling; the reasoning toggle still forces it on.
- Browser web UI through the proxy at `/ui` — opens the running model's stock llama.cpp UI on one port-stable origin, with a chooser when several run (and `/ui/switch` to re-pick) plus HTTP Basic auth (the proxy key as the password) for LAN access.
- KV cache type validation now accepts llama-server's full standard set (`f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`, `q5_0`, `q5_1`) and passes through custom identifiers from modified builds (e.g. `fp4`, `turbo_quant`) instead of rejecting them, so `--cache-type-k` / `--cache-type-v` and the TUI cache-type row no longer block non-standard quant types. (#29)

### Changed

- **Breaking: Auto launch mode is the new default.** Instead of pinning `n_gpu_layers=99` and a computed context size, LlamaStash delegates GPU/CPU placement and context sizing to llama-server's `--fit`, and keeps memory-budget authority itself with pre-spawn admission control: it refuses a launch that would not fit the sampled free memory rather than letting concurrent models OOM the machine. A fit-capable `llama-server` is now required.
- Every launch knob accepts the literal `auto` (`--n-gpu-layers auto`, `start --ctx auto`, and an Auto stop in the TUI knob cycle); a value you pin still wins.
- New config options with `LLAMASTASH_*` env overrides: `default_launch_mode` (`auto`|`inherited`), `fit_ctx_floor` (default 16384), `strict_fit`.
- TUI host pane, init banner, and help legend rename `RAM`/`RAM*` to `MEM`/`MEM*` so unified-memory machines stop reading as roughly twice their physical memory.
- `doctor` gained a hardware section (CPU, memory, GPU pool composition, classification source), a memory-drift finding, and a GTT-cap hint.
- Fit-resolved context surfaces on every running-model view: a `CTX` column on `status` (a trailing `*` flags a memory clamp to the fit floor), a running block on `show`, a new `start --wait` that blocks until the launch settles and prints `ready → ctx=N`, and `resolved_ctx` / `ctx_clamped` on `status --json`.
- Hardware reporting now uses one consistent vocabulary across `status`, `doctor`, `init`, and the TUI: the GPU is named by vendor (`AMD`/`NVIDIA`/`Apple`), memory always prints `GiB`, a unified APU pool reads `unified`/`MEM*` rather than `VRAM`, and `doctor` is the superset (it adds CPU instruction sets and an OS line). `doctor` and `status` now print the same one-line GPU summary, e.g. `AMD · 124.5 GiB (carve signature)`.

### Fixed

- Lemonade preload now waits for the `lemond` umbrella to be ready before loading a model, so an explicit launch on a cold daemon no longer flips to `error` on a transient connection failure.
- `--flash-attn auto` no longer leaves a dangling positional token in the argv tail.
- TUI Chat and Embed output now scrolls with `↑`/`↓` from the composer (the keys were unbound there — mouse-scroll only); a low-priority `↑↓:scroll` hint shows on every scrollable pane. (#34)
- `status` no longer reports `GPU: CPU only` during the daemon's first second (before the metrics sampler ticks); it reads the live host snapshot like the TUI, and the pre-sample window reads `detecting`.

## [0.0.3] — 2026-06-11

### Added

- Multimodal (vision/audio) models — LlamaStash auto-detects an mmproj projector sitting beside a model, loads it with `--mmproj`, and flags vision/audio after the model title in the TUI. (#15, #27)
- Lemonade backend (experimental, opt-in) — drive a user-installed `lemond` as a second backend for NPU / multi-engine inference; off by default (`lemonade.enabled` / `--lemonade` / `LLAMASTASH_LEMONADE`), setup in [docs/lemonade-setup.md](docs/lemonade-setup.md). (#28)
- Opt-in LAN access for the OpenAI-compat proxy behind a required bearer key (`--proxy-host` / `proxy.host`); the key auto-provisions on first bind, the control plane and `llama-server` children stay loopback, and LAN mode is plaintext (no TLS yet). (#25)
- Every typed launch knob is now a first-class `start` flag (`--threads`, `--device`, `--tensor-split`, `--flash-attn`, …), generated from the same spec table as the Settings editor so the CLI and TUI can't drift.
- Multi-GPU and offload launch knobs — `--device` (`-d`) pins a card (#14); `--tensor-split` (`-ts`), `--main-gpu` (`-mg`), `--split-mode` (`-sm`) placement; and `--n-cpu-moe` (`-ncmoe`) (#20). Placement and device controls are hidden on single-GPU / CPU-only hosts.

### Changed

- Repositioned as a backend-pluggable local-LLM launcher (llama.cpp stays the direct, zero-overhead default); `status --json` now carries a `backends` array. (#28)
- Settings tab groups the typed knobs into labelled clusters (Context, GPU/CPU offload, Multi-GPU placement, Attention & KV cache, Throughput, Memory loading, Advanced) ordered by how often they change.
- Daemon start fails fast when a configured backend can't come up (missing `llama-server`, wedged `lemond` port) instead of booting half-alive — surfaced on the CLI and in the TUI Daemon panel.

### Fixed

- Windows `init` no longer selects the CUDA `cudart-*` runtime-DLL zip over the real `llama-*` binary package. (#23)

## [0.0.2] — 2026-06-02

### Added

- Windows 11 x64 as a first-class platform — same binary, TUI, and CLI. Install via `install.ps1` or the release `.zip`; supervision uses Job Objects, the lockfile uses `LockFileEx`, and `runtime.json` / `state.json` get owner-only DACLs. (Windows AMD GPU detection and `aarch64` deferred.)
- Windows GPU detection via DXGI — covers AMD / Intel, plus NVIDIA on installs without `nvidia-smi.exe`; reports adapter name and VRAM (no live util/temp). See [`docs/architecture.md §GPU detection`](docs/architecture.md#gpu-detection).
- `init` routes GitHub Releases to Windows llama.cpp assets, and `safe_extract` gained a hardened `.zip` branch matching the `.tar.gz` defenses.
- Windows CI lane — `clippy` / `test` on `windows-latest`, release ships a `.zip` artifact.
- `init` patches AI dev-tool configs (OpenCode, Aider, Continue.dev, Zed, pi.dev) plus a sourceable `env.sh`; non-interactive via `init --integrations …`. Merges preserve user-authored keys, and API keys are written as env-var references, never literals. Detects existing JSONC variants, gives embed models embed-shaped config, derives the model id from the GGUF stem, and the summary lists each patched tool + path.
- `show <model>` projects everything LlamaStash knows about one model — GGUF metadata, per-shard sizes, and the resolved launch params; `--json` emits a stable envelope.
- Interactive picker for `start` / `stop` when no argument is given (refuses non-TTY / `--json` so CI gets an actionable error).
- `list` shows a live `STATUS` per model (e.g. `● ready :41100`); `list --json` gains a per-row `status` object.
- Idle-TTL eviction for proxy-auto-started supervisors (`proxy.idle_ttl_secs`, default 1800, `0` disables). Refcount-gated so generations are never killed mid-stream; manually launched models stay resident.
- `daemon start --no-proxy-fallback` (+ config / env) makes a failed auto-start return 503 instead of being served by a different Ready model.
- `daemon stop --force` as an escape hatch for a stale daemon holding the flock with no `runtime.json`.
- `init` model picker gains a "Skip — don't download a model" entry.
- `?` help overlay gains a `Legend` explaining the `RAM*` glyph.

### Changed (breaking)

- IPC transport rewritten on HTTP loopback + bearer token — the Unix socket and `SO_PEERCRED` auth are gone. `LLAMASTASH_SOCKET` / `--socket-path` are removed; clients use the URL + token in `runtime.json` (0600 / owner-only DACL) or `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN`. The proxy listener is unchanged.
- Default control-plane port is `48134` (random `41100..41300` on collision), discovered via `runtime.json`.

### Changed

- `status` text output replaces the `PATH` column with `NAME` (basename); `status --json` keeps the full `model_path`.
- Apple Metal GPU row now reads `GPU  unified` instead of `GPU  unified memory`.

### Fixed

- `presets save --json` now returns the overwritten preset (`replaced: <old-params>`) instead of a bare `true`.
- Quant label reads from `general.file_type`, fixing big-vocab models the tensor scan mislabelled (e.g. a `Q4_K_M` gemma showing as `Q6_K`) in `list` / `show` / `/api/tags`.
- `logs` and `stop` accept a model-name reference, matching `start` / `show` / `presets` (ambiguous → exit `66`).
- A malformed config is rejected loudly (`config error: …`, exit `64`) instead of silently booting on defaults; `init` / `doctor` stay exempt so a broken file can be repaired.
- `LLAMASTASH_OFFLINE` accepts `1` / `0` / empty / unset, not just `true` / `false`.
- Usage errors exit `64` consistently — clap rejections and a bad `--render-size` no longer exit `2` / `71`.
- Orphan re-adoption matches llama.cpp's basename id (`b9245+`), and external-process discovery dedupes kernel threads into one `status.external` row.
- Launch health-probe timeout scales with weight size, so a large GGUF on slow disk doesn't trip the 120 s default before weights finish loading.
- `status` surfaces the daemon's error cause (e.g. health-probe timeout + last stderr) so users don't have to grep the launch log.
- `show --json` emits a `{error: {code, message}}` envelope on every failure path.
- Split-GGUF SIZE is the summed on-disk total across all shards (in `list`, `show`, TUI, and the VRAM-fit check), computed directly from shard paths so it self-corrects across upgrades.
- HF pull file-picker now scrolls to keep the cursor in view.

### Infrastructure

- `make snapshot` warns when `HF_TOKEN` is unset and records a `regen_environment` manifest — surfacing the usual cause of local-vs-CI snapshot drift (anonymous-tier HF rate limits).
- Benchmark snapshot releases publish with `--prerelease` so they don't headline the Releases page; asset URLs unchanged.

## [0.0.1] — 2026-05-28

First publicly-installable release. A single `llamastash` binary acts as TUI, CLI, and on-demand daemon for running local LLMs via [llama.cpp](https://github.com/ggml-org/llama.cpp). Distributed via Cargo, a Homebrew tap, and a GitHub-hosted install script, with a marketing site at [llamastash.dev](https://llamastash.dev).

### Zero-to-chat in one command

- `llamastash init` — interactive first-run wizard that detects hardware (NVIDIA / AMD-ROCm / Apple Metal / Vulkan / CPU), installs the right `llama-server` variant, picks a starter GGUF tuned to your VRAM, downloads it, writes a tuned `config.yaml`, and smoke-launches. `--recommended` / `--only` / `--skip` / `--json` / `--offline` flags make it agent-friendly.
- `llamastash doctor` — read-only health check with typed, agent-branchable findings and stable `fix_hint` pointers. Always exits `0`.
- Hardware-aware model recommender with a VRAM-fit filter plus composite ranking (benchmark × tok/s × params × recency), over a daily-CI-refreshed snapshot.

### Discovers what you already have

- Auto-scans HuggingFace, Ollama, and LM Studio caches plus user-configured paths; live filesystem watching surfaces new GGUFs without a restart.
- Rich GGUF intelligence — header parser surfacing architecture, parameter count, quantization, native context, chat template, and reasoning hints. KV-cache-aware memory estimates that account for chosen context length.
- Smart deduplication — symlinks collapse to their target, split GGUFs unify, Ollama content-addressed blobs surface under their human-readable name.

### Launches anything, supervises everything

- Daemon-on-demand over a `0600` Unix socket with peercred auth. First client auto-spawns; running models survive TUI close via three-factor orphan re-adoption (PID alive + port listening + `/v1/models` path match).
- Multi-model concurrency — each launch gets its own port (auto-allocated from a configurable range) and a `Launching → Loading → Ready → Stopping → Stopped` state machine with `/health` probing.
- Auto-fit context when `ctx` is unset — computes the largest window that fits current free VRAM or RAM from GGUF metadata and live host metrics instead of collapsing to a tiny fallback.
- GPU-aware built-in arch-defaults table covering `llama*`, `qwen2*`, `qwen3*`, `mistral`, `mixtral`, `gemma*`, `phi*`, `deepseek*`, `granite`, `falcon`, `stablelm`, `command-r`, plus a `*` fallback. Fresh install gets sensible `n_gpu_layers` / `flash_attn` on every supported backend with zero YAML.
- Typed launch-knob editor with `(user)` / `(last used)` / `(arch default)` / `(model default)` / `(server default)` source chips. Layered resolver: `preset > last-params > yaml arch_defaults > built-in table > llama-server`.
- Named presets, favorites, and last-params recall persisted in `state.json`.
- Low idle overhead on always-on setups — the daemon avoids wasteful full-vendor GPU probing when nothing is running.

### A TUI that doesn't get in your way

- Keyboard-driven everywhere — vim-style `hjkl`, `/` filter, `f` favorite, `u`/`c`/`p` yank URL/curl/path, `t` cycle theme, `?` contextual help.
- Optional mouse focus + scroll (`mouse_focus: true` or `--mouse-focus`) for pane focus, tab switching, and wheel navigation, while keeping native text selection by default.
- Right pane is your smoke test — Logs / Chat / Embed / Rerank tabs hit the same OpenAI-compatible endpoints any external client would use.
- In-TUI HuggingFace browser (`d`) — three-stage Search → File picker → Confirm modal over `/api/models`. Search, sort, paginate, per-file fit `✓` / `⚠` / `✗`, sharded-set collapse, pinned download strip with `Ctrl+X` cancel and `Ctrl+D` delete-from-disk.
- Five built-in themes (Catppuccin Macchiato default + Latte, Gruvbox Dark, Solarized Dark, Monochrome) plus a `custom_theme` config block for user palettes.
- Every TUI action rebindable via a `keybindings:` config block with a kdash-style key-spec dialect. Destructive actions sit behind `Ctrl`; cross-pane navigation behind `Shift`. Unicode keycap glyphs in the help bar (`↹` / `⇥` / `⏎` / `⇧` / `⌃ ⌥ ⌘`).
- Accessible by default — status indicators dual-encoded with colour + glyph; a "terminal too small" placeholder below 40×10.
- Adaptive layout down to `60×20` — on narrow terminals the right pane becomes drill-in-only so the models list stays usable.
- Safer model browsing — `Enter` on a running row opens its live view instead of silently staging a duplicate launch.

### Fits your existing clients

- OpenAI-compatible loopback proxy enabled by default on `127.0.0.1:11435`, with `/v1/models`, `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, and `/v1/rerank` routed by model name.
- Auto-start plus launch coalescing behind one stable URL, so external tools can target LlamaStash without pre-warming each model by hand.
- Optional Ollama-compat mode answers the Ollama root handshake, exposes discovery endpoints (`/api/tags`, `/api/version`, `/api/ps`, `/api/show`), and prefers port `11434` for `OLLAMA_HOST`-aware clients.

### First-class CLI for agents and scripts

- Subcommands cover every TUI capability: `list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `last-params`, `daemon`, `init`, `doctor`, `pull`, `recommend`. Every read+mutation command supports `--json` as the agent contract.
- `llamastash daemon start` detaches by default and returns once the socket is ready; pass `--foreground` when a supervisor should own stdout/stderr.
- Documented exit codes per failure class (`66` ambiguous ref, `67` launch failure, `69` pull failure, `70` missing `llama-server`, `72`/`73`/`74` init phases). Pin numbers, not message text.
- Colored TTY output, byte-stable TSV when piped, `NO_COLOR` / `--no-colors` honored, `--json` byte-stable regardless.
- `llamastash pull <owner/repo[:filename]>` standalone HF fetch via `hf-hub` — honours `HF_TOKEN`, refuses world-readable token cache files, performs disk-space precheck before any bytes hit disk.
- `llamastash recommend` exposes the wizard's recommender on its own. Reproducible pulls via `--revision <SHA>`.

### Built to be safe to run

- Unix-socket peercred auth (`0600`) protects the daemon control plane; the only HTTP surface is a loopback-only local proxy. No auth tokens, no LAN binding.
- Hardened fetch substrate — HTTPS-only with host allowlist, redirect cap, body-size cap, IP-literal refusal. `--offline` / `LLAMASTASH_OFFLINE` short-circuits before any DNS.
- Archive-bomb defenses on installers — entry-count / total-size / compression-ratio caps; refuses hardlink, symlink, absolute-path, or `..` entries. SHA-256 verified before extract from the GitHub Releases asset's `digest` field.
- Atomic, mode-checked config + state writes — `0600` final mode, refuses symlinks and world-writable parents. Corrupt `state.json` quarantined to `state.json.broken-<ts>` rather than blocking daemon boot.
- Side-by-side daemons via `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET` overrides.

## How to read this file

Tagged releases land under their version heading; in-flight work accumulates under **Unreleased** until the next tag promotes it. LlamaStash is pre-1.0; the entire pre-release history is bundled under the first publishable tag, [0.0.1], rather than backfilled into a series of synthetic tags. The ledger starts there.
