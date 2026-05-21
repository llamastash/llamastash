VERSION  := latest
IMG_NAME := deepu105/llamastash
IMAGE    := ${IMG_NAME}:${VERSION}

default: run

## Run all tests (matches AGENTS.md CI parity — needs the test-fixtures feature)
test:
	@make lint && cargo test --features test-fixtures

## Regenerate golden test fixtures
test-golden:
	@UPDATE_GOLDEN=1 cargo test --features test-fixtures --test tui_e2e_render_test dashboard_golden_render_matches_fixture

## Run all tests with coverage — `cargo install cargo-tarpaulin`
test-cov:
	@cargo tarpaulin --features test-fixtures

## Build the release binary for the current os-arch.
## Regenerates data/benchmark-snapshot.json from live sources first so
## every release ships with a fresh catalog (Unit 7 of plan 2026-05-20-001).
## Set SKIP_SNAPSHOT=1 to bypass the regen step for offline / smoke builds.
build: snapshot
	@make test && cargo build --release

## Local Python venv for the snapshot regen scripts. Created on demand;
## `.gitignore`d. Prefers `uv` (fast); falls back to stdlib `venv` + `pip`.
.venv/bin/python:
	@if command -v uv >/dev/null 2>&1; then \
		uv venv --python 3.12 .venv && \
		uv pip install --python .venv/bin/python -r scripts/requirements.txt; \
	else \
		python3 -m venv .venv && \
		.venv/bin/pip install --upgrade pip && \
		.venv/bin/pip install -r scripts/requirements.txt; \
	fi

## Regenerate data/benchmark-snapshot.json from live HF Hub + benchmark
## adapters. Honours HF_TOKEN to clear the 429 floor on whichllm's
## HuggingFace calls. The script enforces the partial-source-failure
## policy: any source missing -> non-zero exit, snapshot unchanged.
snapshot: .venv/bin/python
	@if [ "$(SKIP_SNAPSHOT)" = "1" ]; then \
		echo "snapshot: skipped (SKIP_SNAPSHOT=1)"; \
	else \
		.venv/bin/python scripts/regenerate-benchmark-snapshot.py; \
	fi

## Run the TUI against the local daemon (auto-spawns one if missing)
run:
	@cargo fmt --all && make lint && CARGO_INCREMENTAL=1 cargo run -- $(filter-out $@, $(MAKECMDGOALS))

## Run clippy with the project's deny-warnings policy (test-fixtures gates the integration surface)
lint:
	@CARGO_INCREMENTAL=0 cargo clippy --all-targets --features test-fixtures -- -D warnings

## Fix lint
lint-fix:
	@cargo fix --allow-staged

## Run format
fmt:
	@cargo fmt --all

## Check format (CI parity — fails if anything is misformatted)
fmt-check:
	@cargo fmt --all -- --check

## Build a Docker image (v2 — Dockerfile not yet checked in)
docker:
	@DOCKER_BUILDKIT=1 docker build --progress=plain --rm -t ${IMAGE} .

## Run the Docker image locally
docker-run:
	@docker run --rm -it ${IMAGE}

## Analyse for unsafe usage — `cargo install cargo-geiger`
analyse:
	@cargo geiger

## Release tag — usage: `make release V=v0.1.0`
release:
	@git tag -a ${V} -m "Release ${V}" && git push origin ${V}

## Delete tag — usage: `make delete-tag V=v0.1.0`
delete-tag:
	@git tag -d ${V} && git push --delete origin ${V}
