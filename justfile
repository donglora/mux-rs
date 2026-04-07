set shell := ["bash", "-c"]

default: run

[private]
_ensure_tools:
    @mise trust --yes . 2>/dev/null; mise install --quiet

# Run the mux daemon
run *args: _ensure_tools
    cargo run -- {{args}}

# Run with verbose logging
verbose *args: _ensure_tools
    cargo run -- --verbose {{args}}

# Run all checks (fmt, clippy, test, audit)
check: fmt-check clippy test audit

# Format code
fmt: _ensure_tools
    cargo fmt

# Check formatting without changing files
fmt-check: _ensure_tools
    cargo fmt --check

# Lint with clippy
clippy: _ensure_tools
    cargo clippy --all-targets -- -D warnings

# Run tests
test: _ensure_tools
    cargo test

# Security + license audit
audit: _ensure_tools
    cargo deny check

# Build release
build: _ensure_tools
    cargo build --release

# Check for outdated dependencies
outdated: _ensure_tools
    cargo outdated

# Clean build artifacts
clean:
    cargo clean
