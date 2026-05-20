# Runbook: verify the UAT reintroduction

Background: the `llamastash uat` feature was deleted on `main` during
the `llamadash → llamastash` rename, then squash-merged back from
`feat/uat-hardware-strategy` with the rename applied and the ce-review
fixes folded in. This runbook is the maintainer's local-verification
gate before pushing `main` to the remote.

Source material:

- Plan — [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](../plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md)
- Brainstorm — [`docs/brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md`](../brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md)
- Maintainer-facing UAT guide — [`docs/testing/hardware-uat.md`](../testing/hardware-uat.md)
- ce-review synthesis — `.context/compound-engineering/ce-review/20260520-094246-5776b847/synthesis.json`

The seven steps below are ordered so a regression surfaces as early as
possible: build → contract checks → hermetic UAT exercise → env-var
isolation → revision plumbing → pre-merge follow-ups. Each step has a
"Pass" criterion; if any fails, **stop and triage** before pushing.

---

## 0. Preconditions

```sh
cd /mnt/work/Workspace/oss-libs/llamadash-rs/llamastash

git status --short                          # working tree should be clean (TODO.md edit is expected)
git log --oneline -3                        # HEAD should be the squash-merge commit
git rev-parse HEAD                          # capture for the merge PR description
git remote -v                               # `origin` must point at github.com/llamastash/llamastash
```

**Pass:**

- HEAD is `feat(uat): reintroduce maintainer UAT command + nightly Metal CI lane`.
- `origin` is configured. If `git remote -v` is empty, add it:
  `git remote add origin git@github.com:llamastash/llamastash.git`
- The squash commit is pushed (or you're prepared to push at step 9). Workflows
  in `.github/workflows/uat-*` are only discoverable from `gh workflow list`
  and the Actions UI *after* they land on the remote's default branch.

---

## 1. Build, clippy, tests

```sh
cargo build --features uat
cargo clippy --all-targets --features uat,test-fixtures -- -D warnings
cargo test  --features uat,test-fixtures
```

**Pass:**

- Build clean.
- Clippy clean ("No issues found").
- 933 unit + 46 UAT-module + 6 UAT integration tests pass.

**Known pre-existing failures (not from this merge):**

- `init::benchmark::tests::verify_remote_accepts_fresher_snapshot_at_or_below_build_version`
- `init::benchmark::tests::verify_remote_accepts_release_min_version_against_prerelease_build`

Confirm they fail on `HEAD~1` too before treating them as pre-existing:

```sh
git stash -u && git checkout HEAD~1
cargo test --features test-fixtures -- init::benchmark::tests::verify_remote_accepts
git checkout - && git stash pop
```

If a UAT-related test fails — that is a real regression. Fix before
proceeding.

---

## 2. Release binary must NOT carry UAT

The plan's R4 contract: the shipped binary on crates.io and Homebrew
must never carry the `uat` subcommand. Verify the default-feature
build behaves correctly.

```sh
cargo build --release                              # default features, no `uat`
./target/release/llamastash --help | grep -i uat   # should be empty
./target/release/llamastash uat --backend nvidia
echo "exit=$?"
```

**Pass:**

- `--help` does NOT mention `uat`.
- `llamastash uat ...` exits non-zero with clap's "unrecognized subcommand `uat`".

Also audit the release workflow's build invocation:

```sh
grep -F "features uat" .github/workflows/release.yml
```

**Pass:** the only match is the audit comment line itself; no actual
`--features uat` appears in the build step.

---

## 3. UAT subcommand surface (feature-enabled build)

```sh
cargo build --release --features uat
./target/release/llamastash --help | grep uat       # hidden — empty
./target/release/llamastash uat --help
```

**Pass:**

- Top-level `--help` still does NOT mention `uat` (the `#[command(hide = true)]` gate).
- `llamastash uat --help` lists:
  - `--backend {nvidia,amd,apple_metal,vulkan}` (no `metal` alias)
  - `--mode {warm,cold}` with warm advertised as default
  - `--report-out <PATH>`

---

## 4. Hermetic UAT run — backend-mismatch pre-flight

The pre-flight mismatch path fails before any network or `llama-server`
call, so this is the cheapest end-to-end exercise of the lifecycle,
report serialization, classification field, and exit-code contract.
Pick a backend that is **virtually guaranteed not** to be present on
this host (use `nvidia` on a non-NVIDIA box, `apple_metal` on Linux,
etc.).

```sh
REPORT=/tmp/uat-report.json
./target/release/llamastash uat --backend nvidia --report-out "$REPORT"
echo "exit=$?"

jq '{verdict, failure_summary, host_warnings: .host.warnings, step_count: (.steps | length)}' "$REPORT"
```

**Pass:**

- Exit code `71` (`UNKNOWN` per the 0/1 contract).
- `verdict: "fail"`.
- `failure_summary.step: "doctor_preflight"`.
- `failure_summary.classification: "backend_mismatch"` (the ce-review classification enum).
- `failure_summary.exit_code: 10` (`PREFLIGHT_MISMATCH_CODE` synthetic).
- `steps.length == 5`. `steps[0].verdict == "fail"`; remaining four are `"skipped"`.
- `host.warnings[]` contains `"preserved tempdir at /tmp/llamastash-uat-..."`.

### 4a. stdout / TTY separation

```sh
./target/release/llamastash uat --backend nvidia --report-out - \
  > /tmp/uat-stdout.txt 2> /tmp/uat-stderr.txt

jq . /tmp/uat-stdout.txt | head           # pure parseable JSON
grep "UAT verdict" /tmp/uat-stderr.txt    # TTY summary lands here
```

**Pass:** stdout parses as JSON in one go; the TTY scan-down lives on
stderr. (This validates the ce-review M-P1-06 fix — before it, stdout
was interleaved and tests had to use a permissive fallback.)

### 4b. `--report-out` path validation

```sh
./target/release/llamastash uat --backend nvidia --report-out /tmp/ ; echo "dir: $?"
./target/release/llamastash uat --backend nvidia --report-out ""    ; echo "empty: $?"
./target/release/llamastash --quiet uat --backend nvidia --report-out -  ; echo "mutex: $?"
```

**Pass:** all three exit non-zero **before the lifecycle starts**, each
with a clear stderr message:

- "is a directory — pass a file path"
- "requires a non-empty path"
- "mutually exclusive with `--quiet`"

### 4c. Preserved-tempdir post-mortem

After step 4 fails, the tempdir under `/tmp/llamastash-uat-...` should
still exist (per the preserve-on-failure contract). Confirm:

```sh
TEMPDIR=$(jq -r '.host.warnings[] | select(startswith("preserved tempdir at"))' "$REPORT" \
  | sed 's/^preserved tempdir at //')
ls -la "$TEMPDIR"
ls "$TEMPDIR"/state "$TEMPDIR"/config "$TEMPDIR"/cache "$TEMPDIR"/runtime "$TEMPDIR"/hf
rm -rf "$TEMPDIR"                          # clean up after verification
```

**Pass:** all five sandbox subdirs exist (`state`, `config`, `cache`,
`runtime`, `hf`). The `config` slot is the ce-review M-P1-03 fix — it
must be present, otherwise `init --recommended` would clobber the
maintainer's real `~/.config/llamastash/config.yaml`.

---

## 5. Path-isolation env vars sandbox correctly

Verify the new `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` /
`LLAMASTASH_CACHE_DIR` overrides redirect every path, with no leakage
to the maintainer's real `~/.local/state/llamastash` or
`~/.config/llamastash`.

```sh
SANDBOX=$(mktemp -d)
STATE_BEFORE=$(ls -la ~/.local/state/llamastash 2>/dev/null | wc -l)
CONFIG_BEFORE=$(ls -la ~/.config/llamastash 2>/dev/null | wc -l)

LLAMASTASH_STATE_DIR="$SANDBOX/state" \
LLAMASTASH_CONFIG_DIR="$SANDBOX/config" \
LLAMASTASH_CACHE_DIR="$SANDBOX/cache" \
LLAMASTASH_SOCKET="$SANDBOX/daemon.sock" \
  ./target/release/llamastash status --json 2>&1 | head

STATE_AFTER=$(ls -la ~/.local/state/llamastash 2>/dev/null | wc -l)
CONFIG_AFTER=$(ls -la ~/.config/llamastash 2>/dev/null | wc -l)

echo "real state dir: $STATE_BEFORE → $STATE_AFTER"
echo "real config dir: $CONFIG_BEFORE → $CONFIG_AFTER"
ls -la "$SANDBOX"
rm -rf "$SANDBOX"
```

**Pass:** real `~/.local/state/llamastash` and `~/.config/llamastash`
unchanged; `$SANDBOX` contains the new state / config / cache files.

---

## 6. `init --revision` is plumbed and validated

```sh
./target/release/llamastash init --help | grep -A2 -- '--revision'
./target/release/llamastash init --revision ""        ; echo "empty: $?"
./target/release/llamastash init --revision "   "     ; echo "whitespace: $?"
./target/release/llamastash init --revision "with space"  ; echo "space-mid: $?"
```

**Pass:** flag is documented; all three malformed values rejected at
parse time with clap-style usage errors. The happy path (`init
--recommended --model owner/repo --revision <SHA>`) requires a real HF
pull — skip unless you want the bandwidth.

---

## 7. Nightly workflow shape audit

The `release-readiness` job in `.github/workflows/ci.yml` runs the
shape audit automatically on every PR/push to main — see steps
`uat — R4 audit`, `uat — synthetic exit codes documented`, and
`uat — nightly workflow carries ce-review fix lock-ins`. The manual
checks below mirror what CI enforces, useful when triaging a CI
failure or working on a branch the audit doesn't gate yet.

```sh
yq '.jobs."uat-metal-nightly".steps[] | {name, env}' .github/workflows/uat-metal-nightly.yml
grep -F "actions/cache@v4" -A4 .github/workflows/uat-metal-nightly.yml | head -20
grep -F "gh issue list" .github/workflows/uat-metal-nightly.yml
grep -F -- "--body-file" .github/workflows/uat-metal-nightly.yml
grep -F "brew info" .github/workflows/uat-metal-nightly.yml
yq '.on' .github/workflows/uat-metal-nightly.yml
```

**Pass:**

- Cache `path:` block includes `${{ steps.hf-cache.outputs.dir }}` (stable
  cache root, **not** `~/.cache/huggingface/hub` — that was the
  M-P2-07 fix).
- Cache `key:` folds in `steps.brew-formula.outputs.version` (M-P3-07).
- `gh issue list` passes `--limit 1` and `--search 'in:title "UAT Metal nightly status"'` (M-P3-05).
- `gh issue comment` uses `--body-file /tmp/uat-metal-comment.md` rather than inline `--body` interpolation (M-P2-12).
- `on:` block carries `schedule:`, `push.tags: ['v*']`, **and**
  `workflow_dispatch:`. The tag trigger means every release tag fires
  the Metal UAT alongside `release.yml`; maintainers see a Metal-UAT
  result attributed to the release SHA in the Actions tab without
  having to wait for the next nightly cron.

---

## 8. Pre-merge follow-ups (cannot be automated)

These two items must be completed **before the next push to `main`**.

### 8a. Falsifying spike: prove Metal is exposed to a headless macOS-14 runner

```sh
# Push a temporary branch so the workflow has somewhere to dispatch from
git push origin HEAD:refs/heads/test-uat-spike
gh workflow run uat-metal-spike.yml --ref test-uat-spike
gh run watch                                           # blocks until the run finishes
gh run view --log > /tmp/uat-spike.log
```

**If the spike passes:** delete the spike file and amend `main`:

```sh
git rm .github/workflows/uat-metal-spike.yml
git commit -m "chore(uat): remove spike workflow after metal-exposure confirmed (run #<id>)"
```

Paste the run URL into the merge PR description.

**If the spike fails:** swap to the fallback (Tier 3 collapses to a
macOS build verification lane):

```sh
git mv .github/workflows-fallback/macos-build-nightly.yml .github/workflows/
git rm .github/workflows/uat-metal-nightly.yml
git rm .github/workflows/uat-metal-spike.yml
# Update docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md §R3
# to honestly downgrade "nightly Metal lane" → "nightly macOS build lane".
git commit -m "chore(uat): collapse Tier 3 to macOS build (spike falsified metal exposure)"
```

Clean up the test branch either way:

```sh
git push origin --delete test-uat-spike
```

### 8b. Lock reference-model SHAs

The first warm-mode dry-run on the maintainer's real Apple Silicon box
captures the resolved HuggingFace SHAs that replace the
`<TBD-locked-on-first-dry-run>` sentinel.

On the Apple Silicon box:

```sh
cargo build --release --features uat
./target/release/llamastash uat --backend apple_metal --mode warm \
  --report-out /tmp/uat-warm.json

# Unlock warning should be present:
jq '.host.warnings[]' /tmp/uat-warm.json | grep -i "unlocked"

# Resolved SHA lives in the HF cache layout under HF_HOME:
HF_HOME_PATH=$(jq -r '.host.warnings[] | select(startswith("preserved tempdir"))' \
  /tmp/uat-warm.json | sed 's/^preserved tempdir at //')/hf
ls "$HF_HOME_PATH/hub/models--Qwen--Qwen2.5-0.5B-Instruct-GGUF/snapshots/"
# The directory name under snapshots/ IS the resolved commit SHA.
```

Replace both constants in `src/cli/uat/model.rs`:

```rust
pub const PRIMARY: ReferenceModel = ReferenceModel {
  ...
  commit_sha: "<resolved-SHA-here>",  // was PLACEHOLDER_SHA
  ...
};
pub const FALLBACK: ReferenceModel = ReferenceModel {
  ...
  commit_sha: "<resolved-fallback-SHA-here>",  // was PLACEHOLDER_SHA
  ...
};
```

Run the suite again to confirm `is_unlocked()` returns `false`:

```sh
cargo test --features uat -- model::tests::placeholder_sha_is_detected_by_is_unlocked
# This test should now FAIL — flip it to assert !is_unlocked, then commit.
```

Commit:

```sh
git commit -am "chore(uat): lock reference-model SHAs from first warm-mode dry-run

- PRIMARY:  Qwen/Qwen2.5-0.5B-Instruct-GGUF @ <SHA>
- FALLBACK: HuggingFaceTB/SmolLM2-360M-Instruct-GGUF @ <SHA>

Captured from <date> warm-mode UAT on <hardware-label>.
Closes the 'lock reference-model commit SHAs' item in TODO.md."
```

Strike the corresponding TODO.md item.

---

## 9. Pre-push final check

```sh
git log --oneline origin/main..HEAD               # should be exactly the squash commit
git status --short                                # working tree clean (or just TODO.md edits)
cargo test --features uat,test-fixtures           # green
gh workflow list                                  # confirm uat-metal-spike.yml is gone (8a)
grep -F "<TBD-locked-on-first-dry-run>" src/cli/uat/model.rs  # should be empty (8b)
```

**Pass all four**, then push:

```sh
git push origin main
```

---

## Rollback

If something surfaces post-push that warrants reverting the
reintroduction:

```sh
git revert --no-commit HEAD
git commit -m "Revert \"feat(uat): reintroduce maintainer UAT command\""
git push origin main
```

The squash commit reverts cleanly because the UAT files were
previously deleted on `main` — the revert just re-deletes them.
