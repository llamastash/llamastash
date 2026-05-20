<!--
  Release PR template. Open via:
      gh pr create --template release.md
  The default template (`default.md`) covers every other PR so the UAT
  checklist below isn't repeated on non-release work and trained
  into ignored-banner territory.
-->

## Release summary

Version: vX.Y.Z

<!-- One short paragraph: scope of the release, headline changes. -->

## UAT

See `docs/testing/hardware-uat.md` for how to run each lane and how to
read the JSON report. Attach `uat-*.json` reports as files or paste
verbatim below each backend.

- [ ] NVIDIA CUDA (warm)
- [ ] AMD ROCm (warm)
- [ ] Apple Silicon Metal (warm)
- [ ] Vulkan fallback (warm)
- [ ] ≥ 1 backend run in cold mode this cycle (state which): _______

**Backends not covered this release (with reason)**: _none / list_

If a UAT run caught a regression that would otherwise have shipped,
apply the `uat-caught` label so the 6-month outcome-metric review
(`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`
§Outcome metric) has signal.

## Test plan

- [ ] `cargo test --features test-fixtures`
- [ ] `cargo clippy --all-targets --features test-fixtures -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] CHANGELOG.md updated under the new release section
- [ ] README.md install snippet + `--version` sample refreshed if user-visible CLI changed
