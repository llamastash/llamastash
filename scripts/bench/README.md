# Benchmark scripts

Two families live here. `end_to_end/`, `overhead/` and `proxy/` are the Python
harnesses behind the published numbers in `docs/benchmarks/` — see
`docs/benchmarks/methodology.md` before touching those. The shell scripts below
are speculative-decoding comparisons, kept because they are worth re-running
whenever a backend or a draft head changes.

## Before running any of them

**Check the power state first.** On a laptop the same binary and prompt measured
9.5 t/s at 13% battery and 13.9 t/s with headroom, a ~30% swing with no code
change, while `platform_profile` read `performance` in both cases. That field is
a request, not a guarantee.

```sh
cat /sys/class/power_supply/BAT0/capacity   # and .../status, AC0/online
cat /sys/firmware/acpi/platform_profile
rocm-smi --showpower                        # ~80 W under load, ~20 W throttled
```

Every script here records the power state next to every result for exactly this
reason. Stop stray daemons (`llamastash daemon stop`, `pkill -x ds4-server`)
before a run so nothing competes for the GPU or the power budget.

## `mtp_ab.sh` — MTP on/off for one model

```sh
scripts/bench/mtp_ab.sh <model-path-or-ref> <label> [out.md]
```

Launches the model twice per prompt (`--mtp off`, `--mtp on`) and reports decode
rate, whether llama.cpp engaged the draft path, and acceptance. Needs a pairable
draft head: embedded MTP layers, or a sibling named `mtp-<model-basename>.gguf`.

Watch the `active` column, not just the rate. A truncated or mismatched head
makes the launch *fail*, not run slowly — that is how a broken 32 MiB Gemma head
was caught, which would otherwise have read as "MTP is slow".

## `dspark_ab.sh` — ds4 three-way through llamastash

```sh
scripts/bench/dspark_ab.sh <work-dir> <out.md>
```

pre-0731 / 0731 / 0731+DSpark, launched via llamastash presets. Do **not** set
`LLAMASTASH_BENCH_DISABLE_DEFAULTS` here: it collapses knob resolution to User
layers only and strips the preset knobs that carry `--mtp` / `--dspark`.
ds4 runs as a `backend.generic` entry the script writes into its own config.

## `ds4_dspark_charged.sh` — same three-way, direct `ds4` CLI, power-gated

```sh
BATTERY_TARGET=78 scripts/bench/ds4_dspark_charged.sh [out.md]
```

Bypasses llamastash to isolate engine behaviour, waits for a charged battery on
AC, and samples peak GPU package power per row. Use this one when the question
is about ds4 itself rather than about llamastash's flag composition.

Note the deliberate omission of a pre-0731 + DSpark row: the DSpark support GGUF
is checkpoint-specific to Flash 0731 and drafts nothing (`proposed=0`) against
an older checkpoint.

## `proxy/concurrency.py` — concurrent load on the proxy

```sh
python3 scripts/bench/proxy/concurrency.py --url http://127.0.0.1:11435 \
    --model <id> --workload chat|models|bigbody --concurrency 16 --requests 160
```

The overhead suite sends one request at a time; this drives N concurrent
clients and prints one JSON line with req/s, latency p50/p99 and, for `chat`,
streamed chunks/s and TTFT. `models` hits `GET /v1/models`, which the proxy
answers itself; `bigbody` pads a `max_tokens: 1` chat request with 2 MiB. One
Python client tops out near 700 req/s on `models`, so run several processes in
parallel and read the daemon's CPU time per request
(`/proc/<daemon pid>/stat`) rather than req/s alone. Use it to compare two
daemon builds on the same upstream, alternating builds between rounds.

## `slot_cache_spike.py` — slot save/restore against reprocessing

```sh
scripts/bench/slot_cache_spike.py --model /path/to/model.gguf \
    --tokens 100000 --ctx 110000 --save-dir target/slot-spike
```

Starts a raw `llama-server` with `--slot-save-path`, fills one slot, saves it,
restarts, restores and resends the prompt. Prints the timings and the file size
as JSON. `--ngl` and `--threads` set the placement (default CPU only). Keep
`--save-dir` off tmpfs. Results: `docs/spikes/2026-10-06-slot-save-restore.md`.

## `qwen38-flash-speed/` — Qwen3.8-Flash-Next engine and knob comparison

```sh
ENGINE=halogen ./scripts/bench/qwen38-flash-speed/serve.sh ensure
REPS=3 ./.auto/measure.sh
python3 scripts/bench/qwen38-flash-speed/bench.py --base http://127.0.0.1:41103 \
    --model <id> --engine halogen --reps 3 --deep-tokens 32768
```

Drives gufo (GGUF) and halogen (`.hgn`) through the same OpenAI surface, because
the point is comparing them and each reports timings under its own keys. Reads
the engine's own decode-window rate rather than wall clock, always runs behind a
real `src/**/*.rs` corpus prefix — decode at an empty context is a different
machine — and pins `finish=length` so the rate's denominator cannot drift. Each
rep gets a disjoint corpus slice and a unique first line: both engines keep
prompt prefixes across processes, and a warm rep reports a prefill rate that
measures the cache. `serve.sh` hashes the knob file and restarts only on a real
change, and refuses to load a second engine while anything else holds the GPU.

Engine choice and every launch knob live in `engine.sh`; that file is the write
target of the autoresearch session recorded in `.auto/`. See
`.auto/prompt.md` for the workload, the baseline, and the power state a number is
only comparable within.
