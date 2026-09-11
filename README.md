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

One thing the starter configuration will not do is serve the document below: it names a hostname,
and a host with no `[proxy.http]` has nothing to answer for it. The instance is reported `failed`
saying exactly that, rather than started where nothing could reach it. Give the host a listener
first, or drop `hostnames` to run an app that nothing outside needs to reach.

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

`desiredState` says whether an instance should be up: `running` keeps the microVM up,
`on-request` brings it up for the first deploy and lets it sleep between visitors, `stopped` takes
it down and leaves the app reachable enough to say so.

### A port beside the HTTP one

`httpPort` is what the proxy sends this app's hostnames to. An app may name ports beside it:

```json
"config": {
  "httpPort": 3000,
  "ports": [
    { "name": "ssh", "guestPort": 22 },
    { "name": "dns", "guestPort": 53 }
  ],
  ...
}
```

A port is a port: it carries whatever arrives on it, tcp or udp, the way a firewall rule for one
would. The host reads none of what crosses it, which is what lets a protocol this daemon does not
speak arrive at all. Such a port is reached at `<proxy.raw.listen_address>:<host
port>` rather than by name, because ssh sends no hostname to route on — and that address is the
one a relay reaches this host on, not the world: a port a tenant publishes to its users is
published by definition, and that is a machine of its own.

While the app sleeps, the first arrival wakes it, whichever way it came. A connection is accepted
and held, then spliced once the guest answers, so a client sees a slow banner rather than a closed
socket. A
datagram has no connection to hold, so the datagram itself is kept and delivered after the wake.

The host must name a `[proxy.raw]` section to bind such a port, and a `[proxy.http]` one to
serve a hostname. A document that asks for what this host does not serve is refused by name and
the instance is reported `failed` saying so.
### Activation

What puts an instance back to sleep, and what tells this host it is ready for a caller, are the
instance's to name. `activation` names them:

```json
{
  "desiredState": "on-request",
  "activation": {
    "sleepWhen": { "kind": "traffic-idle", "timeoutMs": 900000 },
    "readyWhen": { "kind": "boot-completed" }
  }
}
```

| `sleepWhen` | When the microVM is suspended |
| --- | --- |
| `{ "kind": "never" }` | Never. The document is the only thing that takes it down. |
| `{ "kind": "traffic-idle", "timeoutMs": N }` | Nothing has been sent to it for `N` ms. |
| `{ "kind": "max-lifetime", "ttlMs": N }` | It has been up for `N` ms, however busy it still is. |

| `readyWhen` | What a wake waits for, and what liveness is then read from |
| --- | --- |
| `{ "kind": "port-answers" }` | The health check answers on `httpPort`. The default. |
| `{ "kind": "boot-completed" }` | The microVM started. Nothing inside it is probed. |

Only an `on-request` instance may name a `sleepWhen` other than `never`: nothing on this host
would wake anything else again, and the next reconcile pass would bring it straight back up. A
document that says so is refused whole, while an operator is still watching.

`idleTimeoutMs` is the older spelling of `sleepWhen: traffic-idle`, and still means exactly that.
A document that names both is refused rather than served under whichever a loop read first, and a
document that names neither means what it has always meant: an `on-request` instance sleeps after
five minutes, and everything else does not sleep.

## Testing

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against mocked collaborators. The second is the only place a ruleset load, a real `mke2fs` or a tap
is ever considered proven.

