#!/usr/bin/env bash
# Builds guest/rootfs.ext4 from the pins in guest/manifest.json, with crates/init as /init,
# and rewrites the manifest to describe what came out. Linux, root, docker and e2fsprogs.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
work=${WORK:-$(mktemp -d)}
init=$root/target/x86_64-unknown-linux-musl/release/nibrunner-init

[[ -x $init ]] || { echo "build crates/init first: cargo build -p nibrunner-init --target x86_64-unknown-linux-musl --release" >&2; exit 1; }

read -r base snapshot epoch < <(python3 -c "
import json
i = json.load(open('$here/manifest.json'))['inputs']
print(i['debian_image'], i['debian_snapshot'], i['source_date_epoch'])
")

# http, not https: the base image carries no ca-certificates yet, and the archive is signed.
cat > "$work/Dockerfile" <<DOCKER
FROM $base
RUN printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$snapshot/ trixie main\n' \
      > /etc/apt/sources.list \
 && rm -f /etc/apt/sources.list.d/*.sources \
 && apt-get -o Acquire::Check-Valid-Until=false update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && apt-get clean \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/* /usr/share/doc /usr/share/man /usr/share/locale
DOCKER
docker build --platform linux/amd64 -t nibrunner-guest:build "$work" >/dev/null

container=$(docker create --platform linux/amd64 nibrunner-guest:build /bin/true)
mkdir -p "$work/rootfs"
docker export "$container" | tar -x -C "$work/rootfs"
docker rm "$container" >/dev/null

# The guest mounts these, and its root is ro while ensure_directory is a bare mkdir that only
# tolerates EEXIST, so every target has to be in the image already — one per layer a document
# may name, which is protocol::MAX_LAYERS.
for directory in dev proc sys run tmp mnt mnt/base mnt/volume mnt/root run/config \
                 mnt/layers/0 mnt/layers/1 mnt/layers/2 mnt/layers/3 \
                 mnt/layers/4 mnt/layers/5 mnt/layers/6 mnt/layers/7; do
    mkdir -p "$work/rootfs/$directory"
done
chmod 1777 "$work/rootfs/tmp"

install -m 0755 "$init" "$work/rootfs/init"
find "$work/rootfs" -exec touch -h -d "@$epoch" {} +

# -d populates from a directory, so this host never mounts the image it is writing.
content=$(du -sb "$work/rootfs" | cut -f1)
truncate -s $(( (content * 3 / 2 + 8 * 1024 * 1024) / 1048576 * 1048576 )) "$here/rootfs.ext4"
mke2fs -t ext4 -F -q -m 0 -E root_owner=0:0 -d "$work/rootfs" "$here/rootfs.ext4"

python3 - "$here" "$init" <<'PY'
import hashlib, json, pathlib, sys
guest, init = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
manifest = json.loads((guest / "manifest.json").read_text())
image = (guest / "rootfs.ext4").read_bytes()
manifest["version"] = manifest["version"].split("+")[0] + "+nibrunner-init"
manifest["artifacts"] = [a for a in manifest["artifacts"] if a["name"] != "rootfs.ext4"]
manifest["artifacts"].append({
    "name": "rootfs.ext4",
    "bytes": len(image),
    "sha256": hashlib.sha256(image).hexdigest(),
})
manifest["inputs"]["init_sha256"] = hashlib.sha256(init.read_bytes()).hexdigest()
(guest / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(manifest["version"], len(image), "bytes")
PY
