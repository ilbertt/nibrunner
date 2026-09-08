#!/usr/bin/env python3
"""Write a desired.json holding the named presets, each in the state it was named with.

    gen-presets.py pocketbase=running gitea=on-request boop=absent

A slug left out is absent from the document entirely, which is how a host is emptied. `absent`
keeps the volume entry and marks it for deletion, which is how one app is removed while the
rest carry on. Secrets a preset names without a value are generated once and kept, so moving
one app between states never rewrites another app's environment.
"""
import argparse, hashlib, json, pathlib, secrets, sys

STATES = ("running", "on-request", "stopped", "absent")

parser = argparse.ArgumentParser()
parser.add_argument("apps", nargs="*", metavar="SLUG=STATE")
parser.add_argument("--presets", default="/root/presets.json")
parser.add_argument("--manifest", default="/root/presets/apps.json")
parser.add_argument("--secrets", default="/root/presets/secrets.json")
parser.add_argument("--domain", default="canister.site")
parser.add_argument("--host-id", default="canister-host")
parser.add_argument("--memory-mib", type=int, default=512)
parser.add_argument("--volume-gib", type=int, default=2)
parser.add_argument("--idle-timeout-ms", type=int, default=120000)
parser.add_argument("--grace-period-ms", type=int, default=120000)
parser.add_argument("--out", default="/var/lib/nibrunner/desired.json")
args = parser.parse_args()

presets = json.loads(pathlib.Path(args.presets).read_text())
manifest = json.loads(pathlib.Path(args.manifest).read_text())

secrets_path = pathlib.Path(args.secrets)
kept = json.loads(secrets_path.read_text()) if secrets_path.exists() else {}

wanted = {}
for pair in args.apps:
    slug, _, state = pair.partition("=")
    if slug not in presets:
        sys.exit(f"unknown preset {slug!r}; have {', '.join(presets)}")
    if state not in STATES:
        sys.exit(f"{slug}: state must be one of {', '.join(STATES)}")
    wanted[slug] = state

volumes, instances = [], []
for slug, state in wanted.items():
    preset = presets[slug]
    volumes.append({
        "volumeId": f"vol-{slug}",
        "appId": slug,
        "sizeBytes": args.volume_gib * 1024**3,
        "desiredState": "absent" if state == "absent" else "present",
    })
    if state == "absent":
        continue

    environment = {}
    for name, value in preset["env"].items():
        if value is None:
            kept.setdefault(slug, {}).setdefault(name, secrets.token_urlsafe(24))
            value = kept[slug][name]
        environment[name] = value

    artifact = manifest[slug]
    config = {
        "httpPort": preset["port"],
        "hasExtraPublicPort": preset["extraPublicPort"],
        "args": preset["args"],
        "environment": environment,
        "resources": {"vcpuCount": 1, "memoryMib": args.memory_mib},
        "healthCheck": {
            "intervalMs": 5000,
            "timeoutMs": 2000,
            "gracePeriodMs": args.grace_period_ms,
            "healthyThreshold": 1,
            "unhealthyThreshold": 3,
        },
        "restartPolicy": {
            "maxRestarts": 5,
            "initialBackoffMs": 500,
            "maxBackoffMs": 30000,
            "backoffFactor": 2,
            "resetAfterMs": 60000,
        },
    }
    # A deployment is the pair of what runs and how, so the id moves only when one of them does
    # and an unrelated app changing state never looks like a redeploy.
    shape = hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest()
    instance = {
        "appId": slug,
        "deploymentId": f"dep-{slug}-{artifact['digest'][:8]}-{shape[:8]}",
        "volumeId": f"vol-{slug}",
        "desiredState": state,
        "artifact": {k: artifact[k] for k in ("digest", "sizeBytes", "objectKey", "filename")},
        "config": config,
        "hostnames": [{"hostname": f"{slug}.{args.domain}", "kind": "platform"}],
    }
    if state == "on-request":
        instance["idleTimeoutMs"] = args.idle_timeout_ms
    instances.append(instance)

secrets_path.parent.mkdir(parents=True, exist_ok=True)
secrets_path.write_text(json.dumps(kept, indent=2) + "\n")
secrets_path.chmod(0o600)

document = {
    "hostId": args.host_id,
    "volumes": volumes,
    "instances": instances,
    "checkpoints": [],
    "exports": [],
}
out = pathlib.Path(args.out)
out.write_text(json.dumps(document, indent=2) + "\n")

summary = ", ".join(f"{slug}={state}" for slug, state in wanted.items()) or "nothing"
print(f"{out}: {len(instances)} instances, {len(volumes)} volumes — {summary}", file=sys.stderr)
