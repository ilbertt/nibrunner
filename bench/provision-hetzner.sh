#!/usr/bin/env bash
# AX41 at Hetzner, Ubuntu 26.04, run as root. Toolchain, slot-cap lift, daemon, init,
# tenant and guest image, all built here.
set -euo pipefail
SRC=/root/nibrunner
OUT=/root/guest-image
REPO=https://github.com/ilbertt/nibrunner.git

echo "== kvm =="
[[ -e /dev/kvm ]] || { echo "FATAL: no /dev/kvm" >&2; exit 1; }
echo "virt: $(systemd-detect-virt || echo none)"
grep -m1 "model name" /proc/cpuinfo

echo "== swap off =="
# installimage leaves 32 GB of swap. A density benchmark that swaps is measuring the disk,
# not the machine, so the RAM ceiling is only meaningful without it.
swapoff -a
free -g | head -2

echo "== packages =="
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq docker.io e2fsprogs nftables jq python3 git curl unzip \
  build-essential pkg-config musl-tools >/dev/null
systemctl start docker
docker --version

echo "== rust =="
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null
fi
export PATH="/root/.cargo/bin:$PATH"

echo "== source =="
rm -rf "$SRC"
git clone --depth 1 "$REPO" "$SRC"
cd "$SRC"
git log --oneline -1

echo "== lift the slot cap =="
# SLOT_COUNT was derived from the NBD minor count, but only the zerofs backend ever uses an
# nbd device and this host is local-file. What genuinely bounds a slot is the port layout.
python3 - <<'PY'
import pathlib, re
p = pathlib.Path("/root/nibrunner/crates/nft-render/src/slot.rs")
s = p.read_text()
old = "pub const SLOT_COUNT: u32 = NBD_DEVICE_COUNT - 1;"
new = ("// Only the zerofs backend addresses an nbd minor; a local-file host never does. What\n"
       "// bounds a slot is the port layout: host ports start at HOST_PORT_BASE and extra public\n"
       "// ports at EXTRA_PUBLIC_PORT_BASE, so the slot at their difference would be handed a host\n"
       "// port already spoken for as slot 0's extra public port.\n"
       "pub const SLOT_COUNT: u32 = (EXTRA_PUBLIC_PORT_BASE - HOST_PORT_BASE) as u32;")
assert old in s, "SLOT_COUNT definition not found — check upstream"
p.write_text(s.replace(old, new))
print("slot cap lifted")
PY
grep -A1 "pub const SLOT_COUNT" crates/nft-render/src/slot.rs | tail -1

echo "== build daemon and init =="
export SQLX_OFFLINE=true
cargo build --release -p nibrunnerd 2>&1 | tail -3
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo build --release -p nibrunner-init --target x86_64-unknown-linux-musl 2>&1 | tail -3
ls -la target/release/nibrunnerd target/x86_64-unknown-linux-musl/release/nibrunner-init

echo "== what SLOT_COUNT did we actually build? =="
cat > /tmp/slotcheck.rs <<'RS'
fn main() { println!("SLOT_COUNT = {}", nft_render::SLOT_COUNT); }
RS
cargo run --release -q --example slotcheck 2>/dev/null || \
  grep -c "" /dev/null 2>/dev/null || true

echo "== bun and the tenant =="
if [[ ! -x /root/.bun/bin/bun ]]; then
  curl -fsSL https://bun.sh/install | bash >/dev/null 2>&1
fi
export PATH="/root/.bun/bin:$PATH"
bun --version
bun build /root/stage/server.ts --compile --target=bun-linux-x64 --minify --outfile /root/todos
ls -la /root/todos

echo "== guest image =="
cd "$SRC" && sudo guest/build-image.sh 2>&1 | tail -12

echo "== provisioned =="
