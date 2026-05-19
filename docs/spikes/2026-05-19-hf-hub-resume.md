---
title: "spike: hf-hub multi-shard download + HTTP Range resume"
date: 2026-05-19
status: superseded
unblocks: ["Unit 9"]
---

> **Resolution (2026-05-19, post-implementation).** v2 pins
> `hf-hub = "0.5.0"` (see the resolution footer in
> [`2026-05-19-hf-hub-client-injection.md`](2026-05-19-hf-hub-client-injection.md)).
> 0.5.0 does not expose the `Range` builder this spike documents,
> so Range-resume is **deferred to v2.1** along with native progress
> reporting. A fresh `repo.get(filename)` on an interrupted pull
> re-downloads from byte zero — acceptable for a launch-MVP, and the
> v2.1 work item is to bump hf-hub once a non-rc line ships both
> the `Range` builder and a custom-`reqwest::Client` hook without
> the reqwest 0.13 transitive.

# Finding

**hf-hub 1.0.0-rc.1 supports native HTTP Range resume. No wrapper needed. The resume strategy reduces to: HEAD-probe for `Content-Length` → check `existing_size` on disk → pass `Range { start: existing_size, end: total }` to the download builder.**

## Evidence

Source: `https://github.com/huggingface/hf-hub/blob/v1.0.0-rc.1/hf-hub/src/repository/download.rs`.

- Lines 6-12: download module doc — "`download_file_stream`, `download_file_to_bytes`, `download_file_to_cache`. Range parameters use Rust `std::ops::Range<u64>` semantics (start-inclusive, end-exclusive)."
- Lines 84-90: `range.start >= range.end` is a structural error caught before the request.
- Lines 137-145: the range translates to a literal `Range: bytes={start}-{end - 1}` HTTP header. Standard RFC 7233.
- Lines 64-167: `download_file_stream` returns a `Stream<Bytes>` for the requested byte slice; `download_file_to_cache` writes to the canonical HF cache path (`~/.cache/huggingface/hub/models--<owner>--<repo>/snapshots/<rev>/<file>`).
- Lines 115-134: XET-hash files (LFS variant) route through a separate `xet_download_stream` path that also supports range parameters.

## Multi-shard pattern (R63)

```rust
// pseudocode for Unit 9
let listing = repository.list_files(revision).await?;
let shards = split_gguf::detect(&listing); // existing src/discovery/split_gguf.rs
for shard in shards {
    let target = cache_path(&shard);
    let existing = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let head = repository.head_file(&shard.filename).await?;
    if existing == head.content_length {
        continue; // already complete; checksum verified separately
    }
    let range = if existing > 0 {
        Some(existing..head.content_length)
    } else {
        None
    };
    let stream = repository
        .download_file_stream()
        .filename(shard.filename.clone())
        .revision(revision)
        .range(range)
        .progress(progress.clone())
        .stream()
        .await?;
    append_stream_to(&target, stream).await?;
}
```

Sequential shard downloads only. Parallel shards complicate progress reporting, share the same network bandwidth ceiling, and trip a common HF rate-limit pattern. Sequential matches the brainstorm's stated R63 scope.

## Resume correctness

- Atomic append: hf-hub writes the streamed bytes; Unit 9's per-shard wrapper appends to the cache file under a `.partial` suffix and atomically renames on completion. A mid-stream crash leaves a `.partial` file the next run resumes from.
- Content-length must match: if the HEAD-reported `Content-Length` shrinks between runs (HF re-uploaded the shard with a smaller size), Unit 9 detects `existing > total`, deletes the `.partial`, restarts. Same defensive check we apply to `state.json` quarantine.
- Checksum gate: HF returns an `ETag`-style hash (sha256 of file bytes for non-LFS, XET hash for LFS). Unit 9 records the expected hash from the listing and verifies after the shard is fully written.

## Unknowns left to implementation

- **Concurrent downloads of unrelated shards:** out of scope for v2 MVP (R63 says sequential). The plumbing is one `join_all` away if a follow-up needs it.
- **Range against an HF mirror that doesn't honor RFC 7233 byte ranges:** rare on the canonical `huggingface.co` CDN. If detected (response body length != requested range length), Unit 9 falls back to a full re-download with a logged warning.
- **`download_file_to_cache` vs manual cache writes:** `download_file_to_cache` writes directly to the canonical path; the wizard uses it to avoid reinventing cache layout. Probe behavior: confirm `download_file_to_cache` is idempotent on a pre-existing complete file (read the integration test in `hf-hub` before Unit 9 lands).
