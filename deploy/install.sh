#!/bin/sh
# Everything between a fresh Linux machine and one `nibrunnerd install` can finish:
#
#   curl -fsSL https://raw.githubusercontent.com/ilbertt/nibrunner/main/deploy/install.sh | sh
#
# It fetches the release, lays the binary and the guest image down, and hands over to
# `nibrunnerd install` — which is what knows how to render this host's ZeroFS configuration, its
# units and its kernel settings from `config.toml`. Nothing here decides anything that file says.
#
# NIBRUNNER_VERSION  a release tag; the newest one otherwise
# NIBRUNNER_CONFIG   where this host's configuration is, or will be
#
# Everything is in a function called on the last line, so a download that stops half way runs
# nothing rather than running half of this.
set -eu

REPO=${NIBRUNNER_REPO:-ilbertt/nibrunner}
CONFIG=${NIBRUNNER_CONFIG:-/etc/nibrunner/config.toml}
GUEST_DIR=/var/lib/nibrunner/guest
BINARY=/usr/local/bin/nibrunnerd

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

refuse_unless_this_machine_can_host() {
    [ "$(id -u)" = 0 ] || die "run this as root: it writes to /usr/local/bin, /etc and /var/lib"
    [ "$(uname -s)" = Linux ] || die "nibrunner turns a Linux machine into an app host; this is $(uname -s)"
    [ "$(uname -m)" = x86_64 ] || die "the release ships for x86_64 only; this is $(uname -m)"
    [ -e /dev/kvm ] || die "no /dev/kvm — a microVM cannot be booted on this machine"
}

# The superset of both backends. nbd-client and fuse3 are only ZeroFS's, and are a few hundred
# kilobytes on a host that turns out not to want them — cheaper than reading the configuration
# before it necessarily exists.
packages() {
    set -- nftables e2fsprogs nbd-client fuse3 kmod passwd curl ca-certificates
    if ! command -v apt-get >/dev/null 2>&1; then
        die "this installs packages with apt-get, which is not here. Install these and re-run: $*"
    fi
    say "packages"
    DEBIAN_FRONTEND=noninteractive apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
}

# Releases are cut as prereleases, which /releases/latest does not answer with — so the newest is
# read off the list rather than asked for by that name.
newest_release() {
    curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=1" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -1
}

fetch_release() {
    version=$1
    say "release $version"

    work=$(mktemp -d)
    # Removed whatever happens: this is 170 MB of it, in /tmp, on a host whose disk is the thing
    # every tenant's cache lives on.
    trap 'rm -rf "$work"' EXIT INT TERM
    base="https://github.com/$REPO/releases/download/$version"
    for asset in nibrunnerd-linux-x64 vmlinux rootfs.ext4 manifest.json checksums.txt; do
        curl -fsSL --retry 3 -o "$work/$asset" "$base/$asset" ||
            die "$asset is not in release $version"
    done

    say "checksums"
    (cd "$work" && sha256sum -c checksums.txt >/dev/null) ||
        die "the release did not hash to what it publishes — refusing to install it"

    install -m 0755 "$work/nibrunnerd-linux-x64" "$BINARY"
    mkdir -p "$GUEST_DIR"
    install -m 0644 "$work/vmlinux" "$work/rootfs.ext4" "$work/manifest.json" "$GUEST_DIR/"

    # Not in the release assets, and taken from the tag rather than from the default branch so a
    # host installed twice a month apart gets the unit its own binary was cut with.
    curl -fsSL --retry 3 -o /etc/systemd/system/nibrunnerd.service \
        "https://raw.githubusercontent.com/$REPO/$version/deploy/nibrunnerd.service" ||
        die "the service unit could not be fetched for $version"
    say "$BINARY, $GUEST_DIR and the unit"
}

# `nibrunnerd install` is what lays the rest of the host out, and it reads the one file nothing
# here can write: what this host's storage is, what it serves on, what it is called.
lay_the_host_out() {
    version=$1
    if [ ! -f "$CONFIG" ]; then
        mkdir -p "$(dirname "$CONFIG")"
        curl -fsSL --retry 3 -o "$CONFIG" \
            "https://raw.githubusercontent.com/$REPO/$version/deploy/config.toml"
        say "$CONFIG laid down as a starting point"
        cat <<EOF

This host has no configuration yet, so one was copied in: volumes as files on this
machine's own disk, no proxy, no object store. Read it, make it this host's, and then:

    nibrunnerd install
    systemctl daemon-reload && systemctl enable --now nibrunnerd

deploy/config.zerofs.toml in the repository is the same file for a host whose volumes
live in an object store and which serves TLS.
EOF
        return
    fi

    say "nibrunnerd install"
    "$BINARY" install
    cat <<EOF

Two things left, and both are yours:

    \$EDITOR $(dirname "$CONFIG")/host.env    the AWS credentials, and on a zerofs host
                                              the encryption password
    systemctl daemon-reload
    systemctl enable --now nibrunnerd

A zerofs host starts nibrunner-zerofs and nibrunner-zerofs-mount alongside it.
EOF
}

main() {
    refuse_unless_this_machine_can_host
    version=${NIBRUNNER_VERSION:-$(newest_release)}
    [ -n "$version" ] || die "no release could be found for $REPO; name one in NIBRUNNER_VERSION"
    packages
    fetch_release "$version"
    lay_the_host_out "$version"
}

main "$@"
