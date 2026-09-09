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

# The image the daemon boots: `crates/init` as PID 1 in a rootfs built from the pins in
# guest/manifest.json, which the script rewrites to describe what came out — the committed one
# names no rootfs at all, so the daemon rejects it. Linux, root, docker and e2fsprogs; from a Mac
# this is what CI is for. `vmlinux` is not built here, it is copied from nibrun.
guest-image:
    cargo build -p nibrunner-init --target x86_64-unknown-linux-musl --release
    sudo guest/build-image.sh

# The version the next temporary prerelease carries, as CalVer `YYYY.M.D-N` with no leading zeros.
# Dates rather than semver because what reaches this daemon reaches it as a side effect of work
# aimed elsewhere. The `-N` is on every release, the day's first included: semver ranks a version
# carrying a pre-release tag below the same version without one, so a bare `2026.9.9` would sort
# above every re-cut that day. Read off the tags you have, so fetch them first.
tmp-version:
    #!/usr/bin/env bash
    set -euo pipefail
    today="$(date -u +%Y.%-m.%-d)"
    # The highest cut today rather than how many were, because these are meant to be deleted once
    # they have served their purpose, and counting the survivors of a day that lost its first
    # release hands back a number the second one is still holding.
    last="$(git tag --list "tmp-v$today-*" | sed "s/^tmp-v$today-//" | sort -n | tail -1)"
    echo "tmp-v$today-$(( ${last:-0} + 1 ))"

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
