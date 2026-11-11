.PHONY: build test lint fmt check bench clean install-deps doc
.PHONY: build-llvm test-llvm check-llvm

# Default target
all: check test

# Build all crates
build:
	cargo build --workspace

# Published artifacts (GitHub Actions release.yml): fat LTO, slowest build, best runtime/size
build-release:
	cargo build --workspace --release

# Default local build: thin LTO and no local/onnx embeddings, ~3x faster to compile
#
# Everyone carries all remote embedding providers by default. Drop the ones you never
# use with DIST_FEATURES, e.g. an OpenAI-compatible-only setup (F2LLM, local vLLM,
# DeepSeek embeddings):
#   make install-dist DIST_FEATURES="daemon,ai-enhanced,server-http,embeddings-openai"
DIST_FEATURES ?= daemon,ai-enhanced,server-http,embeddings-openai,embeddings-ollama,embeddings-jina,embeddings-lmstudio

.PHONY: build-dist install-dist build-dist-openai install-dist-openai
build-dist:
	cargo build --profile release-dist -p codegraph-mcp-server --bin codegraph --features "$(DIST_FEATURES)"

install-dist: build-dist
	install -m 755 target/release-dist/codegraph $(HOME)/.cargo/bin/codegraph

# Shortcut for OpenAI-compatible embeddings only (no jina/ollama/lmstudio).
# Note: `cloud` embeddings require embeddings-jina, so do not use this if you enable them.
build-dist-openai:
	$(MAKE) build-dist DIST_FEATURES="daemon,ai-enhanced,server-http,embeddings-openai"

install-dist-openai: build-dist-openai
	install -m 755 target/release-dist/codegraph $(HOME)/.cargo/bin/codegraph

# macOS: build/test using LLVM lld for faster linking (requires ld64.lld on PATH)
check-llvm:
	@if [ "$$(uname -s)" != "Darwin" ]; then echo "check-llvm is macOS-only"; exit 1; fi
	@if ! command -v ld64.lld >/dev/null 2>&1; then echo "Missing ld64.lld (install LLVM and add it to PATH)"; exit 1; fi

build-llvm: check-llvm
	RUSTFLAGS="-C link-arg=-fuse-ld=lld" cargo build --workspace

test-llvm: check-llvm
	RUSTFLAGS="-C link-arg=-fuse-ld=lld" cargo test --workspace

# Build MCP HTTP server with experimental HTTP transport
.PHONY: build-mcp-http
build-mcp-http:
	cargo build --release -p codegraph-mcp-server --bin codegraph --features "ai-enhanced,embeddings-ollama,embeddings-lmstudio,server-http"

# Run HTTP server (depends on build)
.PHONY: run-http-server
run-http-server: build-mcp-http
	./target/release/codegraph start http --port 3000

# Build API with size-optimized profile and strip/compress
build-api-size:
	cargo build --profile release-size --bin codegraph-api
	./scripts/strip_compress.sh target/release-size/codegraph-api

size:
	@echo "Sizes for codegraph-api binaries (if present):"
	@for p in target/release/codegraph-api target/release-size/codegraph-api; do \
	  if [ -f $$p ]; then echo "$$p: $$(du -h $$p | cut -f1)"; fi; \
	done

# Run all tests
test:
	cargo test --workspace

# Run tests with coverage (requires cargo-tarpaulin)
test-coverage:
	cargo tarpaulin --workspace --out Html

# Lint with clippy
lint:
	# Limit clippy to default member(s) to avoid compiling experimental crates
	cargo clippy -p codegraph-core -- -A clippy::all -A warnings

# Format code
fmt:
	cargo fmt --all

# Check formatting
fmt-check:
	cargo fmt --all -- --check

# Full check (format, lint, test)
check: fmt lint test

# Run benchmarks
bench:
	@echo "Running benchmarks and saving baseline: $${BASELINE_NAME:-baseline} (ENABLE_FAISS_BENCH=$${ENABLE_FAISS_BENCH:-0})"
	BASELINE_NAME=$${BASELINE_NAME:-baseline} ENABLE_FAISS_BENCH=$${ENABLE_FAISS_BENCH:-0} scripts/run_benchmarks.sh

bench-report:
	@echo "Generating benchmark report from latest results"
	python3 scripts/generate_bench_report.py --output benchmarks/reports/benchmark_report_latest.md

bench-compare:
	@echo "Comparing current benchmark results to baseline: $${BASELINE_NAME:-baseline}"
	python3 scripts/compare_bench.py --baseline $${BASELINE_NAME:-baseline} --threshold $${THRESHOLD:-0.10}

# Clean build artifacts
clean:
	cargo clean

# Install development dependencies
install-deps:
	cargo install cargo-tarpaulin
	cargo install cargo-audit
	cargo install cargo-outdated

# Generate documentation
doc:
	cargo doc --workspace --no-deps --open

# Security audit
audit:
	cargo audit

# Check for outdated dependencies
outdated:
	cargo outdated --workspace

# Run the API server
run-api:
	RUST_LOG=debug cargo run --bin codegraph-api

# Sync validation stress tests (standalone harness)
sync-validate:
	cargo run --manifest-path high_perf_test/Cargo.toml

# Development workflow
dev: fmt lint test

# CI workflow
ci: fmt-check lint test

# Quick check without tests
quick: fmt lint

# Run API E2E/integration tests (API crate)
e2e:
	cargo test -p codegraph-api -- --nocapture

# Load testing using k6 (requires k6 installed)
load-test:
	BASE_URL=$${BASE_URL:-http://localhost:3000} scripts/load/run.sh

# Deployment validation against a running endpoint
deploy-validate:
	BASE_URL=$${BASE_URL:-http://localhost:3000} scripts/deploy/validate.sh

# Performance regression check (Criterion benches vs baseline)
perf-regression:
	BASELINE_NAME=$${BASELINE_NAME:-baseline} THRESHOLD=$${THRESHOLD:-0.10} scripts/perf/regression.sh

# Cross-platform build (local)
build-cross:
	@echo "Building release binaries for Linux, macOS, Windows targets"
	bash scripts/build_cross.sh

# Build and run specific examples
example-parse:
	cargo run --bin codegraph-api &
	sleep 2
	curl -X POST http://localhost:3000/parse \
		-H "Content-Type: application/json" \
		-d '{"file_path": "src/main.rs"}'
	pkill -f codegraph-api

# ===================== Memory Safety / Leak Detection =====================

# Run API with leak detection enabled (feature-gated)
run-api-leaks:
	RUST_LOG=debug cargo run --bin codegraph-api --features leak-detect

# Export on-demand leak report to target/memory_reports
leak-report:
	curl -s http://localhost:3000/memory/leaks | jq .

# View runtime memory stats (requires leak-detect)
leak-stats:
	curl -s http://localhost:3000/memory/stats | jq .

# Run tests under Miri (requires nightly toolchain + miri component)
miri:
	rustup toolchain install nightly --component miri || true
	cargo +nightly miri test --workspace

# Run tests with Address/Leak Sanitizer (requires nightly)
asan-test:
	rustup toolchain install nightly || true
	RUSTFLAGS="-Zsanitizer=address" \
	RUSTDOCFLAGS="-Zsanitizer=address" \
	cargo +nightly test --workspace -Zbuild-std --target x86_64-apple-darwin
