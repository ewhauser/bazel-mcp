.PHONY: setup-hooks build test test-unit test-integration test-bazel-matrix \
	setup-oss-corpus test-token-integration run check bench bench-save \
	bench-compare bench-token bench-token-live bench-agentic \
	bench-agentic-smoke bench-agentic-live bench-agentic-presentation \
	bench-agentic-control-smoke publish-token-benchmark \
	generate-bep-goldens fuzz-setup fuzz-list fuzz-smoke \
	fuzz-run harden-release check-release-security \
	mcp-conformance test-claude-code test-claude-code-live generate-sbom

ARGS ?= --help
FUZZ_TARGET ?= bep_framing
FUZZ_ARGS ?= -max_total_time=60
NIX_DEVELOP ?= nix --extra-experimental-features 'nix-command flakes' develop --command
OSS_PROJECT ?= abseil-cpp
TOKEN_ENCODING ?= o200k_base
TOKEN_SAMPLES ?= 5
LIVE_AGENT_ARGS ?=
AGENTIC_SAMPLES ?= 5
AGENTIC_MODEL ?= gpt-5.6-luna
AGENTIC_REASONING_EFFORT ?= xhigh
AGENTIC_PRESENTATION_SAMPLES ?= 5
AGENTIC_ARGS ?=
BENCHMARK_RUN ?= $(shell cat .cache/benchmarks/$(OSS_PROJECT)/LATEST)
BENCHMARK_ARTIFACT_DIR ?= .cache/published-benchmarks/$(OSS_PROJECT)/$(BENCHMARK_RUN)

setup-hooks:
	git config core.hooksPath .githooks

build:
	cargo build --workspace

test:
	cargo test --workspace --all-features

test-unit:
	cargo test --workspace --all-features --lib

test-integration:
	cargo test --workspace --all-features --tests

test-bazel-matrix:
	$(NIX_DEVELOP) ./scripts/test-bazel-matrix.sh

setup-oss-corpus:
	$(NIX_DEVELOP) ./scripts/benchmarks/setup-oss-corpus.sh $(OSS_PROJECT)

test-token-integration:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-token-integration.sh \
		--project $(OSS_PROJECT) --encoding $(TOKEN_ENCODING) \
		--samples $(TOKEN_SAMPLES) --assert-gates

run:
	cargo run -p bazel-mcp-server -- $(ARGS)

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	$(NIX_DEVELOP) cargo shear

bench:
	cargo bench -p bazel-mcp-benchmark

bench-save:
	cargo bench -p bazel-mcp-benchmark -- --save-baseline main

bench-compare:
	cargo bench -p bazel-mcp-benchmark -- --baseline main

bench-token: test-token-integration

bench-token-live:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-token-integration.sh \
		--project $(OSS_PROJECT) --encoding $(TOKEN_ENCODING) \
		--samples $(TOKEN_SAMPLES) --live-agent $(LIVE_AGENT_ARGS)

bench-agentic: bench-agentic-live

bench-agentic-smoke:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-agentic-benchmark.sh \
		--project $(OSS_PROJECT) --samples 1 \
		--model $(AGENTIC_MODEL) --reasoning-effort $(AGENTIC_REASONING_EFFORT) \
		$(AGENTIC_ARGS)

bench-agentic-live:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-agentic-benchmark.sh \
		--project $(OSS_PROJECT) --samples $(AGENTIC_SAMPLES) \
		--model $(AGENTIC_MODEL) --reasoning-effort $(AGENTIC_REASONING_EFFORT) \
		$(AGENTIC_ARGS)

bench-agentic-presentation:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-agentic-benchmark.sh \
		--project $(OSS_PROJECT) --samples $(AGENTIC_PRESENTATION_SAMPLES) \
		--model $(AGENTIC_MODEL) --reasoning-effort $(AGENTIC_REASONING_EFFORT) \
		--task fix-noisy-normalizer --task fix-fanout-macro \
		--adapter shell-default --adapter bazel-mcp $(AGENTIC_ARGS)

bench-agentic-control-smoke:
	$(NIX_DEVELOP) ./scripts/benchmarks/run-agentic-benchmark.sh \
		--project $(OSS_PROJECT) --samples 1 \
		--model $(AGENTIC_MODEL) --reasoning-effort $(AGENTIC_REASONING_EFFORT) \
		--task fix-noisy-normalizer --task fix-fanout-macro \
		--adapter shell-default --adapter shell-mcp-loaded \
		--adapter bazel-mcp $(AGENTIC_ARGS)

publish-token-benchmark:
	python3 ./scripts/benchmarks/publish-token-report.py \
		.cache/benchmarks/$(OSS_PROJECT)/$(BENCHMARK_RUN) \
		$(BENCHMARK_ARTIFACT_DIR) --replace

generate-bep-goldens:
	./scripts/fixtures/generate-bep-goldens.sh
	UPDATE_GOLDENS=1 cargo test -p bazel-mcp-reducer --test golden

fuzz-setup:
	./scripts/fuzz-init.sh

fuzz-list: fuzz-setup
	cd fuzz && cargo +nightly fuzz list

fuzz-smoke: fuzz-setup
	cd fuzz && cargo +nightly fuzz run $(FUZZ_TARGET) -- -runs=1

fuzz-run: fuzz-setup
	cd fuzz && cargo +nightly fuzz run $(FUZZ_TARGET) -- $(FUZZ_ARGS)

harden-release:
	python3 ./scripts/check-release-security.py

check-release-security: harden-release

mcp-conformance:
	cargo build -p bazel-mcp-server --bin bazel-mcp
	python3 ./scripts/test-mcp-conformance.py

test-claude-code:
	cargo build -p bazel-mcp-server --bin bazel-mcp
	python3 ./scripts/compat/test-claude-code.py

test-claude-code-live:
	cargo build -p bazel-mcp-server --bin bazel-mcp
	python3 ./scripts/compat/test-claude-code.py --live

generate-sbom:
	./scripts/generate-release-sbom.sh
