set shell := ["bash", "-c"]

default: run

# Run the mux daemon
run *args:
    cargo run -- {{args}}

# Run with verbose logging
verbose *args:
    cargo run -- --verbose {{args}}

# Run clippy and tests
check:
    cargo clippy -- -D warnings && cargo test

# Build release
build:
    cargo build --release
