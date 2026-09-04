# nibrunner. `just` with no target lists these.
default:
    @just --list

# The whole workspace, for the machine you are on.
build:
    cargo build --workspace

# One static x86_64 Linux binary, which is what a host runs. Needs `zig` and `cargo-zigbuild`
# when you are not already on x86_64 Linux; on a host it is `cargo build --release --target ...`.
release:
    cargo zigbuild -p nibrunnerd --target x86_64-unknown-linux-musl --release
    @ls -la target/x86_64-unknown-linux-musl/release/nibrunnerd

# Everything that needs no kernel: the planner, the codecs, the ruleset, the reconcile.
test:
    cargo test --workspace

# Everything that does. Root, Linux, and `nft`, `mke2fs` and `/dev/net/tun` on the box.
# Nothing here is proven by the lane above, and nothing above is repeated here.
integration:
    NIBRUNNER_INTEGRATION=1 cargo test -p nibrunnerd --test integration -- --test-threads 1 --nocapture

# A host in one directory under ./.nibrunner-dev, watching ./.nibrunner-dev/desired.json.
# Write a document into that file and this converges on it.
run-dev:
    #!/usr/bin/env bash
    set -euo pipefail
    root="$PWD/.nibrunner-dev"
    mkdir -p "$root"/{state,run,guest,artifacts}
    cp -n guest/vmlinux guest/rootfs.ext4 guest/manifest.json "$root/guest/" 2>/dev/null || true
    export NIBRUNNER_STATE_DIR="$root/state" NIBRUNNER_RUNTIME_DIR="$root/run"
    export NIBRUNNER_GUEST_IMAGE_DIR="$root/guest" NIBRUNNER_SNAPSHOT_DIR="$root/state/snapshots"
    export NIBRUNNER_DESIRED_STATE_FILE="$root/desired.json"
    export NIBRUNNER_ARTIFACT_STORE_URL="$root/artifacts"
    export NIBRUNNER_PROXY_HTTP_PORT="${NIBRUNNER_PROXY_HTTP_PORT:-8080}"
    export NIBRUNNER_LOG="${NIBRUNNER_LOG:-info}"
    echo "watching $NIBRUNNER_DESIRED_STATE_FILE"
    exec cargo run -p nibrunnerd

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets -- -D warnings
