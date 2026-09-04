#!/bin/bash
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}Running KOD test suite...${NC}"

# Check formatting
echo -e "${YELLOW}Checking formatting...${NC}"
if cargo fmt --all -- --check; then
    echo -e "${GREEN}✓ Formatting is correct${NC}"
else
    echo -e "${RED}✗ Formatting issues found${NC}"
    exit 1
fi

# Run clippy
echo -e "${YELLOW}Running clippy...${NC}"
if cargo clippy --workspace --all-targets -- -D warnings; then
    echo -e "${GREEN}✓ Clippy passed${NC}"
else
    echo -e "${RED}✗ Clippy failed${NC}"
    exit 1
fi

# Run tests
echo -e "${YELLOW}Running tests...${NC}"
if cargo test --workspace; then
    echo -e "${GREEN}✓ Tests passed${NC}"
else
    echo -e "${RED}✗ Tests failed${NC}"
    exit 1
fi

# Build release
echo -e "${YELLOW}Building release...${NC}"
if cargo build --release; then
    echo -e "${GREEN}✓ Release build successful${NC}"
else
    echo -e "${RED}✗ Release build failed${NC}"
    exit 1
fi

echo -e "${GREEN}All tests passed!${NC}"
