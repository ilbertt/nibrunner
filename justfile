# nibrunner. `just` with no target lists these.
default:
    @just --list

# The whole workspace, for the machine you are on.
build:
    cargo build --workspace

# How the host binary is linked: an x86_64 Linux box has a musl toolchain of its own (`musl-tools`),
# anything else crosses to it through `zig` and `cargo-zigbuild`.
cargo-musl := if os() + "-" + arch() == "linux-x86_64" { "cargo build" } else { "cargo zigbuild" }

# One static x86_64 Linux binary, what a host runs.
release:
    {{cargo-musl}} -p nibrunnerd --target x86_64-unknown-linux-musl --release
    @ls -la target/x86_64-unknown-linux-musl/release/nibrunnerd

# Builds guest/rootfs.ext4 and rewrites the manifest to describe it. Linux, root, docker, e2fsprogs.
guest-image:
    cargo build -p nibrunner-init --target x86_64-unknown-linux-musl --release
    sudo guest/build-image.sh

# Checks the guest image the way the daemon checks it before booting anything.
verify-guest-image:
    #!/usr/bin/env bash
    set -euo pipefail
    sums=$(mktemp)
    trap 'rm -f "$sums"' EXIT
    jq -r '.artifacts[] | select(.name == "vmlinux" or .name == "rootfs.ext4") | "\(.sha256)  \(.name)"' \
        guest/manifest.json > "$sums"
    # Two, because the manifest as committed describes no rootfs and would otherwise pass on none.
    test "$(wc -l < "$sums")" -eq 2
    cd guest && sha256sum -c "$sums"

# Everything a host installs, in one directory, next to the sums it should hash to.
stage-release dist:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{dist}}"
    install -m 0755 target/x86_64-unknown-linux-musl/release/nibrunnerd "{{dist}}/nibrunnerd-linux-x64"
    install -m 0644 guest/vmlinux guest/rootfs.ext4 guest/manifest.json "{{dist}}"
    # Written from inside the directory, so it names what `sha256sum -c` will be run next to.
    cd "{{dist}}" && sha256sum nibrunnerd-linux-x64 vmlinux rootfs.ext4 manifest.json > checksums.txt

# The version the next temporary prerelease carries, as CalVer `YYYY.M.D-N`, read off the tags.
tmp-version:
    #!/usr/bin/env bash
    set -euo pipefail
    today="$(date -u +%Y.%-m.%-d)"
    # `-N` is on every release, the day's first included: semver ranks a version carrying a
    # pre-release tag below the same version without one. The highest cut today rather than how
    # many were, because counting the survivors of a day that lost its first release hands back a
    # number the second one is still holding.
    last="$(git tag --list "v$today-*" | sed "s/^v$today-//" | sort -n | tail -1)"
    echo "v$today-$(( ${last:-0} + 1 ))"

# Everything that needs no kernel: the planner, the codecs, the ruleset, the reconcile.
test:
    cargo test --workspace

# The tests that need a kernel. Root, Linux, and `nft`, `mke2fs` and `/dev/net/tun` on the box.
integration:
    NIBRUNNER_INTEGRATION=1 cargo test -p nibrunnerd --test integration -- --test-threads 1 --nocapture

fmt:
    cargo fmt --all

# `fmt` as a check: fails on anything it would have rewritten.
fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings
