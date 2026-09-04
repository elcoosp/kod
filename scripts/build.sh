#!/bin/bash
set -e

VERSION=$(grep -E '^version' Cargo.toml 2>/dev/null | head -1 | cut -d'"' -f2 || echo "0.1.0")
echo "Building KOD v${VERSION}..."

# Build release
cargo build --release

# Create distribution directory
mkdir -p dist

# Copy binary
cp target/release/kod dist/

# Create tarball
tar -czf dist/kod-${VERSION}.tar.gz -C dist kod

# Generate checksums
cd dist
sha256sum kod-${VERSION}.tar.gz > kod-${VERSION}.sha256
cd ..

echo "Build complete!"
echo "Binary: dist/kod"
echo "Package: dist/kod-${VERSION}.tar.gz"
echo "Checksum: dist/kod-${VERSION}.sha256"
