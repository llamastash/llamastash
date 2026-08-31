# Runbook: proxy latency + throughput bench (R160)

Origin requirement: **R160** in
[`docs/brainstorms/2026-05-21-proxy-router-requirements.md`](../brainstorms/2026-05-21-proxy-router-requirements.md).
Plan unit: **Unit 7** of
[`docs/plans/2026-05-21-001-feat-proxy-router-plan.md`](../plans/2026-05-21-001-feat-proxy-router-plan.md).
Bench source: [`benches/proxy_overhead.rs`](../../benches/proxy_overhead.rs).

This procedure validates that the OpenAI-compat proxy adds **negligible
overhead** to the underlying `llama-server` round-trip. R160 phrases this
as three guardrails:

| Axis                          | Target (R160)        |
| ----------------------------- | -------------------- |
| p50 routing-decision latency  | **< 0.5 ms**         |
| Streaming first-token overhead | **< 5%** vs direct  |
| Throughput overhead           | **< 2%** vs direct   |

The targets are guardrails on the broader "negligible overhead" promise.
If reality on the reference machine misses one, the right move is to
investigate (it might be a real regression) and then either land a fix
or update R160 in the brainstorm with the achievable numbers — see
[Remediation](#remediation) below.

## How the bench works

`benches/proxy_overhead.rs` stands up an in-process harness on each
`cargo bench` invocation:

1. Spawns the `fake_llama_server` test fixture through the real
   `supervisor::spawn` path, the same way `tests/proxy_routing.rs`
   does. The supervisor probes `/health` and transitions to `Ready`
   before the bench starts measuring.
2. Builds a minimal `ProxyState` with a one-entry catalog + registry
   referencing the fake's port.
3. Spawns the proxy listener on an ephemeral loopback port via
   `proxy::server::serve`.
4. Runs three Criterion benchmark groups, each with a `direct` and
   `proxied` arm:

   - **`routing_decision`** — full `POST /v1/chat/completions`
     round-trip on a small non-streaming body. The `direct` arm
     targets the fake's port; the `proxied` arm targets the proxy's
     port. The proxied/direct delta is "what the proxy adds" —
     resolver call + supervisor snapshot + `reqwest` forward.
   - **`first_token`** — `stream: true` request; the bench
     measures the moment the first byte of the response head
     arrives. Validates R160's streaming first-token overhead.
   - **`throughput`** — `stream: true` request; the bench drains
     the entire SSE body. Criterion reports bytes/second so the
     direct/proxied throughput ratio is directly comparable.

The bench measures client-side round-trips end-to-end. The plan calls
out this fallback: a clean in-process instrumentation hook would
shave the client's TCP round-trip out of the routing-decision number,
but at our load level the difference between client-roundtrip-proxied
and client-roundtrip-direct is a close-enough proxy of "what the
proxy adds." A `bench-hooks` feature gate was considered and rejected
to keep the production hot path free of compile-time conditionals.

## Prerequisites

- Rust toolchain on the project's pinned version (`rust-version =
  "1.85"` in `Cargo.toml`).
- A clean working tree (other heavy processes can skew the
  `<0.5 ms` ceiling).
- ~2 minutes of wall time per full bench run on a 2020+ x86 server-
  class CPU. `--quick` runs in ~20 s for smoke purposes but its
  numbers are noisy — use them for harness sanity, not for R160
  sign-off.

## Quickstart

```bash
# Full convergence run — what the maintainer uses for R160 sign-off:
cargo bench --features test-fixtures --bench proxy_overhead

# Smoke / harness sanity (does the bench compile + run cleanly?):
cargo bench --features test-fixtures --bench proxy_overhead -- --quick
```

Criterion writes an HTML report to `target/criterion/`. Open
`target/criterion/report/index.html` for the per-axis distribution
plots; the maintainer pastes the headline numbers into the PR
description.

## Reading the numbers

For each group the report shows the per-arm timing with confidence
intervals. Compute the three R160 numbers as follows.

### Routing decision (p50 < 0.5 ms target)

The R160 target is the *added* latency the proxy introduces — i.e.
the difference between the proxied and direct arms of the
`routing_decision` group:

```
routing_decision_p50_added = routing_decision/proxied[p50]
                           - routing_decision/direct[p50]
```

On the reference machine (Linux x86_64, 2020+ server-class CPU,
idle), `routing_decision_p50_added` should land **under 500 µs**.
Numbers from a smoke run during Unit 7 implementation were ~100 µs
added overhead; the maintainer's reference run is the authoritative
number for the PR description.

### First-token overhead (< 5% target)

```
first_token_overhead_pct = (first_token/proxied[p50]
                          / first_token/direct[p50] - 1) * 100
```

R160 target: **< 5%**. Numbers above that range are worth
investigating; the proxy's pass-through pipeline should add only a
few microseconds per stream once warm.

### Throughput overhead (< 2% target)

Criterion already reports throughput in MiB/s under the `thrpt:`
line. The R160 target maps to:

```
throughput_overhead_pct = (1 - throughput/proxied[median_thrpt]
                             / throughput/direct[median_thrpt]) * 100
```

R160 target: **< 2%**. Note that on loopback with such small
payloads (the fake's canonical SSE response is ~200 bytes), the
direct-vs-proxied throughput ratio is dominated by per-request
syscall overhead, not stream pumping. Treat the throughput target
as the noisiest of the three.

### Reference number ranges (smoke run, single laptop)

These are *not* the official R160 numbers — they're a sanity-check
band so a developer running the bench locally can spot an obvious
regression without having the reference machine handy:

| Metric                                   | Smoke band   |
| ---------------------------------------- | ------------ |
| `routing_decision/direct` median         | 30-100 µs    |
| `routing_decision/proxied` median        | 100-300 µs   |
| `routing_decision_p50_added`             | 50-300 µs    |
| `first_token/direct` median              | 40-100 µs    |
| `first_token/proxied` median             | 100-300 µs   |
| `first_token_overhead_pct`               | varies       |

The first-token + throughput percentage targets are tight; on
loopback with sub-millisecond responses, a constant ~50 µs proxy
overhead is a much higher *fraction* of a sub-100 µs direct
roundtrip than it would be on a real production request (which is
seconds of model time). The runbook's "what to do if a target slips"
section covers this exact case.

## Reproducibility check

Per the Unit 7 plan, two consecutive bench runs on the same machine
should land within ~10% of each other on p50. To confirm:

```bash
# Run 1
cargo bench --features test-fixtures --bench proxy_overhead | tee /tmp/bench-run-1.txt

# Run 2
cargo bench --features test-fixtures --bench proxy_overhead | tee /tmp/bench-run-2.txt

# Eyeball the p50 / median columns; criterion's HTML report also
# carries a "change" line if you've run the bench previously against
# the same target/criterion baseline.
```

If run-2's p50 differs by > 10% from run-1, suspect:

- Background processes contending for CPU (browser tabs, docker
  daemon doing IO, indexer running).
- Thermal throttling — laptops with quiet fan profiles can hit
  this on the second run.
- Frequency scaling — pin the governor to `performance` on Linux
  (`sudo cpupower frequency-set -g performance`).

## Validity check

Both arms exercise the same upstream behavior — same input bytes,
same `fake_llama_server` response. The bench's `discovered()` /
`MethodContext` plumbing is identical to `tests/proxy_routing.rs`,
which has direct test coverage for byte-exactness of the forwarded
body. If the bench numbers ever look implausible (e.g. proxied
faster than direct), confirm the harness still spins up cleanly by
running:

```bash
cargo test --features test-fixtures --test proxy_routing
```

If those tests are green, the bench harness is valid; the implausible
numbers are noise or measurement artefact.

## Remediation

What to do when a target is missed (per the Unit 7 plan):

1. **Investigate before reacting.** Run the bench twice; if both
   runs miss the same target by the same margin, it's not noise.
2. **Bisect against `main`.** `cargo bench --features test-fixtures
   --bench proxy_overhead` against the merge base of the current
   branch — if `main` also misses the target, the regression
   predates this branch and the right next step is a separate
   investigation, not a hold on the current PR.
3. **Look at the obvious cost centres** in `src/proxy/`:
   - `route::decide` — the resolver call is the main pre-forward
     CPU cost. A catalog with hundreds of rows + fuzzy substring
     search can be expensive.
   - `forward::forward_to_upstream` — `reqwest::Client` pool reuse,
     hop-by-hop header stripping, body-bytes cloning.
   - `route::buffer_and_extract` — the body cap (default 16 MiB,
     `proxy.max_body_size`) means a small body is always buffered in
     full; serde + `Limited` shouldn't dominate but check.
4. **If the slip is real and unfixable**, update R160 in the
   brainstorm (
   [`docs/brainstorms/2026-05-21-proxy-router-requirements.md`](../brainstorms/2026-05-21-proxy-router-requirements.md))
   with the achievable numbers. One-line PR note: "R160 revised:
   p50 routing decision relaxed from < 0.5 ms to < N ms; see
   bench output in PR." The "negligible overhead" promise is what
   matters; specific µs figures serve as guardrails.

## Alternative harness (if the in-process scaffolding ever breaks)

The bench currently builds `ProxyState` directly from a
`MethodContext` — the same path `tests/proxy_routing.rs` uses. If
the `ProxyState` surface ever needs substantial scaffolding to be
testable (e.g. a future unit adds opaque required fields), the
fallback is to spin up a full daemon via:

```bash
cargo run --release -- daemon start
# … bench against the running proxy port, then:
cargo run --release -- daemon stop
```

That path is significantly noisier (additional process boundary,
real `state.json` IO, supervisor lifecycle in a separate
runtime) and is not the preferred measurement; it exists as an
escape hatch documented here so the bench can keep moving if
internal API churn forces it.
