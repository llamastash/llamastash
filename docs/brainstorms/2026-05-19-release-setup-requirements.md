---
date: 2026-05-19
topic: release-setup
---

# llamadash 0.2.0 Release Setup — Requirements

> Companion to [`docs/brainstorms/llamatui-requirements.md`](./llamatui-requirements.md) (v1 R1–R47) and [`docs/brainstorms/2026-05-18-init-wizard-requirements.md`](./2026-05-18-init-wizard-requirements.md) (v2 R48–R80). This document covers the release-engineering and distribution surface needed to ship the first public binaries: org migration, distribution channels, install script, and the marketing site at `llamadash.cli.rs`. IDs continue from the v2 doc.

## Problem Frame

llamadash v1 and v2 are feature-complete enough that the README already advertises distribution channels that do not exist yet: *"`cargo install llamadash`, a Homebrew tap, and pre-built release binaries land alongside the first tagged release."* The current state is the opposite — the only path to a working binary is `git clone && cargo install --path .`. The `Cargo.toml`'s `repository` / `homepage` / `documentation` URLs all point at `github.com/llamadash/llamadash`, which is **not** the org the project will live under (`llamadash-rs` per the existing kdash-rs convention). There is a `.github/workflows/release.yml` that builds tarballs on tag and uploads them to GH Releases — but nothing pushes to crates.io, no Homebrew tap exists, there is no install script, no website, and no CNAME for `llamadash.cli.rs`.

The goal of 0.2.0 is to close the gap between what the README promises and what actually works. A user landing on the marketing site or the README should pick *exactly one* of three commands and have a working binary on `$PATH` in under thirty seconds — no manual unzip, no quarantine workaround, no `cargo install --git` fallback.

This document is **release-engineering only**. The functional surface of 0.2.0 (v1 launcher per [`llamatui-requirements.md`](./llamatui-requirements.md) + v2 init wizard / doctor / pull per [`2026-05-18-init-wizard-requirements.md`](./2026-05-18-init-wizard-requirements.md)) is assumed already implemented and merged. Anything still open in those documents must land before tagging; this brainstorm does not re-litigate feature scope.

The release surface mirrors the **engineering** structure of [kdash-rs](https://github.com/kdash-rs) (org layout, brew tap naming, install-script shape, release flow) but **not** its website aesthetic. The site is its own visual brand: opencode.ai's section structure rendered in Catppuccin Macchiato — the same palette the TUI uses by default — so site and tool feel like one product.

Audience:
- **Primary:** a developer landing on `llamadash.cli.rs` from Hacker News / a colleague's share, who wants a binary on disk in one command.
- **Secondary:** an existing user upgrading from a `cargo install --path .` build to the published 0.2.0 tag.
- **Tertiary:** maintainers (currently one) — the release workflow must be hands-off after `git tag v0.2.0 && git push --tags`. Manual steps drift.

## Requirements

**Organization & Repository Migration**

- **R81.** The main source repo lives at `github.com/llamadash-rs/llamadash`. The local working tree (verified at doc-write time) has **no `origin` remote configured** and `github.com/llamadash/llamadash` does **not** exist on GitHub — the `Cargo.toml` URL pointing there was aspirational, not a reference to an existing repo. Setup is therefore a first-push, not a transfer: create `llamadash-rs/llamadash` (empty), `git remote add origin git@github.com:llamadash-rs/llamadash.git`, push `main`. `Cargo.toml` keys `repository`, `homepage`, `documentation` and every `README.md` / `AGENTS.md` / `CONTRIBUTING.md` reference update to the new URL in the **same commit** that adds the remote — so the first thing the new repo sees is consistent metadata, not the wrong-org URL. No dangling `llamadash/llamadash` URLs anywhere in the initial commit history visible after push.
- **R82.** Supporting repos under `llamadash-rs`:
  - `llamadash-rs/homebrew-llamadash` — Homebrew tap. Contains `Formula/llamadash.rb` and a `bump.yml` workflow that listens for `repository_dispatch` events from the main repo.
  - `llamadash-rs/llamadash-rs.github.io` — GitHub Pages org default repo. Source for the marketing site; deployed to `llamadash.cli.rs` via CNAME.
  - `llamadash-rs/.github` (optional, recommended) — org profile repo with a `profile/README.md` rendered on the org page. Short description, link to llamadash.cli.rs, link to llamadash repo.
  - **Not** separate repos for the install script (kept in main repo, see R91), or for AUR / Nix / Snap / Docker packaging (deferred to 0.2.x point releases, see R100–R103).

**Release Artifact Pipeline (extends existing `release.yml`)**

- **R83.** Build matrix stays `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`. No Windows binaries for 0.2.0. Windows users build from source via `cargo install llamadash`. Per-tarball SHA-256 sidecar file already produced by existing `release.yml`; this stays. **Release-asset inventory:** tarballs (×4) + per-tarball `.sha256` sidecars (×4) + aggregate `SHA256SUMS` (R84) + `install.sh` (copied from `scripts/install.sh` of the tagged commit) + `install.sh.sha256` (computed at release time). The install.sh + sidecar additions are what makes the R91 immutable-per-tag contract work. **Release atomicity:** the existing `release` job is modified to create the GitHub Release as `draft: true`, upload every asset listed above as draft assets, and only flip the release to published once all uploads succeed. Partially-uploaded releases are never visible to users — install.sh's "latest" query never resolves to an incomplete release.
- **R84.** Additional release-job step produces an aggregate `SHA256SUMS` file (one file, one line per tarball, `sha256  filename` format — same as Cargo's release pipeline) and uploads it as a GH Release asset. The install script (R90) verifies against `SHA256SUMS`, not the per-file sidecars, so a single signed object covers all platforms. No cosign signing for 0.2.0 (deferred — adds OIDC complexity without unblocking any user).
- **R85.** Release workflow extension: after the `release` job that publishes tarballs to GH Releases, a `publish` job runs in **two ordered phases**, both gated on the tag matching `v[0-9]+.[0-9]+.[0-9]+` (no pre-release tags trigger any of these). Phase ordering is load-bearing: every step except `cargo publish` is reversible (commit revert / formula re-bump); `cargo publish` is **irrevocable** (yank is metadata-only, not removal). Put the irreversible step last so a failure in a reversible step never leaves the channels permanently asymmetric.
  - **Phase 1 — reversible dispatches, in parallel:**
    - `gh api repos/llamadash-rs/homebrew-llamadash/dispatches -F event_type=bump -F 'client_payload[version]=<version>' -F 'client_payload[sha256_linux_x64]=<hash>' …` — fires one `repository_dispatch` event at the tap repo carrying version + all four SHA-256 hashes. The tap's `bump.yml` workflow rewrites `Formula/llamadash.rb` and commits.
    - `gh api repos/llamadash-rs/llamadash-rs.github.io/dispatches -F event_type=bump …` — fires a parallel event at the site repo. The site's `bump.yml` handler does two things on receipt: (1) updates UI version labels (the hero install-command tabs and the "latest version" line in the footer), and (2) **regenerates `public/install.sh` from the new GH Release asset with mandatory SHA-256 verification** — fetches `https://github.com/llamadash-rs/llamadash/releases/download/<version>/install.sh` and `install.sh.sha256`, runs `shasum -a 256 -c install.sh.sha256` (or `sha256sum -c`), aborts the workflow on mismatch, commits both files only on verification success along with the provenance header described in R91. The verification step is the security contract.
    - Both dispatches use `gh run watch` (or equivalent polling against the receiving repo's most recent workflow run for this `event_type`) to confirm the downstream workflows reach `completed: success` before phase 1 reports success. Either dispatch failing aborts phase 2.
  - **Phase 2 — irreversible publish, only on phase-1 success:**
    - `cargo publish --token $CRATES_IO_TOKEN` to push to crates.io. Token stored as a repo secret. If phase 2 fails, phase 1's effects are easily revertable: the tap repo's bump commit is reverted, the site's version-string commit is reverted; the GitHub Release stays as a draft pending retry. No state is unrecoverable.
- **R86.** A 0.2.0 tag is rejected by CI if any of the above downstream steps would fail predictably. Pre-tag guards run in `ci.yml` (not in `release.yml`, so failures appear before the irreversible publish): (a) `cargo publish --dry-run` verifies the package builds and the manifest is publishable — this does **not** check name availability; (b) an explicit crates.io API check (`curl -fsSL https://crates.io/api/v1/crates/<name>`) verifies the crate slot is either **unclaimed** (404 from the API) or **owned by a current maintainer of the `llamadash-rs` GitHub org** (the maintainer list is checked via `gh api orgs/llamadash-rs/members`, then cross-referenced against the crate's `owners` array from crates.io). Anything else fails the guard and triggers R87's `llamadash-cli` fallback; (c) a grep guard fails the build if any `llamadash/llamadash` URL survives in `Cargo.toml`, `README.md`, `AGENTS.md`, `CONTRIBUTING.md`; (d) CHANGELOG.md must have a non-empty section header matching the tag version (the maintainer adds this entry in the same commit that bumps `Cargo.toml`'s version — the only manual pre-tag step the workflow expects).

**crates.io Publish**

- **R87.** Pre-tag verification: confirm `llamadash` is available on crates.io. If reserved or taken by an unrelated project, fall back to `llamadash-cli` and propagate the rename across: `Cargo.toml` `name`, `[[bin]]` stays `name = "llamadash"` (binary keeps the short name), `README.md` install instructions, `install.sh`, the brew formula, and the website install tabs. Status of the crate name **is unverified at the time of this doc** — first action of the planning phase.
- **R88.** Cargo.toml metadata cleanup, done in the same commit as R81:
  - `repository` / `homepage` / `documentation` → `https://github.com/llamadash-rs/llamadash`.
  - Add `categories = ["command-line-utilities"]` (and consider `["development-tools::cargo-plugins"]` — no, llamadash is not a cargo plugin, skip).
  - Verify `keywords` count ≤ 5 (currently 5: `llamadash`, `llama-cpp`, `llm`, `tui`, `gguf`) — already at the limit, no changes needed.
  - Confirm `exclude` keeps the published crate under crates.io's 10 MB uncompressed limit. Run `cargo package --list` and `cargo package --no-verify` once before tagging; add `tests/fixtures/` and `docs/benchmark_sources/` to `exclude` if size exceeds 8 MB (1 MB headroom).
  - `readme = "README.md"` already present, keep as-is — crates.io renders it.

**Homebrew Tap**

- **R89.** Tap repo layout at `llamadash-rs/homebrew-llamadash`:
  ```
  Formula/llamadash.rb
  .github/workflows/bump.yml
  README.md
  ```
  Initial `llamadash.rb` ships a **binary formula** (downloads the prebuilt tarball from GH Releases, verifies SHA-256, installs the `llamadash` binary into `Cellar`), **not** a source build. Source-build formulas need `rust` as a build dependency and re-compile on every install — for a TUI built with `lto = "thin"`, that's a 90s+ compile users don't need to wait for. Cellar layout: `bin/llamadash`, `share/man/man1/llamadash.1` (if a manpage exists at tag time; otherwise omitted from 0.2.0), `share/doc/llamadash/{LICENSE,README.md}`.
- **R90.** Tap users install with: `brew install llamadash-rs/llamadash/llamadash` (tap + formula). Short form `brew tap llamadash-rs/llamadash && brew install llamadash` also works. The tap repo's name (`homebrew-llamadash`) is Homebrew's mandatory prefix convention, and the user-visible tap path strips the `homebrew-` prefix per Homebrew's rules. The formula supports `brew install --HEAD` (builds from `main` via cargo) for users who want pre-release builds — implemented as a `head "..." do … end` block in the same formula.

**Install Script**

- **R91.** Single shell script at `scripts/install.sh` in the main repo. Source of truth. **The script that users pipe into `sh` is the highest-trust surface in the release, so the served copy is pinned to the per-tag immutable GitHub Release asset — not freely editable by the site repo.** On every release the script is uploaded as a GitHub Release asset alongside the tarballs (`install.sh` and `install.sh.sha256`, per R83). The website's `https://llamadash.cli.rs/install.sh` is served from `public/install.sh` in the site repo, but that file is **never hand-edited and never produced from `scripts/install.sh` directly** — it is regenerated by the site repo's `bump.yml` (R85 site dispatch handler) by fetching the GH Release asset and verifying its SHA-256 against the uploaded `install.sh.sha256` before commit. The verification step is the trust boundary: an attacker would need to compromise both the GH Release upload (gated by org-admin release permissions) AND the site repo's `bump.yml` workflow file to inject content. The committed `public/install.sh` carries an HTML-style provenance comment header (e.g. `# Source: github.com/llamadash-rs/llamadash@v0.2.0/scripts/install.sh, SHA-256 verified at bump time`) so a grep over the site repo's history shows which release each copy came from. Users invoke either:
  - `curl -fsSL https://llamadash.cli.rs/install.sh | sh` (preferred, short — content is the verified copy of the latest release's `install.sh`).
  - `curl -fsSL https://github.com/llamadash-rs/llamadash/releases/download/v0.2.0/install.sh | sh` (works without site, useful for issue templates and version pinning).
  - `curl -fsSL https://raw.githubusercontent.com/llamadash-rs/llamadash/main/scripts/install.sh | sh` (uses the HEAD of `main` — useful for testing in PR branches; not recommended for end users since it floats with the main branch).
- **R92.** Script contract:
  - Runs on Bash 3.2+ (macOS ships Bash 3 by default — newer Bash features like `[[`, `==`, arrays are fine; do not use `mapfile`, `readarray`, or `${var,,}`/`${var,^}` case conversion).
  - Detects OS (`linux` / `darwin`) and arch (`x86_64` / `aarch64` / `arm64`); maps to one of the four R83 target triples. Refuses with a clear error on `windows`, `mingw`, `cygwin`, or unknown arch.
  - Downloads the matching `llamadash-${version}-${target}.tar.gz` from GH Releases. Version defaults to `latest` (resolved via `https://api.github.com/repos/llamadash-rs/llamadash/releases/latest`); honors `LLAMADASH_VERSION=v0.2.0 sh install.sh` for pinning.
  - Verifies the downloaded tarball against the aggregate `SHA256SUMS` file (R84). Aborts on mismatch with a non-zero exit code (use exit code `2` for checksum failure, distinct from `1` generic).
  - Extracts to `$LLAMADASH_INSTALL_DIR` (default `$HOME/.local/bin`). Creates the directory if missing.
  - Prints a one-line success message naming the install path and the resolved version, plus a hint if `$LLAMADASH_INSTALL_DIR` isn't on the user's `$PATH`.
  - Flags: `--version <vX.Y.Z>`, `--prefix <dir>`, `--quiet`, `-h | --help`. Environment-variable equivalents (`LLAMADASH_VERSION`, `LLAMADASH_INSTALL_DIR`) for non-interactive / piped invocations.
  - Idempotent: re-running with the same version is a no-op after the binary already matches the published SHA.
  - **No `PATH` mutation.** The script does not edit `~/.bashrc`, `~/.zshrc`, `~/.profile`, or any other shell startup file. Users put `~/.local/bin` on `$PATH` themselves; the script prints the one-line hint if it's missing.
- **R93.** Script test surface: `scripts/install.sh` has a shellcheck pass in CI (`shellcheck -s sh scripts/install.sh` — `-s sh` forces POSIX-ish lint mode despite the Bash 3 target) and a Bats integration test (`scripts/install.test.bats`) that runs against a mock GH Releases endpoint and asserts: download + checksum-verify + extract for each of the four targets, refusal on Windows / unknown arch, refusal on checksum mismatch, idempotence on re-run.

**Website (`llamadash.cli.rs`)**

- **R94.** Site source repo: `llamadash-rs/llamadash-rs.github.io`. Layout:
  ```
  src/                  # Astro project source
    pages/index.astro
    components/*.astro
    styles/global.css   # Catppuccin Macchiato tokens
    layouts/Base.astro
  public/
    install.sh          # SHA-256-verified copy of latest GH Release asset (R91)
    install.sh.sha256   # sidecar from the release; lets users self-verify
    demo.cast           # asciinema cast (committed binary)
    favicon.svg
    og-image.png
    screenshots/*.png
  astro.config.mjs
  package.json
  CNAME                 # contains exactly: llamadash.cli.rs
  .github/workflows/
    deploy.yml          # build on push, deploy to gh-pages
    bump.yml            # repository_dispatch handler for R85
  ```
  The `public/screenshots/` directory is intentionally **not** part of the 0.2.0 site asset list — the single asciinema cast (R97) is the only product demo for launch. Add screenshots only if a future requirement introduces a screenshot section.
- **R95.** Tech stack: **Astro 4 + Tailwind CSS 3 + asciinema-player**. Astro is the best fit (static-first, islands for the few interactive components, GH-Pages-deployable out of the box). Tailwind is the styling layer with a custom Catppuccin Macchiato preset committed at `src/styles/catppuccin-macchiato.ts`. **Full palette** (canonical hex values from the upstream Catppuccin spec — every site color must come from this list):
  - **Surfaces:** `crust #181926`, `mantle #1e2030`, `base #24273a`, `surface0 #363a4f`, `surface1 #494d64`, `surface2 #5b6078`.
  - **Text:** `text #cad3f5`, `subtext1 #b8c0e0`, `subtext0 #a5adcb`.
  - **Overlays:** `overlay2 #939ab7`, `overlay1 #8087a2`, `overlay0 #6e738d`.
  - **Accents:** `rosewater #f4dbd6`, `flamingo #f0c6c6`, `pink #f5bde6`, `mauve #c6a0f6` (primary CTA), `red #ed8796` (errors / destructive), `maroon #ee99a0`, `peach #f5a97f`, `yellow #eed49f`, `green #a6da95` (success / checkmarks), `teal #8bd5ca`, `sky #91d7e3`, `sapphire #7dc4e4`, `blue #8aadf4` (links), `lavender #b7bdf8`.

  asciinema-player handles the hero demo; loaded only on the index page (Astro island). Asciinema's terminal-theme tokens are mapped to Macchiato accents so the embedded cast renders in the same palette as the surrounding page.
- **R96.** Page structure mirrors opencode.ai's section order but with llamadash content and Catppuccin styling:
  1. **Top nav** — logo + `docs` + `GitHub` + a `[ Install ↗ ]` button anchored to the install section.
  2. **Hero** — tagline ("Run local llama.cpp models — from your terminal or your agent's CLI.") + secondary line, multi-tab install command (default tab `curl`, others `brew`, `cargo`), CTA to GitHub. The tagline is deliberately precise: llamadash is a *launcher* for llama.cpp with an agent-friendly CLI, not an agent itself. The wording closes off the misread that visitors arrive expecting an agent product. Asciinema cast embedded below the install block, auto-looping, fall-back static screenshot for `prefers-reduced-motion: reduce`.
  3. **Features grid** — 6 checkmark items: GGUF discovery across HF / Ollama / LM Studio caches, keyboard-driven launcher, multi-model supervisor, smoke test (chat / embed / rerank), agent-ready CLI with stable JSON, init wizard + doctor diagnostic. Each feature line links to the relevant README anchor — no on-site docs duplication.
  4. **Social proof** — two stat cards at 0.2.0: GitHub stars and contributors (both live via API or build-time fetch). A third card is added in 0.2.x once crates.io monthly downloads become a meaningful number (> 100/month). The "GGUFs in the recommender's snapshot" stat — considered in an earlier draft — was rejected because the snapshot is curated benchmark data, not models llamadash hosts, and would form a wrong mental model. If a third card is wanted before downloads are meaningful, use "supported GGUF architectures: llama, qwen2, qwen3, mistral, gemma, phi" (honest, self-explanatory). Cards omit any stat whose live value is < 5 (no "1 star" embarrassment at launch); when only one card has data, the row collapses to a single centered card rather than rendering empty slots.
  5. **"Why local-first" pitch** — short prose section adapted from README's "Why" block, positioning vs Ollama / LM Studio (transparency, agent-friendly, GGUF-native). One paragraph, not a comparison table.
  6. **FAQ accordion** — 5–7 questions, each answer 2–4 sentences with a deep-link to the README/docs for the long form. Pre-decided question set and framing direction:
     - *Is llamadash a chat UI?* — No. It's a launcher for `llama-server` that exposes the standard OpenAI-compatible HTTP endpoints; chat happens in your own client (or the right-pane smoke-test tab for quick checks).
     - *Why not just use Ollama?* — Transparency + agent-friendliness + GGUF-native intelligence; llamadash doesn't abstract llama.cpp away. Link to README's "Why" block.
     - *Where do my GGUFs end up?* — In the canonical HuggingFace cache (`~/.cache/huggingface/hub`), so other HF-aware tools see the same files.
     - *Does it work without a GPU?* — Yes; CPU-only mode is fully supported and `init` recommends a CPU-class starter model when no GPU is detected.
     - *How does the daemon avoid orphaning models?* — Three-factor adoption sweep (PID alive + recorded port answering + `/v1/models` matches recorded path) on restart. Link to AGENTS.md "Process survival."
     - *Is it Windows-compatible?* — llamadash is Linux + macOS first. Windows via WSL works today; building from source with `cargo install llamadash` works on native Windows but isn't pre-built or smoke-tested. A native Windows binary is on the 0.3.x roadmap — follow the project's GitHub Releases for the announcement. (Framed as a strategic focus, not an unflattering gap; the entry is included rather than omitted because visitors who care will find out anyway, and controlling the framing beats not.)
  7. **Footer** — three columns: Project (GitHub, Releases, Changelog), Community (Discussions, Issues), Brand (Apache logo + Catppuccin attribution + license + maintainer handle).
- **R97.** Hero demo content: the `demo.cast` asciinema recording shows, in order, `llamadash list` (5–10 GGUFs found), `llamadash init --yes` (compressed to skip the slow download), the TUI launcher opening with the discovered-models list, a `Ctrl-Enter` launching `qwen2.5-coder-7b`, a one-line `curl ... /v1/chat/completions` returning a streaming response. Total runtime ≤ 40s. Recorded once, committed; no auto-regeneration on releases. A static PNG of the same scene serves as the `og:image` and the reduced-motion fallback.
- **R98.** `llamadash.cli.rs` CNAME setup: the `cli.rs` domain is community-managed; subdomains are provisioned by opening a PR against the community zone-config repo (same mechanism used for `kdash.cli.rs`). The PR adds a CNAME record `llamadash IN CNAME llamadash-rs.github.io.` to the zone file. The site can be built and deployed to `llamadash-rs.github.io` (which works without the CNAME) before the PR merges — the CNAME flip is the *last* step that swaps the marketing-visible URL. The site repo's `CNAME` file (containing `llamadash.cli.rs`) should be committed in the same PR that announces 0.2.0 publicly, not before — GitHub Pages serves the cleaner `*.github.io` URL until the DNS resolves, so committing CNAME early without DNS just yields a broken hostname error.
- **R99.** Deploy workflow: `deploy.yml` runs `astro build` on every push to `main` of the site repo, then publishes `dist/` to GitHub Pages via the official `actions/deploy-pages` + `actions/upload-pages-artifact` action pair. GitHub Pages's "Build and deployment" source for the repo must be set to **GitHub Actions** (not "Deploy from a branch") in the repo Pages settings — this is a one-time manual setup, not part of the workflow YAML. The `CNAME` file in `dist/` (copied from `public/CNAME`) routes the domain. Build time target: ≤ 30s on the standard GH runner (a static Astro site with no SSR easily clears this).

**Deferred to 0.2.x Point Releases**

- **R100.** AUR package (`llamadash-bin`) — separate repo `llamadash-rs/llamadash-aur` holds the canonical `PKGBUILD`, mirrored to the AUR via the maintainer's AUR push key. Bumped via R85-style dispatch. Defer to **0.2.1**; the bottleneck is committing to the AUR account ownership story, not the technical work.
- **R101.** Nix flake — `flake.nix` lives in the main repo (not a separate overlay; flake.nix at the root is the modern idiom). Provides `packages.default` and `apps.default`. Defer to **0.2.2**; gates a nixpkgs PR once the flake is exercised.
- **R102.** Snap or Flatpak — pick exactly one: **Snap** is preferred for cost reasons (snapcraft.yaml + Launchpad build is simpler infrastructure than Flathub's review queue + sandbox justification). Defer to **0.2.3**; revisit Flatpak only if a contributor steps up to maintain the Flathub manifest.
- **R103.** Docker image — two viable cuts: `llamadash-rs/llamadash:cli` (≈30 MB distroless image with just the binary; useful for agent / CI driving llamadash over IPC) and `llamadash-rs/llamadash:server` (CPU-only image with llama-server bundled; demo + smoke-test convenience, not a production target since GPU passthrough across Docker on macOS is broken and on Linux requires nvidia-container-toolkit). Defer to **0.2.4** or skip until a concrete use case lands in issues.

## Success Criteria

A user on a fresh macOS or Linux machine runs **exactly one** of these three commands and has a working `llamadash` on `$PATH` within thirty seconds, with no manual unzip, no `xattr` workaround, no PATH editing:

```bash
brew install llamadash-rs/llamadash/llamadash
cargo install llamadash
curl -fsSL https://llamadash.cli.rs/install.sh | sh
```

A maintainer running `git tag v0.2.0 && git push --tags` triggers a fully automated release: GH Releases publishes the four tarballs + `SHA256SUMS`, crates.io receives the published crate, the Homebrew tap formula is auto-bumped and committed, the site's install-script copy + tab version strings auto-refresh. **Zero manual post-tag steps.** Any failure in this chain is visible in the GitHub Actions tab of the originating repo and aborts the rest of the chain — no half-released state where crates.io has 0.2.0 but the tap is still on 0.1.x.

`llamadash.cli.rs` resolves to a Catppuccin-Macchiato-themed single-page site whose three install commands match the working commands above, with an asciinema cast in the hero showing the TUI in under 40 seconds, and whose `Cargo.toml`-linked README, AGENTS.md, and Cargo metadata all point at `github.com/llamadash-rs/llamadash` — no surviving references to the old `github.com/llamadash/llamadash` URL anywhere in the published 0.2.0 artifact.

## Scope Boundaries

These are deliberate omissions, not gaps:

- **No Windows native binaries.** Source-only via `cargo install` on Windows. Scoop, winget, and Windows-specific signing are post-0.2.0.
- **No macOS code signing or notarization.** Per the explicit analysis: cargo + brew + curl|sh paths all avoid Gatekeeper quarantine; browser-download is the only friction case and the README already documents the `xattr -d` workaround. The Apple Developer cert ($99/yr) doesn't unblock any of the three primary install channels.
- **No cosign / Sigstore signatures on tarballs.** SHA-256 sums published in `SHA256SUMS` are the integrity contract for 0.2.0. Cosign keyless OIDC is a 0.3.x consideration if supply-chain auditing demand surfaces.
- **No AUR, Nix, Snap, Flatpak, or Docker for the 0.2.0 tag.** Each gets its own follow-up brainstorm; landing schedule R100–R103.
- **No on-site documentation duplication.** README and `docs/usage.md` stay canonical. The site links out; it does not render them. (Revisit in 0.3.x if SEO data shows the README isn't being found.)
- **No newsletter, analytics, or third-party JS.** GitHub's repo-traffic counters are sufficient signal for 0.2.0. No Plausible / GoatCounter / GA. (Revisit if the site needs conversion data later.)
- **No comments / Disqus / utterances on the site.** Discussion happens in GitHub Discussions.
- **No automatic release-note generation beyond `softprops/action-gh-release`'s default `generate_release_notes: true`.** CHANGELOG.md is the canonical changelog; the GH-generated PR-list note is supplementary.

## Key Decisions

1. **The kdash-rs analogy is engineering-only.** Repo layout (`llamadash-rs/llamadash`, `llamadash-rs/homebrew-llamadash`, `llamadash-rs/llamadash-rs.github.io`), brew-tap conventions, install-script shape, and release-workflow patterns all mirror kdash-rs. The **website does not** — it's modeled on opencode.ai's structure, painted in Catppuccin Macchiato.
2. **First tag is 0.2.0, foundation channels only.** Cargo, brew tap, install script, website. AUR / Nix / Snap / Docker (R100–R103) land in 0.2.x point releases. Holding 0.2.0 for the long tail is a worse outcome than shipping the foundation now and iterating.
3. **No macOS signing, no Windows binaries for 0.2.0** — each independently justified above. Both decisions can be revisited in 0.3.x without breaking any 0.2.x user.
4. **Install script source of truth is in the main repo at `scripts/install.sh`; the *served* copy at `llamadash.cli.rs/install.sh` is a content-verified mirror of the per-tag GitHub Release asset.** The site never edits the script by hand — it fetches from GH Releases and SHA-256-verifies before commit (R91 + R85). This shape was chosen over a Cloudflare 302 (more infra) and over dropping the short URL (worse UX). The trust boundary is the bump-workflow's verification step: tamper requires compromising both the GH Release upload AND the workflow file, which is enforceable via branch protection on the site repo's `.github/workflows/bump.yml` plus required reviews on any workflow change.
5. **Brew tap is end-to-end automated.** No manual `Formula/llamadash.rb` edits. Manual tap maintenance is where formulas drift; we automate it on day one. Initial formula is binary-only (R89); source-build is a `--HEAD` branch, not the default.
6. **Crate name verification is the first planning-phase action.** If `llamadash` is unavailable on crates.io, the entire downstream chain (install command in README + site + brew tap + install.sh) has to use `llamadash-cli` instead. Catching this before tag is cheap; catching it after tag is a rollback.
7. **CNAME + DNS for `llamadash.cli.rs` is a hard prerequisite, not part of the site build.** The site repo committing a `CNAME` file with `llamadash.cli.rs` is necessary but not sufficient — DNS on `cli.rs` has to point at `llamadash-rs.github.io` first. Outstanding (see below).

## Dependencies / Assumptions

- **`llamadash` crate name availability on crates.io.** **Verified available** at doc-write time via `curl -fsSL https://crates.io/api/v1/crates/llamadash` → HTTP 404 (no crate by that name registered). R87's `llamadash-cli` fallback is therefore unused for 0.2.0, but kept in the doc as a guard in case someone else publishes between now and the first tag (R86's pre-tag guard catches that race).
- **GitHub org `llamadash-rs` exists and the maintainer has admin rights.** Confirmed by the user (per the brainstorm prompt: *"I have created https://github.com/llamadash-rs org"*).
- **`cli.rs` subdomain process.** Resolved: subdomains are provisioned via PR to the community-managed `cli.rs` zone-config repo (same mechanism the maintainer used for `kdash.cli.rs`). The PR adds a `CNAME` record for `llamadash → llamadash-rs.github.io.` and merges on the cli.rs maintainer's cadence. This is a single planning-phase action with no carrying cost.
- **A `CRATES_IO_TOKEN` and a `GH_BUMP_TOKEN` (a PAT with `contents:write` + `actions:write` on the tap and site repos) need to be added as secrets** on the main repo before 0.2.0 — without them, R85's downstream dispatch jobs no-op.
- **Existing `release.yml`'s artifact format already matches what install.sh + the brew formula expect.** Verified by reading `.github/workflows/release.yml`: tarballs are named `llamadash-${version}-${target}.tar.gz`, sidecar SHA-256 is `${name}.sha256`. No format change needed beyond the additive `SHA256SUMS` aggregate of R84.

## Outstanding Questions

### Resolve Before Planning

All three prerequisites that originally lived here have been resolved at brainstorm time — see "Resolved at Doc-Write Time" below.

### Resolved at Doc-Write Time

- ✅ **`llamadash` crate name availability on crates.io.** Verified available via `crates.io/api/v1/crates/llamadash` → 404. R87's `llamadash-cli` fallback stays in the doc as a race-guard for the small window between brainstorm and tag.
- ✅ **Old-org migration mechanics.** No migration needed: `github.com/llamadash/llamadash` does not exist; the local working tree has no `origin` remote. The "migration" is a first push to a freshly-created `github.com/llamadash-rs/llamadash`, executed as R81's first commit. R81 reflects this.
- ✅ **`cli.rs` subdomain process.** Confirmed: PR to the community-managed `cli.rs` zone-config repo (same path the maintainer walked for `kdash.cli.rs`). R98 reflects the workflow and ordering.

### Deferred to Planning

- Exact `SHA256SUMS` file format — Cargo's `sha256` vs OpenSSL `dgst -sha256` output format differ in whitespace. Pin once during planning.
- Brew formula language version (`Formula::DSL` vs the older block syntax) — pin against `brew style` lint output, not by guessing.
- Astro version and Tailwind config exact shape — pin during the website implementation unit; the requirements doc shouldn't over-specify.
- Specific FAQ wording — drafted during site build, reviewed by the maintainer before deploy.
- Whether to include a `--uninstall` flag in `install.sh` — leans no for 0.2.0 (users `rm $HOME/.local/bin/llamadash`), but plannable cheaply.

### Deferred to Post-0.2.0

- AUR PKGBUILD shape and AUR account ownership (R100).
- Nix flake structure and nixpkgs PR strategy (R101).
- Snap vs Flatpak final pick and snapcraft.yaml details (R102).
- Docker image base + GPU strategy (R103).
- Cosign / Sigstore signing of release artifacts.
- Whether to add a Scoop bucket or winget manifest once Windows binaries land.

## Next Steps

1. Verify crate-name availability and `cli.rs` subdomain process (the two unverified prerequisites).
2. Run `/ce:plan` against this requirements document to produce the implementation plan — likely split into Implementation Units: (Unit A) repo migration + Cargo.toml cleanup, (Unit B) release-workflow extensions for crates.io + tap dispatch + site dispatch, (Unit C) Homebrew tap repo bootstrap, (Unit D) install.sh + bats test, (Unit E) website scaffolding + Catppuccin theme, (Unit F) website content + asciinema cast + FAQ.
3. Execute Units A–F. Tag `v0.2.0` once all green.
4. Open follow-up brainstorms for R100 (AUR), R101 (Nix), R102 (Snap), R103 (Docker) in priority order against observed user demand post-launch.
