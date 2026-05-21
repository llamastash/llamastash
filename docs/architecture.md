# Architecture

This is the architecture as it ships through v2. Authoritative sources for design intent and tradeoffs: [v1 plan](plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md), [v2 plan](plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md). This document is a stable, user-readable summary of what's actually in the binary.

## v2 additions in one breath

```
llamastash init   ─┬─► detection (gpu::probe + RAM + binary locate)
                  ├─► install (GH Releases | brew | custom_path)
                  ├─► recommender (snapshot models × hardware fit × score)
                  ├─► download (hf-hub → ~/.cache/huggingface/hub/...)
                  ├─► config writer (atomic + 0600 + recursive merge + redaction)
                  ├─► smoke (phase-1 dry-run + binary --version probe)
                  └─► init_snapshot.json (sibling of state.json)

llamastash doctor ─► detection + init_snapshot diff → typed findings
llamastash pull   ─► hf-hub → HF cache layout
```

Three submodule groupings under `src/init/`:

- **Fetch substrate** — `fetch.rs` + `fetch_policy.rs` enforce the v2 fetch contract (host allowlist, redirect cap, body cap, HTTPS-only) on snapshot fetch and GH Releases install. HF traffic is carved out: `download.rs` uses the `hf-hub` crate, which talks only to `huggingface.co` and its LFS CDN — already constrained host families. `FetchClient::is_offline()` is still consulted so `--offline` / `LLAMASTASH_OFFLINE` short-circuits the HF path too.
- **Snapshots** — `snapshot.rs` owns `init_snapshot.json` (per-machine wizard record); `benchmark.rs` owns the bundled+remote `BenchmarkSnapshot` (the curated model catalog + recommender weights).
- **Wizard surface** — `detection.rs` (shared by init + doctor), `recommender.rs` (pure ranking, plus `vram_fit_for_file` used by the TUI HF picker), `install/*` (three install routes), `download.rs` (HF pull via `hf-hub`), `config_writer.rs` (atomic write + diff + redaction), `smoke.rs` (phase-1 + version probe), `wizard.rs` (6-step orchestrator), `doctor.rs` (read-only diagnostic).
- **HF Hub API client** — `hf_api.rs` issues `/api/models` search + per-repo file listing through `FetchClient` (cap, allowlist, offline branch fall out for free); pagination reads off the `Link` header but re-validates the next URL against the HF allowlist and extracts only the opaque `cursor` token. Powers the in-TUI HF pull dialog (`d`). Downloads still flow through `download.rs`'s `hf-hub` carve-out.

The TUI grows two new modules to host the dialog and its async surface:

- `tui::hf_dialog` — three-state modal (Search → File picker → Confirm), debounced live search with `query_seq` cancellation, slug-shortcut parsing via `RepoSpec::parse`, shard-collapse logic over the HF sibling listing, hardware-fit indicator pulling from the host-metrics snapshot.
- `tui::download_strip` — pinned single-line strip rendered below the info row when active; FIFO queue of pending pulls, an EMA-smoothed throughput readout, one active pull at a time, AlreadyCached short-circuit per R116.

## One binary, three roles

```mermaid
flowchart LR
    subgraph user[User-facing entrypoints]
        TUI[llamastash<br/>TUI]
        CLI[llamastash list / start / stop / ...<br/>CLI subcommands]
    end

    subgraph daemon[llamastash daemon]
        IPC[Unix-socket JSON-RPC server<br/>peercred auth]
        SCAN[Discovery<br/>scan + watch + caches]
        GGUF[GGUF parser<br/>metadata + identity]
        SUP[Process supervisor<br/>spawn / health / stop]
        RES[Resource monitor<br/>RAM/VRAM/CPU]
        STATE[Persisted state<br/>favorites / presets / running]
    end

    subgraph external[External]
        LS1[llama-server PID 1]
        LS2[llama-server PID 2]
        FS[(filesystem)]
    end

    TUI -- attach --> IPC
    CLI -- attach --> IPC
    IPC --> SCAN
    IPC --> SUP
    IPC --> STATE
    SCAN --> GGUF
    SCAN --> FS
    SUP --> LS1
    SUP --> LS2
    SUP --> RES
```

- **Daemon-on-demand.** The TUI and CLI both try to attach to the daemon socket first. If absent or stale, they fork/exec `llamastash daemon start --detach` and retry with exponential backoff.
- **Socket.** Unix domain socket, mode `0600`, with peer-credential auth (`SO_PEERCRED` on Linux, `getpeereid` on macOS). Wire protocol: length-prefixed JSON-RPC 2.0 envelopes.
- **State separation.** XDG-aware. `$XDG_STATE_HOME/llamastash/state.json` for favorites / presets / last-params / running snapshot. `$XDG_CONFIG_HOME/llamastash/config.yaml` for user-authored config. `$XDG_CACHE_HOME/llamastash/logs/<id>-<ts>.log` for per-launch logs.

## Model lifecycle

```mermaid
stateDiagram-v2
    [*] --> Launching: start_model
    Launching --> Loading: process spawned, PID known
    Launching --> Error: spawn failed
    Loading --> Ready: /health returns 200
    Loading --> Error: probe timeout / process exit
    Ready --> Stopping: stop_model
    Ready --> Error: process exit unexpectedly
    Stopping --> Stopped: SIGTERM grace OK
    Stopping --> Stopped: SIGKILL after 5s
    Error --> [*]: dismiss
    Stopped --> [*]
```

Each launch is owned by a `ManagedModel`. The supervisor health-probes `/health` every 500 ms during `Loading`; transitions to `Ready` on first 200 OK. After Ready, a longer 30 s liveness re-check runs in the background.

Per-launch logs are tee'd to a 10 MB × 5-file rotating log on disk and a 4K-line in-memory ring buffer so the TUI's Logs tab and the `logs_tail` IPC method don't need to re-open files.

`llama-server` children are started in their own session (`setsid` on Linux) so they survive daemon exit. On daemon restart, the orphan sweep re-adopts each entry in `state.running` only after three-factor confirmation:

1. PID is alive (`kill(pid, 0)` via sysinfo).
2. Recorded port answers on `127.0.0.1`.
3. The port's `/v1/models` response mentions the recorded model path.

A failed factor drops the entry from the running snapshot. Unmanaged `llama-server` processes the daemon doesn't own surface read-only in `status.external`.

## Model identity

`(canonical absolute path, BLAKE3 of GGUF header bytes)`. The header is small (up to ~1 MB); hashing it gives an identity that survives renames but doesn't fingerprint the whole weight file.

The discovery scanner emits one entry per canonical path — symlinks dedupe to their target — so the same model file doesn't appear twice. Split GGUFs (`model-00001-of-00003.gguf`) collapse into a single entry whose launch target is shard 1.

## Right pane tabs

| Model focus state | Mode | Tabs shown |
|---|---|---|
| Not launched | (n/a) | Logs only (empty/grey) |
| Launching / Loading / Error | chat / embedding / rerank | Logs |
| Ready | chat | Logs, Chat |
| Ready | embedding | Logs, Embed |
| Ready | rerank | Logs, Rerank |

The Chat / Embed / Rerank tabs hit the same OpenAI-compatible endpoints any external client would use (`/v1/chat/completions`, `/v1/embeddings`, `/v1/rerank`). This is deliberate: it proves the model is consumable by anything, not just LlamaStash's own smoke test.

## IPC surface

The daemon dispatches on `req.method`. Wire format: `{"jsonrpc": "2.0", "id": <int|null>, "method": "...", "params": {...}}`. Errors come back as JSON-RPC error objects; transport problems close the connection.

| Method | Purpose |
|---|---|
| `ping`, `version`, `shutdown` | Liveness, build metadata, graceful exit |
| `list_models` | Catalog snapshot |
| `status` | Managed launches + external + GPU info + daemon health block |
| `start_model` | Spawn supervisor for a model |
| `stop_model`, `stop_all` | Stop a managed launch / all managed launches |
| `stop_external` | Kill an unmanaged llama-server (PID must already be in the external snapshot) |
| `logs_tail` | Tail snapshot from a launch's ring buffer |
| `presets_list / save / delete / show` | Per-model named preset CRUD |
| `favorite_list / add / remove` | Favorites CRUD |
| `last_params_list` | Persisted last-successful-launch params per model |

JSON-RPC error codes follow the spec (`-32601 Method not found`, `-32602 Invalid params`, etc.) plus `InternalError` for daemon-side faults.

## Persistence shape

`state.json` is read at daemon start, written via temp-file + rename after every mutation. Top-level keys:

- `favorites: ModelId[]`
- `last_params: { <ModelId>: LaunchParams }`
- `presets: { <ModelId>: NamedPreset[] }`
- `running: RunningSnapshot[]` (PID + port + started_at + params)

Corruption → quarantine. A `state.json` that fails to parse is renamed to `state.json.broken-<unix-secs>` and the daemon starts with defaults rather than refusing to boot.

## Theming

Five themes ship in v1: Catppuccin Macchiato (default), Catppuccin Latte, Gruvbox Dark, Solarized Dark, Monochrome. Themes are static palettes hard-coded into the binary; no dynamic loading. Switch at runtime with `t`, or pin in `config.yaml`.

Status icons are dual-encoded (colour + glyph) so the TUI stays usable in monochrome terminals and for users who can't rely on colour alone:

| State | Glyph |
|---|---|
| Launching | `◌` |
| Loading | `◐` |
| Ready | `●` |
| Error | `▲` |
| Stopped | `○` |
| External (read-only) | `⇪` |

## Testing strategy

Inline `#[cfg(test)] mod tests` per source file plus an integration suite under `tests/`. The integration suite uses a `fake_llama_server` binary (built only with the `test-fixtures` cargo feature) that fakes `/health`, `/v1/models`, `/v1/chat/completions` streaming, `/v1/embeddings`, and `/v1/rerank` — so CI never needs a real llama.cpp build.

## What's not here

- HTTP and MCP surfaces (v2, R34).
- HuggingFace pull worker (v2, R46). The CLI `pull` subcommand is hidden and exits unimplemented.
- Multi-user / remote / network daemon. v1 is loopback-only; v2 will require explicit opt-in.
- Daily-driver chat history; markdown rendering. The right-pane Chat tab is a single-shot smoke test.
