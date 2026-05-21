---
date: 2026-05-20
topic: hf-pull-tui-dialog
---

# HuggingFace Pull TUI Dialog (Search / Sort / Pagination + Friendly Names)

> **2026-05-21 update.** R118 / R119 / R120 (friendly display names for
> HF-cache models) were dropped from the shipped PR. The renames were
> cosmetic on real catalogs whose GGUFs already have descriptive
> filenames; revisit if the ambiguity (`model.gguf` /
> `ggml-model-q4_k_m.gguf`) becomes a real pain point. The original
> requirements text is kept below for historical context.

> Companion to [`docs/brainstorms/llamatui-requirements.md`](./llamatui-requirements.md) (origin: R46, the deferred TUI HF-pull hotkey) and [`docs/brainstorms/2026-05-18-init-wizard-requirements.md`](./2026-05-18-init-wizard-requirements.md) (R65 — the TUI HF-pull surface was the schedule-flexible piece of R46's release). IDs continue from R103 to stay globally unambiguous.

## Problem Frame

The v1 surface for getting a new GGUF into llamastash is "alt-tab to HuggingFace web, find a repo, copy the slug, run `llamastash pull owner/repo` in another terminal." The R46 follow-on shipped that primitive but left the TUI side a future hotkey (R65) and never specified browse, sort, or pagination — only "open input box, type repo ID, download." That's enough for the user who already knows what they want; it does nothing for the user who knows the *shape* of what they want (a 7B coder model, a small embedding model, the latest mistral-derived chat tune) but not the exact repo slug.

Once a file lands, the second papercut shows up: the TUI's list pane prints `display_name(m)` = `m.path.file_stem()`. For GGUFs that publishers named carefully that's fine, but plenty of repos publish files with stems like `model`, `ggml-model-q4_k_m`, or stems that lose the repo's name once the path's HF cache prefix is stripped. The user can't tell two `model.gguf` rows apart without inspecting the parent directory. Repeating the publisher name (which is already encoded in the HF cache layout: `models--<owner>--<repo>/...`) would solve scannability in one line of rendering code.

This brainstorm bundles both into one slice because they're cohesive at the user level: the TUI dialog is the moment the user pulls a model from HF, and the same dialog is where the model's eventual display label is decided. Shipping the dialog without the rename leaves the new feature populating a list pane that still prints `model.gguf`; shipping the rename without the dialog is a small generic polish without a narrative.

**Audience:**
- Primary: an llamastash TUI user who wants to add a new model without leaving the terminal and without already knowing the HF repo slug.
- Secondary: the same user a week later, scanning their model list and trying to remember which `model.gguf` was which.
- Tertiary: agents / CI driving `llamastash pull` from the CLI continue to use the existing R65 primitive unchanged.

## Requirements

**Dialog Shell & Entry**

- **R104.** `Ctrl+D` opens the HuggingFace pull dialog from anywhere in the TUI (preserves the original R46 hotkey assignment). The dialog is a modal overlay; `Esc` closes it without side effects. The hotkey is wired into the existing help-overlay + keybindings tables (R20).
- **R105.** The dialog has three states in a single overlay: (1) **Search** — sort selector + search input + paginated results list, (2) **File picker** — list of `.gguf` files inside the selected repo with hardware-fit highlight, (3) **Confirm** — final pre-pull confirmation showing the resolved display name and destination. Forward navigation is enter / select; back-out is backspace from File picker → Search, and Esc from Search → close.

  ```
  (closed) ──Ctrl+D──▶ [Search] ──enter on result──▶ [File picker] ──enter──▶ [Confirm] ──enter──▶ pull starts ─▶ (closed)
                          ▲   │                          │                       │
                          │   └─enter on owner/repo slug─┘                       │
                          │                                                      │
                          │                                                      │
                          └─────────── backspace ────────────── backspace ───────┘
              Esc from any state ─▶ (closed)
  ```
- **R106.** The search input is the primary control: typing a free-text fragment runs against the HuggingFace search endpoint with `library=gguf` (or equivalent GGUF-filter parameter, see Outstanding Questions). Typing a token that matches the `<owner>/<repo>` slug shape (per the existing `RepoSpec` parser at `src/init/download.rs`) and pressing enter bypasses search and jumps straight to the File picker for that repo, preserving R46's "I know the slug" path.

**Search Results & Sort**

- **R107.** Sort options, presented as a single dropdown / cycle-key control: **Downloads** (default), **Likes**, **Recently Updated**, **Trending**. Changing the sort re-issues the current query starting at page 1. Sort labels mirror what HuggingFace's web UI uses so users don't have to translate.
- **R108.** Pagination is page-by-page with a fixed page size (target: 20 results per page; tune during planning if the HF API enforces or caps differently). `n` / `p` (or arrow keys at the list boundaries) move between pages. Page indicator displays `page X / Y` when total-count is available, otherwise `page X` with a next-page affordance hidden when the previous fetch returned fewer rows than the page size.
- **R109.** Each result row shows: repo slug (`<owner>/<repo>`), short description (first line of the HF model card / `pipeline_tag`-style summary if available), and the sort-relevant metric (download count when sorting by Downloads, etc.). Display is truncated cleanly when the terminal is narrow; the dialog never horizontally scrolls.
- **R110.** Filter chips (parameter-count band, tag filter beyond `library=gguf`) are **out of this slice** — punted to a follow-up release. The dialog ships with sort + free-text search only.

**File Picker & Quant Choice**

- **R111.** After a repo is selected, the dialog drills into a list of that repo's `.gguf` files. Each row shows: filename, quant label (extracted from the filename per the existing GGUF quant heuristic), file size, and a hardware-fit indicator (✓ fits, ⚠ tight, ✗ over VRAM) computed using the same recommender path that init uses for the snapshot universe (R55: 0.90 × detected VRAM minus backend overhead band). The recommended pick is pre-highlighted; arrow keys override.
- **R112.** Split-shard sets (`*-00001-of-NNNNN.gguf`) are grouped into one logical row whose size is the sum of shards, mirroring discovery's existing collapse behavior (R5 / `src/discovery/split_gguf.rs`). Selecting that row pulls the full shard set; the user does not pick shards individually.
- **R113.** When R44 returns `GpuInfo::Unknown` (e.g. Vulkan-only Linux), the hardware-fit indicator is omitted rather than shown with fake confidence; the file picker still lists everything and lets the user choose. This mirrors R55's VRAM-unknown branch behavior.

**Pull Execution & Progress**

- **R114.** Once the user confirms, the download runs through the same `hf-hub`-backed primitive that backs `llamastash pull` and the init wizard's model step (R65). No new download path is introduced. The dialog closes and control returns to the main TUI; the pull continues in the background.
- **R115.** Active downloads render in a **pinned single-line status strip above the global help bar**. The strip shows: model display name (per R118), bytes transferred / total, percent, and instantaneous throughput. Only one pull's progress is visible at a time; subsequent pulls queued by the user wait in a FIFO and the strip cycles to the next when the active pull finishes or errors.
- **R116.** If the user picks a file that already exists in the HF cache (full file present, etag matches), the dialog does not start a download. Instead it shows a one-shot toast ("already downloaded — selected in main list") and selects the corresponding row in the main list pane. No re-pull prompt; the user can manually delete the cached file outside llamastash if they want a fresh copy in this slice.
- **R117.** On download failure (network error, disk full, checksum mismatch, etc.), the status strip shows a one-line error long enough to read ("pull failed: <short reason>") and clears after a short delay or on the next pull; full diagnostics are written to the existing logs surface (R30). The user can re-trigger the pull from the dialog; no automatic retry in this slice.

**Friendly Display Names**

- **R118.** The TUI display name for any model whose `parent` matches the HuggingFace cache layout (`.../models--<owner>--<repo>/snapshots/<rev>/...`) is **`<repo-basename> (<quant>)`** — for example, `Qwen2.5-7B-Instruct-GGUF (Q4_K_M)`. The repo basename is derived from the parent path (the segment after `models--<owner>--`); the quant label comes from the existing GGUF metadata parser. Renaming is **derived, not stored** — no alias config, no rename hotkey, no persistence layer.
- **R119.** Non-HF discovered models (UserPath, Ollama, LM Studio per `ModelSource`) keep their current `display_name` behavior (file stem). Bringing friendly names to those sources is out of this slice and tracked as a follow-up; the rename logic added in this slice must be source-aware so it doesn't accidentally rewrite labels for files outside the HF cache.
- **R120.** The derived display name replaces `file_stem` wherever the TUI currently renders a model's primary label — at minimum the list pane row (`src/tui/list_pane.rs:330`) and the right-pane info tab. Implementation must audit every model-name render site so the new label is consistent; an HF-cached model must never show `model` or `ggml-model-q4_k_m` as its primary label after this slice ships.

## Success Criteria

- A user on a working network can open the TUI, press `Ctrl+D`, type `qwen coder`, sort by Downloads, page through results, select a 7B-class repo, accept the highlighted file, and have the download complete and the model appear in the main list pane — all without leaving the TUI or knowing any repo slug in advance.
- A user who already knows the slug can type `owner/repo` into the search field, press enter once, pick a file, and pull — no more keystrokes than the original R46 input-box would have required.
- After pulling, the new model's row in the list pane reads `<repo-basename> (Q4_K_M)`, not `model` or the raw filename stem.
- Two different HF repos that publish a file literally named `model.gguf` show as two distinguishable rows in the list pane, no path inspection required.
- The dialog gracefully degrades in offline mode (`LLAMASTASH_OFFLINE=1` or `--offline`): search returns a clear "offline — paste a repo ID to use the cached entry, or reconnect" message instead of hanging on the network call.

## Scope Boundaries

- **Out:** User-editable model aliases. No rename hotkey, no config-stored alias map, no per-file user-chosen labels. The derived label is what the user lives with.
- **Out:** Filter chips beyond search + sort (parameter-count band, tag filter, license filter). Deferred to a follow-up release once usage shows where the friction is.
- **Out:** Infinite scroll / lazy loading. Page-by-page only.
- **Out:** Multi-select / batch pull from the dialog. One file per confirm.
- **Out:** Resume of partially-failed downloads beyond what `hf-hub` already provides natively. No bespoke resume UI.
- **Out:** Authentication UI for private repos. The existing `HF_TOKEN` env path (R65 / `src/init/download.rs`) continues to work transparently; no in-TUI token entry or storage in this slice.
- **Out:** Friendly names for Ollama / LM Studio / UserPath sources (R119). HF-cache-layout only.
- **Out:** HTTP / MCP browse surfaces. CLI browse remains the existing `llamastash pull <slug>`; this slice is TUI-only.

## Key Decisions

- **Live HF Hub API as the search universe, not the bundled benchmark snapshot.** The snapshot is curated and small; users searching for a model in 2026 expect the long tail to be reachable. **Why:** the cost of "snapshot only" is invisibility for any model not in the curated set, which forces a fallback to typing a slug — exactly the friction this slice is supposed to remove. The cost of "live only" is a network dependency, which we already pay during pull itself, so the marginal cost at browse time is small. The recommender keeps using the snapshot (R55) — different problem (ranking) vs. this slice's problem (discovery).
- **One bundled feature, not two.** The dialog and the friendly-name rename ship in the same slice. **Why:** rename without dialog is a small polish with no narrative; dialog without rename leaves the new surface populating a list pane that still prints `model.gguf`. The rename also gives the dialog something concrete to show in its confirm step (R105's Confirm state previews the resolved display name).
- **Derived display name, not stored alias.** No rename UI, no config, no migration. **Why:** the simplest design that solves the scannability problem. A user-editable alias system is a meaningful surface — alias file format, conflict resolution, sync across machines, CLI surface for editing — and none of that pays for itself until we have evidence users want it. The derived label is computed once per render and costs ~one line of rendering code per call site.
- **`Ctrl+D` hotkey, dedupe by toast.** Preserves R46's hotkey assignment so help text and muscle memory don't churn. Duplicate handling is the simple path: select the existing row, no re-pull prompt. **Why:** the "force re-pull" branch is rare enough to handle via "delete the file and pull again" rather than a dialog branch; punting it keeps the dialog's state machine small.
- **Search + sort only; no filter chips in this slice.** **Why:** filter chips multiply keybindings and the dialog's state machine. The brainstorm explicitly preferred the smallest dialog. If post-launch usage shows users typing param-band hints into the search box, that's the signal to add a chip; if not, we never paid for it.
- **Page-by-page pagination, not infinite scroll.** **Why:** TUI cursor state and "how far down am I" semantics get fuzzy with infinite scroll. Page-by-page maps cleanly to a fixed visible region and predictable keyboard navigation.

## Dependencies / Assumptions

- Depends on R65 — the `hf-hub`-backed pull primitive at `src/init/download.rs` is the download engine; no second downloader is introduced. The TUI dialog is a producer of pull requests, not a parallel implementation.
- Depends on R55 — the recommender's VRAM-fit math (0.90 × detected VRAM minus backend overhead band) is reused for the file picker's hardware-fit indicator (R111).
- Depends on R44 — GPU detection. R113's VRAM-unknown branch falls back to the same `GpuInfo::Unknown` path the recommender already handles.
- Depends on R20 — the help overlay / keybindings table is the registry where `Ctrl+D` gets advertised.
- **Unverified assumption (resolve in planning):** the HuggingFace Hub API exposes the search/list endpoints we need (free-text query against GGUF-tagged models, sort by downloads / likes / recently-updated / trending, pagination) at stable URLs we can call directly. The `hf-hub` crate is download-focused and does not expose a search API surface, so this slice adds direct HTTP calls (likely routed through the existing `FetchClient` with HuggingFace hosts added to the allowlist). The exact endpoint shape (`/api/models?search=...&filter=gguf&sort=downloads&limit=20&cursor=...` or similar) needs to be verified during the planning spike before scope is locked.
- **Unverified assumption:** the HF cache layout under the user's home (`.cache/huggingface/hub/models--<owner>--<repo>/...`) is the parent path we can reliably parse the repo basename out of. Discovery's existing `known_caches` logic already trusts this layout; R118's name derivation extends the same assumption to the rendering layer.

## Outstanding Questions

### Resolve Before Planning

_(none — the brainstorm produced complete product decisions; remaining questions are technical and are deferred to planning.)_

### Deferred to Planning

- **[Affects R106, R107, R108][Needs research]** Verify the HuggingFace Hub API search endpoint shape: confirm the exact query parameters for `library=gguf` filtering, the four sort modes (Downloads / Likes / Recently Updated / Trending), pagination semantics (cursor vs. skip/limit), and any rate-limit behavior that affects browse cadence.
- **[Affects R106, R114][Technical]** Decide whether the new HF Hub API calls (search + per-repo file listing) route through the existing `FetchClient` (consistent redirect-cap / body-cap / host-allowlist gates) or use a separate `reqwest::Client` like the `hf-hub` download path already does. Trade-off: `FetchClient` parity vs. one less HTTP client to maintain.
- **[Affects R109, R111][Technical]** Determine where to source the per-result short description (`pipeline_tag`? first line of model card README? `description` field?) and how to source the per-repo `.gguf` file list (HF API `siblings` array on the model info endpoint? a tree-listing call?).
- **[Affects R115][Technical]** Decide the layout integration for the pinned status strip — render order with respect to `help_bar`, vertical-space budget when not active, and whether the strip is part of the global render pass in `src/tui/render.rs` or owned by a new module.
- **[Affects R118, R120][Technical]** Enumerate every model-name render site (list pane, info pane, advanced panel, confirm overlay, launch picker, …) so the derived label change lands consistently. The list pane and info pane are known; an audit pass is needed to find the rest before planning locks scope.
- **[Affects R111, R112][Technical]** Confirm whether the existing quant-from-filename heuristic covers the GGUF naming conventions in long-tail HF repos, or whether the file picker needs a fallback (parse the GGUF header at pull time for unknown filenames — slower but exhaustive).

## Next Steps

`-> /ce:plan` for structured implementation planning.
