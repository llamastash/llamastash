# Release 0.0.1 bootstrap — agent-native runbook

This runbook lists the GitHub org-admin actions that have to happen **before** the first `v0.0.1` tag can produce a working release. Every step has a `gh` CLI primitive a human or an agent can run from the terminal; the web-UI fallback is documented inline for steps that benefit from it.

Companion plan: [`docs/plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md`](../plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md) (filename retains the original `0.2.0` slug for historical record; the actual first release is `0.0.1` per the WIP-versioning decision documented in CHANGELOG).

## Prerequisites

```sh
gh auth status       # must show org membership in llamastash-rs
gh --version         # 2.40+ recommended for the api commands below
```

- Owner of the `llamastash-rs` GitHub organization (or invited as Owner before starting).
- A crates.io account capable of receiving verification email for the publish-update token mint (Step 2). **This is the one step with no CLI primitive** — crates.io has no token-mint API.
- A user account capable of minting a fine-grained PAT under the `llamastash-rs` org. **Also CLI-unavailable** — GitHub has no PAT-mint API.

---

## Step 1 — Create the four repos

| Repo                                    | Purpose                                 | Visibility | Initial content                       |
| --------------------------------------- | --------------------------------------- | ---------- | ------------------------------------- |
| `llamastash-rs/llamastash`                | Main source repo (this one)             | public     | Push `main` from the local checkout   |
| `llamastash-rs/homebrew-llamastash`       | Homebrew tap                            | public     | Push from `../homebrew-llamastash/`    |
| `llamastash-rs/llamastash-rs.github.io`   | Marketing site → `llamastash.cli.rs`     | public     | Push from `../llamastash-rs.github.io/` |
| `llamastash-rs/.github`                  | Org profile (optional, low priority)    | public     | Minimal `profile/README.md`           |

```sh
# Main repo — assumes you're inside the existing checkout.
gh repo create llamastash-rs/llamastash \
  --public \
  --description "Fast, keyboard-driven TUI for launching local llama.cpp models" \
  --source=. --remote=origin --push

# Homebrew tap
cd ../homebrew-llamastash
git init -b main && git add -A && git commit -m "chore: bootstrap tap"
gh repo create llamastash-rs/homebrew-llamastash \
  --public \
  --description "Homebrew tap for llamastash" \
  --source=. --remote=origin --push

# Marketing site
cd ../llamastash-rs.github.io
git init -b main && git add -A && git commit -m "chore: bootstrap site"
gh repo create llamastash-rs/llamastash-rs.github.io \
  --public \
  --description "Marketing site for llamastash — llamastash.cli.rs" \
  --source=. --remote=origin --push
```

The org-profile repo is optional. If you want one:

```sh
mkdir -p ../.github/profile && cd ../.github
git init -b main
printf '# llamastash-rs\n\nA fast TUI for local llama.cpp models.\n' > profile/README.md
git add -A && git commit -m "chore: bootstrap org profile"
gh repo create llamastash-rs/.github --public --source=. --remote=origin --push
```

Verify each push:

```sh
for repo in llamastash homebrew-llamastash llamastash-rs.github.io; do
  gh repo view "llamastash-rs/$repo" --json name,defaultBranchRef \
    --jq '"\(.name) | default=\(.defaultBranchRef.name)"'
done
```

---

## Step 2 — Add secrets

Two secrets live on `llamastash-rs/llamastash`. Both come from mint flows GitHub and crates.io expose only via the web — set them once, then push via `gh secret set`.

### `CRATES_IO_TOKEN`

Scope: **`publish-update` on crate `llamastash` only.** Do not use a global account token.

1. Open https://crates.io and sign in with the GitHub account that will own the crate.
2. Reserve the crate name by publishing a placeholder version (or use a verification-only token first).
3. **Account → API tokens → New token.** Scope: `publish-update` on crate `llamastash` (per-crate scopes shipped mid-2024).
4. Copy the token.

```sh
gh secret set CRATES_IO_TOKEN --repo llamastash-rs/llamastash --body '<paste-here>'
# or pipe from a password manager:
op read 'op://Personal/crates-io/llamastash-publish-token' \
  | gh secret set CRATES_IO_TOKEN --repo llamastash-rs/llamastash
```

### `GH_BUMP_TOKEN`

Fine-grained PAT used by `release.yml`'s `publish-homebrew` and `publish-site` jobs to clone + commit + push directly into the tap and site repos.

1. **Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token.**
2. **Resource owner:** `llamastash-rs`.
3. **Repository access:** Only select repositories → `homebrew-llamastash`, `llamastash-rs.github.io`.
4. **Permissions:** `Contents: Read and write` only. Do **not** grant `Actions: Read and write` — a leaked token with that scope can fire any workflow in the tap or site repo (including a hostile one a maintainer pushes to a feature branch). The release pipeline does not need it.
5. **Expiration:** 1 year. Set a calendar reminder. Rotation steps are at the bottom of this file.

```sh
gh secret set GH_BUMP_TOKEN --repo llamastash-rs/llamastash --body '<paste-here>'

# Confirm both secrets are set (values are not shown):
gh secret list --repo llamastash-rs/llamastash
```

---

## Step 3 — Configure GitHub Pages on the site repo

Source must be **GitHub Actions** (not "Deploy from a branch"). The deploy workflow calls `actions/configure-pages@v5` with `enablement: true`, which auto-enables Pages on first deploy — but doing it explicitly first removes the 404 window.

```sh
gh api -X POST -H 'Accept: application/vnd.github+json' \
  /repos/llamastash-rs/llamastash-rs.github.io/pages \
  -f 'build_type=workflow' \
  -f 'source[branch]=main' \
  -f 'source[path]=/'
```

Verify:

```sh
gh api /repos/llamastash-rs/llamastash-rs.github.io/pages \
  --jq '"build_type=\(.build_type) | https=\(.https_enforced)"'
```

Custom domain is left empty for now — the `cli.rs` zone-config PR sets it later.

---

## Step 4 — Branch protection (recommended)

Pre-bootstrap, you'll be pushing direct commits. Apply protection after the first real tag completes; until then the rules will block your own bootstrap pushes.

```sh
# Main repo — require PR review, status checks pass, conversation resolution.
gh api -X PUT -H 'Accept: application/vnd.github+json' \
  /repos/llamastash-rs/llamastash/branches/main/protection \
  -F 'required_pull_request_reviews[required_approving_review_count]=1' \
  -F 'required_pull_request_reviews[dismiss_stale_reviews]=true' \
  -F 'required_status_checks[strict]=true' \
  -F 'required_status_checks[contexts][]=ci' \
  -F 'enforce_admins=false' \
  -F 'required_conversation_resolution=true' \
  -F 'allow_force_pushes=false' \
  -F 'allow_deletions=false'

# Tap + site — required PR review on any workflow change. The bump.yml
# fallback workflows + deploy.yml are the trust boundary for GH_BUMP_TOKEN,
# so workflow file edits must be human-reviewed.
for repo in homebrew-llamastash llamastash-rs.github.io; do
  gh api -X PUT -H 'Accept: application/vnd.github+json' \
    "/repos/llamastash-rs/$repo/branches/main/protection" \
    -F 'required_pull_request_reviews[required_approving_review_count]=1' \
    -F 'allow_force_pushes=false' \
    -F 'allow_deletions=false'
done
```

Add a CODEOWNERS file in each downstream repo so workflow changes require maintainer review:

```sh
for repo in homebrew-llamastash llamastash-rs.github.io; do
  cd "../$repo"
  printf '* @deepu105\n.github/workflows/* @deepu105\n' > CODEOWNERS
  git add CODEOWNERS && git commit -m "chore: add CODEOWNERS" && git push
  cd -
done
```

---

## Step 5 — Dry-run the release pipeline with `v0.0.0-rc1`

Pre-release tags (`vX.Y.Z-<suffix>`) exercise the upstream half of the pipeline only: `create-release` → `build` → `publish-shasums`. The `publish-homebrew`, `publish-site`, and `publish-cargo` jobs all gate on `is_prerelease == 'false'` and are skipped. **This is intentional** — it means the dry run never writes to the tap, site, or crates.io, so cleanup after the dry run is just deleting the tag and the test release.

```sh
# From a throwaway branch in the main repo:
git checkout -b release-dry-run
git tag v0.0.0-rc1
git push origin release-dry-run v0.0.0-rc1

# Watch the run live (blocks until completion):
gh run list --repo llamastash-rs/llamastash --workflow=release.yml --limit 1 \
  --json databaseId --jq '.[0].databaseId' \
  | xargs -I {} gh run watch --repo llamastash-rs/llamastash --exit-status {}

# Verify what landed:
gh release view v0.0.0-rc1 --repo llamastash-rs/llamastash \
  --json assets --jq '.assets[].name | "  " + .'
# Expect: 4 tarballs, 4 .sha256 sidecars, SHA256SUMS, install.sh, install.sh.sha256.

# Verify nothing was written downstream (publish-homebrew / -site / -cargo
# should all be skipped):
gh api /repos/llamastash-rs/homebrew-llamastash/commits/main \
  --jq '.commit.message'   # must NOT mention v0.0.0-rc1
gh api /repos/llamastash-rs/llamastash-rs.github.io/commits/main \
  --jq '.commit.message'   # must NOT mention v0.0.0-rc1
```

Cleanup:

```sh
gh release delete v0.0.0-rc1 --repo llamastash-rs/llamastash --yes --cleanup-tag
git push origin --delete release-dry-run
git branch -D release-dry-run
```

---

## Step 6 — Real release

Only after Step 5 succeeds:

```sh
# 1. Confirm Cargo.toml + CHANGELOG agree (the release-readiness CI job and
#    create-release both verify this — these are local belt-and-suspenders).
grep '^version' Cargo.toml                # version = "0.0.1"
grep -E '^## \[0\.0\.1\]' CHANGELOG.md    # ## [0.0.1] — <date>

# 2. Tag and push.
git tag v0.0.1
git push origin v0.0.1

# 3. Watch the full pipeline (10-15 min on cold caches).
gh run list --repo llamastash-rs/llamastash --workflow=release.yml --limit 1 \
  --json databaseId --jq '.[0].databaseId' \
  | xargs -I {} gh run watch --repo llamastash-rs/llamastash --exit-status {}

# 4. Verify each channel:
gh release view v0.0.1 --repo llamastash-rs/llamastash --web
gh api /repos/llamastash-rs/homebrew-llamastash/commits/main \
  --jq '.commit.message'    # mentions v0.0.1
gh api /repos/llamastash-rs/llamastash-rs.github.io/commits/main \
  --jq '.commit.message'    # mentions v0.0.1

# 5. Fresh-box smoke (Ubuntu container + macOS VM):
docker run --rm -it ubuntu:24.04 bash -c '
  apt-get update && apt-get install -y curl
  curl -fsSL https://llamastash.cli.rs/install.sh | sh
  ~/.local/bin/llamastash --version
'
# macOS smoke: cargo install llamastash, brew install llamastash-rs/llamastash/llamastash,
# curl -fsSL https://llamastash.cli.rs/install.sh | sh — all three.
```

If anything in the post-`publish-cargo` chain fails (rare):

```sh
# Re-run a single failed job from the Actions UI, or via:
gh run rerun --repo llamastash-rs/llamastash --failed <run-id>
```

---

## Token rotation cadence

| Secret             | Trigger to rotate                              | Default cadence | Rotation primitive |
| ------------------ | ---------------------------------------------- | --------------- | ------------------ |
| `CRATES_IO_TOKEN`  | First publish, suspected leak, annually        | annual          | crates.io UI → `gh secret set CRATES_IO_TOKEN --repo llamastash-rs/llamastash --body '<new>'` |
| `GH_BUMP_TOKEN`    | PAT expiry, leak, annually                     | annual          | GitHub PAT UI → `gh secret set GH_BUMP_TOKEN --repo llamastash-rs/llamastash --body '<new>'` |

The long-term answer (tracked in `TODO.md`) is migrating to a scoped GitHub App with OIDC instead of PATs — eliminates rotation entirely. Out of scope for 0.0.1.

To monitor PAT expiry without waiting for the first failed release, set a calendar reminder for ~30 days before the configured expiry. GitHub does not yet expose token expiration via API for fine-grained PATs.
