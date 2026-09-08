#!/usr/bin/env bash
# Lays out the nibrunner host from what was built here, with the new guest image.
set -euo pipefail
SRC=/home/ubuntu/nibrunner
OUT=/home/ubuntu/guest-image
STATE=/var/lib/nibrunner
PROXY_PORT=8080

echo "== instance store =="
nvme=$(lsblk -dn -o NAME,MODEL | awk '/Instance Storage/ {print $1; exit}')
if [[ -n "${nvme:-}" ]] && ! mountpoint -q "$STATE"; then
  mkfs.ext4 -F -q "/dev/$nvme"; mkdir -p "$STATE"; mount "/dev/$nvme" "$STATE"
fi
df -h "$STATE" | tail -1

echo "== host network =="
modprobe nf_conntrack
echo 1 > /proc/sys/net/ipv4/ip_forward

echo "== layout =="
mkdir -p "$STATE"/{guest,artifact-store,snapshots}
install -m 0755 "$SRC/target/release/nibrunnerd" /usr/local/bin/nibrunnerd
cp "$SRC/guest/vmlinux" "$STATE/guest/"
cp "$OUT/rootfs.ext4" "$STATE/guest/rootfs.ext4"
cp /home/ubuntu/todos "$STATE/artifact-store/todos"
install -m 0755 /tmp/stage/ramp2.sh /tmp/stage/gen-desired.py /root/

echo "== manifest describes what we actually built =="
python3 - <<'PY'
import json, hashlib, pathlib
src = pathlib.Path("/home/ubuntu/nibrunner/guest/manifest.json")
guest = pathlib.Path("/var/lib/nibrunner/guest")
m = json.loads(src.read_text())
rootfs = (guest / "rootfs.ext4").read_bytes()
init = pathlib.Path("/home/ubuntu/nibrunner/target/x86_64-unknown-linux-musl/release/nibrunner-init").read_bytes()
m["version"] = m["version"].split("+")[0] + "+nibrunner-init"
for a in m["artifacts"]:
    if a["name"] == "rootfs.ext4":
        a["bytes"], a["sha256"] = len(rootfs), hashlib.sha256(rootfs).hexdigest()
m["inputs"]["init_sha256"] = hashlib.sha256(init).hexdigest()
m["inputs"]["init_is_stub"] = False
(guest / "manifest.json").write_text(json.dumps(m, indent=2) + "\n")
print("version:", m["version"], "| init_is_stub:", m["inputs"]["init_is_stub"])
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
control_plane_cidrs_v4 = []
control_plane_cidrs_v6 = []

[proxy.http]
port = $PROXY_PORT
TOML

echo '{"hostId":"bench-host","volumes":[],"instances":[],"checkpoints":[],"exports":[]}' > "$STATE/desired.json"
cp "$SRC/deploy/nibrunnerd.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now nibrunnerd
sleep 3
systemctl is-active nibrunnerd

echo "== boot one tenant on the new image =="
DIGEST=$(sha256sum "$STATE/artifact-store/todos" | cut -d' ' -f1)
SIZE=$(stat -c%s "$STATE/artifact-store/todos")
python3 /root/gen-desired.py 1 --digest "$DIGEST" --size "$SIZE" --memory-mib 256 --state running --out "$STATE/desired.json"
for i in $(seq 1 40); do
  state=$(jq -r '.instances[0].state // "none"' "$STATE/reported.json" 2>/dev/null)
  [[ "$state" == "running" || "$state" == "failed" ]] && break
  sleep 3
done
echo "instance state: ${state:-unknown}"
echo "--- console ---"
tail -8 /run/nibrunner/vm-app-1.console 2>/dev/null
echo "--- endpoints ---"
curl -s -m 20 -w "\nPOST %{http_code} in %{time_total}s\n" -X POST http://127.0.0.1:8080/todos \
  -H "Host: app-1.bench.local" -H 'content-type: application/json' -d '{"title":"metal"}'
curl -s -m 20 -w "\nGET %{http_code}\n" http://127.0.0.1:8080/todos -H "Host: app-1.bench.local"
