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

All three scripts record the power state next to every result for exactly this
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
layers only and strips the preset `backend_knobs` that carry `--mtp` /
`--dspark`.

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
