#!/usr/bin/env bash
# Brings up the nibrunner host. The daemon and guest images are already staged and
# checksum-verified in /tmp/stage; only the tenant is built here, by bun.
set -euo pipefail

STATE=/var/lib/nibrunner
PROXY_PORT=8080
STAGE=/tmp/stage

echo "== kvm =="
[[ -e /dev/kvm ]] || { echo "FATAL: no /dev/kvm" >&2; exit 1; }
ls -la /dev/kvm

echo "== packages =="
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq nftables e2fsprogs jq python3 curl unzip >/dev/null
echo "packages in"

echo "== bun and the tenant =="
if [[ ! -x "$HOME/.bun/bin/bun" ]]; then
  curl -fsSL https://bun.sh/install | bash >/dev/null 2>&1
fi
export PATH="$HOME/.bun/bin:$PATH"
bun --version
bun build "$STAGE/server.ts" --compile --target=bun-linux-x64 --minify --outfile "$HOME/todos"
ls -la "$HOME/todos"

echo "== host network =="
sudo modprobe nf_conntrack
echo 1 | sudo tee /proc/sys/net/ipv4/ip_forward >/dev/null

echo "== instance store =="
nvme=$(lsblk -dn -o NAME,MODEL | awk '/Instance Storage/ {print $1; exit}')
if [[ -n "${nvme:-}" ]] && ! mountpoint -q "$STATE"; then
  sudo mkfs.ext4 -F -q "/dev/$nvme"
  sudo mkdir -p "$STATE"
  sudo mount "/dev/$nvme" "$STATE"
  echo "mounted /dev/$nvme at $STATE"
fi
df -h "$STATE"

echo "== layout =="
sudo mkdir -p "$STATE"/{guest,artifact-store,snapshots}
sudo install -m 0755 "$STAGE/nibrunnerd" /usr/local/bin/nibrunnerd
sudo cp "$STAGE/vmlinux" "$STAGE/rootfs.ext4" "$STAGE/manifest.json" "$STATE/guest/"
sudo cp "$HOME/todos" "$STATE/artifact-store/todos"
sudo install -m 0755 "$STAGE/ramp.sh" "$STAGE/gen-desired.py" /root/

sudo mkdir -p /etc/nibrunner
sudo tee /etc/nibrunner/config.toml >/dev/null <<TOML
[paths]
state_dir = "$STATE"
runtime_dir = "/run/nibrunner"
snapshot_dir = "$STATE/snapshots"
guest_image_dir = "$STATE/guest"
desired_state_file = "$STATE/desired.json"
api_socket = "/run/nibrunner/nibrunner.sock"
versions_file = "$STATE/versions.json"

[artifacts]
store_url = "$STATE/artifact-store"

[volumes]
backend = "local-file"
storage_prefix = "volumes"

[exports]
store_url = "$STATE/export-store"
staging_dir = "$STATE/exports"

[network]
denied_egress_addresses_v4 = []
denied_egress_addresses_v6 = []

[proxy.http]
port = $PROXY_PORT
TOML

echo '{"hostId":"bench-host","volumes":[],"instances":[],"checkpoints":[],"exports":[]}' \
  | sudo tee "$STATE/desired.json" >/dev/null

sudo tee /etc/systemd/system/nibrunnerd.service >/dev/null <<'UNIT'
[Unit]
Description=nibrunner app host
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
ExecStart=/usr/local/bin/nibrunnerd

Restart=always
RestartSec=5s

StateDirectory=nibrunner
RuntimeDirectory=nibrunner
RuntimeDirectoryPreserve=yes

NoNewPrivileges=false

LogExtraFields=SOURCE=nibrunnerd

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now nibrunnerd
sleep 3
systemctl is-active nibrunnerd
echo "== ready =="
sudo sha256sum "$STATE/artifact-store/todos"
sudo stat -c%s "$STATE/artifact-store/todos"
