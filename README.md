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

That is the install. It refuses a machine that cannot host one, puts the packages on, takes the
newest release and checks the binary against the digests it publishes, and hands over to
`nibrunnerd install` — which fetches the guest image, sets the kernel settings this host serves
nothing without, creates the account ZeroFS runs under, renders its two configuration files and
every unit, and stamps what it laid down into `versions.json`.

A host with no configuration is given the smallest one this daemon accepts and told to make it its
own. That is the one thing nothing can do for you, because it is where this host's storage,
hostnames and certificates are: **[docs/config.md](docs/config.md) is every key in it.** Then

```bash
nibrunnerd install
```

again, and it says what is left — the secrets, which only you hold, and the units to enable.

Deploying is two files after that: the binary into `artifacts.store_url` under the key its digest
names, and the document below into `paths.desired_state_file`. Everything past that is the daemon
converging.

`NIBRUNNER_VERSION` pins a release rather than taking the newest, and `NIBRUNNER_CONFIG` names a
configuration somewhere other than `/etc/nibrunner/config.toml`. Nothing has to be repeated after a
reboot: the units are enabled and the kernel settings are written where the boot reads them.

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

## Testing

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against mocked collaborators. The second is the only place a ruleset load, a real `mke2fs` or a tap
is ever considered proven.

