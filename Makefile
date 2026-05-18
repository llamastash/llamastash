VERSION  := latest
IMG_NAME := deepu105/llamadash
IMAGE    := ${IMG_NAME}:${VERSION}

default: run

## Run all tests (matches AGENTS.md CI parity — needs the test-fixtures feature)
test:
	@make lint && cargo test --features test-fixtures

## Run all tests with coverage — `cargo install cargo-tarpaulin`
test-cov:
	@cargo tarpaulin --features test-fixtures

## Build the release binary for the current os-arch
build:
	@make test && cargo build --release

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
