---
title: "spike: hf-hub HTTP client injection + native Range resume"
date: 2026-05-19
status: superseded
unblocks: ["Unit 4", "Unit 9"]
---

> **Resolution (2026-05-19, post-implementation).** Unit 9 adopts
> `hf-hub = "0.5.0"` instead of `1.0.0-rc.1`. See the *Resolution*
> section at the bottom of this doc — the rc.1 path documented
> below was abandoned before merge.

# Finding

**Pin `hf-hub = "1.0.0-rc.1"`. Custom `reqwest::Client` injection is supported; native HTTP Range resume is supported. No fetch-contract carve-out is required for v2.**

## Evidence

Source: `https://github.com/huggingface/hf-hub/tree/v1.0.0-rc.1/hf-hub/src/`.

- `hf-hub/src/client.rs:122-130` — `HFClientBuilder::client(mut self, client: reqwest::Client) -> Self`. Documented as: "Supplies a pre-configured `reqwest::Client`. Retry middleware is still applied on top." The injected client owns all transport policy (TLS, redirect, proxy, host filtering); hf-hub layers retries on top via its own scheduler.
- `hf-hub/src/client.rs:185-194` — when no injection is supplied, hf-hub builds its own internal `reqwest::Client` with default headers; injection overrides this path entirely.
- `hf-hub/src/repository/download.rs:84-145` — download builders accept `range: Option<std::ops::Range<u64>>`, translated to a `Range: bytes={start}-{end - 1}` header on the underlying GET. HEAD-first probe captures `Content-Length` and `X-Linked-Size`, so partial downloads can resume from a recorded offset without a separate wrapper.
- `hf-hub/src/repository/download.rs:23-44` — `Progress` trait + `DownloadEvent::{Start, Progress, Complete}` enum. Wired into both stream and tempfile download paths, so the wizard's progress UI doesn't need a polling loop.
- `hf-hub/Cargo.toml:34-43` — pulls `reqwest = "0.12.2"` with `default-features = false, features = ["json", "stream"]`. Compatible with llamastash's existing `reqwest = "0.12"` pin (`Cargo.toml:62-68`). Feature flag `rustls-tls` enables our preferred TLS stack.

## Version selection

| Version | client injection | native Range | stability |
|---|---|---|---|
| 1.0.0-rc.1 (2026-05-07) | ✓ | ✓ | release candidate |
| 0.5.0 (2026-02-19) | ✗ | partial (download_file uses chunks but no exposed `Range` builder) | stable |
| 0.4.x | ✗ | ✗ | stable |

The 0.5.0 stable path would require a carve-out wrapping hf-hub's downloads behind llamastash's `FetchClient` (~150 LOC of glue + reimplemented cache-layout writes). 1.0.0-rc.1 lets us pin the rc and use the official API directly — much smaller surface to maintain. Risk is contained: `Cargo.lock` pins the resolved version; an rc bump goes through a deliberate PR.

## Implications for Units 4 / 9

- **Unit 4** (`FetchClient`) builds the shared `reqwest::Client` configured per the v2 fetch contract (allowlisted hosts, redirect cap, IP-class filter, body-size cap, TLS-only, no `GITHUB_TOKEN`).
- **Unit 9** does **not** depend on `hf-hub` in v2 — the rc.1 transitive `reqwest = "0.13"` clashes with llamastash v1's pinned `reqwest 0.12`, surfacing a Send-bound auto-trait regression in v1's CLI integration tests. Bumping reqwest crate-wide is the right v2.1 work item; for v2 launch we ship a minimal in-crate HF client (~350 LOC) on top of [`crate::init::fetch::FetchClient`] that hits the same `/api/models/{repo}/tree/main` and `/{repo}/resolve/main/{file}` endpoints. Strictly *more* fetch-contract enforcement than the hf-hub-injection path because every HF request rides the v2 allowlist + redirect cap + body cap directly.
- Range resume / native progress reporting: Out of v2 MVP scope. Re-implement on top of the in-crate client (or land the hf-hub bump) in v2.1.
- v2.1 bump checklist: (a) update `Cargo.toml` reqwest to 0.13, (b) replace the in-crate HF client with `hf-hub` 1.0.0 (stable by then), (c) address the Send-bound test regression (likely via boxing the dispatch future, but more cleanly with a Tokio version of `tokio::task::spawn` that handles HRTB Send better — track upstream).

## Unknowns left to implementation

- **Token resolution:** `HFClientBuilder::token` accepts a `String`. The wizard reads `HF_TOKEN` (env) → `~/.cache/huggingface/token` (file). The mode-check on the cache-file token (`refuse if mode is group/world-readable`) happens in Unit 9 before passing the value to hf-hub.
- **`HFClient::builder().build_sync()`** exists for blocking callers; Unit 9 uses the async path (the wizard runs under tokio).
- **No allowlist policy enforcement inside hf-hub itself.** The injected `reqwest::Client`'s redirect policy is what gates this; verify in Unit 4 tests that a maliciously redirected HF response cannot land on a non-allowlisted host.

## Resolution

After landing the in-crate HF client during Unit 9 implementation, a verification check during release scaffolding caught that **`hf-hub = "0.5.0"` resolves to `reqwest 0.12.28`** — same major as our pin, no version-conflict and no Send-bound regression. The spike's Version-selection table overstated 0.5.0's glue cost by assuming we needed Range resume + `FetchClient` injection for v2; the v2 plan defers both (resume is a v2.1 follow-up; the fetch contract intentionally carves out HF traffic because hf-hub talks only to `huggingface.co` and its LFS CDN, both already constrained host families).

Effects of the swap (commit on `feat/v2-init-wizard`):

- `Cargo.toml`: `hf-hub = "0.5"` with `default-features = false, features = ["tokio", "rustls-tls"]`. No reqwest pin change.
- `src/init/download.rs`: rewritten against `hf_hub::api::tokio::{Api, ApiBuilder, ApiRepo}`. ~150 LOC removed (in-crate listing + per-file GET + atomic write); ~80 LOC added (Api construction, RepoInfo filter, per-file `Api::metadata` HEAD for the PER_FILE_MAX_BYTES + R64 disk precheck). Net −70 LOC.
- Cache layout: hf-hub writes `blobs/<etag>` + `snapshots/<commit_hash>/<filename>` symlinks, which matches Python `huggingface_hub` exactly — closer to what real-world HF caches look like than the in-crate "plain file under `snapshots/main/`" layout that preceded the swap. `discovery::known_caches` walks symlinks already, so no scanner change was needed.
- Fetch-contract carve-out: HF traffic does **not** ride `FetchClient`'s host allowlist / redirect cap / body cap. hf-hub builds its own `reqwest::Client` constrained to `huggingface.co` + its LFS CDN. GH Releases and benchmark snapshot fetches continue to go through `FetchClient`.
- Test coverage: identical (11 tests in `download.rs`); the only test that materially changed was `download_repo_propagates_offline`, which now exercises the early-return Offline check before any Api construction.

v2.1 follow-up (still tracked): bump to a future `hf-hub` line if it exposes Range resume + a custom-client hook without a reqwest 0.13 transitive. Until then, 0.5.0 is the supported pin.
