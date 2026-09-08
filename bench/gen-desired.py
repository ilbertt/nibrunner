#!/usr/bin/env python3
"""Write a desired.json holding N identical todos apps, all on one binary."""
import argparse, json, pathlib, sys


parser = argparse.ArgumentParser()
parser.add_argument("count", type=int)
parser.add_argument("--digest", required=True)
parser.add_argument("--size", type=int, required=True)
parser.add_argument("--object-key", default="todos")
parser.add_argument("--memory-mib", type=int, default=256)
parser.add_argument("--volume-mib", type=int, default=64)
parser.add_argument("--state", default="running", choices=["running", "on-request", "stopped"])
parser.add_argument("--out", default="desired.json")
args = parser.parse_args()

volumes, instances = [], []
for index in range(1, args.count + 1):
    app_id = f"app-{index}"
    volumes.append({
        "volumeId": f"vol-{index}",
        "appId": app_id,
        "sizeBytes": args.volume_mib * 1024 * 1024,
        "desiredState": "present",
    })
    instances.append({
        "appId": app_id,
        "deploymentId": f"dep-{index}",
        "volumeId": f"vol-{index}",
        "desiredState": args.state,
        "artifact": {
            "digest": args.digest,
            "sizeBytes": args.size,
            "objectKey": args.object_key,
            "filename": args.object_key,
        },
        "config": {
            "httpPort": 3000,
            "hasExtraPublicPort": False,
            "args": [],
            "environment": {},
            "resources": {"vcpuCount": 1, "memoryMib": args.memory_mib},
            "healthCheck": {
                "path": "/health",
                "intervalMs": 5000,
                "timeoutMs": 2000,
                "gracePeriodMs": 60000,
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
        },
        "hostnames": [{"hostname": f"{app_id}.bench.local", "kind": "platform"}],
    })

document = {
    "hostId": "bench-host",
    "volumes": volumes,
    "instances": instances,
    "checkpoints": [],
    "exports": [],
}
pathlib.Path(args.out).write_text(json.dumps(document, indent=2) + "\n")
print(f"{args.out}: {args.count} apps at {args.memory_mib} MiB, {args.volume_mib} MiB volumes, desiredState={args.state}", file=sys.stderr)
