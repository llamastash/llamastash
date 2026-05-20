# Release 0.2.0 bootstrap — org-admin runbook

This runbook lists the manual GitHub org-admin actions that have to happen **before** the first `v0.2.0` tag can produce a working release. They cannot be scripted from inside the repo — they require the org owner to click through GitHub's web UI.

The companion plan is [`docs/plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md`](../plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md); this runbook is the operational checklist for the parts that plan defers to humans.

## Prerequisites

- Owner of the `llamadash-rs` GitHub organization (or invited as Owner before starting).
- `gh` CLI authenticated with that account (`gh auth status` shows org membership).
- A crates.io account; ability to receive verification email at the address tied to that account.

---

## Step 1 — Create the four repos

| Repo                                | Purpose                                  | Visibility | Initial content                       |
| ----------------------------------- | ---------------------------------------- | ---------- | ------------------------------------- |
| `llamadash-rs/llamadash`            | Main source repo (this one)              | public     | First push of local `main` from here  |
| `llamadash-rs/homebrew-llamadash`   | Homebrew tap                             | public     | Push from `../homebrew-llamadash/`    |
| `llamadash-rs/llamadash-rs.github.io` | Marketing site → `llamadash.cli.rs`    | public     | Push from `../llamadash-rs.github.io/` |
| `llamadash-rs/.github`              | Org profile (optional, low priority)     | public     | Minimal `profile/README.md`           |

Create each via the web UI (`https://github.com/organizations/llamadash-rs/repositories/new`) with:

- **No** README / LICENSE / .gitignore checkboxes (we push the existing tree).
- Default branch name: `main`.
- Description: copy the short one-liner from each repo's README.

Then point a remote at the existing local checkout and push:

```sh
# Main repo
cd /path/to/llamadash
git remote add origin git@github.com:llamadash-rs/llamadash.git
git push -u origin main

# Homebrew tap
cd ../homebrew-llamadash
git init && git add -A && git commit -m "chore: bootstrap tap"
git branch -M main
git remote add origin git@github.com:llamadash-rs/homebrew-llamadash.git
git push -u origin main

# Marketing site
cd ../llamadash-rs.github.io
git init && git add -A && git commit -m "chore: bootstrap site"
git branch -M main
git remote add origin git@github.com:llamadash-rs/llamadash-rs.github.io.git
git push -u origin main
```

Verify after each push: the repo loads in a browser with the expected README + file tree.

---

## Step 2 — Add secrets

### Main repo (`llamadash-rs/llamadash`)

**Settings → Secrets and variables → Actions → New repository secret**, add two:

#### `CRATES_IO_TOKEN`

Scope: **only the `llamadash` crate** — do **not** use a global account token.

1. Sign in at https://crates.io with the GitHub account that will own the crate.
2. Reserve the crate name (publishing v0.0.0 is one option; another is to use a verification-only token first).
3. Account → API tokens → New token. **Scope: `publish-update` on crate `llamadash`** (per-crate scope was added in mid-2024).
4. Copy the token; paste into the GitHub secret.

If the per-crate scope isn't yet available (cargo registry policy churns), use a token scoped to `publish-update` only and rotate immediately after the first publish.

#### `GH_BUMP_TOKEN`

Fine-grained personal access token used by `release.yml`'s `publish-homebrew` and `publish-site` jobs to clone + commit + push directly into the tap and site repos (kdash pattern, mirrors `cd.yml` in `github.com/kdash-rs/kdash`).

1. Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token.
2. **Resource owner:** `llamadash-rs`.
3. **Repository access:** Only select repositories → `homebrew-llamadash`, `llamadash-rs.github.io`.
4. **Permissions:**
   - `Contents`: Read and write
   - (no others)
   - `Actions: Read and write` is **not** required under the kdash pattern (no `repository_dispatch` triggers), but it's harmless if granted. Add it only if you ever flip back to dispatch-style automation.
5. Expiration: 1 year (set a calendar reminder to rotate; mark in `docs/runbooks/secret-rotation.md` if/when that runbook exists).
6. Copy the token; paste into the GitHub secret.

---

## Step 3 — Configure GitHub Pages on the site repo

`llamadash-rs/llamadash-rs.github.io` → **Settings → Pages**:

- **Source:** GitHub Actions (not "Deploy from a branch").
- Custom domain: leave empty for now — Unit 7's CNAME step sets it after the `cli.rs` PR resolves.

The first push to `main` triggers `.github/workflows/deploy.yml`, which calls `actions/configure-pages@v5` with `enablement: true`. That action programmatically enables Pages with the Actions source, but the value of clicking the setting yourself is that you see the toggle is on before the workflow runs. If you don't, the first deploy may 404 for a few minutes while GH propagates the source change.

---

## Step 4 — Branch protection (optional but recommended)

For each repo, **Settings → Branches → Add rule** on `main`:

- Require a pull request before merging (1 review).
- Specifically for `homebrew-llamadash` and `llamadash-rs.github.io`: also restrict pushes that change `.github/workflows/bump.yml`. The bump workflow is the trust boundary for `GH_BUMP_TOKEN`; pinning its review is the operational mitigation if the token ever leaks.

Pre-Unit-7, you'll be pushing direct commits to bootstrap each repo. After Unit 7's first real tag completes, flip the rules on. Until then, the rules will block your own bootstrap pushes.

---

## Step 5 — Test the chain end-to-end before tagging `v0.2.0`

The plan calls this the `v0.0.0-rc1` test cycle. With everything bootstrapped:

1. On a throwaway branch in the main repo, push a tag like `v0.0.0-rc1`.
2. Watch `.github/workflows/release.yml`:
   - `create-release` job extracts the version + flags the tag as `is_prerelease=true`.
   - `build` matrix produces four tarballs + sidecars; each job uploads to the GH Release.
   - `publish-shasums` aggregates SHA256SUMS and uploads `install.sh` + `install.sh.sha256`.
   - `publish-homebrew` runs `deployment/homebrew/packager.py`, clones the tap, commits the new formula.
   - `publish-site` fetches install.sh from the release, re-verifies the SHA-256, clones the site, commits the mirror + version bump.
   - `publish-cargo` is **skipped** because `is_prerelease=true` — no test publish hits crates.io.
3. Verify each downstream artifact exists: the GH Release page, the new tap commit, the new site commit (which triggers the site's `deploy.yml`).
4. Delete the test tag (`git push --delete origin v0.0.0-rc1`) and the throwaway branch.
5. On the published release page, delete the test release.

---

## Step 6 — Real release

Only after Step 5 succeeds:

1. Confirm `Cargo.toml` is at `0.2.0` (Unit 7 commit), CHANGELOG promotes `[Unreleased]` → `[0.2.0]`, README mentions live install commands.
2. `git tag v0.2.0 && git push --tags`.
3. Monitor the GH Actions tab through the full ~10 minute pipeline.
4. Verify the live URLs:
   - `cargo install llamadash` works on a fresh box.
   - `brew install llamadash-rs/llamadash/llamadash` works on a fresh macOS box.
   - `curl -fsSL https://llamadash.cli.rs/install.sh | sh` works (or the `github.com/.../releases/.../install.sh` URL if cli.rs DNS hasn't landed yet — see Unit 7).
5. Record transcripts of each fresh-box install for the GH Release notes.

---

## Token rotation cadence

| Secret             | Rotation trigger                          | Rotation cadence default |
| ------------------ | ----------------------------------------- | ------------------------ |
| `CRATES_IO_TOKEN`  | First publish, suspected leak, annually   | annual                   |
| `GH_BUMP_TOKEN`    | Expiry (set at creation time), leak, annually | annual                |

The long-term answer (deferred to 0.3.x) is a GitHub App with OIDC instead of PATs. Track in the project's TODO/follow-up doc; not in scope for 0.2.0.
