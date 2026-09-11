#!/bin/sh
# Puts `nibrunnerd` on a fresh Linux machine and hands over to it:
#
#   curl -fsSL https://raw.githubusercontent.com/ilbertt/nibrunner/main/deploy/install.sh | sh
#
# This knows one thing — where a release is. Everything about where files go, what this host
# needs, what is rendered and what is left for a person is `nibrunnerd install`, which reads it
# from `config.toml`. Anything this script also knew would be a second copy of an answer that
# already lives there.
#
# NIBRUNNER_VERSION  a release tag; the newest one otherwise
# NIBRUNNER_CONFIG   passed through, for a configuration somewhere other than the default
#
# Everything is in a function called on the last line, so a download that stops half way runs
# nothing rather than running half of this.
set -eu

REPO=${NIBRUNNER_REPO:-ilbertt/nibrunner}
BINARY=/usr/local/bin/nibrunnerd

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

refuse_unless_this_machine_can_host() {
    [ "$(id -u)" = 0 ] || die "run this as root: it installs a system daemon"
    [ "$(uname -s)" = Linux ] || die "nibrunner turns a Linux machine into an app host; this is $(uname -s)"
    [ "$(uname -m)" = x86_64 ] || die "the release ships for x86_64 only; this is $(uname -m)"
}

# The superset of both volume backends. Which one this host wants is in a configuration that need
# not exist yet, and nbd-client and fuse3 are a few hundred kilobytes on a host that turns out not
# to want them.
packages() {
    set -- nftables e2fsprogs nbd-client fuse3 kmod passwd curl ca-certificates
    if ! command -v apt-get >/dev/null 2>&1; then
        die "this installs packages with apt-get, which is not here. Install these and re-run: $*"
    fi
    say "apt-get update"
    DEBIAN_FRONTEND=noninteractive apt-get update -q
    say "apt-get install $*"
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q "$@"
}

# Releases are cut as prereleases, which /releases/latest does not answer with — so the newest is
# read off the list rather than asked for by that name.
newest_release() {
    curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=1" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -1
}

# The whole release, checked against the digests it publishes. Both things that come out of it go
# somewhere: this binary to a path a shell has to know to be able to run it, and the guest image to
# wherever the configuration says — which is why the second one is handed to `nibrunnerd install`
# as a directory rather than put somewhere by this.
fetch_release() {
    base=$1
    into=$2
    for asset in nibrunnerd-linux-x64 vmlinux rootfs.ext4 manifest.json checksums.txt; do
        say "$asset"
        curl -fL --progress-bar --retry 3 -o "$into/$asset" "$base/$asset" ||
            die "$asset is not in this release"
    done
    say "checksums"
    (cd "$into" && sha256sum -c checksums.txt >/dev/null) ||
        die "the release did not hash to what it publishes — refusing to install it"
    install -m 0755 "$into/nibrunnerd-linux-x64" "$BINARY"
    say "$BINARY"
}

main() {
    refuse_unless_this_machine_can_host
    version=${NIBRUNNER_VERSION:-$(newest_release)}
    [ -n "$version" ] || die "no release could be found for $REPO; name one in NIBRUNNER_VERSION"

    packages

    # Not exec'd below, so that this runs: the release is 170 MB in /tmp, on a host whose disk is
    # the thing every tenant's cache will live on.
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT INT TERM

    # Said here rather than before the packages, which scroll anything said before them off the
    # screen: the version goes next to the downloads it names.
    say "release $version — https://github.com/$REPO/releases/tag/$version"
    fetch_release "https://github.com/$REPO/releases/download/$version" "$work"

    say "nibrunnerd install"
    "$BINARY" install --from "$work"
}

main "$@"
