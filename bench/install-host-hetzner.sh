#!/usr/bin/env bash
# Lays out the nibrunner host on a Hetzner dedicated root server.
# Differs from the AWS one only in the disk step: no instance store to find, and
# installimage has already made the NVMe pair into the root filesystem.
set -euo pipefail
SRC=${SRC:-/root/nibrunner}
OUT=${OUT:-/root/guest-image}
STATE=/var/lib/nibrunner
PROXY_PORT=8080

echo "== kvm =="
[[ -e /dev/kvm ]] || { echo "FATAL: no /dev/kvm" >&2; exit 1; }
echo "virt: $(systemd-detect-virt || echo none)"
grep -m1 "model name" /proc/cpuinfo
grep -qE " (vmx|svm) " /proc/cpuinfo && echo "hardware virtualisation: present"

echo "== disk =="
# No instance store here. installimage typically leaves the two NVMe as a RAID1 root,
# so the state directory just lives on it. Check there is room for the volumes.
df -h / | tail -1
mkdir -p "$STATE"

echo "== host network =="
modprobe nf_conntrack
echo 1 > /proc/sys/net/ipv4/ip_forward
# nibrunner renders its own nft table; anything hand-written must live in a different one.
nft list tables 2>/dev/null || true

echo "== layout =="
mkdir -p "$STATE"/{guest,artifact-store,snapshots}
install -m 0755 "$SRC/target/release/nibrunnerd" /usr/local/bin/nibrunnerd
cp "$SRC/guest/vmlinux" "$STATE/guest/"
cp "$OUT/rootfs.ext4" "$STATE/guest/rootfs.ext4"
cp /root/todos "$STATE/artifact-store/todos"
install -m 0755 /root/stage/ramp2.sh /root/stage/gen-desired.py /root/

python3 - <<'PY'
import json, hashlib, pathlib, os
src = pathlib.Path(os.environ.get("SRC", "/root/nibrunner")) / "guest/manifest.json"
guest = pathlib.Path("/var/lib/nibrunner/guest")
m = json.loads(src.read_text())
rootfs = (guest / "rootfs.ext4").read_bytes()
init = (pathlib.Path(os.environ.get("SRC", "/root/nibrunner"))
        / "target/x86_64-unknown-linux-musl/release/nibrunner-init").read_bytes()
m["version"] = m["version"].split("+")[0] + "+nibrunner-init"
for a in m["artifacts"]:
    if a["name"] == "rootfs.ext4":
        a["bytes"], a["sha256"] = len(rootfs), hashlib.sha256(rootfs).hexdigest()
m["inputs"]["init_sha256"] = hashlib.sha256(init).hexdigest()
m["inputs"]["init_is_stub"] = False
(guest / "manifest.json").write_text(json.dumps(m, indent=2) + "\n")
print("guest image:", m["version"], "| init_is_stub:", m["inputs"]["init_is_stub"])
PY

mkdir -p /etc/nibrunner
cat > /etc/nibrunner/config.toml <<TOML
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

echo '{"hostId":"bench-host","volumes":[],"instances":[],"checkpoints":[],"exports":[]}' > "$STATE/desired.json"
cp "$SRC/deploy/nibrunnerd.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now nibrunnerd
sleep 3
systemctl is-active nibrunnerd
journalctl -u nibrunnerd --no-pager -o cat | grep -m1 "nibrunnerd starting"
