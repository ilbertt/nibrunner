#!/usr/bin/env bash
# Downloads every preset, checks the digest its release publishes, unpacks the two that ship
# as archives, and writes apps.json describing the exact bytes a host will fetch. nibrunnerd
# takes one file per artifact and packs it as /server, so an archive has to be opened here.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=${WORK:-/root/presets}
mkdir -p "$work/download" "$work/binary"

python3 - "$here/presets.json" "$work" <<'PY'
import hashlib, json, pathlib, subprocess, sys, tarfile, zipfile

presets = json.loads(pathlib.Path(sys.argv[1]).read_text())
work = pathlib.Path(sys.argv[2])
apps = {}

for slug, preset in presets.items():
    archive = work / "download" / pathlib.Path(preset["url"]).name
    if not archive.exists():
        subprocess.run(["curl", "-fsSL", "-o", str(archive), preset["url"]], check=True)
    downloaded = hashlib.sha256(archive.read_bytes()).hexdigest()

    published = preset.get("sha256")
    if published and published != downloaded:
        sys.exit(f"{slug}: release publishes {published}, downloaded {downloaded}")

    binary = work / "binary" / slug
    kind = preset.get("archive")
    if kind is None:
        binary.write_bytes(archive.read_bytes())
    else:
        opened = work / "unpacked" / slug
        opened.mkdir(parents=True, exist_ok=True)
        if kind == "zip":
            with zipfile.ZipFile(archive) as bundle:
                bundle.extractall(opened)
        else:
            with tarfile.open(archive) as bundle:
                bundle.extractall(opened)
        found = [p for p in opened.rglob(preset["member"]) if p.is_file()]
        if len(found) != 1:
            sys.exit(f"{slug}: {len(found)} files named {preset['member']} in the archive")
        binary.write_bytes(found[0].read_bytes())
    binary.chmod(0o755)

    apps[slug] = {
        "digest": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "sizeBytes": binary.stat().st_size,
        "objectKey": f"artifacts/{slug}",
        "filename": slug,
        "publishedSha256": published,
        "downloadedSha256": downloaded,
    }
    verified = "release digest" if published else "no digest published"
    print(f"{slug:16} {apps[slug]['sizeBytes']:>10} bytes  {apps[slug]['digest'][:16]}…  ({verified})")

(work / "apps.json").write_text(json.dumps(apps, indent=2) + "\n")
print(f"\n{work / 'apps.json'}: {len(apps)} apps")
PY
