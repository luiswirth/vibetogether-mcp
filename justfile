# Warnings are errors here, so that a local build stays warning-tolerant.
export RUSTFLAGS := "-D warnings"
# Doc comments carry the mathematics, so a broken intra-doc link is an error.
export RUSTDOCFLAGS := "-D warnings"

# Everything CI runs.
ci:
    nix fmt -- --ci
    cargo clippy --all-targets
    cargo test
    cargo doc --no-deps

# The server, locally, at http://127.0.0.1:8792/mcp.
dev:
    cargo run
