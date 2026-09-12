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

That installs the packages and the release — checked against the digests it publishes — and runs
`nibrunnerd install`, which lays the host out from `/etc/nibrunner/config.toml`: the guest image,
the kernel settings, ZeroFS if that file asks for it, and every unit. A host with no such file is
given one. It starts nothing, and ends with what is left:

```
Laid out; nothing started. What is left:

  1. edit /etc/nibrunner/config.toml
     written just now: volumes as files on this disk, plain HTTP on :80, nothing in an
     object store. Every key: https://github.com/ilbertt/nibrunner/blob/main/docs/config.md
  2. edit /etc/nibrunner/host.env
     the secrets that configuration needs, each named in the file
  3. nibrunnerd start
```

`nibrunnerd start` lays the host out again from the file as edited, refuses while a secret it
needs is still empty, and then starts — or restarts — the units that file names. It is also every
later change: edit, `nibrunnerd start`.

**[docs/config.md](docs/config.md) is every key in that file.** `deploy/config.example.toml` is
one with every section: volumes in an object store, TLS behind an edge, raw ports, metrics. A host
given its configuration before the script runs is asked only for the secrets it needs.

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
      "layers": [
        {
          "kind": "executable",
          "destinationPath": "/app/server",
          "digest": "<sha256 of the binary, lowercase hex>",
          "objectKey": "my-server"
        }
      ],
      "config": {
        "httpPort": 3000,
        "command": { "program": "/app/server", "args": [], "workingDirectory": "/app", "environment": {} },
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

### Layers, and what runs in them

The root a microVM's program sees is a stack, bottom to top:

```
the volume        writable, and the only thing backed up
layers[n-1]
…                 the document's layers, in the order it lists them
layers[0]
Debian            the guest image nibrunner ships, always at the bottom
```

Each layer is an object in the store named by its digest and a `kind` saying what the object is.
An `executable` is one program, which the host packs into an image at `destinationPath`; a
`filesystem` is an image already — a squashfs or ext4 — attached as it was uploaded. A layer is
fetched once per host and cached by digest, so ten apps on the same base hold it once.

The volume is not mounted anywhere in particular: it is the writable top of the whole root, so
every write anywhere lands on it and persists, and nothing else does. An export is the volume's
contents and the environment — never what a layer already holds.

`command` is what the guest's init runs in that root once it is stacked: `program` with `args`,
in `workingDirectory`, with `environment`, as uid 65534. The working directory is made if no
layer holds it, given to that uid, and is where the program's persistent state lives — it is the
one place under the root the program can write besides `/tmp`. The program cannot reach the
guest's init, the volume's own bookkeeping, or the host: it is 65534 in a root it cannot leave.

### What a volume starts with

A volume is formatted empty the first time the document names it. One that should not be names
an archive in the store and where to unpack it:

```json
"volumes": [
  {
    "volumeId": "vol-1", "appId": "app-1", "sizeBytes": 8589934592, "desiredState": "present",
    "initialContents": {
      "digest": "<sha256 of the archive, lowercase hex>",
      "objectKey": "seeds/app-1",
      "destinationPath": "/app/data"
    }
  }
]
```

The archive is a tar, gzipped or not — what `tar -cz` writes. Its entries land under
`destinationPath` as the program will see them, so an entry `nested/hello.txt` is
`/app/data/nested/hello.txt` in the root above, and they are the program's: the directory and
everything unpacked into it are given to uid 65534, whatever the archive said, with set-id bits
dropped. The directories above it are made the way init makes the working directory. An entry
that reaches outside the directory it is unpacked into fails the volume by name, and so does an
archive that does not fit.

The copy happens as the volume is formatted, which is once: a volume already formatted is its
app's, and a document that changes or drops `initialContents` on one changes nothing about it.
The contents are in the volume rather than under it, so an export carries them like anything
else the app wrote, and a layer that holds a file at the same path is shadowed by the volume's
copy the way it would be by a write. An export's bundle is itself such an archive, with the
volume under `data/` and the environment in `.env`: to start a volume from one, give `/` and an
archive of what is under `data/`.

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

### The schema

[`desired-state.schema.json`](crates/protocol/schema/desired-state.schema.json) is the document
above as a JSON Schema (draft 2020-12), and
[`reported-state.schema.json`](crates/protocol/schema/reported-state.schema.json) is the one the
daemon writes back to `reported.json` in its state directory. Both are generated from the Rust
types in `crates/protocol`, so a tool built against them is built against what the daemon parses.
An editor will complete and check a document that names one:

```json
{
  "$schema": "https://raw.githubusercontent.com/ilbertt/nibrunner/main/crates/protocol/schema/desired-state.schema.json",
  "hostId": "host-1",
  ...
}
```

`just schema` writes them afresh from the code, and `just check-schema` fails when what is checked
in is behind it — CI runs the latter on every pull request.

## Testing

`mise install` puts `just` on the path at the version `mise.toml` pins.

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against mocked collaborators. The second is the only place a ruleset load, a real `mke2fs` or a tap
is ever considered proven.

