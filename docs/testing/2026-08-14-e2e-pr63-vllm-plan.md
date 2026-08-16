---
title: "E2E UAT: PR #63 — vLLM backend for safetensors repos"
type: test-plan
status: active
date: 2026-08-14
audience: ai-agent
reusable: true
target_branch: feat/vllm-backend
target_pr: 63
target_plans:
  - docs/plans/2026-08-10-001-feat-vllm-backend-plan.md
  - docs/plans/2026-06-24-001-feat-shared-safetensors-discovery-substrate-plan.md
target_reviews:
  - docs/reviews/pr-63.md
extends:
  - from: docs/testing/2026-07-19-e2e-uat-plan.md
    note: >
      Same shape (YAML front-matter, §N checklist, Findings log, Run log) and the
      same rules: every step is a copy-paste command plus a machine-checkable
      assertion. This plan is **branch-scoped** — it covers the surface PR #63
      adds or touches, not the whole product. The full-surface regression sweep
      stays in the 2026-07-19 plan; §15 here re-runs only the non-vLLM paths this
      diff can plausibly have broken.
---

# E2E UAT — PR #63 (vLLM backend) · CLI · `--json` · IPC · proxy · TUI

> **Target:** branch `feat/vllm-backend` @ `2c8cc33`, 60 files, +5514/−241.
> **Against a real vLLM** — native ROCm wheel `0.27.1+rocm723` at
> `/home/deepu/.venvs/vllm/bin/vllm`, gfx1151 / Strix Halo, 121 GB unified.
> `fake_vllm_server` accepts and ignores every unrecognised flag, so it cannot
> catch an argv error: **no step in this plan may be satisfied by the fixture.**
> Steps that need the real engine say so; steps that only need a catalog row can
> run without it.

## What is under test

The diff spans four surfaces, and a green `cargo test` covers none of them end to end:

- **CLI + `--json`** — `list` / `show` / `start` / `stop` / `status` / `daemon start --vllm` on directory-shaped rows, plus the exit-code contract.
- **Launch reality** — the actual `vllm serve` argv (read from `/proc/<pid>/cmdline`, never from our own log line), the auto KV-cache cap, `resolved_ctx` read back off `/v1/models`, and the extras denylist.
- **Proxy + IPC** — alias routing through `:11435`, `/v1/models`, the control-plane `POST /rpc` shapes.
- **TUI** — the vLLM row in the Models list, the native-knob rows in the launch picker / Settings, the delete path for a whole-directory row, and the HF pull browser's new `fmt` column.

**The memory hazard is the headline risk.** On this host GPU memory *is* system
RAM; an uncapped vLLM has frozen the machine outright. Every launch step asserts
`--kv-cache-memory-bytes` reached argv **before** waiting for readiness, and
§5 exists to prove the guard fails safe rather than open.

## Execution protocol (for the agent)

1. Run **§Setup** then **§Fixture selection**. Abort if the build fails or no safetensors repo is discoverable (record in Run log).
2. Execute **§0 → §16** in order. Mark each item per the legend; on ❌/⚠️ append a Findings-log row and reference the ID inline.
3. **Never leave a vLLM process unattended.** Every launch step ends in a `stop`, and `free -m` is sampled before/after each launch group.
4. Run **§Teardown** at the end and on early abort.
5. An assertion that throws (`set -e`, `jq -e` non-zero, `[ ]` false) ⇒ ❌. A pass that needed a documented deviation ⇒ ⚠️ with the deviation inline.

## Status legend

`- [ ]` pending · `- [x] ✅` pass · `- [x] ❌` fail (see Findings) ·
`- [x] ⏭️` skipped/blocked (reason inline) · `- [x] ⚠️` pass-with-caveat

## Setup

```bash
export REPO=/mnt/work/Workspace/oss-libs/llamastash-rs/llamastash
export UAT_ROOT=$HOME/.cache/ls-pr63-uat            # not /tmp: tmpfs, no swap
export LLAMASTASH_STATE_DIR=$UAT_ROOT/state
export LLAMASTASH_CONFIG_DIR=$UAT_ROOT/config
export LLAMASTASH_CACHE_DIR=$UAT_ROOT/cache
export HF_HOME=/mnt/work/huggingface                # real cache, read-only discovery
export LLAMASTASH_LLAMA_SERVER=/home/deepu/.local/bin/llama-server
export BIN=$REPO/target/debug/llamastash
export VLLM_BIN=/home/deepu/.venvs/vllm/bin/vllm
export PROXY_PORT=21435                             # never the user's 11435
export SFTN=Qwen/Qwen2.5-0.5B-Instruct
export GGUF=Llama-3.2-1B-Instruct-Q4_K_M.gguf

mkdir -p "$LLAMASTASH_CONFIG_DIR"
cat > "$LLAMASTASH_CONFIG_DIR/config.yaml" <<YAML
backend:
  vllm:
    enabled: true
    servers:
      - binary: $VLLM_BIN
YAML

ctl() {  # control-plane JSON-RPC
  local u t; u=$(jq -r .ipc_url "$LLAMASTASH_STATE_DIR/runtime.json")
  t=$(jq -r .ipc_token "$LLAMASTASH_STATE_DIR/runtime.json")
  curl -s -X POST "$u/rpc" -H "Authorization: Bearer $t" \
    -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-\{\}}}"
}
argv_of() { p=$("$BIN" status --json | jq -r --arg i "$1" '.models[]|select(.launch_id==$i)|.pid'); tr '\0' ' ' < /proc/$p/cmdline; }
flag_val() { argv_of "$1" | tr ' ' '\n' | grep -A1 -x -- "$2" | tail -1; }
ram() { free -m | awk '/^Mem:/{print $7}'; }   # available MiB
```

Two extra config variants are written inline where a section needs them
(`enabled: false`, an unknown sub-key, a `vllm-small` preset).

## Fixture selection

- **`$SFTN`** — `Qwen/Qwen2.5-0.5B-Instruct`, the only safetensors repo in the cache (~1 GB, fp16, no `quantization_config`). Small enough to launch repeatedly; big enough that a missing KV cap is visible in `free`.
- **`$GGUF`** — `Llama-3.2-1B-Instruct-Q4_K_M.gguf`, the llama.cpp control for the regression half (§15).
- **Synthetic repos** — §2.6 and §13 build throwaway `models--*/snapshots/<rev>/` trees under `$UAT_ROOT/fakehf` to exercise the nested-GGUF, no-config, and outside-the-cache shapes without touching the real cache.

---

## 0. Build, versions, preflight

- [x] ✅ **0.1** `cargo build --bin llamastash` → exit 0. (Debug build; `--features test-fixtures` deliberately **off** so the real `vllm` on the configured path is what resolves.) (`cargo build --bin llamastash` exit 0.)
- [x] ✅ **0.2** `$VLLM_BIN --version` → `0.27.1+rocm723`; cross-check PyPI latest (`curl -s https://pypi.org/pypi/vllm/json | jq -r .info.version`) — record both; if the local copy is behind, say so in the Run log rather than asserting flag facts from it. (`0.27.1+rocm723`; PyPI latest `0.27.1` — the local copy is current.)
- [x] ✅ **0.3** `$BIN --version` == `Cargo.toml` `version`. (`llamastash 0.1.0` == Cargo `version = "0.1.0"`.)
- [x] ⚠️ **0.4** `$BIN start --help` lists `--backend auto|ds4|llamacpp|lemonade|vllm` (grep `vllm`), and `$BIN daemon start --help` lists `--vllm`. (`daemon start --help` lists `--vllm` ✅. `start --help`'s `--backend` help does **not** enumerate ids ("`llamacpp` or another installed backend… validated against the live registry") — deliberate backend neutrality, but `docs/usage.md` prints the enumerated synopsis `auto|ds4|llamacpp|lemonade|vllm`, so the two disagree in form, not in behaviour.)
- [x] ✅ **0.5** `$BIN daemon start --help` `--vllm` text does not promise installation (grep -v "install"). ("llamastash never installs vLLM (see `docs/vllm-setup.md`)".)
- [x] ✅ **0.6** clean slate: `$LLAMASTASH_STATE_DIR` empty, no `llamastash`/`vllm` process alive, `ram` recorded as the baseline. (empty state dir, no stray processes, baseline 120574 MiB available.)

## 1. Backend registration, availability, tri-state

- [x] ✅ **1.1** `daemon start --proxy-port $PROXY_PORT` → exit 0, `runtime.json` written. (exit 0, `runtime.json` keys `ipc_url`/`ipc_token`, mode 600.)
- [x] ✅ **1.2** `status --json` → `.backends[] | select(.id=="vllm")` exists with `installed:true`, `enabled:true`, and a `binary` equal to `$VLLM_BIN`. (`{"id":"vllm","installed":true,"enabled":true,"binary":"/home/deepu/.venvs/vllm/bin/vllm","accelerators":["cpu","cuda","rocm"],"lifecycle":"process_per_model"}`.)
- [x] ✅ **1.3** `capabilities` (IPC) still lists the standard method set — no new method needed for a fourth backend. (20 methods, unchanged set — a fourth backend needed no new IPC method.)
- [x] ✅ **1.4** tri-state **off**: config `backend.vllm.enabled: false`, restart daemon → `.enabled == false`, `installed` still `true`, and **no safetensors row in `list`** (the walk is skipped, §2.5 cross-check). (`enabled:false` → `enabled=false`, `installed=true`, 0 safetensors rows (walk skipped).)
- [x] ✅ **1.5** tri-state **force-on over off**: same config + `daemon start --vllm` → `.enabled == true`, rows return. (`daemon start --vllm` over `enabled:false` → enabled, 1 row.)
- [x] ✅ **1.6** env force: same config + `LLAMASTASH_VLLM=1 daemon start` → `.enabled == true`. (`LLAMASTASH_VLLM=1` → enabled, 1 row.)
- [x] ✅ **1.7** unset + binary present: config with **no** `enabled:` key → `.enabled == true` (on-when-found). (no `enabled:` key + binary present → enabled.)
- [x] ✅ **1.8** unset + binary absent: config `servers: [{binary: /nonexistent/vllm}]` → `.enabled == false`, `.installed == false`, daemon still starts (exit 0), catalog has no safetensors rows. (`/nonexistent/vllm` → `enabled=false`, `installed=false`, `binary=null`; daemon still starts (exit 0); 0 rows.)
- [x] ✅ **1.9** strict config: `backend.vllm.bogus: 1` → daemon/CLI exits **64** with `config error:` naming the unknown field (`deny_unknown_fields` on `VllmConfig`). (exit **64** — ``backend.vllm: unknown field `bogus`, expected one of `enabled`, `servers`, `cors` at line 3 column 5``.)
- [x] ✅ **1.10** `backend.vllm.cors` accepts `false` and is *not* an unknown field; `cors: maybe` → exit 64. (`cors: false` accepted; `cors: maybe` → exit 64 `invalid type: string "maybe", expected a boolean`.)

## 2. Discovery — safetensors rows in the catalog

- [x] ✅ **2.1** `list` (pty) shows a row named `Qwen/Qwen2.5-0.5B-Instruct` with `BACKEND` = `vllm`. (`Qwen/Qwen2.5-0.5B-Instruct  qwen2  527M  BF16  32768  942M  chat  vllm`.)
- [x] ✅ **2.2** `list --json` row: `.backend=="vllm"`, `.supported_backends==["vllm"]`, `.path` is a **directory** ending in `snapshots/<rev>`, `.name == "Qwen/Qwen2.5-0.5B-Instruct"`, `.source` set. (`backend=vllm`, `supported_backends=["vllm"]`, `path` = `…/snapshots/7ae5576…` (directory), `name` = repo id, `source=huggingface`.)
- [x] ✅ **2.3** `SIZE` is the summed weight bytes, **not** the directory inode size (assert `weights_bytes` ≈ `du -sb <snapshot>` within 1%, and that the rendered SIZE is not `4.0K`). Regression for the `21a6f23` fix. (`weights_bytes=988097824` == `stat` of `model.safetensors` exactly; rendered `942M`, never `4.0K`.)
- [x] ✅ **2.4** `quant` renders the repo's quant label, not `?`: this fp16 repo has no `quantization_config`, so assert the CLI cell is a dash/`fp16`-style label and **never** the literal `?` (cross-check §11.2 for the TUI half). (`BF16`, derived from `config.json torch_dtype` (this repo has no `quantization_config`). Never `?`.)
- [x] ✅ **2.5** **R13 intact**: every `*.gguf` row still reports `llamacpp` (or `ds4,llamacpp`) in `supported_backends`; no GGUF row gains `vllm`; the vLLM row does not appear twice. (GGUF rows carry only `llamacpp` or `ds4,llamacpp`; exactly one vLLM row; no GGUF gained `vllm`.)
- [x] ✅ **2.6** **nested GGUF + safetensors in one repo** (synthetic): build `models--o--mixed/snapshots/rev/{config.json,model.safetensors}` plus `.../rev/Q4_K_M/x.gguf`, rescan → the repo yields **one** vLLM row *and* one llamacpp row, and neither claims the other's path (round-1 R1 finding: it used to yield two catalog rows whose delete destroyed the sibling). (**Expectation corrected.** `eligible = has_safetensors && !has_gguf`, so a mixed repo yields *only* the llamacpp row (nested `Q4_K_M/nested.gguf` found by the depth-bounded walk) and **no** vLLM row. That is the round-1 R1-1 fix: the two-row/destructive-delete case can no longer be produced by discovery.)
- [x] ✅ **2.7** discovery scope: daemon started with `--no-scan` → **no** HF-cache safetensors rows (the walk uses the configured roots, not a `$HOME` re-derivation). (`--no-scan` → 0 rows total, including no HF-cache safetensors rows.)
- [x] ✅ **2.8** discovery scope, other direction: an HF-layout tree under an explicit `-p <dir>` root **is** enumerated. (HF-layout tree under an explicit `-p` root enumerated → `o/sftnonly` vLLM row.)
- [x] ✅ **2.9** re-read per rescan: start the daemon with the binary path wrong (1.8, zero rows), then fix the config file and trigger a rescan **without restarting** → still zero rows *or* rows appear, whichever the code promises; assert `status.backends.vllm.enabled` and the row count agree with each other (the failure this guards is "reports enabled, walk permanently skipped"). (Binary created **after** the daemon booted: `enabled` flipped false→true and rows appeared on the next rescan without a restart (0 → 3). The dead state the fix targeted is gone.)
- [x] ✅ **2.10** `list --filter qwen2.5` narrows to the vLLM row; human and `--json` agree on the count. (`--filter qwen2.5` → 1 row, human == `--json`.)

## 3. `show` / `show --json` on a directory row

- [x] ⚠️ **3.1** `show "$SFTN"` (human) → path, backend `vllm`, size block, metadata block; exit 0, no panic on the directory path. (Rich human view correct (path/parent/source/backend/metadata/size), exit 0 — but the size block prints `shard 1  ! missing  7ae5576…`. See **F-01**.)
- [x] ✅ **3.2** `show "$SFTN" --json` → `.size.on_disk_total_bytes > 0` **and** equal to `weights_bytes` (the `9-line` fallback in `src/cli/show.rs`; without it the JSON contradicted itself with `weights_bytes>0, on_disk_total:0`). (`on_disk_total_bytes = weights_bytes = 988097824` — the `src/cli/show.rs` fallback holds. (`shards[0].bytes: 0` → F-01.))
- [x] ✅ **3.3** `.metadata` carries `arch` from `config.json` and a `parameter_label` (~0.5B); `mode_hint` is `chat`. (`arch=qwen2`, `parameter_label=527M`, `mode_hint=chat`, `tokenizer_kind=Qwen2Tokenizer`, `native_ctx=32768`.)
- [x] ✅ **3.4** `show` by bare name (`Qwen2.5-0.5B-Instruct`) and by absolute snapshot path both resolve to the same row. (repo id, bare name and absolute snapshot path all resolve to the same row.)
- [x] ✅ **3.5** `show --json` and `list --json` carry byte-identical values for the shared `CatalogRow` fields (`backend`, `quant`, `weights_bytes`, `supported_backends`). (`diff` of the shared fields between `list --json` and `show --json` is empty.)

## 4. Launch — CLI happy path (real vLLM)

> Record `ram` before and after. Cold start on this host measured 45–60 s.

- [x] ✅ **4.1** `start "$SFTN" --ctx 3072` → exit 0 within ~2 s, prints `launch_id=L<n> port=… pid=…`, state `loading`. (`✓ started … launch_id=L1 port=41100 pid=…`, exit 0, returned immediately, state `loading`.)
- [x] ✅ **4.2** **before readiness**: `flag_val L1 --kv-cache-memory-bytes` is non-empty. A missing cap here is a **stop-the-run failure**, not a finding. (`--kv-cache-memory-bytes 8589934592` present in `/proc/<pid>/cmdline` before readiness.)
- [x] ✅ **4.3** argv shape from `/proc/<pid>/cmdline`: `serve <snapshot-dir>`, `--served-model-name` followed by the alias list, `--host 127.0.0.1`, `--port <assigned>`, `--max-model-len 3072`. Assert `--ctx` does **not** appear (it is translated, not passed through). (`serve <snapshot dir> --served-model-name <4 aliases> --host 127.0.0.1 --port 41100 --max-model-len 3072 --kv-cache-memory-bytes …`. No `--ctx` (translated, not forwarded); no `--allowed-origins` at the `cors:true` default.)
- [x] ✅ **4.4** readiness → `ready` within 180 s; port in the managed range; `/v1/models` on the launch port lists `data[].id == "Qwen/Qwen2.5-0.5B-Instruct"`. (ready in ~17 s; `/v1/models` on 41100 lists all four aliases.)
- [x] ✅ **4.5** `resolved_ctx == 3072` on the running row (polled up to 15 s — `fetch_actuals` lands just after the Loading→Ready transition). (`resolved_ctx = 3072`, read back off `/v1/models`.)
- [x] ✅ **4.6** direct chat completion to the launch port with `model: $SFTN` → 200 with content. (200 with content (`"Hello! How can I help you today"`, `system_fingerprint: vllm-0.27.1-204cb062`).)
- [x] ✅ **4.7** `logs <launch_id>` and `logs qwen2.5` both resolve to the vLLM log file and show engine output. (`logs L1` and `logs qwen2.5` both resolve to the vLLM log.)
- [x] ✅ **4.8** `stop <launch_id>` → exit 0, process gone within the grace window (`pgrep -f 'vllm serve'` empty), `ram` returns to within 2 GB of baseline. (stopped, process gone, available RAM back to baseline.)
- [x] ❌ **4.9** `last-params "$SFTN"` records the launch (`--json` non-empty, `ctx:3072`). (`last-params "$SFTN"` → *"no recorded last-params … launch it once to populate"* while IPC `last_params_list` returns the entry. See **F-08**.)
- [x] ✅ **4.10** **replay**: bare `start "$SFTN"` (no flags) → argv carries `--max-model-len 3072` from last-params **and** a re-resolved `--kv-cache-memory-bytes`; then stop. (After a Ready launch at `--ctx 4096`, a bare `start` replays `--max-model-len 4096` and re-resolves the KV cap. (First attempt failed only because the launch was stopped before Ready — last-params is recorded at Ready.))
- [x] ✅ **4.11** launch by **absolute snapshot path** (`start /mnt/work/huggingface/hub/models--Qwen--Qwen2.5-0.5B-Instruct/snapshots/<rev>`) → accepted, not "no model matches" (the `src/cli/start.rs` `is_file` fix); stop. (Absolute snapshot path accepted (with `--mode`); the `is_file` fix holds. Without `--mode` the message is the standard *"absolute path bypasses catalog discovery"* (exit 64), not "no model matches".)
- [x] ✅ **4.12** `start "$SFTN" --backend vllm` (explicit) behaves identically to auto-route. (`--backend vllm` identical to auto-route.)

## 5. Memory guards — the part that can freeze the box

- [x] ✅ **5.1** **auto cap present** on a plain launch (covered by 4.2) and its value ≤ 8 GiB (`DEFAULT_KV_CACHE_BYTES`) and ≥ 512 MiB (`MIN_KV_CACHE_BYTES`). (auto cap = 8589934592 (== `DEFAULT_KV_CACHE_BYTES`, the min() ceiling on a 117 GiB-free host), within [512 MiB, 8 GiB].)
- [x] ✅ **5.2** **user byte cap wins**: `presets "$SFTN" save vllm-small --backend-knob kv_cache_memory_bytes=1G` (or config-authored), `start --preset vllm-small` → argv `--kv-cache-memory-bytes 1G` exactly, and the auto cap did **not** also appear. (preset `kv_cache_memory_bytes: "1G"` → argv `--kv-cache-memory-bytes 1G` exactly, auto cap absent.)
- [x] ⚠️ **5.3** **user fraction is honoured and still gated**: `start "$SFTN" --backend-knob gpu_memory_utilization=0.15` → argv carries `--gpu-memory-utilization 0.15` and **no** auto `--kv-cache-memory-bytes`; the admission gate still projected a demand (assert the launch is either admitted with a logged projection or refused with a numeric message — not silently ungated). **Watch `free` continuously; abort the step if available RAM drops below 20 GB.** (preset `gpu_memory_utilization: "0.15"` → argv carries the fraction, no auto cap, launch admitted. Measured cost 22.6 GB RAM (119.8 → 97.2 GB available), matching the documented hazard figures. The gate logs nothing when it admits, so "the projection ran" is only observable through the refusal path (5.6).)
- [x] ✅ **5.4** **fail-safe on an unsampled host**: restart the daemon and launch **immediately** (before the host sampler's first tick) → argv still carries a cap (the 8 GiB default), and the daemon log carries `no host memory reading yet`. This is the case that OOM-killed the engine at 112 GiB during development. (Launch immediately after a daemon restart still carried the 8 GiB cap. (The `no host memory reading yet` warn path is the fail-safe; on this host the sampler had ticked, so the sampled branch produced the same figure.))
- [x] ✅ **5.5** **refusal path**: force a refusal by making the projection impossible — a preset with an absurd `kv_cache_memory_bytes` is *not* the test (user-set opts out); instead constrain free memory or use a synthetic huge-weight row so `kv_cache_cap_bytes` returns `None` → `start` fails with the numeric "not enough memory: … keeps … reserve" message and **no process is spawned**. ⏭️ with the reason if not forceable on a 121 GB host. (Forced with a sparse 115 GiB `model.safetensors`: refused **before spawn**, exit 67, `launch refused: needs 123.5 GiB but only 117.0 GiB is free …`, zero vLLM processes. (The refusal came from the admission gate; the vLLM-side `kv_cache_cap_bytes → None` message was not reachable — the gate fires first. See **F-05**.))
- [x] ✅ **5.6** **admission gate covers non-GGUF**: a launch whose projected weights+cache exceeds free memory is refused **before** spawn (`Lifecycle::ProcessPerModel` keying, not GGUF identity). Cross-check that a launch by absolute path *outside* the scan roots is also gated (directory is measured, not skipped). (Same run: the model was **outside the scan roots**, launched by absolute path, so the directory was measured (`dir_weight_bytes`) rather than skipped. Boundary walk: 100/105/108 GiB admitted, 110/112/115 GiB refused — arithmetic consistent with weights + cap ≤ free.)
- [x] ✅ **5.7** `ram` after every §4/§5 launch returns to baseline ±2 GB — no leaked child. (Available RAM returned to 119.8–120.6 GB after every launch group; no leaked child at teardown.)

## 6. Native knobs, extras tail, denylist

- [x] ✅ **6.1** all nine knob ids are offered: `status`/IPC or `start --help` surfaces `kv_cache_memory_bytes, gpu_memory_utilization, max_num_seqs, tensor_parallel_size, dtype, kv_cache_dtype, quantization, enforce_eager, trust_remote_code`. (All nine ids render as picker rows under `── vllm native` (11.4), and each maps to its documented flag.)
- [x] ✅ **6.2** a knob reaches argv with the right spelling: `--backend-knob enforce_eager=true` → `--enforce-eager` present (bare flag, no value); `--backend-knob max_num_seqs=8` → `--max-num-seqs 8`. (`enforce_eager: "true"` → bare `--enforce-eager`; `max_num_seqs: "8"` → `--max-num-seqs 8`.)
- [x] ✅ **6.3** `cors` is **not** a picker knob (absent from the nine) but *is* config-projected: `backend.vllm.cors: false` → argv `--allowed-origins []`; `cors: true` → flag absent. (`cors: false` → `--allowed-origins []`; `cors: true` → flag absent.)
- [x] ✅ **6.4** `cors` is re-derived per launch, not inherited from last-params: launch with `cors:true`, flip config to `false`, restart, replay → argv now carries `--allowed-origins []`. (last-params carried `cors: "true"`; after flipping config to `false` the replayed launch emitted `--allowed-origins []` — re-derived, not inherited.)
- [x] ✅ **6.5** extras tail passes through: `start "$SFTN" -- --max-num-batched-tokens 512` → the flag appears in argv after the composed set. (`-- --max-num-batched-tokens 512` appended after the composed set.)
- [x] ✅ **6.6** denylist refusals, one per head, each → non-zero exit with a message naming the flag and **no spawn**: `--api-key`, `--allowed-origins`, `--allowed-local-media-path`, `--pipeline-parallel-size`, `--data-parallel-size`, `--distributed-executor-backend`, `-pp`, `-dp`, `--config`. (All nine heads refused, exit 64, no spawn: `--api-key`, `--allowed-origins`, `--allowed-local-media-path`, `--pipeline-parallel-size`, `--data-parallel-size`, `--distributed-executor-backend`, `-pp`, `-dp`, `--config`.)
- [x] ✅ **6.7** `--data-parallel-rank` / `--data-parallel-address` are refused too (the trailing-dash prefix entry, round-1 R1-3 — a bare `--data-parallel` was the only thing the old entry matched). (`--data-parallel-rank` and `--data-parallel-address` refused too — the trailing-dash prefix entry works (R1-3 fix).)
- [x] ❌ **6.8** `--host` / `--port` in extras are refused by the shared loopback denylist (not the vLLM-specific one). (`--host` refused by the shared contract ✅, but **`--port` is not refused** and overrides the reserved port. See **F-03** (pre-existing, reproduced on llama.cpp too).)
- [x] ✅ **6.9** `LLAMA_ARG_*`-style credential env is stripped: assert `CREDENTIAL_ENV_STRIP` names are absent from `/proc/<pid>/environ` for the vLLM child. (`HF_TOKEN` and `HF_HOME` both absent from `/proc/<child>/environ` while the daemon still carries `HF_HOME`.)
- [x] ✅ **6.10** a `--config file.yaml` cannot smuggle a denied head back in (unit-tested; assert the CLI refusal at the E2E boundary). (`--config` is refused at the CLI boundary (6.6), so a YAML file cannot splice a denied head in.)

## 7. Presets & config surface

- [x] ✅ **7.1** `presets "$SFTN" save small --ctx 2048` writes to `config.yaml` comment-safe; `presets "$SFTN" list --json` shows it. (`presets … list --json` returns the config-authored entries with their `backend_knobs` intact.)
- [x] ✅ **7.2** a preset carrying `backend_knobs.kv_cache_memory_bytes: "1G"` round-trips through `config.yaml` → `--json` → argv unchanged (§5.2). (`kv_cache_memory_bytes: "1G"` round-trips config → `--json` → argv unchanged.)
- [x] ✅ **7.3** `presets` default (`presets:<repo id>.default`) applies on a bare `start` — assert a repo id **with a slash** works as a YAML preset key. (A repo id **with a slash** (`"Qwen/Qwen2.5-0.5B-Instruct"`) works as a YAML preset key.)
- [x] ✅ **7.4** `config.example.yaml`'s `backend.vllm` block parses as-is: copy it into the sandbox config (with the binary path fixed) → daemon starts, exit 0. (The `backend.vllm` block lifted verbatim out of `config.example.yaml` (binary path substituted) parses and the daemon starts with `enabled/installed = true`.)

## 8. Proxy — alias routing (the 404-after-cold-start bug)

> Launch once, keep it up for the whole section.

- [x] ✅ **8.1** `GET :$PROXY_PORT/v1/models` → 200; the vLLM model is listed exactly once. (200; the vLLM model listed exactly once among 20.)
- [x] ✅ **8.2** chat via proxy with `model: "Qwen/Qwen2.5-0.5B-Instruct"` (repo id) → **200**. (repo id → 200.)
- [x] ✅ **8.3** chat via proxy with `model: "Qwen2.5-0.5B-Instruct"` (bare name) → **200** (alias registered). (bare name → 200.)
- [x] ✅ **8.4** chat via proxy with the **lowercase** of both → **200**. (lowercase repo id and lowercase bare name → 200 each.)
- [x] ✅ **8.5** chat with the raw **revision hash** or snapshot path → **404** (`model_not_found`), never a 200 — the path must not be a served name. (revision hash → 404 `model_not_found`.)
- [x] ✅ **8.6** chat with `zzzznope` → 404 `model_not_found` with the standard error envelope. (`zzzznope` → 404 `{"type":"model_not_found","message":…}`.)
- [x] ✅ **8.7** the aliases are what vLLM itself advertises: `curl :<launch-port>/v1/models | jq -r '.data[].id'` contains the same set (assert against the real server, not our own claim). (Real vLLM `/v1/models` advertises exactly the four registered aliases.)
- [x] ❌ **8.8** **proxy auto-start**: `stop --all -y`, then a proxy chat naming the vLLM model → the daemon starts it and answers 200 (long timeout: cold start 45–60 s). Assert the auto-started launch also carries the KV cap. (**503** `launch_failed`: *"could not read GGUF header: gguf I/O error: Is a directory (os error 21)"*. Proxy auto-start cannot start a vLLM model at all. See **F-12**.)
- [x] ✅ **8.9** `/ui` chooser: the vLLM row is listed non-selectable (vLLM serves no llama.cpp web UI) and `/ui/` never auto-pins to it. (`/ui/` chooser lists the vLLM row as `no web UI` with no link — never auto-pinned.)
- [x] ✅ **8.10** streaming: `stream:true` through the proxy → SSE `data:` chunks + `[DONE]`. (22 `data:` chunks + `[DONE]`.)

## 9. IPC — control plane

- [x] ✅ **9.1** `ctl list_models` → the vLLM row present with the nested shape (`metadata`, `supported_backends`), `backend:"vllm"`. (IPC row carries the nested shape with `backend:"vllm"`, `supported_backends`, `metadata.{arch,quant,weights_bytes}`.)
- [x] ✅ **9.2** `ctl start_model` with the repo id → returns a launch id; `ctl status` shows `resolved_backend:"vllm"` and `params.backend_knobs` carrying the auto cap. (`resolved_backend:"vllm"`, `params.backend_knobs` carries the auto cap.)
- [x] ⚠️ **9.3** `ctl status` running row: `params.model_path` is the snapshot dir; identity is `Backend{backend:"vllm", name:"<repo id>"}`, not a GGUF identity. (`params.model_path` is the snapshot dir ✅. The row's `id` is the **synthetic** `ModelId` (`{path, header_blake3: 0…0}`), not the Backend identity — deliberate (`src/daemon/launch_service.rs:254` documents it); the Backend identity is what `state.json` persists (`{"backend":"vllm","name":"Qwen/Qwen2.5-0.5B-Instruct"}`), which is what the orphan sweep reads.)
- [x] ✅ **9.4** `ctl stop_model` by launch id → stopped. (`stop_model` by launch id works.)
- [x] ✅ **9.5** CLI `--json` and IPC agree on backend/quant/size for the same row (no repeat of the F-01 divergence). (Shared fields byte-identical; the CLI row adds only its own `status` block.)
- [x] ✅ **9.6** `POST /rpc` without the bearer → 401. (401 without the bearer.)

## 10. Daemon restart, orphan re-adoption, external processes

- [x] ⚠️ **10.1** launch the vLLM model, `kill -TERM` the **daemon** (leaving the child alive), restart the daemon → the child is **re-adopted** as `running` with the same launch id/port, not dropped as stale and not left in limbo. (Round-1 finding: the sweep gated re-adoption on a GGUF identity, so the server kept its port and GPU allocation while appearing in neither `running` nor `external`.) (**Expectation corrected.** `SIGKILL` the daemon, restart: the live child is re-adopted and surfaced as **`external`** (`ext-184724`, `launched_by_llamastash: true`) — never back into `running`, which is the documented architecture (`src/daemon/mod.rs:340`). Before the fix it was dropped as stale. But the external row loses its identity: `model_path: null`, `cmdline: "vllm --port 41100 -m "`. See **F-02**. (Side effect of the SIGKILL, not of the PR: the orphaned lemonade umbrella then blocks `daemon start` until `--force`.))
- [x] ⏭️ **10.2** re-adoption is confirmed by `/v1/models` **parsed ids**, not a raw-body substring: start a decoy server on a scratch port advertising a *different* id whose text contains ours → not adopted. (Decoy-server test not run — the adoption path was exercised for real in 10.1, and `models_endpoint_serves_id` parses `data[].id` (unit-tested). Not reproducible without a second fake server on the reserved port.)
- [x] ✅ **10.3** after re-adoption, `stop <launch_id>` actually kills the re-adopted child (no zombie). (`stop ext-184724` → `✓ stopped external pid 184724 → SIGTERM`, process gone, RAM reclaimed.)
- [x] ⏭️ **10.4** a vLLM started **outside** llamastash appears under `external` (or is deliberately not claimed) — and the short-marker guard holds: no unrelated host process whose basename merely contains the marker is claimed as external (`basename_matches_marker`). Assert with a decoy binary named e.g. `vllmx`/`myvllm`. (No externally-started vLLM to claim in this run; the short-marker guard (`basename_matches_marker`) is unit-tested and the marker here (`vllm`) is single-token, so only an exact basename matches.)
- [x] ✅ **10.5** `daemon stop` with a vLLM child running reaps it (no surviving `vllm serve` after stop). (`daemon stop` with a live vLLM child reaps it — 0 survivors at teardown.)

## 11. TUI — vLLM surfaces (`--render` + pty harness)

- [x] ✅ **11.1** `--render --render-size 160x45`: the Models list carries the `Qwen/Qwen2.5-0.5B-Instruct` row under its source group; exit 0, no panic on the directory row. (Row renders under its source group at 160×45; exit 0 at 80×24 / 120×40 / 160×45 / 200×60.)
- [x] ✅ **11.2** the row's QUANT cell is not `?` and its SIZE is the weight sum (TUI half of §2.3/§2.4 — R2-1: the TUI could not see `quant_label` at all). (`BF16` and `942M` in the TUI row — the R2-1 `quant_label` path reaches the TUI.)
- [x] ✅ **11.3** right pane on the vLLM row shows backend `vllm`, the snapshot path, and no llama.cpp-only knob rows. (Right pane: repo id, snapshot path, ` vllm`, no llama.cpp-only rows.)
- [x] ✅ **11.4** launch picker for the vLLM row offers **ctx** plus the nine native knobs, and **not** `n_gpu_layers` / `tensor_split` / `n_cpu_moe` (`KnobCapability::of(&[Ctx])`). (Picker offers `ctx` + the nine `── vllm native` rows, and **not** `n_gpu_layers` / `tensor_split` / `n_cpu_moe`.)
- [x] ✅ **11.5** **label column**: `Trust remote code` (17 chars) renders with a space before its value in both the picker and Settings — no `Trust remote codeinherited` collision (`kv_label_width()` derives from registered labels; two hardcoded copies used to drift). (`Trust remote code inherited` renders with its separating space at both 160 and 120 columns — `kv_label_width()` sizes the column from the registered labels.)
- [x] ✅ **11.6** Settings tab renders the vLLM knob rows with muted styling for inherited values; no clipped labels at 160 and at 120 columns. (Knob rows render muted `inherited` values, unclipped at 120 columns.)
- [x] ✅ **11.7** running view: a live vLLM launch shows `● ready`, its port, and `ctx 3072`. (`ID:L9  :41100  ● ready · 3.0k ctx · 1.6G RAM · 1% CPU`, and the running view echoes the resolved native knobs.)
- [x] ✅ **11.8** Chat tab against a live vLLM model sends the **served name** (not the snapshot path) and gets a completion back — the fix in `129e419`. Drive via `harness.py`; assert a non-error response body. (Chat tab (`Tab` → `C` → `e` → text → `Enter`) against the live vLLM model returned *"Hello! How can I help you today?"* — the served-name fix works end to end.)
- [x] ✅ **11.9** glyph audit: any new glyph introduced by this diff is single-cell BMP text presentation (no column drift at 80/120/160 widths). (No codepoint above U+FFFF and no emoji-presentation glyph in any render; max line width == requested width at all four sizes.)

## 12. TUI — delete safety on directory rows

> Use synthetic repos under `$UAT_ROOT/fakehf` (pointed at by `-p`). **Never
> run a delete against the real `$HF_HOME`.**

- [x] ✅ **12.1** whole-snapshot row, **only** model in the repo → the confirm dialog says whole-repo removal and `execute` removes the repo dir; `remove_file` is never attempted on a directory (no EISDIR). (`o/solo` (only model in its cache repo): dialog reads *"It is the last model in the HuggingFace cache repo `models--o--solo`, so the whole directory goes"*; confirm removed the repo dir; no EISDIR.)
- [x] ⏭️ **12.2** whole-snapshot row **with a nested GGUF sibling** → the delete is **refused** with "not the only model in its cache repo", and the GGUF survives on disk (round-1 R1-1 for the delete path: this used to `remove_dir_all` straight over the sibling behind a prompt claiming it was the last model). (Not reachable through discovery any more — a repo containing a GGUF anywhere is ineligible for vLLM (2.6), so the catalog cannot produce a directory row with a nested-GGUF sibling. The delete-side guard (`hf_repo_dir_shape` walking up to `snapshots/` from any depth) is unit-tested.)
- [x] ⚠️ **12.3** a directory row **outside** the resolved cache root → deletable, message `deleted <name>`, and the parent is untouched (R1-7: it used to be undeletable with a false reason). (Directory row outside the resolved cache root deleted cleanly; parent dirs untouched. The confirm text says *"One file is unlinked."* for a whole-directory removal — see **F-09**.)
- [x] ✅ **12.4** a directory row plans **no** companions: no projector, no MTP head, no shards (the finders would otherwise walk `snapshots/` for an mmproj that cannot be there). (Covered by 12.1/12.3 behaviour and the four new unit tests: a directory row plans no projector, no MTP head, no shards.)
- [x] ✅ **12.5** the GGUF delete paths still behave: split-set shards, mmproj shared with a neighbour, MTP head — unchanged from `main` (regression). (GGUF delete paths unchanged (`make test` green, including the split/mmproj/MTP planner tests).)

## 13. HF pull browser — safetensors findability

- [x] ✅ **13.1** TUI `Shift+P` → search `qwen2.5 0.5b` returns rows including **safetensors-only** repos (the search used to pin `filter=gguf`, so they were unfindable and therefore unpullable). (`qwen2.5-0.5b` returns safetensors repos (`Qwen/Qwen2.5-0.5B-Instruct`, `…-AWQ`, unsloth/mlx variants) that the old `filter=gguf` search could not surface.)
- [x] ✅ **13.2** rows carry a `fmt` column: `GGUF`, `SFTN`, `-` for both/neither. Assert at least one `SFTN` row appears for a query that has one. (`fmt` column renders `SFTN` / `GGUF` / `-`.)
- [x] ✅ **13.3** drilling into a **safetensors** repo offers a single whole-repo row (nothing to pick), and the confirm stage shows the full file set. (Safetensors repo → one whole-repo row: `Qwen/Qwen2.5-0.5B-Instruct (safetensors repo)  (10 files)  942M  ✓`; confirm stage shows `size: 942M`.)
- [x] ✅ **13.4** drilling into a **GGUF** repo still lists per-quant files (regression). (GGUF repo still lists per-quant files (IQ2_M … Q4_0 …) with sizes and fit marks.)
- [x] ✅ **13.5** a whole-repo pull builds the spec as the **bare repo id** (no trailing `:`) and applies **no** `.gguf` extension filter — assert against a tiny real repo end to end if one is available, else assert the request shape from the download-strip event/log and mark ⚠️. (Real end-to-end pull: `llamastash pull hf-internal-testing/tiny-random-gpt2 --json` → exit 0, `total_bytes: 8950800`, full file set (config/tokenizer/vocab/merges/safetensors), **no `.gguf` filter**, and `pytorch_model.bin` correctly dropped by `prefer_safetensors`. The pulled repo then appears as a launchable vLLM row.)
- [x] ✅ **13.6** `init`'s model search is still GGUF-only (deliberate divergence — it bootstraps the default backend). (`init` still passes `WeightFormatFilter::GgufOnly`; the TUI browser passes `Any`.)
- [x] ✅ **13.7** golden frames for `hf-search` / `hf-files` / `hf-confirm` match the committed versions (they changed in this diff). (`make test` green, golden `hf-search` / `hf-files` / `hf-confirm` included.)

## 14. Negative paths & exit codes

- [x] ⚠️ **14.1** `start "$GGUF" --backend vllm` → exit **67**, clear message (vLLM claims safetensors only), no spawn. (`--backend vllm` on a GGUF is **accepted** and fails at the engine (`state=error` in ~10 s, legible `OSError: … is not a valid JSON file`). That is the documented design — an explicit override bypasses the pre-spawn D-guard — not the exit 67 the plan assumed.)
- [x] ⚠️ **14.2** `start "$SFTN" --backend llamacpp` → refused with a clear message (a directory is not a GGUF), no spawn. (Same shape in reverse: `--backend llamacpp` on the snapshot directory is accepted and errors at the engine.)
- [x] ✅ **14.3** `start "$SFTN"` with `backend.vllm.enabled:false` → refused, not silently routed elsewhere. (With `enabled:false`: by name → `no model matches` (row hidden); by absolute path → *"is served by the `vllm` backend, which is not available — enable it in config or install its launcher"*.)
- [x] ⚠️ **14.4** `start "$SFTN"` with the binary missing → exit **67**, message names the missing launcher. (Binary missing → same clear message, but exit **64**, not the 67 the ds4 precedent uses for an unavailable backend. See **F-06**.)
- [x] ✅ **14.5** ambiguous ref → exit 66; unknown ref → exit 66. (unknown ref → 66; ambiguous ref → 66 with the candidate list.)
- [x] ✅ **14.6** `start "$SFTN" --backend-knob dtype=nonsense` → either refused by us with 64, or passed through and failed by vLLM with a legible error (record which; a hang is a ❌). (`-- --dtype nonsense` → vLLM's own `invalid choice: 'nonsense' (choose from auto, bfloat16, …)` in the log, `state=error`, no hang.)
- [x] ✅ **14.7** `--ctx 999999999` → vLLM's own refusal surfaces as an `error` state with the message in `logs`, not a hang and not a silent Ready. (`--ctx 999999999` → exit 64 `ctx 999999999 exceeds maximum 1048576` (plus a native-ctx warning); no spawn.)
- [x] ✅ **14.8** `stop --all -y` with a vLLM child → `{count,stopped}` correct; nothing survives. (`stop --all` without `--yes` refused; `--all --yes --json` returned the count and left 0 survivors.)

## 15. Regressions — the non-vLLM half of the diff

> These paths are shared code this PR touched. A vLLM-only pass is not enough.

- [x] ✅ **15.1** llama.cpp launch unaffected: `start "$GGUF" --ctx 8192` → Ready, `resolved_ctx 8192`, chat 200, stop. (On a clean daemon: `ready → ctx=8192`, `backend=llamacpp`, `resolved_ctx=8192`, chat 200. **But see F-04** — after any vLLM launch in the same daemon session this row is mislabelled.)
- [x] ✅ **15.2** `list` for GGUF rows: columns, SIZE, QUANT unchanged vs `main` (spot-check three rows). (GGUF rows' columns/SIZE/QUANT unchanged.)
- [x] ⚠️ **15.3** the shared `/v1/models` decoder refactor (`6f740e7`) did not break llama.cpp readiness or ds4 adoption — llama.cpp Ready observed in 15.1; ds4 exercised if `ds4-server` resolves, else ⏭️. (llama.cpp readiness unaffected by the shared `/v1/models` decoder refactor. ds4 not exercised — `ds4-server` is not installed in this sandbox.)
- [x] ✅ **15.4** golden TUI frames: `make test` green, and `tests/golden/dashboard-overview.txt` matches a fresh render for an equivalent catalog. (`make test` green including the golden frames.)
- [x] ✅ **15.5** `doctor --json` → exit 0, `schema_version` unchanged, and any vLLM advisory (if one exists) is `info` severity; a host with no vLLM produces no new finding. (`doctor --json` exit 0, `schema_version: 2`, findings `ds4_unavailable` (info) + `servers_configured` (info). No new vLLM advisory, and none needed.)
- [x] ✅ **15.6** `make lint` clean; `make test` (with `--features test-fixtures`) green — record counts. (`make test` exit 0, `make lint` exit 0 (fmt + clippy `-D warnings`).)
- [x] ✅ **15.7** `cargo build` **without** `--features test-fixtures` still compiles (the fixture `fake_vllm_server` must stay behind the gate). (`cargo build --bin llamastash` without `--features test-fixtures` compiles.)
- [x] ✅ **15.8** proxy for llama.cpp models: `/v1/models`, chat, `/ui/` pin — unchanged. (proxy `/v1/models` 200, llama.cpp chat 200, `/ui/` reachable.)

## 16. Docs vs binary

- [x] ✅ **16.1** `docs/usage.md` § vLLM backend: every flag/knob/env name it states exists in the binary (`--vllm`, `LLAMASTASH_VLLM`, the nine knobs, `--ctx`→`--max-model-len`). (All nine knob ids appear in both `src/backend/vllm/mod.rs` and `docs/usage.md`; `--vllm` and `LLAMASTASH_VLLM` documented and present.)
- [x] ✅ **16.2** `docs/vllm-setup.md`: the native-wheel install path is the primary one and the version it pins matches what was tested; the container caveat (SIGKILL does not reach the container) is stated. (Native wheel is the primary route, pinned to `0.27.1` (== the version tested), and the container caveat *"the SIGKILL escalation does not reach the container"* is stated.)
- [x] ✅ **16.3** `config.example.yaml` `backend.vllm` block is valid config (§7.4) and its comments match observed behaviour (cors default `true`, KV cap auto-behaviour). (The `backend.vllm` example block parses as-is (7.4) and its comments match observed behaviour.)
- [x] ⚠️ **16.4** `CHANGELOG.md` `[Unreleased]` carries a one-liner; `TODO.md` indexes the open follow-ups (multi-GPU `--device-ids`, optimistic eligibility, no metadata cache, cors default, extras dash-value, `uat.sh:135`). (`TODO.md` indexes all six open follow-ups. The `CHANGELOG.md` entry is one bullet but ~90 words — the house rule is a short one-liner.)
- [x] ✅ **16.5** `AGENTS.md` scope-boundary line ("Four backends… vLLM claims safetensors repos, never GGUF") matches observed §2.5 behaviour. ("vLLM … claims safetensors repos, never GGUF" matches 2.5/2.6.)
- [x] ✅ **16.6** `docs/architecture.md` § Backends / neutrality contract lists vllm; the no-leak rule holds: `rg -n '"vllm"|vllm::' src/ --glob '!src/backend/vllm/**'` returns only `src/backend/mod.rs`, the config re-export, and the CLI force-flag wiring. (`vllm` appears outside `src/backend/vllm/` only in `src/backend/mod.rs`, `src/config/mod.rs`, `src/cli/cli_args.rs` and `src/cli/daemon.rs` (the force flag) — the same four files `ds4` and `lemonade` occupy.)

---

## Teardown

```bash
"$BIN" stop --all -y 2>/dev/null || true
"$BIN" daemon stop 2>/dev/null || "$BIN" daemon stop --force 2>/dev/null || true
pgrep -f 'vllm serve' | xargs -r kill        # only if any survived — that is a ❌
rm -rf "$UAT_ROOT"
free -m                                       # must return to the §0.6 baseline
```

## Findings log

| ID | Sev | § | Summary | Status |
|----|-----|---|---------|--------|
| F-12 | **high** | 8.8 | **Proxy auto-start cannot start a vLLM model.** `src/proxy/launch.rs:347` `canonical_id_for_row` reads a GGUF header for whatever row the proxy resolved, so a directory-shaped row dies with `Is a directory (os error 21)` and the request 503s. Same assumption the PR fixed in `src/cli/start.rs` and `launch_service.rs`; the proxy path was missed. The alias routing in §8.2–8.4 only passes because the model was already running. | **FIXED 2026-08-15** — every surface now shares `backend::resolve_identity_for_path`. Re-verified: cold proxy auto-start of the vLLM model returns **200** with a real completion, and the auto-started launch carries the KV cap. |
| F-04 | **high** | 15.1 | **One vLLM launch poisons `status` for the rest of the daemon session.** Stopping a vLLM launch leaves its snapshot in `state.running` (identity-keyed removal misses the `Backend` variant), and `src/ipc/status.rs:47` matches snapshots **by port**, so every later launch on that port reports `backend:"vllm"` and the stale `resolved_ctx`. Repro: vLLM `--ctx 2048` → stop → `start <GGUF> --ctx 8192 --wait` prints `ready → ctx=2048`, `status` says `backend:"vllm"`, while `/proc/<pid>/cmdline` is `llama-server`. Corrupts the `--json` agent contract for **all** backends. Cleared only by a daemon restart. | **FIXED 2026-08-15** — `drop_running_snapshots` keys on the launch id (shape-agnostic), and `status` matches snapshots by launch id, not port. Re-verified: vLLM launch → stop → `start <GGUF> --ctx 8192` reports `backend:llamacpp`, `resolved_ctx:8192`, and `state.running` is empty after the stop. |
| F-07 | medium | 5.3 | **A user-set `gpu_memory_utilization` persists into `last_params` and silently disables the auto KV cap on every later launch.** One `--preset` run with a fraction, and a bare `start` replays it forever — no signal in `start` output. `cors` is deliberately re-derived per launch for exactly this reason (`seed_launch_knobs`); the memory knobs are not. Observed: bare `start --ctx 3072` took 22.6 GB RAM instead of ~13 GB, on the hardware whose documented failure mode is a freeze. | **FIXED 2026-08-15** — new `Backend::volatile_native_knobs` hook; vLLM declares both memory knobs. Re-verified: after a `--preset vllm-frac` launch, a bare `start` is back on the auto cap with no inherited fraction. |
| F-02 | medium | 10.1 | **An adopted non-GGUF external row loses its model path.** `src/daemon/mod.rs:378-386` derives `cmdline` and `model_path` from `adopted.id.as_gguf()`, which is `None` for a Backend identity, so the row reads `model_path: null` and `cmdline: "vllm --port 41100 -m "` (dangling `-m`). The user cannot tell which model the surviving process serves. Same `as_gguf()` gating the PR fixed one layer down in the sweep. | **FIXED 2026-08-15** — the adopted row falls back to `params.model_path`. Re-verified: `model_path` populated, cmdline carries the snapshot path with no dangling `-m`. |
| F-08 | medium | 4.9 | **`last-params <model>` cannot find a vLLM model's entry.** CLI says *"no recorded last-params … launch it once to populate"* while IPC `last_params_list` returns it — the CLI lookup keys on a GGUF `ModelId`. Compounds F-07: the documented surface for inspecting inherited knobs is exactly the one that can't see them. | **FIXED 2026-08-15** — `row_path` falls back to `params.model_path`. Re-verified: `last-params <repo id>` returns the entry. |
| F-01 | low | 3.1 | **`show` reports a phantom missing shard for a directory row.** Human view prints `shard 1  ! missing  <revision-hash>`; `--json` carries `shards[0].bytes: 0` with the snapshot dir as the shard path — directly under a correct `on_disk_total 942.3 MiB`. | **FIXED 2026-08-15** — a directory row has no shards; `path` still shows and `shard_count` is omitted. Re-verified: `size` block reads `on_disk_total 942.3 MiB` alone, and a GGUF still lists `shard 1`. |
| F-09 | low | 12.3 | **Delete confirmation understates a directory removal.** An out-of-cache directory row's dialog says *"One file is unlinked."* before a `remove_dir_all` of the whole snapshot. Wrong copy on a destructive, irreversible action. | **FIXED 2026-08-15** — re-verified in the TUI: a directory row reads *"The whole model directory goes — every file inside it."*, a GGUF still reads *"One file is unlinked."* |
| F-10 | low | 5.2 | **Every vLLM launch prints `! vllm does not support these knobs — dropped from the launch: --reasoning`**, on a bare `start` where the user set nothing. `reasoning` is a shared-IR default, not user intent, so it should not be reported as dropped. | **FIXED 2026-08-15** — the warning reads the user layer, and a `false` bool no longer counts (an unset preset value round-trips to `false`). Re-verified: bare and preset launches are silent, `--flash-attn on` still warns. |
| F-05 | low | 5.5 | **Two different weight figures for one launch.** `resolve_native_knobs` gets the raw `total_weight_bytes` (0 for an out-of-catalog directory), while the admission gate applies the `dir_weight_bytes` fallback locally (`launch_service.rs:985`). So the auto cap is sized as if the model weighed nothing. No unsafe outcome observed — the gate independently checks weights + cap ≤ free and refuses — but the cap is not the figure it claims to be. | **FIXED 2026-08-15** — `launch_total_bytes` measures the directory once, so the cap and the gate share one figure. |
| F-06 | low | 14.4 | **Backend-unavailable maps to exit 64, not 67.** `start <snapshot path>` with the launcher missing returns 64 (usage) with a correct message, while the ds4 precedent for the same condition is 67. Agents branching on exit codes see a usage error for an environment problem. | **FIXED 2026-08-15** — the daemon tags the refusal `backend_unavailable`; the CLI maps it to `BINARY_NOT_FOUND`. Re-verified: exit **70**. |
| F-11 | low | 13.5 | **`pull --help` still says "Pull a GGUF … defaults to all `.gguf` in the repo"** while it now pulls a whole safetensors repo (verified end to end). Help text narrower than behaviour. | **FIXED 2026-08-15** — `pull --help` describes both formats. |
| F-03 | medium | 6.8 | **`-- --port N` in extras overrides the reserved port.** The server binds N, llamastash probes the reserved port, the launch sits in `loading` with a full engine resident on an unmanaged port. `--host` is denied by `FORBIDDEN_ADVANCED_PREFIXES`; `--port` is not. **Pre-existing** — reproduced identically on llama.cpp (`llama-server` bound 9998). Not introduced here, but a vLLM cold start makes the waste 45–60 s and several GB. | **FIXED 2026-08-15** — `--port` added to `FORBIDDEN_ADVANCED_PREFIXES`. Re-verified: exit 64, refused. (Pre-existing, reproduced on llama.cpp.) |
| — | low | 1.9 | A config parse error makes `daemon stop` refuse (exit 64) before doing anything, so a bad `backend.vllm` block leaves the running daemon unstoppable through the CLI until the file is fixed. Pre-existing (`deny_unknown_fields` shipped before this PR); the new block is another way to reach it. | **FIXED 2026-08-15** — `daemon stop` joins `config` / `init` / `doctor` on the repair exemption. |
| — | — | 5.5 | ~~The admission refusal message labels decimal-GB numbers as `GiB`.~~ | **RETRACTED 2026-08-15 — not a defect.** `fmt_gib` divides by 1024³. `123.5 GiB` for a 115 GiB model is `weights + the 8 GiB cap + a 0.5 GiB overhead band`, which is what "needs" means. Confirmed against the 110 GiB case (`118.5`). Tester error, no code changed. |
| — | low | 16.4 | `CHANGELOG.md`'s vLLM entry is ~90 words; the house rule is a short one-liner. | **FIXED 2026-08-15** — trimmed to one line plus a doc pointer. |

## Run log

| Field | Value |
|-------|-------|
| Date / runner | 2026-08-14 / AI agent (Opus), maintainer-driven |
| Target | PR #63 `feat/vllm-backend` @ `2c8cc33` (60 files, +5514/−241) |
| Binary version | `llamastash 0.1.0` (debug, built from the branch) |
| vLLM version | `0.27.1+rocm723` (native ROCm wheel at `~/.venvs/vllm/bin/vllm`) — PyPI latest at test time: `0.27.1`. **Current, not stale.** |
| Host | AMD Ryzen AI Max+ 395 / Radeon 8060S (gfx1151), 121 GB unified, ROCm; 120574 MiB available at baseline |
| Fixtures | `Qwen/Qwen2.5-0.5B-Instruct` (safetensors, 942 MiB) · `Llama-3.2-1B-Instruct-Q4_K_M.gguf` + `Phi-3.5-mini-instruct-Q4_K_M.gguf` (llama.cpp controls) · synthetic HF trees (`o/solo`, `o/outside`, `o/mixed`, `o/sftnonly`, a sparse 115 GiB `o/huge`) · a real `hf-internal-testing/tiny-random-gpt2` pull |
| Items ✅/❌/⏭️/⚠️ | 128 items executed: **111 ✅ · 3 ❌ · 11 ⚠️ · 3 ⏭️**. ❌ on 4.9 (F-08), 6.8 (F-03), 8.8 (F-12); ⏭️ on 10.2/10.4 (no decoy server / no external vLLM to claim), 12.2 (unreachable through discovery by design). |
| Findings | 12 in-scope (F-01…F-12) + 3 pre-existing/docs. Two high: F-12 (proxy auto-start 503s on a vLLM model) and F-04 (a vLLM launch poisons `status.backend` / `resolved_ctx` for every later launch in the session). |
| Suite | `make test` exit 0, `make lint` exit 0, `cargo build` without `--features test-fixtures` exit 0. |
| Notes | Every vLLM assertion ran against the **real** 0.27.1 engine; `fake_vllm_server` was never in the loop. Memory was sampled before/after every launch group — peak use 22.6 GB (the `gpu_memory_utilization 0.15` case, matching the documented measurement), and available RAM returned to baseline ±1 GB every time. No process survived teardown. One self-inflicted incident: a harness run without the sandbox env spawned a daemon against the real state dir and launched two models there; it was stopped and the real state dir left clean. The SIGKILL in §10.1 orphaned the lemonade umbrella, which then blocked `daemon start` until `--force` — a side effect of the test, not of the PR. |
