<div align="center">
  <h1>nibrunner</h1>
  <p><em>Self-host hundreds of microVMs on one Linux machine</em></p>
</div>

nibrunner turns your Linux machine (`/dev/kvm` required!) into microVM host and manages
wake/sleep cycles, snapshots and backups to S3, public HTTPS endpoints, resources assignation for you.
Just write the desired configuration in the JSON file nibrunner watches.

## Getting started

```bash
curl -fsSL https://raw.githubusercontent.com/ilbertt/nibrunner/main/deploy/install.sh | sh
```

That is the whole install: the packages, the release downloaded and checked against the digests it
publishes, and `nibrunnerd install` — which writes a configuration if this host has none, lays the
guest image down wherever that file says, sets the kernel settings, creates the account ZeroFS runs
as, and renders every config file and every unit.

It ends by telling you the only things it cannot do for you:

```
This host is laid out. What is left:
  systemctl daemon-reload
  systemctl enable --now nibrunnerd

It is running the configuration this binary carries: volumes as files on its own disk,
no proxy, no object store. To make it this host's —

  edit /etc/nibrunner/config.toml
  then `nibrunnerd install` again, and `systemctl restart nibrunnerd`
```

**[docs/config.md](docs/config.md) is every key in that file.** Give a host its configuration up
front instead — `deploy/config.zerofs.toml` is one for volumes in an object store and TLS behind an
edge — and all that is left is the secrets and the units.

Then deploy: the binary into `artifacts.store_url` under the key its digest names, and the document
below into `paths.desired_state_file`. Everything past that is the daemon converging.

### The document

```json
{
  "hostId": "host-1",
  "volumes": [
    { "volumeId": "vol-1", "appId": "app-1", "sizeBytes": 8589934592, "desiredState": "present" }
  ],
  "instances": [
    {
      "appId": "app-1",
      "deploymentId": "dep-1",
      "volumeId": "vol-1",
      "desiredState": "on-request",
      "artifact": {
        "digest": "<sha256 of the binary, lowercase hex>",
        "sizeBytes": 12345678,
        "objectKey": "my-server",
        "filename": "my-server"
      },
      "config": {
        "httpPort": 3000,
        "args": [],
        "environment": {},
        "resources": { "vcpuCount": 1, "memoryMib": 256 },
        "healthCheck": { "intervalMs": 5000, "timeoutMs": 2000, "gracePeriodMs": 30000, "healthyThreshold": 1, "unhealthyThreshold": 3 },
        "restartPolicy": { "maxRestarts": 5, "initialBackoffMs": 500, "maxBackoffMs": 30000, "backoffFactor": 2, "resetAfterMs": 60000 }
      },
      "hostnames": [{ "hostname": "app-1.example.com", "kind": "platform" }]
    }
  ],
  "checkpoints": [],
  "exports": []
}
```

`desiredState` is the whole of the activation policy: `running` keeps the microVM up,
`on-request` brings it up for the first deploy and lets it sleep between visitors, `stopped` takes
it down and leaves the app reachable enough to say so.

### A port beside the HTTP one

`httpPort` is what the proxy sends this app's hostnames to. An app may name one more, and one more
is the limit — an `ssh -L` carries every other port a tenant could have wanted:

```json
"config": {
  "httpPort": 3000,
  "ports": [{ "name": "ssh", "guestPort": 22, "ingress": "tcp" }],
  ...
}
```

A `tcp` port is a byte pipe: the host reads none of what crosses it, which is what lets a protocol
this daemon does not speak arrive at all. It is reached at `<ingress.listen_address>:<host port>`
rather than by name, because ssh sends no hostname to route on. While the app sleeps, the first
connection wakes it and is spliced through once it answers, so a client sees a slow banner rather
than a closed socket.

The host must name a `[proxy.passthrough]` section to bind such a port, and a `[proxy.http]` one to
serve a hostname. A document that asks for what this host does not serve is refused by name and
the instance is reported `failed` saying so.

## Testing

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against mocked collaborators. The second is the only place a ruleset load, a real `mke2fs` or a tap
is ever considered proven.

