# Known flakes and how to recover

Read this before changing code or the pipeline because a run went red once. The release commit is a version bump, so a gate failure on it is almost never about the release itself.

## How to judge a failure

```bash
gh run view <id> --json jobs --jq '.jobs[] | "\(.name) | \(.status) | \(.conclusion)"'
gh run view <id> --log-failed 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | tail -40
```

Read the failing job's log, not the whole run. Answer three questions: which test, what did it assert, does the failure touch anything this commit changed (a release commit changes `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and the goldens, nothing else). If the answer is no, it is a flake or the environment.

`gh run rerun <id> --failed` re-runs only the failed jobs and the jobs that depended on them. That is the fix for a flake: cheap, and it keeps the tag pointing at the commit that was actually tested. `gh run rerun` prints nothing on success; confirm with `gh run view <id> --json status,conclusion,attempt`.

## `release-gate (macos-cpu-only)` on `prefer_port_falls_back_to_range_allocator_when_busy`

`tests/start_model_ipc_test.rs`, message like `port allocation failed: no free port in 51940-51941 (2 reserved by caller)`.

The test reserves two ephemeral ports to force a `prefer_port` collision, and a loaded macOS runner can take them back before the daemon binds. It retries five times and asserts on the last attempt, so five consecutive losses fail the gate. The ports, not the code, are the problem.

Recovery: `gh run rerun <id> --failed`, once. It passed on the second attempt on 2026-10-30. If it fails a second time on the same commit, the test needs work before the release goes out: harden the port reservation on `main`, get `ci` green, then move the tag (`git tag -f vX.Y.Z && git push -f origin vX.Y.Z` before anything has been published from it, deleting the GitHub release first with `gh release delete vX.Y.Z` if `create-release` made one; or cut the next patch version if anything was published).

## `release-gate (macos-cpu-only)` on `case_variant_of_a_live_name_reuses_that_launch`

`tests/proxy_fallback.rs`, `first named launch must succeed` with `left: 503`. The first auto-start failed fast and the proxy answered `launch_failed`; the assertion message carries the response body, whose `cause` says why. Seen once, on v0.11.0's second gate attempt, while the Linux gate passed and the test passed 40 threaded local runs. Probably the same port race as above: the test's launch range is built from just-released ephemeral ports. Tracked in `TODO.md` under `## Next`.

Recovery: `gh run rerun <id> --failed`. If it comes back, read the `cause` in the message before touching the test.

## A macOS job `cancelled` with no steps

The job annotation reads `The job was not acquired by Runner of type hosted even after multiple attempts` and `Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer queue times`. GitHub had no macOS arm64 runner and gave up after about 15 minutes; no code ran. Hit twice on v0.11.0, once on `release-gate (macos-cpu-only)` and once on `build aarch64-apple-darwin`. Read it with `gh api repos/llamastash/llamastash/check-runs/<job-id>/annotations --jq '.[].message'`.

Recovery: `gh run rerun <id> --failed`. When it hits a build after `create-release`, the release is already public without that target's assets and every publish job is skipped until the rerun passes.

## Daemon-spawning tests timing out in a local pre-push run

`cli_integration_test` and friends time out at 20-44 s when the box is busy (a model server loaded, a package sync, a build). The hook aborts the push with only `error: test run failed` in the log, so run the suite yourself to see which test tripped:

```bash
cargo nextest run --features test-fixtures --workspace 2>&1 | tail -40
```

If it is a timeout under load, re-run when the box is idle. Do not lower the bar in the test to make a busy box pass.

## The watcher said the run was over but `gh` disagrees

A background poller on `gh run list -c <sha>` can read a stale status or miss a run entirely on a short SHA. Always pass the full SHA, and confirm any verdict with a direct `gh run view <id> --json status,conclusion,attempt` before acting on it.
