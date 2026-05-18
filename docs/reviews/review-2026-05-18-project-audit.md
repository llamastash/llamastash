# LlamaDash project review — 2026-05-18

**Scope:** entire codebase at `HEAD` (commit `96b26dd`). ~33 kLOC Rust across `src/{cli,daemon,discovery,gguf,gpu,ipc,launch,theme,tui,util}/`.
**Intent:** standing project audit across six angles — DRY/YAGNI, library substitutions, consistency, performance, UI/UX, and other improvements. Not a PR-diff review.
**Methodology:** four parallel specialist agent passes (DRY/YAGNI, library research, consistency, performance) over the full tree plus a manual UI/UX read of the render layer and golden snapshots. Findings already documented in `docs/review.md` (the R1+R2 consolidation) are deliberately excluded — this report is *additive*.

---

## Verdict

Healthy codebase. The files that look big (`gguf/*`, `keybindings.rs`, `ipc/methods.rs`) are necessarily big — they encode genuine project constraints. The recurring taxes are concentrated and mechanical:

1. **Render-path waste** — `rendered_rows()` runs ~5× per frame, allocations dominate idle CPU. One PR closes most of the gap.
2. **Enum ↔ label round-trips** spelled out in 3–5 sites each. One canonical `label()` + `from_label()` per enum collapses dozens of matches.
3. **`Style::default().fg(palette.X)`** repeated 123 times across TUI panes. A handful of `Palette` helper methods is a mechanical sed.
4. **Hand-rolled lockfile + atomic-write + setsid + nvidia-smi parsing** are all replaceable with well-maintained crates, with one *latent correctness fix* embedded (process-group SIGTERM).
5. **UX has good bones** (live keymap chips, dual-encoded status, inline filter). The friction points are local: chip ordering, glyph fonts, hidden destructive keys.

None of the findings below are blocking. Triage gives a clear two-week worth of high-leverage cleanup.

---

## 1. DRY & YAGNI

### 1.1 High-value simplifications

| # | Finding | Site | Fix |
|---|---|---|---|
| 1 | `ManagedState → label` pattern-matched in two places | `src/ipc/methods.rs:420-432`, `:598-613` | Add `ManagedState::label() / cause()` to `src/daemon/supervisor.rs:49`; removes the `unreachable!()` |
| 2 | `mode_label()` helper duplicated 3× | `src/cli/start.rs:113-119`, `src/cli/presets.rs:191-197`, `src/tui/events.rs:908-912` | `LaunchMode::label()` already exists in `src/launch/mode.rs:23` — delete the copies |
| 3 | `Quant::from_label` only handles 15 of 31 variants | `src/tui/app.rs:1009-1034` (`parse_quant`) | Derive from the canonical table in `src/gguf/metadata.rs:183-218`; closes silent `Quant::Unknown(0)` swallow for IQ/TQ quants |
| 4 | `gpu_backend` string handling forks 3× | `src/tui/info_pane.rs:225-231`, `src/cli/output.rs:183-218`, `src/tui/host_stats_pane.rs:179-196` | Route all three through `HostMetricsSnapshot::BACKEND_*` constants in `src/daemon/host_metrics.rs:58-73` |
| 5 | Tab-submit scaffold duplicated 3× | `src/tui/events.rs:572-589, 594-619, 622-663` | Extract `spawn_tab_call(app, focus_label, port_input, f)`; pair with consolidating `record_error` across `src/tui/tabs/{chat,embed,rerank}.rs` |
| 6 | `focused_managed` empty-toast guard duplicated 5× | `src/tui/events.rs:572-578, 594-600, 622-628, 670-674, 712-716` | `with_focused_managed(app, ctx, |m| {...})` helper |
| 7 | `v.get("id").get("path")` decode repeated 5× | `src/cli/{favorites,last_params,stop,output}.rs` | `cli::output::row_path(&Value) -> Option<&str>` |
| 8 | `LaunchParamsWire` field set spelled out in 5 sites | `StartParams`, `PresetsSaveParams`, `start.rs::PartialParams`, `presets.rs::Save` arm, `WriterCmd::StartModel` | One `#[serde(flatten)]`-able struct shared across IPC and consumers |

### 1.2 YAGNI — delete now

- **`Quant::bytes_per_elem`** (`src/gguf/metadata.rs:157`) — never called; `CacheType::bytes_per_elem` (memory.rs:34) is the live path.
- **`Action::all_config_names`** (`src/tui/keybindings.rs:984`) — no callers.
- **`MethodContext::with_peer_authorizer`** (`src/ipc/methods.rs:202`) — production wires the default; no test override.
- **`MethodContext::with_host_metrics`** (`src/ipc/methods.rs:238`) — fully shadowed by `with_sampler`.
- **`ReasoningHint` single-variant enum** (`src/gguf/metadata.rs:235`) — `Option<ReasoningHint>` is used purely as a `bool` in `src/discovery/catalog.rs:104,107`. Collapse to `bool reasoning_hint_present` until a second variant exists.
- **Legacy free `action_for`/`bindings_for`** (`src/tui/keybindings.rs:810,820`) — comment already labels them "legacy"; `KeyMap` methods at `:869,879` replaced them.

### 1.3 Duplication clusters worth a single refactor

- **`ManagedState` / `LaunchMode` / `Quant` ↔ string label mappings.** Each enum has a `label()` but every consumer that needs the reverse — or that emits the forward mapping at a callsite — re-types the match.
- **`gpu_backend` string-tagged dispatch** in three render layers (see §1.1 #4). Move classification into `daemon::host_metrics`; renderers ask `snap.flavor()`.
- **`PresetsSaveParams` / `StartParams` parallel fields** — R2 P3-26 calls this out for the wire types, but the *consumers* (`PartialParams`, `WriterCmd::StartModel`, `presets.rs::Save` arm) all also respell. A shared flatten-able struct unifies all five sites.

---

## 2. Library substitutions (hardware & system primitives focused)

### 2.1 Strong recommends

| # | Subsystem | Current | Proposed | Delta | Verdict |
|---|---|---|---|---|---|
| 1 | PID lockfile | `src/daemon/lockfile.rs` ~140 LOC impl + `cfg(not(unix))` stub | **`fd-lock`** 4.0.4 (used by Cargo) | −80 / +10 | Drops untested non-unix branch |
| 2 | Atomic state write | `src/daemon/state_store.rs::write_tmp_safely` ~80 LOC + `random_suffix` | **`tempfile::NamedTempFile::persist`** (already transitive) | −80 / +15 | Symlink-tmp tests still pass |
| 3 | Process group / setsid | `src/daemon/supervisor.rs:265-283` + signal at `:490-509` | **`command-group`** 5.0.1 | Wash on LOC | **Latent correctness fix**: signals whole process group, fixing grandchild leaks |
| 4 | NVIDIA GPU probe | `src/gpu/nvidia.rs` 182 LOC `nvidia-smi` CSV parser | **`nvml-wrapper`** 0.12.1 (dlopens `libnvidia-ml.so`) | −120 / +50 | Also eliminates `gpu::run_with_timeout` on NVIDIA path — no more 200-800ms cold-start hang risk |

### 2.2 Worth investigating

- **Orphan HTTP probe** — `src/daemon/orphans.rs::fetch_models_body` hand-rolls HTTP/1.1 (~70 LOC) to avoid pulling a heavy client. `reqwest` is already a top-level dep for the chat tab; the avoidance comment is stale.

### 2.3 Rejected (so they don't come up again)

- **GGUF parser replacement.** The identity-hash contract (`read.raw.len() == bytes.len()` at `src/gguf/header.rs:461`) is load-bearing. `candle-core::gguf` and the `gguf` crate don't expose it. The 2090 LOC is honest cost; ~40% is tests/fixtures.
- **`jsonrpsee` for IPC.** Would not bolt onto length-prefixed Unix-socket framing; framing layer is already 197 LOC fully tested.
- **`portpicker`.** Needs bounded range + reserved-set carve-out, both missing. Current 80-LOC implementation is right-sized.
- **`wgpu::AdapterInfo` for cross-vendor GPU info.** Would pull 100+ transitive deps to read adapter name. Wrong size for the problem.
- **Peer-cred wrapper (`nix`).** ~60 LOC platform code each on Linux/macOS would shrink one `libc::getsockopt` call. Not worth a new dep.
- **Custom keybinding parser** (`tui/keybindings.rs`, 1576 LOC). Only ~75 LOC is `parse_key_spec`/`parse_key_token`; the rest is the bindings table and focus dispatch — genuinely project-specific.
- **`atomicwrites` instead of `tempfile`.** Same idea as #2 but adds a second crate doing the same job; `tempfile` is already transitive.

---

## 3. Consistency — top 10 standardisations

Ordered by leverage (highest first — most-touched-on-every-PR at the top).

1. **`Palette` style helper methods.** Add `label_style()`, `muted_style()`, `title_style()`, `error_style()`, etc. and mechanically replace **123** inline `Style::default().fg(palette.X)` sites. Worst clusters: `tui/info_pane.rs` (26), `tui/right_pane.rs` (16), `tui/host_stats_pane.rs` (16), `tui/list_pane.rs` (13).
2. **`panel_block(title, palette, focused) -> Block<'static>` helper** in `tui/fmt.rs`. Collapses 10 identical `Block::default().borders(Borders::ALL).border_style(...).title(...)` chains. The existing `fmt::panel_title` is used in only 2 of 10 callers.
3. **`ok_or_err(id, result) -> Response` helper** to collapse 14 nearly-identical dispatch arms at `src/ipc/methods.rs:342-392`.
4. **Migrate 10 hand-rolled error enums to `thiserror::Error`.** Targets: `gguf/errors.rs`, `daemon/{ports,lockfile,state_store,supervisor}.rs`, `ipc/{client,framing}.rs`, `launch/binary.rs`, `theme/palette.rs`, `util/clipboard.rs`. Removes ~80 lines of plate. (`gguf/errors.rs` already mentions `thiserror` in a comment but doesn't use it.)
5. **Tighten pane render signatures.** Replace `app: &App` (couples the pane to the whole app) with per-pane snapshot structs. Template already exists in `tui/tabs/logs.rs:147` (`&LogsTabState`) and `tui/host_stats_pane.rs:32` (`&HostMetricsSnapshot`). Drift: `info_pane.rs:30`, `right_pane.rs:30`, `tabs/{chat,embed,rerank,settings}.rs`, `logo_pane.rs:24`.
6. **CLI handler signatures uniform.** `cli/daemon.rs:25` returns bare `anyhow::Result<()>`; every other handler returns `CliResult`. The `map_anyhow` shim at `cli/mod.rs:65` exists only to bridge this outlier. Rewrite + delete the shim. Also bring `cli/pull.rs:12` into `(args, cli, config)` shape (if not deleted per §1.2).
7. **One suffix for "named bag of params to a fn".** Currently four: `*Options` (most common), `*Inputs` (`LocateInputs`, `SweepInputs`, `RowInputs`, `TitleInputs`, `RenderInputs`), `*Opts` (singleton `InputPaneOpts`), `*Args`. Settle on `*Options` (defaultable) vs `*Inputs` (mandatory); drop the `Opts` truncation.
8. **Four sync-FS-in-async sites.** `daemon/state_store.rs:158,263` (**held under a `tokio::sync::Mutex`!**), `discovery/ollama.rs:72`, `discovery/metadata_cache.rs:157`, `cli/client.rs:88`. Swap to `tokio::fs::*`.
9. **Rename `Header::get_string` → `string`, `Header::get_u64` → `u64`** (`gguf/header.rs:184,194`). Only `get_*` getters in the crate; everything else is bare noun.
10. **Wrap 12 silent `let _ = tx.send(...).await` drops.** Sites: `tui/events.rs:619,663,843,846,849,852,857,1105`; `tui/oai_client.rs:70,90,123,130`. Add a `try_emit!(tx, msg)` macro that logs at `warn` when the channel is closed — mirrors the daemon's pattern and surfaces shutdown bugs.

### Other consistency notes

- **IPC error message tone** mixes lowercase fragments (`"unknown launch_id: …"`) with mid-sentence imperatives (`"set exactly one of \`port\` …"`). Pick: lowercase fragment, no terminating period, `key: value` for variables.
- **Builder vs struct-literal split.** `MethodContext` uses a fluent builder (`with_sampler`, `with_catalog`, …); every other struct in the crate uses public-fields literal construction. Builders should be reserved for runtime-wired objects with optional collaborators.
- **Test conventions are clean.** 533 `#[test]` attributes, all inside `#[cfg(test)] mod tests` blocks. No drift worth flagging.

---

## 4. Performance opportunities

### 4.1 Tier 1 — user-visible, measurable

| # | Finding | Site | Win | Risk |
|---|---|---|---|---|
| 1 | `rendered_rows()` invoked ~5× per frame | `tui/render.rs:42,212`, `tui/right_pane.rs:226,268,310,319`, `tui/app.rs:466-789` | 60–80% reduction in per-frame heap alloc on 200-model catalog. **Biggest single perf finding.** | Low. Compute once at top of `render::render`, pass `&[ListRow]` to every panel |
| 2 | TUI draws unconditionally every 8ms (~125 idle draws/s) | `tui/events.rs:979-1010` | Idle CPU near-zero | Medium. Need `App::mark_dirty()` and audit of every state-mutating site; force redraw every ~250ms for toast expiry |
| 3 | `host_cpu_temp_c()` rebuilds `Components` each tick | `daemon/host_metrics.rs:185-207` | Dozens of allocations/s eliminated on server-grade boards | Low. Hoist into sampler state, cache matched sensor indices |
| 4 | `ModelCatalog::to_list_response()` clones every model every IPC poll | `discovery/catalog.rs:67-78`, `ipc/methods.rs:337-340` | Steady-state IPC poll on 500-model catalog drops by ~the whole projection cost | Low. Revision counter + cached `Arc<Value>`, invalidate on mutate |
| 5 | `status_response` does 4×N awaits per status | `ipc/methods.rs:406-464` | Halves-to-thirds status build time; reduces lock contention | Low. `ManagedModel::snapshot_for_status()` returning one struct under one borrow |
| 6 | Probe opens fresh TCP every 500ms during Loading | `daemon/probe.rs:49-87` | Marginal CPU; cleaner network surface (≤240 socket churns per slow load) | Low. Pipeline on one keep-alive socket |
| 7 | `ModelId::header_hex` uses `format!("{b:02x}")` 32× per call | `gguf/identity.rs:60-79, 31-40` | Small per-call but called per status per launch | None. Use `write!` or `hex::encode` |

### 4.2 Tier 2 — cheap cleanup

- Per-row `format!` in `tui/list_pane.rs:330-378` for `display_name`/`mode_hint_label`. The four `mode_hint_label` values are `&'static`; cache `display_name` on `DiscoveredModel` at parse time.
- `take_tail_by_width` (`tui/info_pane.rs:152-161`) allocates a `String` per char for unicode width. Use `UnicodeWidthChar::width(ch)` (already imported transitively).
- `BTreeSet<PathBuf>` for scanner dedup at `discovery/scanner.rs:186-209` → `HashSet`. Same loop also does 1000 sequential `canonicalize` syscalls; `buffer_unordered` over a parallel canonicalise if cold-FS becomes a bottleneck.
- `KeyMap::action_for` (`tui/keybindings.rs:866-877`) is `BTreeMap::get` + linear scan. Collapse focus dispatch into `HashMap<(KeyCode, KeyModifiers), Action>` per focus — 5-line refactor, O(1) lookup.
- Per-launch `params.advanced.iter().map(to_string_lossy).collect()` runs every status poll (`ipc/methods.rs:443-447`). Cache the JSON shape on `ManagedModel` since `params` is immutable.
- `aggregate_gpu` (`daemon/host_metrics.rs:243-258`) collects then sums — fold in one pass.
- Render-path chip-strip computation runs per frame in `tui/render.rs:289-330` and `tui/right_pane.rs:97-180`. Cache per `(focus, right_tab, filter_active, on_running)` tuple.

### 4.3 Tier 3 — structural

- **Unified per-frame `RenderFrame` cache** with revision counter. Combined with the dirty-bit, idle TUI cost becomes "one chip strip every ~250ms."
- **Incremental `logs_tail` cursor** (per-conn `last_seq`) instead of resending the full 4096-line ring every 500ms.
- **Coalesce `state_store::save`** with a 50ms debounce — a `favorites add x; add y; add z` script triggers three full serialise+rename now.
- **Discovery walker on rayon** for the parse-fanout half; existing `buffer_unordered` covers per-file but `collect_gguf_paths` + `canonicalize` are still serial.
- **BLAKE3 incremental hash during GGUF header read.** Current path materialises the buffer first, then hashes; switch to `blake3::Hasher::new()` fed by chunks so hash overlaps with disk I/O.
- **Hot-path `BTreeMap<PathBuf, _>` → `HashMap`.** `SupervisorRegistry`, `ModelCatalog`, `surface_states` all key on `PathBuf`; each lookup does multi-segment path comparison. Only worth doing alongside the per-tick cache.

### 4.4 Profiling

Concrete `cargo flamegraph` runs to confirm Tier-1 wins:

```bash
# 1. Idle TUI cost (Tier 1 #1 + #2)
cargo flamegraph --bin llamadash -- tui
# Leave on Models pane 60s, no input. Expect rendered_rows / build_rows
# / BTreeMap::insert to dominate. Post-fix: mostly mio::poll.

# 2. Scroll cost
# Hold PageDown for 10s on a 200-model catalog. Compare
# model_row / display_name / String::clone percentages.

# 3. list_models IPC throughput (Tier 1 #4)
# Script 1000 list_models calls vs 500-model catalog. Look for
# to_list_response / serde_json::Value::clone weight.

# 4. Cold-start discovery (Tier 2 scanner + Tier 3 rayon + BLAKE3)
cargo flamegraph --bin llamadash -- list --json
# Against a synthetic tree of 2000 minimal GGUFs (hfcache layout).
```

Release profile in `Cargo.toml` is already optimal (`lto="thin"`, `codegen-units=1`, `strip=true`). No changes needed there.

---

## 5. UI / UX

Based on the golden snapshot at `tests/golden/dashboard-overview.txt`, the `tui/render.rs` composition layer, and the help surface (`help_overlay.rs`, `help_bar.rs`).

### Strengths

- **Dual-encoded status** (colour + glyph) in the legend strip — works in mono terminals and for colour-blind users.
- **Inline filter chip** inside the Models block title rather than a dedicated bottom row.
- **Live keybinding resolution** in every chip — `keybindings:` config overrides flow through automatically without code changes.
- **Empty-state copy is actionable** (`render.rs:376-386`): "Drop a `.gguf` into a watched directory or run `llamadash --model-path <DIR>`."
- **Width-adaptive layout** — logo panel drops, info row drops, hint strip clips chips by importance order. Good defensive work.

### Friction & gaps

1.  WONT-FIX: **Two glyph systems coexist.** Title row uses emoji (🦙) and BMP glyphs (●, ◐); legend strip uses geometric Unicode (◌ ◐ ● ▲ ○ ⇪). Users in font-poor terminals see a mix of "renders / box / renders". Either pin everything to BMP geometrics (drop 🦙 in a `font_minimal: true` config), or ship a pre-rendered ASCII fallback strip.
2.  WONT-FIX: **`Q:kill daemon` is unsafely placed.** Current chip order is `?:help · Tab:fields · ←/→:panes · Q:kill daemon · t:theme · q:quit`. Destructive key sits two chips from the cosmetically-identical `q:quit`. Move `Q:kill daemon` to the far right or gate it behind a modal-only path (no chip).
3. **Hint chip placement is asymmetric across panes.** Models block title has a smart budget-based chip dropper (`render.rs:289-332`); the right pane has no equivalent. On narrow terminals the right-pane chips clip first and unpredictably.
4. WONT-FIX: **Logo panel is decorative overhead.** Eats 11 cells at width 100 (`LOGO_PANEL_WIDTH=11`). Either add `show_logo: false` config or repurpose for denser daemon metrics. The existing `models  3 found · 1 ready · 2 ★` line is excellent and could expand.
5. **"Mode" column dead until embed/rerank surfaces.** Golden snapshot shows `chat` / `chat` / `chat`. Suppress when every value is the same, or relegate to the detail/picker view.
6. **`?:help` overlay's editorial grouping by `Focus` is opaque.** The `MODELS_ENTER` row collapses two distinct contexts ("while filtering" / "otherwise") into one row described as "apply filter/launch". Render as two rows with a contextual prefix so users learn the model.
7. **`s:stop` vs `s:auto-scroll` collision.** Same key, different focus. Border colour does signal focus, but a first-time user pressing `s` in the right pane while looking at a running model gets a non-obvious surprise. Annotate the chip with focus on first encounter or auto-stop showing `s:stop` when right pane has focus.
8.  WONT-FIX: **Active right-pane tab is hard to spot.** `Logs │ Chat │ Settings` reads as equal-weight in the golden. Use underline + bold for active, or a leading `▸` glyph.
9. **No "terminal too small" placeholder.** `--render-size` minimum (`cli/cli_args.rs:89`) is enforced nowhere; sub-minimum terminals get a clipped paint silently.
10. **Toast surface not documented.** `App::show_toast` is called from many sites; the user-visible contract for placement, dismissal, and modal precedence isn't in `docs/usage.md`.
11. **Esc-in-filter-while-overlay-open is ambiguous.** `Esc` closes the help overlay and also clears filter buffer. If both are simultaneously active, the user loses their typed filter.

---

## 6. Other improvements

- **`config.example.yaml` referenced in `docs/usage.md:15` was not visible in the repo listing.** Verify it exists; documented onboarding ("copy it to the path above and edit") depends on it.
- **Field-level over-documentation.** `theme/palette.rs:107-158` puts 3–6 line docs on every palette slot — worst offender. Hoist semantics into one block at module top; per-field docs ≤ 2 lines. Same pattern in `ipc/methods.rs:64-104` on `MethodContext` fields.
- **Stale comments in `daemon/supervisor.rs:17-22`** mention rotation specifics that contradict current code (already noted in R1 P1-10).

---

## 7. Recommended fix order

Each tier is sized for one focused unit.

### Tier A

1. Delete the four YAGNI-confirmed methods/enums (`bytes_per_elem`, `all_config_names`, `with_peer_authorizer`, `with_host_metrics`, `ReasoningHint`, legacy `action_for`/`bindings_for`).
2. Rename `Header::get_string` → `string`, `get_u64` → `u64`.
4. Swap 4 sync-FS-in-async sites to `tokio::fs::*` (see §3 #8).

### Tier B 

5. **`Palette` style helpers** (§3 #1) — mechanical sed across 123 sites.
6. **`panel_block` helper** (§3 #2) — collapses 10 Block construction chains.
7. **`ok_or_err` IPC dispatch helper** (§3 #3) — collapses 14 match arms.
8. **`ManagedState::label()/cause()` + `Quant::from_label` + `gpu_backend` centralisation** (§1.1 #1, #3, #4) — three small symmetric simplifications.

### Tier C 

9. **Render-row hoist** (§4.1 #1) — compute `rendered_rows()` once at top of `render::render`, pass `&[ListRow]` to every panel. Biggest user-visible win and unblocks the unified `RenderFrame` cache.

### Tier D 

10. **`fd-lock` + `tempfile::persist` + `command-group`** together. Three mechanical changes, each well-tested. Process-group SIGTERM is a latent correctness fix worth surfacing in the PR description.

### Tier E : This needs user confirmation so discuss first.

11. **`nvml-wrapper`** swap. Biggest user-visible win on NVIDIA hosts (no more 200-800ms `nvidia-smi` cold start) but needs careful dlopen-fallback testing on driver-less hosts.

### Tier F — UX iteration

12. Dual-glyph audit (§5 #1) and `Q:kill daemon` chip placement (#2). Both are essentially 1-line fixes with disproportionate clarity gain.
13. Active right-pane tab styling (§5 #8), terminal-too-small placeholder (#9).

### Tier G — structural perf (later)

14. Dirty-bit redraw gate (§4.1 #2) + `RenderFrame` cache (§4.3) + `list_models` revision cache (§4.1 #4). Each can land independently but they compound.

---

## Appendix A — files touched per recommendation

Quick map for PR planning. Where a recommendation modifies many files, only the headline targets are listed.

| Recommendation | Headline files |
|---|---|
| `Palette` style helpers | `src/theme/palette.rs`, `src/tui/{info_pane,right_pane,host_stats_pane,list_pane}.rs` |
| `panel_block` helper | `src/tui/fmt.rs`, then every pane caller |
| `ok_or_err` IPC | `src/ipc/methods.rs:342-392` |
| `thiserror` migration | `src/{gguf/errors,daemon/{ports,lockfile,state_store,supervisor},ipc/{client,framing},launch/binary,theme/palette,util/clipboard}.rs` |
| Render-row hoist | `src/tui/{render,app,right_pane,list_pane,info_pane}.rs` |
| Dirty-bit gate | `src/tui/{app,events,render}.rs` |
| `fd-lock` swap | `src/daemon/lockfile.rs`, `Cargo.toml` |
| `tempfile::persist` swap | `src/daemon/state_store.rs`, `Cargo.toml` |
| `command-group` swap | `src/daemon/supervisor.rs`, `Cargo.toml` |
| `nvml-wrapper` swap | `src/gpu/nvidia.rs`, `src/gpu/mod.rs`, `Cargo.toml` |

---

## Appendix B — methodology

Four parallel `general-purpose` agent passes were dispatched concurrently, each with a self-contained brief naming the files to read first and instructions to skip findings already in `docs/review.md`:

| Agent | Angle | Output |
|---|---|---|
| A | DRY / YAGNI / duplication clusters | §1 |
| B | Library substitution research (hardware focus) | §2 |
| C | Consistency / style / API signatures | §3 |
| D | Performance opportunities | §4 |

UI/UX (§5) and "other improvements" (§6) were done manually by reading `tests/golden/dashboard-overview.txt`, `src/tui/render.rs`, `src/tui/help_overlay.rs`, `src/tui/help_bar.rs`, and the project root state.

No tests were run as part of this review; all findings are static-analysis level and require validation against a running build.

---

# Follow-up review — rounds 6-8 polish (2026-05-18, additive)

**Scope:** `git diff HEAD~3..HEAD` — three TUI commits that together churn ~2.2 kLOC across `src/tui/{events,keybindings,launch_picker,help_overlay,list_pane,right_pane,help_bar,app,confirm_overlay,advanced_panel,render,tabs/*}.rs` plus the golden snapshot and smoke tests.

| Commit | Round | Summary |
|---|---|---|
| `a57c036` | 6-7 | Delete centred launch-picker modal (`Focus::LaunchPicker` enum variant gone); arrow-driven navigation overhaul (`Tab/Shift+Tab` = pane cycle; `↑↓` = in-axis; `←→` = value-cycle-or-no-op); Settings rows wrap focused values in `◀ value ▶`; favorites surface in folder groups via `ListRow::Divider`; Home/End alias vi `g`/`G`. |
| `43cce21` | 8 | Confirm-popup before launching a model that already has managed instances; `s` on Settings = stop focused managed launch; `p/u/c` yank path/url/curl from right pane; chip-strip rework (header chips trim, body chips expand); arrow scroll for chat/embed/rerank output viewports; `→` from Models opens right pane (asymmetric — `←` stays unbound). |
| `0ee01df` | — | Swap three Nerd-Font Private-Use-Area glyphs for standard Unicode: `󰘶 → ⇧`, `󰑐 → ▶`, `󱑎 → ↺`. |

**Methodology:** same four parallel `general-purpose` agent passes as the parent audit (DRY/YAGNI, consistency, performance, UI/UX-and-correctness), each briefed to ignore the parent audit's already-catalogued findings and produce only additive deltas. All findings below cite `file:line` and are scoped to behaviour these three commits introduced or aggravated.

---

## F1. Bugs and regressions (must-fix)

Severity-ordered. Each verified against the source, not just inferred from the diff.

| # | Severity | Finding | Site | Notes |
|---|---|---|---|---|
| 1 | **P0** | **`s` on Chat/Embed/Rerank silently flips Logs auto-scroll.** The fall-through `else` branch in `Action::ToggleAutoScroll` executes `app.logs_state.auto_scroll = !…` for *every* `Focus::RightPane` that isn't Settings — directly contradicting the comment three lines above ("Any other right tab → no-op so we don't accidentally fire a stop or scroll toggle on Chat/Embed/Rerank"). Repro: focus a running model, Tab to Chat, press `s`, Tab to Logs — auto-scroll state has silently flipped. | `src/tui/events.rs:312-325` (commit `43cce21`) | Gate the else on `RightTab::Logs`. |
| 2 | **P1** | **`cycle_right_tab` / `cycle_right_tab_prev` hardcode `Logs` as the empty-set fallback, contradicting round-8's `ensure_right_tab_reachable` fix** which explicitly snaps to the first available tab and is unit-tested for "Settings is the universal fallback" (`app.rs:1619`). The two cycle helpers should match. | `src/tui/app.rs:870, 883` | One-line fix each: replace `RightTab::Logs` with `tabs.first().copied().unwrap_or(RightTab::Settings)`. |
| 3 | **P1** | **`snap_cursor_to_launch` mutates `list_cursor` without `sync_picker_to_focus`.** It is the only `list_cursor` writer that skips the helper the round-8 commit message specifically introduced to clear stale picker state on cursor moves. When a status snapshot lands during launch and snaps to a different path than the picker was staged for, Settings renders ports/name for the *previous* path. | `src/tui/app.rs:517-528` | Wrap with the standard `let before = self.focused_path(); … self.sync_picker_to_focus(before);` pattern. |
| 4 | **P2** | **Confirm popup hint promises `Esc / n cancel` but `n` has no explicit binding** — every non-Submit key cancels (`events.rs:120-138`), so `n` works by accident, and so does any other key. The hint teaches a binding that doesn't really exist. | `src/tui/confirm_overlay.rs:71-77` | Either bind `n`/`N` explicitly alongside `y`/`Y`, or drop `/ n` from the hint copy. |
| 5 | **P2** | **`s` on Settings with no managed launch is a fully silent no-op.** `apply_stop_model` (`:476-479`) toasts `"nothing to stop — focus a running model"` when reached from List, but the Settings-tab dispatch at `events.rs:318-321` early-returns before that path. The chip is correctly hidden (`right_pane.rs:172-175`), but the keybinding is still live. | `src/tui/events.rs:318-321` | Drop the `is_some()` guard so the standard toast fires; the inner helper already handles the empty case. |
| 6 | **P3** | **Commit `43cce21`'s body claims "Render Shift as the Nerd Font 󰘶 glyph in every key label" but the code at `keybindings.rs:1163` is `SHIFT_GLYPH = "⇧"` (U+21E7, standard Unicode).** The code is correct (commit `0ee01df` scrubs Nerd-Font codepoints); the commit message is stale. Affects no behaviour, but worth a release-notes correction so future archaeology doesn't get confused. | `src/tui/keybindings.rs:1163`, commit `43cce21` body | Note in CHANGELOG, no code change. |

---

## F2. DRY & YAGNI (additive to §1)

### F2.1 High-value simplifications (round 6-8)

| # | Finding | Site | Fix |
|---|---|---|---|
| 1 | **Right-pane arrow-nav table is open-coded twice.** `Action::MoveDown` and `Action::MoveUp` each have five near-identical `Focus::RightPane if app.right_tab == …` arms (Logs / Settings / Chat / Embed / Rerank / `_`). | `src/tui/events.rs:235-257` (`a57c036`) | One `fn arrow_target(right_tab) -> ArrowTarget` table, or a `RightTabExt::scroll_up/down(&mut App)` dispatcher. 10 arms → 2. |
| 2 | **`scroll_up` / `scroll_down` / `scroll_offset` triplicated across three tab states**, with `LogsTabState` as a 4th near-clone (different type — `usize` vs `u16`). | `src/tui/tabs/chat.rs:46,80-87`, `tabs/embed.rs:24,46-52`, `tabs/rerank.rs:37,51-57`, `tabs/logs.rs:36,59-69` | Extract a `ScrollableViewport { offset: u16 }` value type; embed by composition. The chat/embed/rerank copies also lack the `auto_scroll` clamp `LogsTabState` has — they currently let `scroll_offset` grow to `u16::MAX` past content. |
| 3 | **Launch-picker auto-stage block triplicated.** Same 4-line `if app.launch_picker.is_none() { … } if let Some(p) = … { … }` in three siblings. | `src/tui/events.rs:386-394, 400-407, 418-425` (`a57c036`) | `fn with_picker(app, f: impl FnOnce(&mut LaunchPickerState))` — auto-stages on first call, dispatches `f`. |
| 4 | **Caret-cursor span open-coded in three single-line inputs.** Same `Span::styled("▏", accent + REVERSED)` pattern — round 8 unified the visual style but didn't extract the helper. | `src/tui/advanced_panel.rs:96-103`, `tui/list_pane.rs:669-674`, `tui/tabs/input_pane.rs:90-100` | `fn caret(palette) -> Span<'static>` in `tui/fmt.rs`. Pairs naturally with §3 #1 (`Palette::accent_style()`). |
| 5 | **`ConfirmAction::LaunchDuplicate` re-states all six `WriterCmd::StartModel` payload fields** (`model_path / ctx / reasoning / advanced / mode / prefer_port`). `apply_confirmed` immediately destructures the variant just to rebuild the writer cmd. **This is a sixth site of the §1.1 #8 `LaunchParamsWire` field-set respelling.** | `src/tui/app.rs:185-197`, `tui/events.rs:514-531` (`43cce21`) | Embed a shared `LaunchSpec` struct in both variants — same shape as §1.1 #8 lands. |
| 6 | **Settings-tab chip strip lives in two places** (`contextual_hints` for title strip + `build_form_hints` for body). Both walk the same `app.hint(Focus::RightPane, …)` table. | `src/tui/right_pane.rs:158-178`, `tui/tabs/settings.rs:84-107` (`43cce21`) | `fn settings_chips(app) -> SettingsChips { title: Vec, body: Vec }` so the placement decision is data, not code. |
| 7 | **`Tab / Shift+Tab → Next/PrevFocus` binding triple lives in five binding tables.** Round 7 made the pane-cycle universal, exposing the duplication. `Esc → ExitEdit` similarly repeats across the three `*_INPUT_BINDINGS`. | `src/tui/keybindings.rs:351-363, 501-513, 677-689, 728-740, 769-782` | `const PANE_CYCLE: &[Binding] = …` + `const EXIT_EDIT: &[Binding] = …`; concat at call sites. |

### F2.2 YAGNI — delete or fold

- **`ReasoningSetting::from_persisted(prev: bool)`** is a 7-line wrapper around a bool→enum match called from a single site (`app.rs:752`). Fold inline. (`launch_picker.rs:76-82`, `43cce21`)
- **`ReasoningSetting::next` / `prev`** + **`cycle_reasoning_next` / `cycle_reasoning_prev`** are two layers of single-caller forwarders. Drop one layer. (`launch_picker.rs:55-71, 158-167`)
- **`cycle_ctx_preset_prev`** has one caller. Same pattern. (`launch_picker.rs:148-156`)
- **`SETTINGS_FIELD_NEXT` / `SETTINGS_FIELD_PREV`** are single-element `&[(Focus, Action)]` slices wrapped in `Row::Multi` purely to reach the description-override path. Add a `Row::Override { focus, action, description }` variant; collapse the misleading 1-element `Multi`s. (`help_overlay.rs:243-244, 253-260`)
- **Stale doc comments** referring to deleted `Focus::LaunchPicker`: `keybindings.rs:74-75, :1303`, `help_overlay.rs:572` (all comments only — no live references).

### F2.3 Duplication clusters worth one refactor

- **Chip-strip composition** — six functions (`right_pane::contextual_hints`, `tabs/settings::build_form_hints`, `tabs/{chat,embed,rerank}::idle_status_chips`, `help_bar::global_hint_text`) all build `Vec<String>` by walking `app.hint(...)` / `app.hint_with(...)` for a hard-coded `(focus, action[, override])` list. A `fn build_chips(app, &[(Focus, Action, Option<&str>)]) -> Vec<String>` collapses all six and pairs with §3 #2's `panel_block` helper.
- **Tab-keyed dispatch policy** lives in both `events.rs::Action::MoveDown/Up` (§F2.1 #1) and the `Action::ToggleAutoScroll` branch (`events.rs:312-325`). Both encode "Settings-tab is special" twice. Combine via a `RightTabBehavior` enum or trait-object on the tab state.

---

## F3. Consistency (additive to §3)

1. **Arrow-semantics drift on the Models list.** Round 7's contract is `Tab` = pane-cycle, `↑↓` = in-axis, `←→` = value-cycle. The Models list breaks the second half: `→` is bound to `NextFocus` with description `"right pane"` (`keybindings.rs:343-349`). `←` is intentionally unbound — asymmetric. Either re-label `→` to acknowledge the exception, or migrate it off arrows. **Extends §F1 #24 (UX QUESTION).**
2. **`Action::MoveUp` / `MoveDown` descriptions are tab-context-specific but stored once per focus.** Default labels (`"scroll up"`/`"scroll down"`) are correct only on Logs/Chat/Embed/Rerank viewports. On Settings the same keys cycle fields. `help_overlay.rs:233-241` works around this with `Row::Multi` overrides, but `help_bar.rs` doesn't — a Settings user sees a stale `↓:scroll down` chip on narrow strips. Either split the action (`ScrollDown` vs `NextField`) or thread description overrides into the chip resolver. (Extends §3 #1.)
3. **`ListRow::Divider` paint convention** uses `palette.muted` (correct) but is the only `ListRow` variant that emits `repeat()` against `content_w` — exactly the surface a `panel_separator(palette)` helper would normalise. Pair with §3 #2.
4. **Three new `scroll_up`/`scroll_down` helpers fork from the `LogsTabState` pattern they cite.** No upper clamp, no `auto_scroll` side-effect (see §F2.1 #2). Either hoist the shared abstraction or comment the divergence.
5. **`InputPaneOpts` still uses the truncated `Opts` suffix.** Round 8 added `scroll_offset` to the only struct in the tree that uses `*Opts` (`tabs/input_pane.rs:27`) — a free renaming opportunity that was missed. Extends §3 #7.
6. **Confirm popup chrome hand-rolls a `Block::default().title(...).borders(...).border_style(...)` chain** at `confirm_overlay.rs:31-39` — exactly the §3 #2 target. Help overlay does the same at `help_overlay.rs:339-346`.
7. **`ConfirmAction::LaunchDuplicate` payload conventions are inconsistent.** `StopModel { launch_id, name }` and `KillDaemon` are tight; `LaunchDuplicate` carries 9 fields that mirror `WriterCmd::StartModel`. See §F2.1 #5 — same pattern, same fix.
8. **Help overlay close chip vs. confirm-popup footer have different chrome.** Help overlay: `Esc/?:close` in title bar; confirm popup: footer line. Both bind Esc + a hardcoded char alias. Worth a single "primary-key + char-alias" rendering convention.
9. **`right_pane::contextual_hints` sources the `s:stop` chip from `Action::ToggleAutoScroll` with description override "stop"** (`right_pane.rs:172-175`). That's a meaning-collision (`ToggleAutoScroll` doesn't stop anything) — clean fix is a `ToggleAutoScrollOrStop` virtual action, or routing the chip via `Action::StopModel` directly.
10. **`hint()` "first match wins" contract is documented locally but not on `hint()` itself.** `keybindings.rs:604-630` has the comment "Ordering matters: `hint()` returns the first binding it finds, so the chip strip surfaces the arrow glyphs." But `app.rs::hint` (`:289-295`) has no symmetric note — a future `HashMap<(KeyCode, KeyModifiers), Action>` refactor (per §4.2) will silently break chip ordering. Either move the contract into `hint()`'s docstring or add a `chip_priority: u8` field to `Binding`.

### Other consistency notes

- **Glyph migration is clean.** Repo-wide grep across `src/`, `tests/`, `docs/` for U+E000-U+F8FF and U+F0000-U+FFFFD returns no Private-Use-Area hits. All three originally-cited Nerd Font codepoints (U+F0636, U+F0450, U+F144E) are scrubbed including from `list_pane.rs` group-header tests.
- **`format_key_label`'s "Shift+Shift+Tab" fix** at `keybindings.rs:1181-1183` is correct but the regression scaffold is thin. `KeyCode::F(n) + SHIFT` and `KeyCode::Tab + SHIFT` (crossterm can emit either) are uncovered by tests; add cases at `:1716`.
- **`Action` description text** mostly follows lowercase-fragment convention, but `"launch (Settings)"` at `keybindings.rs:491` breaks it — and the help overlay already overrides it to `"launch/save"`. Drop the parenthetical from the binding table.

---

## F4. Performance (additive to §4)

**Top-line:** no serious regressions. `rendered_rows()` call count is unchanged (verified — 18 references at both `HEAD~3` and `HEAD`). Arrow-scroll math is O(1). Confirm popup adds zero async/timer work. Two material new findings below.

### F4.1 Tier 1

| # | Finding | Site | Win | Risk |
|---|---|---|---|---|
| 1 | **Inline launch form clones `LaunchPickerState` per frame** plus runs `app.hint(...) / hint_with(...)` 5-7× via `build_form_hints`. With round-8 making Settings the default snap target for unlaunched rows, this becomes the steady-state idle path. | `src/tui/tabs/settings.rs:90` (clone), `:201-225` (`build_form_hints`); `tui/app.rs:289-306` (linear binding scan per hint); `tui/right_pane.rs:166-178` (2 more hints in title strip) | 5-7 fewer Strings + 5-7 fewer linear `bindings_for(focus).iter().find(...)` per frame on Settings. Compounds with §4.1 #2 (idle redraw gate). | Low. Take `&LaunchPickerState`; memoise via the §4.2 cache tuple (now needs a `right_tab` axis). |
| 2 | **`available_right_tabs()` allocates a fresh `Vec<RightTab>` 3× per frame** (`render.rs:42`, `right_pane.rs:31, :488`) — each scan walks `self.models` linearly. Round 8 added the third call when reworking `ensure_right_tab_reachable`. | `src/tui/app.rs:839-863`; callers at `render.rs:42`, `right_pane.rs:31, 488` | Eliminates 3 Vec allocs + 3 linear model scans per frame. **Extends §4.1 #1** — should be hoisted into the same per-frame snapshot as `rendered_rows()`. | Low. |

### F4.2 Tier 2

- **`ListRow::Divider` allocates `"─".repeat(content_w)` per render per frame** (`list_pane.rs:790`). ~360 bytes per paint at width 120. Cache via `OnceCell<String>` keyed on `content_w`, or pre-render once on `App` and slice.
- **Favorited models now build_rows twice** (`list_pane.rs:213-258`): once in `★ Favorites`, once in folder group. Doubles `model_row(...)` cost for favorites. The §4.1 #1 `rendered_rows()` hoist now saves proportionally more — sharpens the headline win 15-25%.
- **`format!` per frame** in Settings tab running-launch branch: `format!("{n}")`, `format!(":{}", m.port)`, `format!("{cpu:.0}%")` at `tabs/settings.rs:39, 49, 106, 141-148`. Same pattern as §4.2 list_pane `format!` finding.
- **The §4.2 chip-strip cache tuple** (`(focus, right_tab, filter_active, on_running)`) was already proposed; round-8 widens its need because both `contextual_hints` and `build_form_hints` now branch on the same axes. Implementing the cache now collapses an extra ~6 strings/frame.
- **`format_key_label` is *not* on the hot path** — verified at `keybindings.rs:1165`, called once per binding at parse time and stored in `Binding.label: String`. Commit 43cce21's glyph swap is zero-cost per frame.

---

## F5. UI / UX (additive to §5)

Numbered continuing from §5 (#1-11). Tags: **BUG** = definite regression; **UX** = friction/inconsistency; **Q** = worth flagging but may be intentional.

12. **BUG (P0).** `s` on Chat/Embed/Rerank silently flips Logs auto-scroll. See §F1 #1.
13. **UX.** `s` on Settings with no managed launch is a silent no-op (no toast). See §F1 #5.
14. **UX.** `cycle_right_tab` / `cycle_right_tab_prev` still fall back to `Logs` instead of `Settings` — contradicting round-8's `ensure_right_tab_reachable` contract. See §F1 #2.
15. **BUG (P1).** `snap_cursor_to_launch` doesn't sync the picker on cursor move; Settings can render stale fields after a launch-status snapshot. See §F1 #3.
16. **UX.** **Arrow keys silently auto-open the launch picker.** `events.rs:386-388, 401-402, 419-421` open `LaunchPickerState` on first `↑/↓/←/→` in Settings. Combined with #15, any `snap_cursor_to_launch` race leaves a picker keyed to the wrong path that arrow keys then mutate. At minimum: when auto-opening, re-seed from `last_params` for the *current* focused path.
17. **UX.** **Stale doc comments referencing the deleted `Focus::LaunchPicker`** in `keybindings.rs:74` ("in the LaunchPicker / AdvancedPanel overlays"), `tabs/settings.rs:6` ("renders inline instead of as a centred overlay"), `help_overlay.rs:221-222` ("Shift+Tab in rerank cycles back to the query field" — round 7 moved that to `↑`). Three drift sites the round-6 sweep missed.
18. **UX.** **Round-8 yank keys `p`/`u`/`c` are absent from `help_overlay.rs`.** `keybindings.rs:564-583` adds three new bindings to `RIGHT_PANE_BINDINGS`, but the help overlay's CHAT_EMBED_RERANK (`:200-227`) and SETTINGS (`:246-278`) groups list none of them. Same for `s` on Settings re-routing to StopModel — the overlay still surfaces `ToggleAutoScroll`'s default description.
19. **UX.** Confirm popup hint promises `Esc / n cancel` but `n` has no explicit binding. See §F1 #4.
20. **UX.** `LaunchDuplicate` confirm has no default-action indication — shares the red `palette.error` block with destructive `StopModel` / `KillDaemon`, but is itself *additive*. Title `Launch again` + scary red border is mis-signal. Consider a softer palette slot for non-destructive confirms.
21. **UX.** Settings tab arrow keys are silently no-ops in `CycleValue` when the focused field isn't cyclable (e.g. the `advanced` row). The round-7 chip `→:cycle value` advertises behaviour the user can't predict will work. Compare to `apply_focus_chat_tab:456` which toasts on miss. Add a toast.
22. **UX.** Settings chip strip teaches a forward-only model. `tabs/settings.rs:206, 209` surface only `MoveDown` and `CycleValueNext` — `↑` and `←` are bound but get no chip. Help overlay does row them (`help_overlay.rs:253-271`) but the in-pane chip strip is the discoverable surface.
23. **UX.** Models pane title chip `Enter:launch` still surfaces when the focused row is a `★ Favorites` header — Enter is a silent no-op then (`open_launch_picker:733-737`). The chip strip is supposed to be context-aware (it already gates `s:stop`); extend to gate `Enter:launch` on `focused_name().is_some()`.
24. **Q.** Round-6's `→` from List binding (`keybindings.rs:341`) creates asymmetric `Left=unbound, Right=NextFocus` shape that breaks the "every navigation chord has a paired reverse" intuition. Commit justifies this via "Esc on the right pane handles the return path", but Esc is already overloaded (filter-clear, modal-close, §5 #11). Consider binding `←` from `RightPane=Logs` back to `FocusList` for symmetric round-trip.
25. **Q.** Round-7 freed `Tab` for pane-cycle inside `RerankInput`, but the 2-field rerank (Query ↔ Candidate) now uses `↑↓` for a 2-cycle whose forward and reverse paths are identical (`apply_prev_field:417` even calls `cycle_field()` rather than a true reverse). Was the goal to keep `↑↓` symmetric for future N-field expansion, or did `↓` become a wasted binding?
26. **UX.** `Q:kill daemon` chip ordering (§5 #2) was not improved by round 7. The widened `Tab/⇧+Tab:panes` chip on the title strip now sits adjacent to `Q:kill daemon`, keeping destructive `Q` two chips from cosmetic `q:quit` — the round-7 chip widening was a free opportunity to move `Q` to the far right and didn't take it.

---

## F6. Recommended fix order (follow-up tiers)

### Tier α — bugs (one PR, today)

1. §F1 #1 — gate the `ToggleAutoScroll` else branch on `RightTab::Logs` (one-line fix).
2. §F1 #2 — `cycle_right_tab` / `cycle_right_tab_prev` fall back to `Settings` (two-line fix; one test exists for the symmetric case).
3. §F1 #3 — wrap `snap_cursor_to_launch` with `sync_picker_to_focus`.
4. §F1 #5 — drop the `is_some()` guard on Settings-tab `s`, let `apply_stop_model` toast.

### Tier β — UX honesty (one PR)

5. §F5 #17 — sweep stale `LaunchPicker` comments (`keybindings.rs:74`, `tabs/settings.rs:6`, `help_overlay.rs:221`).
6. §F5 #18 — add `p`/`u`/`c` to the help overlay's RightPane groups; add Settings-tab `s` override.
7. §F5 #19 — bind `n`/`N` explicitly on the confirm popup, or drop `/ n` from the hint.
8. §F5 #21 — toast on no-op `CycleValue`.
9. §F5 #22 — surface `↑:cycle fields back` and `←:cycle value back` in the Settings body chip strip.
10. §F5 #23 — gate `Enter:launch` chip on `focused_name().is_some()`.

### Tier γ — DRY consolidation (small PR)

11. §F2.1 #1 — extract `arrow_target(right_tab)` from the 10 arrow-nav match arms.
12. §F2.1 #2 — extract `ScrollableViewport` shared by chat/embed/rerank/logs; fix the missing upper clamp.
13. §F2.1 #3 — `with_picker(app, f)` helper for the auto-stage triplication.
14. §F2.1 #4 — `caret()` helper in `tui/fmt.rs`.

### Tier δ — perf compounders (later, paired with §4.1 #1 hoist)

15. §F4.1 #1 — `&LaunchPickerState` instead of clone in Settings.
16. §F4.1 #2 — hoist `available_right_tabs()` into the per-frame snapshot.
17. §F4.2 — divider `String` cache; chip-strip cache axis widening.

### Tier ε — payload-shape unification (depends on §1.1 #8)

18. §F2.1 #5 — when `LaunchParamsWire` field-set lands, fold `ConfirmAction::LaunchDuplicate` into the same struct.

### Tier ζ — release-notes correction

19. §F1 #6 — note in CHANGELOG that commit `43cce21`'s body wrongly claims a Nerd Font 󰘶 glyph; the actual rendering is `⇧` (U+21E7).

---

## Appendix C — methodology (follow-up)

Four parallel `general-purpose` agent passes, each briefed with:

- The exact three-commit range (`a57c036..HEAD`).
- An instruction to read `docs/reviews/review-2026-05-18-project-audit.md` first and produce only **additive** findings — pre-existing items get sharpened (and tagged "extends §X #Y") but never repeated.
- A specific angle (DRY, consistency, performance, UX-and-correctness).

| Agent | Angle | Output |
|---|---|---|
| E | DRY / YAGNI / clusters | §F2 |
| F | Consistency / API / naming | §F3 |
| G | Performance / allocations / async | §F4 |
| H | UI/UX + correctness on the new interaction model | §F1 (bugs) + §F5 (UX) |

Bug findings were verified against source (`src/tui/events.rs:312-325`, `app.rs:870/883`, `keybindings.rs:1163`) — not just inferred from the diff. Word count of agent outputs was capped at ~3.3 k total; this section condenses to ~2.4 k.

No tests were run; all findings remain static-analysis level and require validation against a running build before fixes land.

---

## Implementation log — 2026-05-18

Ten commits landed against this audit. Tests go from 718 → 725 (the four Tier α bug repros + Tier B `Quant::from_label` round-trip + Tier B `Palette` helpers + Tier B `GpuFlavor` + Tier C cache-primer test + Tier β `bidirectional_chip` + Tier F too-small placeholder); a few obsolete tests (the hand-rolled HTTP parser; the `apply filter/launch` collapsed help row; one `LockfileError` shape) were either deleted or rewritten alongside the underlying change. `cargo clippy --all-targets --features test-fixtures -- -D warnings` is clean. `cargo fmt --all -- --check` is clean.

| Commit | Tier | Audit refs |
|---|---|---|
| `f3a90a9` | α (bugs) | §F1 #1 (P0), #2 (P1), #3 (P1), #5 (P2) — gated `ToggleAutoScroll` else on `RightTab::Logs`; `cycle_right_tab` fallback to Settings; `snap_cursor_to_launch` + `sync_picker_to_focus`; Settings `s` toasts via `apply_stop_model` |
| `a134047` | A (cleanup) | §1.2 YAGNI deletes; §3 #9 `Header::{string,u64}` rename; §3 #8 partial (`cli/client.rs::canonicalize`) |
| `0fe776c` | B (helpers) | §3 #1 `Palette` helpers (123 sites); §3 #2 `panel_block`; §3 #3 `respond` IPC helper; §1.1 #1 `ManagedState::label/cause`; §1.1 #3 `Quant::from_label`; §1.1 #4 `GpuFlavor` |
| `a2fe3a0` | C (perf) | §4.1 #1 `rendered_rows` per-frame memo |
| `0eb3f20` | D (libs) | §1.2 + §2.1 #3 grandchild SIGTERM (one-character `kill(-pid, …)`); §2.1 #2 `tempfile::NamedTempFile::persist`; §2.2 `reqwest` orphan probe; §3 #4 `thiserror` migration (9 enums) |
| `6caad70` | γ + ζ | §F2.1 #1 `arrow_target` dispatcher; §F2.1 #3 `with_picker`; §F2.1 #4 `caret()`; §F1 #6 CHANGELOG correction |
| `6126496` | β (UX honesty) | §F5 #17, #18, #19, #21, #22, #23 |
| `9ee5665` | F (UX iteration) | §5 #6 split MODELS_ENTER row; §5 #9 too-small placeholder; §5 #10 toast docs |
| `5d8023a` | δ (perf compounders) | §F4.1 #1 borrow picker in Settings; §F4.1 #2 hoist `available_right_tabs` into frame cache |
| `eacaeea` | I (cleanup) | §1.1 #6 `focused_managed_or_toast`; §1.1 #7 `cli::output::row_path`; `cargo fmt` + `cargo clippy` |

### Deferred — intentional

- **§5 WONT-FIX** items (#1 dual glyph systems, #2 `Q:kill daemon` chip placement, #4 logo panel toggle, #8 active right-pane tab styling) — flagged in the audit as out-of-scope for this pass.
- **Tier E — `nvml-wrapper` swap (§2.1 #4).** Needs careful dlopen-fallback testing on driver-less hosts; the audit explicitly requires user confirmation before landing. Not run.
- **Tier G — structural perf** (dirty-bit redraw gate §4.1 #2; `RenderFrame` cache §4.3; `list_models` revision cache §4.1 #4). The audit labels these "later". The biggest wins from this family (rendered_rows hoist, available_right_tabs hoist) already landed in C + δ; the remaining items are structural and need their own focused PR.
- **§F2.1 #2 `ScrollableViewport` with upper clamp.** Fixing the missing upper clamp cleanly requires threading content height through tab state and pairs naturally with Tier G.
- **§3 #5 pane-snapshot signatures, §3 #6 CLI handler shim removal, §3 #7 `*Opts` suffix normalisation, §3 #10 `try_emit!` macro, §6 field-level over-doc.** Each is a medium-scope refactor on tightly-coupled callsites; landing them inside the same sweep would push commit boundaries past usefulness.

### Deferred — dependency unused

- `fd-lock` was added to `Cargo.toml` (Tier D approved deps) but the audit-recommended swap (§2.1 #1) turned out to need ~50 lines of project-specific defences (`O_NOFOLLOW`, `S_IFREG` mode check, 0o600 mode, stale-pidfile rewriting) that `fd-lock` alone doesn't replace. The crate is kept as a top-level dep for a follow-up that wraps the existing module behind `fd-lock`'s API while preserving these defences.

### Audit-claim corrections (already correct in source)

- **§1.2 `Action::all_config_names`** — the audit reports "no callers", but `keybindings.rs::apply_overrides` (warning message for unknown actions) calls it. Kept.
- **§6 stale comments in `daemon/supervisor.rs:17-22`** — verified against current code; the rotation specifics (10 MiB rotate, 5 segments kept) match the consts. No edit needed.
