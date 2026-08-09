# CLI catalog-surface parity: `show --json` on the shared serializer + `list` MODE/DEVICE columns

**Status:** ✅ done (2026-08-09) — both work items landed, E2E-verified on real models in an isolated daemon; docs (CHANGELOG / AGENTS.md / usage.md / UAT findings+plan / TODO.md) updated in the same change. Target **0.1.0** (breaking changes acceptable, no migration).

Closes the two parked follow-ups from the 2026-07-19 E2E UAT:

- **F-01 / §25.2 / §25.3** — surface `multimodal` (and `split_siblings`) on the CLI `list --json` + `show --json`, and render the TUI vision glyph.
- **F-11 (CLI parity)** — add MODE/DEVICE columns to the `llamastash list` table to match the TUI Models list.

Origin: [`docs/testing/2026-07-19-e2e-uat-findings.md`](../testing/2026-07-19-e2e-uat-findings.md) §Parked for maintainer input, and the matching lines in `TODO.md`.

## What is already fixed (verified 2026-08-09 — do not redo)

Both halves below were fixed by `9d0ddef` ("one CatalogRow serializer for list_models + list --json"), which landed **after** the UAT commit `d043f99`. The UAT simply predates the fix.

**F-01 `list --json` — fixed.** All 17 catalog rows are byte-identical to the IPC `list_models` rows (sorted-JSON `diff` produced no output). Rows carry `multimodal`, `mtp`, `split_siblings`, and the nested `metadata` block. `src/cli/output.rs:179` serialises the shared `CatalogRow`; there is no second flat serializer.

**§25.3 TUI vision glyph — fixed.** The right-pane header renders the glyphs for the focused model:

| model | header |
| --- | --- |
| `Qwen3.6-27B-Q4_K_M` (vision mmproj) | `Qwen3.6-27B-Q4_K_M  ◉` |
| `gemma-4-E2B-it-Q4_K_M` (vision + audio) | `gemma-4-E2B-it-Q4_K_M  ◉ ♪` |
| `Qwen3.5-4B-Q4_K_M` (vision + embedded MTP) | `Qwen3.5-4B-Q4_K_M  ◉  ↯` |
| `Llama-3.2-1B-Instruct-Q4_K_M` (no projector) | no glyph |

The UAT's "glyph count = 0" was a `--render` frame focusing row 0, not the vision model. The glyph is right-pane-header only by design (§25.3 asserts exactly that); the Models **list rows** carry no capability glyph, which is not a gap.

**Action for whoever lands this work:** tick both `TODO.md` items only once the two open work items below are done — the F-01 line bundles `show --json`, so it cannot be struck on the `list --json` evidence alone.

## Work item 1 — `show --json` must reuse the catalog serializer

### The defect

`src/cli/show.rs:168` hand-builds a **second envelope** rather than serialising the row. Verified on the real vision model (`Qwen3.6-27B-Q4_K_M.gguf`):

```
$ llamastash show Qwen3.6-27B-Q4_K_M.gguf --json | jq '{multimodal, mtp, split_siblings, supported_backends}'
{ "multimodal": null, "mtp": null, "split_siblings": null, "supported_backends": null }
```

The human rendering (`render_human`, `src/cli/show.rs:223`) has no mmproj indicator either — its `metadata` block is a hand-listed `kv_block`.

The divergence is wider than the TODO text implies. On the ds4-compatible row:

```
list --json:  supported_backends: ["ds4","llamacpp"]
show --json:  supported_backends: null
```

### The constraint (maintainer instruction, 2026-08-09)

> ensure there is no separate code paths, all the info should be derived from same code paths and just massaged if needed to fit formats.

So this is not "add three keys to the hand-built envelope". Build the envelope **from** the serialised `CatalogRow` and layer the `show`-only sections on top:

```rust
let mut envelope = serde_json::to_value(&row)?;   // name/path/parent/source/backend/
                                                  // supported_backends/split_siblings/
                                                  // metadata{…}/multimodal/mtp/
                                                  // display_label/parse_error/model_id
envelope["size"]          = json!({ … });          // show-only: on-disk totals + per-shard
envelope["arch_defaults"] = json!({ … });          // show-only
envelope["last_params"]   = last_params;           // show-only
envelope["running"]       = running;               // show-only
```

`row` already carries everything — `fetch_catalog` (`src/cli/resolve.rs:101`) deserialises the same `CatalogRow` the daemon serialises, so no extra IPC call is needed. `CatalogRow`'s wire shape is defined once in `to_wire_value` (`src/launch/resolve.rs:102`).

Apply the same rule to `render_human`: derive its metadata rows from the row's fields and add multimodal / MTP lines, rather than growing a parallel hand-list.

### Shape consequences (intended, must be documented)

| change | note |
| --- | --- |
| gains `multimodal`, `mtp`, `split_siblings`, `supported_backends` | the point of the change |
| `metadata` gains `weights_bytes` | it lives in the shared metadata block; `size.weights_bytes` stays as the show-only aggregate |
| `model_id` omitted when unset | the shared serializer drops it rather than emitting `null` (today `show` always emits the key) |
| `backend` now comes from the row, not `backend_for_source(row.source)` | more honest; can be `null` on rows the daemon didn't tag, where today it always resolves to a string |

This is a `show --json` breaking shape change and rides the same 0.1.0 wave as the `list --json` change. It needs a `CHANGELOG.md` entry under `[Unreleased]`.

### Files

- `src/cli/show.rs:168` — `build_view`'s envelope (the rewrite target).
- `src/cli/show.rs:223` — `render_human` (metadata block + new multimodal / MTP rows).
- `src/cli/show.rs:217` — `shard_breakdown` already reads `row.split_siblings`; unchanged.
- `src/launch/resolve.rs:102` — the single wire-shape definition. **Do not add a `show`-specific variant here.**

## Work item 2 — F-11: MODE + DEVICE columns on `llamastash list`

### Current state (verified 2026-08-09, 2-GPU daemon with a model running)

```
TUI:  Name  Arch  Params  Quant  Ctx  Size  Mode  Port    Device
      ● Llama-3.2-1B…    llama      1.2B  Q4_K     128k  770M  chat  :41100  all
        DeepSeek-V4…     deepseek4  284B  IQ2_XXS  1.0M  81G   chat  —       —

CLI:  NAME  ARCH  PARAMS  QUANT  CTX  SIZE  BACKEND  STATUS
```

### Data sources — everything is already on the wire

- **MODE** → `metadata.mode_hint`, on the `CatalogRow` the CLI already holds. Coverage on the real catalog: 14 `chat`, 2 `embedding`, 1 `rerank`.
- **DEVICE** → `status.models[].params.knobs.device`. Verified: `null` on a plain launch, `"ROCm0"` after `start --device ROCm0`. `RunningRow` (`src/cli/resolve.rs:31`) carries `params`, and `running_index` (`src/cli/resolve.rs:160`) already keys it by model path.
- **DEVICE visibility gate** → `status.servers[].devices.len() > 1`, mirroring `App::multi_device` (`src/tui/app.rs:1530`). `fetch_status` already returns `servers` (`StatusSnapshot.servers`), so `src/cli/list.rs:14` needs to pass one more value into `list_human`.

### Cell semantics — copy the TUI exactly

Mirror `column_value` (`src/tui/list_pane.rs:1056`) so the two surfaces never drift:

- `Mode` → `list_cell(mode_hint)`; the CLI table's existing placeholder for unknown metadata is `?` (`src/cli/output.rs:76`), the TUI's is the glyph dash. Pick one and be consistent within the CLI table.
- `Device` → the explicit selector when set; `all` when the row **is running** on the device-selecting default backend with no override; placeholder otherwise. The `all` rule exists so running rows don't show a device for some and blank for others.

### Column order — one open decision

The TUI order is `… Size Mode Backend Port Device` (Device rendered last). Matching it puts DEVICE **after** STATUS:

```
NAME  ARCH  PARAMS  QUANT  CTX  SIZE  MODE  [BACKEND]  STATUS  [DEVICE]
```

Placing DEVICE before STATUS reads better in a padded table, since STATUS is the variable-width cell. **Default to matching the TUI** unless the maintainer says otherwise. MODE goes before BACKEND either way (§16.10 asserts "Backend column renders after Mode").

BACKEND keeps its existing multi-backend gate (`src/cli/output.rs:54`); DEVICE gets the new multi-device gate. Both gates stay off on a plain single-GPU, llama.cpp-only host so the default table does not grow.

### Byte-stability contract

`list_human` has two branches (`src/cli/format.rs:48`): a padded TTY table and a `\t`-separated TSV for pipes. Adding columns changes the TSV row shape — that is the intended breaking change for 0.1.0, but it must be called out in `CHANGELOG.md`, because `awk -F\t` pipelines pin against it. `list --json` is unaffected.

### Files

- `src/cli/output.rs:45` — `list_human` (header + body + the two gates).
- `src/cli/list.rs:14` — pass the servers/device-gate input through.
- `src/tui/list_pane.rs:1056` — the semantics to mirror (read, don't edit).

## Docs to update in the same change

Per `AGENTS.md` §"Docs stay in sync with code":

- `CHANGELOG.md` — one short entry per work item under `[Unreleased]`; both are user-visible breaking shape changes.
- `AGENTS.md` §"CLI agent surface" — the "one catalog-row serializer" paragraph currently describes `list --json` only; extend it to say `show --json` shares the same row shape plus its own `size` / `arch_defaults` / `last_params` / `running` sections.
- `docs/usage.md` — the `list` column list and the `show --json` shape. **Note:** this file is currently modified in the working tree by a parallel session; rebase rather than clobber.
- `docs/testing/2026-07-19-e2e-uat-findings.md` — flip F-01 and F-11 from "parked" to fixed, and correct the §25.3 row (the glyph was never broken; the frame focused the wrong model).
- `docs/testing/2026-07-19-e2e-uat-plan.md` — §25.2 / §25.3 / §16.10 outcomes.
- `TODO.md` — strike both lines once the work lands.

## Test plan

Unit / integration (`make test`, i.e. `cargo test --features test-fixtures`):

- `show --json` carries `multimodal` / `mtp` / `split_siblings` / `supported_backends` for a row that has them, and the keys track the row (absent capability → `null`).
- A `show --json` row and the matching `list --json` row agree on every shared key — the regression test that keeps a second serializer from reappearing.
- `list_human` header contains MODE always, DEVICE only under the multi-device gate; a running row with no override renders `all`, an explicit selector renders verbatim, an idle row renders the placeholder.
- TSV branch stays `\t`-separated with the new column count.

E2E (required by `AGENTS.md`; the recipe below is the one that produced this document's evidence).

## Reproducible verification harness

Real models, isolated daemon, no fixtures. Never point this at the user's default state dir.

```bash
# 1. isolated worktree so a parallel session's half-written tree can't break the build
git worktree add /tmp/verify-wt --detach HEAD
cd /tmp/verify-wt && CARGO_TARGET_DIR=/tmp/vt cargo build --bin llamastash

# 2. isolated daemon; lemonade off because the user's real lemond holds :13305
export LLAMASTASH_STATE_DIR=/tmp/ls-verify/state
export LLAMASTASH_CONFIG_DIR=/tmp/ls-verify/config
export LLAMASTASH_CACHE_DIR=/tmp/ls-verify/cache
export HF_HOME=/mnt/work/huggingface
export BIN=/tmp/vt/debug/llamastash
mkdir -p "$LLAMASTASH_CONFIG_DIR"
printf 'backend:\n  lemonade:\n    enabled: false\n' > "$LLAMASTASH_CONFIG_DIR/config.yaml"
$BIN daemon start --proxy-port 21435        # non-default port, never 11434/11435

# 3. CLI-vs-IPC row identity (the F-01 regression guard)
TOKEN=$(jq -r .ipc_token "$LLAMASTASH_STATE_DIR/runtime.json")
URL=$(jq -r .ipc_url "$LLAMASTASH_STATE_DIR/runtime.json")
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"list_models"}' "$URL/rpc" | jq -S '.result.models' > /tmp/ipc.json
$BIN list --json | jq -S '.models' > /tmp/cli.json
diff /tmp/ipc.json /tmp/cli.json && echo IDENTICAL

# 4. multi-GPU + running-row device (debug builds only)
$BIN daemon stop
LLAMASTASH_DEBUG_FAKE_GPUS=2 $BIN daemon start --proxy-port 21435
$BIN start Llama-3.2-1B-Instruct-Q4_K_M --device ROCm0 --wait
$BIN status --json | jq -c '.models[] | {mode: .params.mode, device: .params.knobs.device}'

# 5. TUI glyphs (needs pyte in a venv; the driver inherits LLAMASTASH_* env)
python3 -m venv /tmp/tuivenv && /tmp/tuivenv/bin/pip install -q pyte pexpect
cd /tmp/verify-wt && /tmp/tuivenv/bin/python scripts/tui/tui_drive.py \
  '[["", 5, "boot"], ["/Qwen3.5-4B|<enter>", 3, "vision-mtp"]]' --bin $BIN --size 160x45

# teardown
$BIN stop --all -y; $BIN daemon stop; git worktree remove /tmp/verify-wt
```

### Real models on this host that exercise each path

The 2026-07-19 UAT's vision model is gone from `HF_HOME`; the projectors live under LM Studio:

| path | signal |
| --- | --- |
| `/mnt/work/lmstudio-models/lmstudio-community/Qwen3.6-27B-GGUF/` | 3 quants + `mmproj-Qwen3.6-27B-BF16.gguf` → vision |
| `/mnt/work/lmstudio-models/lmstudio-community/gemma-4-E2B-it-GGUF/` | vision **+ audio** (omni, `◉ ♪`) |
| `/mnt/work/lmstudio-models/unsloth/Qwen3.5-4B-MTP-GGUF/` | vision + embedded MTP head (`◉ ↯`) |
| `/mnt/work/huggingface/hub/models--antirez--deepseek-v4-gguf/` | `supported_backends: ["ds4","llamacpp"]` |
| `/mnt/work/huggingface/hub/models--bartowski--Llama-3.2-1B-Instruct-GGUF/` | no projector (negative case), small enough to launch |

Two `gemma-4-E2B-it-Q4_K_M.gguf` rows exist (an HF copy without a projector, an LM Studio copy with one). The TUI filter matches name/arch/quant but **not** the full path, so isolate the projector-bearing copy with a `model_paths:` scan root rather than a filter.

## Environment caveats for the next agent

- A parallel session has been working the `Ctrl+D` full-delete TODO in this repo: `src/tui/delete.rs` (untracked) plus edits to `app.rs` / `events.rs` / `confirm_overlay.rs` / `mod.rs` / `docs/usage.md`. Check `git status` before building — the shared tree has been transiently non-compiling. The work items here touch `src/cli/*` only, so file overlap is limited to `docs/usage.md`.
- The user's real `lemond` occupies `:13305`; an isolated daemon must disable the lemonade backend or pass `--force`.
- `make` targets are the sanctioned commands (`make build`, `make test`, `make lint`); never hand the user a bare global `llamastash` for verification work.
