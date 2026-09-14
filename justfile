# Build the workspace
build:
    cargo build --workspace

# Run all tests
test:
    cargo test --workspace

# Run clippy lints
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format all code
format:
    cargo fmt --all

# Run format, lint, and test
check: format lint test

# Clean build artifacts
clean:
    cargo clean

# Run benchmarks
bench:
    cargo bench --workspace

# Build and open docs
doc:
    cargo doc --workspace --no-deps --open

# Development: build and run chat
dev: build
    cargo run -p kod-cli -- kod chat

# Install the CLI binary
install:
    cargo install --path crates/kod-cli
wr:
    watchexec -w ./wr.sh --clear -r "./wr.sh"
