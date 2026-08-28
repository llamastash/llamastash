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

AUDIT_DIR ?= target/audit
UAT_MODE ?= warm
UAT_REPORT_DIR ?= /tmp
UAT_EXTRA ?=
UAT_CMD = cargo run --features uat -- uat

## Run the full maintainer audit suite (lint, tests, release build,
## duplicate deps, advisories, unsafe scan, Tarpaulin XML). Requires
## `cargo-audit`, `cargo-geiger`, and `cargo-tarpaulin` to be installed.
audit:
	@for tool in cargo-audit cargo-geiger cargo-tarpaulin; do \
		command -v $$tool >/dev/null 2>&1 || { \
			printf '%s\n' "missing $$tool; install it before running 'make audit'"; \
			exit 1; \
		}; \
	done
	@mkdir -p "$(AUDIT_DIR)/tarpaulin"
	@$(MAKE) test
	@cargo build --release
	@bytes=$$(wc -c < "target/release/llamastash"); \
		printf '%s\n' "$$bytes" > "$(AUDIT_DIR)/release-binary-bytes.txt"; \
		printf '%s\n' "release binary bytes: $$bytes"
	@cargo tree --duplicates > "$(AUDIT_DIR)/cargo-tree-duplicates.txt"
	@cargo audit --json > "$(AUDIT_DIR)/cargo-audit.json"
	@status=0; \
		cargo geiger --all-targets --features test-fixtures > "$(AUDIT_DIR)/cargo-geiger.txt" 2>&1 || status=$$?; \
		printf '%s\n' "$$status" > "$(AUDIT_DIR)/cargo-geiger.exit-code.txt"; \
		if [ $$status -ne 0 ]; then \
			printf '%s\n' "cargo geiger exited $$status; captured output in $(AUDIT_DIR)/cargo-geiger.txt"; \
		fi
	@cargo tarpaulin --features test-fixtures --engine llvm --out Xml --output-dir "$(AUDIT_DIR)/tarpaulin"
	@printf '%s\n' "audit artifacts written to $(AUDIT_DIR)"
	@$(MAKE) audit-summary

## Print a one-screen summary of the latest audit artifacts under
## `$(AUDIT_DIR)`. Run `make audit` first.
audit-summary:
	@python3 scripts/audit_summary.py "$(AUDIT_DIR)"

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
## HuggingFace calls — without it, local runs commonly diverge from
## the CI-published snapshot in the `source` and `score` fields. The
## script enforces the partial-source-failure policy: any source
## missing -> non-zero exit, snapshot unchanged.
snapshot: .venv/bin/python
	@if [ "$(SKIP_SNAPSHOT)" = "1" ]; then \
		echo "snapshot: skipped (SKIP_SNAPSHOT=1)"; \
	else \
		if [ -z "$$HF_TOKEN" ]; then \
			echo "snapshot: HF_TOKEN unset — local output will likely differ from CI; set HF_TOKEN to a read-only token to match." >&2; \
		fi; \
		.venv/bin/python scripts/regenerate-benchmark-snapshot.py; \
	fi

## Suite B end-to-end benchmark runner (maintainer-only — see
## docs/benchmarks/methodology.md). Honours env vars LLAMASTASH_BENCH_*;
## any forwarded args go straight to the orchestrator (e.g. `--dry-run`).
bench-end-to-end: .venv/bin/python
	@scripts/bench/end_to_end/run.sh $(filter-out $@,$(MAKECMDGOALS))

## Suite A overhead-band runner: `llamastash start` vs raw `llama-server`
## byte-equal argv check + two-tier delta thresholds.
bench-overhead: .venv/bin/python
	@scripts/bench/overhead/run.sh $(filter-out $@,$(MAKECMDGOALS))

## Suite C proxy-overhead runner: chat_turn alternating between the direct
## llama-server port and the proxy on 127.0.0.1:11434 against one model.
## Forwards args (e.g. `--model <gguf>`, `--measured 15`) to the orchestrator.
bench-proxy: .venv/bin/python
	@scripts/bench/proxy/run.sh $(filter-out $@,$(MAKECMDGOALS))

## Run the bench harness's own pytest suite (unit tests for schema,
## drivers, workloads, render). Real benchmarks are launched via the
## `bench-*` targets above; this only exercises the harness code.
bench-test: .venv/bin/python
	@.venv/bin/python -m pytest scripts/bench/ -v

## Pivot the existing bench JSONs under docs/benchmarks/runs/ into a
## per-model summary table (markdown). Auto-detects engine variants
## from `host_id` suffixes. Forwards extra args (e.g. `--host`,
## `--json`, `--engine-map`) to the underlying module.
bench-table: .venv/bin/python
	@.venv/bin/python -m scripts.bench.end_to_end.table $(filter-out $@,$(MAKECMDGOALS))

## Maintainer UAT shortcuts. Override:
##   UAT_MODE=warm|cold
##   UAT_REPORT_DIR=/tmp/uat-reports
##   UAT_EXTRA='--some-extra-uat-flag' (appended after the per-target defaults)
## For the Vulkan lane, set either UAT_VULKAN_SERVER=/path/to/llama-server
## or the standard LLAMASTASH_LLAMA_SERVER env var.
##
## Windows: these recipes use POSIX shell syntax and `make` is not in-box.
## Each target's PowerShell-friendly equivalent is shown above the rule —
## paste those into PowerShell directly. Warm mode on Windows AMD reports
## zero VRAM-used (no `rocm-smi.exe`); use `--mode cold` or `nvidia` /
## `cpu_only` lanes there.

## PowerShell equivalent (Linux/macOS bash also works):
##   cargo run --features uat -- uat --host-backend amd --mode warm --report-out uat-amd-warm.json
uat-amd:
	@mkdir -p "$(UAT_REPORT_DIR)"
	@$(UAT_CMD) --host-backend amd --mode "$(UAT_MODE)" --report-out "$(UAT_REPORT_DIR)/uat-amd-$(UAT_MODE).json" $(UAT_EXTRA)

## PowerShell equivalent:
##   $env:LLAMASTASH_LLAMA_SERVER='C:\path\to\llama-server.exe'
##   cargo run --features uat -- uat --host-backend amd --runtime-backend vulkan --mode warm --report-out uat-amd-vulkan-warm.json
uat-amd-vulkan:
	@mkdir -p "$(UAT_REPORT_DIR)"
	@if [ -z "$(UAT_VULKAN_SERVER)" ] && [ -z "$$LLAMASTASH_LLAMA_SERVER" ]; then \
		printf '%s\n' "set UAT_VULKAN_SERVER=/path/to/build-vulkan/bin/llama-server or export LLAMASTASH_LLAMA_SERVER"; \
		exit 1; \
	fi
	@LLAMASTASH_LLAMA_SERVER="$${LLAMASTASH_LLAMA_SERVER:-$(UAT_VULKAN_SERVER)}" \
		$(UAT_CMD) --host-backend amd --runtime-backend vulkan --mode "$(UAT_MODE)" \
		--report-out "$(UAT_REPORT_DIR)/uat-amd-vulkan-$(UAT_MODE).json" $(UAT_EXTRA)

## PowerShell equivalent:
##   $env:LLAMASTASH_LLAMA_SERVER='C:\path\to\llama-server.exe'
##   cargo run --features uat -- uat --host-backend nvidia --runtime-backend vulkan --mode warm --report-out uat-nvidia-vulkan-warm.json
uat-nvidia-vulkan:
	@mkdir -p "$(UAT_REPORT_DIR)"
	@if [ -z "$(UAT_VULKAN_SERVER)" ] && [ -z "$$LLAMASTASH_LLAMA_SERVER" ]; then \
		printf '%s\n' "set UAT_VULKAN_SERVER=/path/to/build-vulkan/bin/llama-server or export LLAMASTASH_LLAMA_SERVER"; \
		exit 1; \
	fi
	@LLAMASTASH_LLAMA_SERVER="$${LLAMASTASH_LLAMA_SERVER:-$(UAT_VULKAN_SERVER)}" \
		$(UAT_CMD) --host-backend nvidia --runtime-backend vulkan --mode "$(UAT_MODE)" \
		--report-out "$(UAT_REPORT_DIR)/uat-nvidia-vulkan-$(UAT_MODE).json" $(UAT_EXTRA)

## PowerShell equivalent (warm mode needs `nvidia-smi.exe` on PATH):
##   cargo run --features uat -- uat --host-backend nvidia --mode warm --report-out uat-nvidia-warm.json
uat-nvidia:
	@mkdir -p "$(UAT_REPORT_DIR)"
	@$(UAT_CMD) --host-backend nvidia --mode "$(UAT_MODE)" --report-out "$(UAT_REPORT_DIR)/uat-nvidia-$(UAT_MODE).json" $(UAT_EXTRA)

## PowerShell equivalent (Apple-only — Windows hosts have no Metal):
##   cargo run --features uat -- uat --host-backend apple_metal --mode warm --report-out uat-apple-metal-warm.json
uat-apple-metal:
	@mkdir -p "$(UAT_REPORT_DIR)"
	@$(UAT_CMD) --host-backend apple_metal --mode "$(UAT_MODE)" --report-out "$(UAT_REPORT_DIR)/uat-apple-metal-$(UAT_MODE).json" $(UAT_EXTRA)

## PowerShell equivalent (the canonical Windows lane — the only one that
## doesn't require a vendor `*-smi` tool for warm-mode sampling):
##   cargo run --features uat -- uat --host-backend cpu_only --mode cold --report-out uat-cpu-only-cold.json
uat-cpu-only:
	@mkdir -p "$(UAT_REPORT_DIR)"
	@$(UAT_CMD) --host-backend cpu_only --mode "$(UAT_MODE)" --report-out "$(UAT_REPORT_DIR)/uat-cpu-only-$(UAT_MODE).json" $(UAT_EXTRA)

## Run llamastash against the local daemon (auto-spawns one if missing).
## Extra goals are forwarded to cargo as subcommand args, so:
##   make run                     -> cargo run --                (launches TUI)
##   make run list                -> cargo run -- list           (CLI subcommand)
##   make run start qwen2.5-coder -> cargo run -- start qwen2.5-coder
## The trailing `%:` catch-all turns the forwarded goals into no-op targets
## so make doesn't error with "No rule to make target 'list'" afterward.
run:
	@cargo fmt --all && make lint && CARGO_INCREMENTAL=1 cargo run -- $(filter-out $@, $(MAKECMDGOALS))

%:
	@:

## Render a single TUI frame at a handful of representative terminal sizes.
## Useful for eyeballing the adaptive layout (split breakpoints, info-row /
## logo cutoffs) without resizing your real terminal. Override with SIZES=...
SIZES ?= 80x20 100x30 120x30 139x30 140x30 160x40 200x50
render:
	@for s in $(SIZES); do \
		printf '\n\033[1m── %s ──\033[0m\n' "$$s"; \
		cargo run --quiet -- --render --render-size $$s; \
	done

## Run clippy with the project's deny-warnings policy (test-fixtures gates the integration surface)
lint:
	@CARGO_INCREMENTAL=0 cargo clippy --all-targets --features test-fixtures -- -D warnings

## Check rustdoc — fails on broken / private-cross-visibility intra-doc
## links so the `cargo doc` CI job stays green. Mirrors CI exactly:
## RUSTDOCFLAGS (RUSTFLAGS reaches rustc, not rustdoc, so it gated nothing
## and let two broken links through), --no-deps, --all-features.
doc:
	@RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

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
	@git tag -a ${V} -m "Release ${V}" && git push origin ${V} --no-verify

## Delete tag — usage: `make delete-tag V=v0.1.0`
delete-tag:
	@git tag -d ${V} && git push --delete origin ${V} --no-verify
