# SGLang scripts

## `probe.sh`

Reads the facts the SGLang backend is built on off the real container image,
without a GPU or a model load: the console script's presence, the flag surface,
and — against a running server — the `/get_server_info` keys the actuals read.
See `docs/plans/2026-09-09-001-feat-sglang-backend-plan.md`.

```bash
scripts/sglang/probe.sh version   # sglang version + whether the console script exists
scripts/sglang/probe.sh help      # `sglang serve --help`, no GPU needed
scripts/sglang/probe.sh flags     # the subset the backend declares, refuses or reads
scripts/sglang/probe.sh info      # /get_server_info + /v1/models of a server on SGLANG_PORT
scripts/sglang/probe.sh shell     # interactive shell in the image
```

`help`, `version` and `shell` cost only a container start. `info` only reads.
The image tag is a constant at the top of the script; re-verify against a live
server before trusting any flag list.

For the wrapper script that makes a containerised SGLang usable as a LlamaStash
backend binary, see `docs/sglang-setup.md` — that is a different thing from
this probe harness.

## `uat.sh`

End-to-end UAT against a **real** SGLang, in an isolated state dir
(`~/.cache/ls-sglang-e2e`, override with `LS_UAT_HOME`). Every stage asserts
and exits non-zero on failure — a stage that could not run is a failure, not a
pass.

```bash
scripts/sglang/uat.sh all      # clean → boot → launch → chat → replay → stop
scripts/sglang/uat.sh launch   # or drive one stage at a time
```

| Stage | What it proves |
|---|---|
| `boot` | the backend is actually available, and the model reaches the catalog |
| `launch` | the token cap reached argv, and `resolved_ctx` came back from `/get_server_info` |
| `replay` | `last_params` re-applied `--ctx`, and the cap re-resolved |
| `preset` | a preset's own `max_total_tokens` beat the auto cap (needs an `sglang-small` preset) |
| `chat` | the repo id serves 200 |

Every accessor selects by the `LaunchId` the stage started, so a later stage
cannot report an earlier launch's argv as its own result.

`launch` is the one that matters: a missing token cap means SGLang sizes its
pool from `--mem-fraction-static`, a fraction of the whole pool, which on a
unified-memory host is system RAM.
