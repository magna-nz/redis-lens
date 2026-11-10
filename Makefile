DEMO := docker compose -f demo/compose.yml

.PHONY: help build test it lint fmt demo-up demo-seed demo-profiles demo demo-down gif

help:
	@echo "make build          build a release binary"
	@echo "make test           unit tests"
	@echo "make it             integration tests (starts Redis and Valkey in Docker)"
	@echo "make lint           fmt check and clippy"
	@echo "make demo-up        start and seed the demo Redis (6390) and Valkey (6391)"
	@echo "make demo           demo-up, save the demo connections, open the TUI"
	@echo "make demo-down      stop the demo and delete its data"
	@echo "make gif            record docs/demo.gif from demo/gif-steps.json (needs the demo up, python3 and agg)"

build:
	cargo build --release --locked

test:
	cargo test --locked

it:
	cargo test --locked --features integration-tests --tests

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

# Needs the demo servers up (make demo-up), a release build, and agg (https://github.com/asciinema/agg).
# demo/gif-steps.json lists the keystrokes to replay.
gif: build
	python3 demo/record.py demo/demo.cast demo/gif-steps.json -- ./target/release/redis-lens
	agg demo/demo.cast docs/demo.gif
