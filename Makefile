.PHONY: build test lint format check clean bench doc

build:
	cargo build --workspace

test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

format:
	cargo fmt --all

check: format lint test

clean:
	cargo clean

bench:
	cargo bench --workspace

doc:
	cargo doc --workspace --no-deps --open

# Development helpers
dev: build
	cargo run -p kod-cli -- kod chat

install:
	cargo install --path crates/kod-cli
