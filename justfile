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

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets -- -D warnings
