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
wr1:
    watchexec -w ./wr1.sh --clear -r "./wr1.sh"

# Regenerate COMMITS.md — the exhaustive commit index behind CHANGELOG.md.
changelog:
    ./scripts/gen-commit-index.sh

# Same, for a rev range (e.g. `just changelog-range v0.1.0..HEAD`).
changelog-range range:
    ./scripts/gen-commit-index.sh "{{ range }}"
