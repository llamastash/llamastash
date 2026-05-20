<!--
  Default template for every non-release PR. Released-versions get the
  richer `release.md` template instead — open release PRs with
  `gh pr create --template release.md`.
-->

## Summary

<!-- One short paragraph (or a few bullets) describing what changed and why. -->

## Test plan

- [ ] `cargo test --features test-fixtures`
- [ ] `cargo clippy --all-targets --features test-fixtures -- -D warnings`
- [ ] `cargo fmt --all -- --check`
