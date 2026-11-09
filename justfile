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

# Moves the agentwire pin to a full commit, and its hash in the flake with it.
bump rev:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ "{{ rev }}" =~ ^[0-9a-f]{40}$ ]] || { echo "give a full commit of agentwire"; exit 1; }
    sed -i -E 's|(luiswirth/agentwire", rev = ")[0-9a-f]+|\1{{ rev }}|' Cargo.toml
    cargo update -q -p agentwire -p agentwire-relay
    fake=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
    sed -i -E "s|(outputHashes\.\"agentwire[^\"]*\" = \")[^\"]+|\1$fake|" flake.nix
    got=$(nix build --no-link .#default 2>&1 | grep -oP 'got:\s+\K\S+' | head -1 || true)
    [ -n "$got" ] || { echo "the build named no hash"; exit 1; }
    sed -i "s|$fake|$got|g" flake.nix
    nix build --no-link .#default
