DEMO := docker compose -f demo/compose.yml

.PHONY: help build test it coverage lint fmt demo-up demo-seed demo-profiles demo demo-down gif

help:
	@echo "make build          build a release binary"
	@echo "make test           unit tests"
	@echo "make it             integration tests (starts Redis and Valkey in Docker)"
	@echo "make coverage       line coverage of unit + integration tests (needs cargo-llvm-cov and Docker); HTML in target/llvm-cov/html"
	@echo "make lint           fmt check and clippy"
	@echo "make demo-up        start and seed the demo Redis (6390) and Valkey (6391)"
	@echo "make demo           demo-up, save the demo connections, open the TUI"
	@echo "make demo-down      stop the demo and delete its data"
	@echo "make gif            record docs/demo.gif from demo/gif-steps.json with temporary connections (needs the demo up, python3 and agg)"

build:
	cargo build --release --locked

test:
	cargo test --locked

it:
	cargo test --locked --features integration-tests --tests

# cargo-llvm-cov needs llvm-cov and llvm-profdata. rustup's llvm-tools-preview provides
# them; without rustup (e.g. Homebrew Rust on macOS) this falls back to Xcode's copies.
coverage:
	@if ! command -v rustup >/dev/null 2>&1 && command -v xcrun >/dev/null 2>&1; then \
		export LLVM_COV="$$(xcrun --find llvm-cov)" LLVM_PROFDATA="$$(xcrun --find llvm-profdata)"; \
	fi; \
	cargo llvm-cov --locked --features integration-tests --tests --html && \
	cargo llvm-cov report --summary-only

lint:
	cargo fmt --all --check
	cargo clippy --locked --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all

demo-up:
	$(DEMO) up -d --wait redis valkey
	$(DEMO) run --rm seed

# Saves two connections. The demo password goes to your OS keychain, not the config file.
demo-profiles:
	printf 'lens-demo-only' | cargo run --quiet -- conn add demo-redis --url redis://lens@127.0.0.1:6390 --env local --password-stdin --force
	printf 'limited-demo-only' | cargo run --quiet -- conn add demo-redis-limited --url redis://lens-limited@127.0.0.1:6390 --env stage --password-stdin --force
	cargo run --quiet -- conn add demo-valkey --url redis://127.0.0.1:6391 --env local --force

demo: demo-up demo-profiles
	cargo run --release

demo-down:
	$(DEMO) down -v

# Needs the demo servers up (make demo-up), python3 and agg (https://github.com/asciinema/agg).
# demo/gif-steps.json lists the keystrokes to replay. The recording uses its own temporary
# config file (REDIS_LENS_CONFIG) holding only the two connections the steps expect:
# demo-cache (ACL user lens, password in the keychain) and demo-valkey (no auth). Both are
# removed afterwards with `conn remove`, keychain entries included, even when recording
# fails. Keychain entries are per profile name, so this replaces and then deletes any entry
# you already have under the names demo-cache and demo-valkey.
LENS := ./target/release/redis-lens

gif: build
	@set -e; \
	dir="$$(mktemp -d)"; \
	export REDIS_LENS_CONFIG="$$dir/config.toml"; \
	cleanup() { \
		$(LENS) conn remove demo-cache >/dev/null 2>&1 || true; \
		$(LENS) conn remove demo-valkey >/dev/null 2>&1 || true; \
		rm -f "$$REDIS_LENS_CONFIG"; \
		rmdir "$$dir" 2>/dev/null || true; \
	}; \
	trap cleanup EXIT; \
	printf 'lens-demo-only' | $(LENS) conn add demo-cache --url redis://lens@127.0.0.1:6390 --env local --password-stdin; \
	$(LENS) conn add demo-valkey --url redis://127.0.0.1:6391 --env local; \
	python3 demo/record.py demo/demo.cast demo/gif-steps.json -- $(LENS); \
	agg demo/demo.cast docs/demo.gif
