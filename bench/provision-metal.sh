#!/usr/bin/env bash
# Bare-metal host: toolchain, repo, daemon, init and tenant all built here.
set -euo pipefail
SRC=$HOME/nibrunner
REPO=https://github.com/ilbertt/nibrunner.git

echo "== kvm =="
[[ -e /dev/kvm ]] || { echo "FATAL: no /dev/kvm" >&2; exit 1; }
echo "virt: $(systemd-detect-virt || echo none)"

echo "== packages =="
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq docker.io e2fsprogs nftables jq python3 git curl unzip \
  build-essential pkg-config musl-tools >/dev/null
sudo systemctl start docker
echo "packages in"

echo "== rust =="
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo "== source =="
rm -rf "$SRC"
git clone --depth 1 "$REPO" "$SRC"
cd "$SRC"
git log --oneline -1
rustc --version

echo "== build daemon and init =="
export SQLX_OFFLINE=true
cargo build --release -p nibrunnerd 2>&1 | tail -3
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo build --release -p nibrunner-init --target x86_64-unknown-linux-musl 2>&1 | tail -3
ls -la target/release/nibrunnerd target/x86_64-unknown-linux-musl/release/nibrunner-init

echo "== bun and the tenant =="
if [[ ! -x "$HOME/.bun/bin/bun" ]]; then
  curl -fsSL https://bun.sh/install | bash >/dev/null 2>&1
fi
export PATH="$HOME/.bun/bin:$PATH"
bun --version
bun build /tmp/stage/server.ts --compile --target=bun-linux-x64 --minify --outfile "$HOME/todos"
ls -la "$HOME/todos"
echo "== toolchain ready =="
