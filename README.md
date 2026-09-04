<div align="center">
  <h1>nibrunner</h1>
  <p><em>One binary that turns a Linux machine into an app host.</em></p>
</div>

A single static daemon. Give it a machine with `/dev/kvm` and a JSON file, and it runs each
app in its own Firecracker microVM with a persistent disk, behind an HTTP reverse proxy, with
idle apps snapshotted to disk and woken by their next request.

It is a Rust reimplementation of the app-host half of [nibrun](https://github.com/ilbertt/nibrun).
That codebase made every hard decision; this one carries them across. Where it does something
differently, [DECISIONS.md](DECISIONS.md) says what and why.

## The shape of it

**Level-triggered, never commanded.** There is no start endpoint and no stop endpoint. The daemon
reads one document describing what should be true of the host, compares it with what the host is
observed to be doing, and closes the difference. A missed message, a daemon restart and a control
plane restart are all non-events: the next read re-reads the truth.

**One file in, one file out.** `desired.json` is watched; `reported.json` is written. Nothing has
to hold a connection to this daemon to tell it something or to find out what it is doing, and a
daemon that is not running still leaves the last thing it observed behind. A remote control plane
is an addon that polls its endpoint and writes the same file.

**Nothing this daemon does stops a tenant.** Each microVM runs in a session of its own, adopted
from a pidfile on the way back up. Restarting the daemon, or killing it, leaves every app serving.

## From a fresh Ubuntu machine to a served app

```bash
sudo apt-get install -y nftables e2fsprogs                     # the only two host tools
sudo modprobe nf_conntrack && echo 1 | sudo tee /proc/sys/net/ipv4/ip_forward
curl -fsSL -o nibrunnerd <your build of target/x86_64-unknown-linux-musl/release/nibrunnerd>
sudo install -m 0755 nibrunnerd /usr/local/bin/nibrunnerd
sudo mkdir -p /var/lib/nibrunner/guest && sudo cp vmlinux rootfs.ext4 manifest.json /var/lib/nibrunner/guest/
sudo install -m 0644 deploy/nibrunnerd.service /etc/systemd/system/
sudo systemctl enable --now nibrunnerd
sudo cp my-server /var/lib/nibrunner/artifact-store/   # the binary, named by its sha256
sudo tee /var/lib/nibrunner/desired.json < desired.json # the document below
```

Ten commands, and the tenth is the deploy. Everything after it is the daemon converging.

Without systemd, `just run-dev` does the same under `./.nibrunner-dev`.

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
        "hasExtraPublicPort": false,
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

## Configuration

Everything is read once from the environment. A host that names nothing gets a working default.

| Variable | Default | What it is |
| --- | --- | --- |
| `NIBRUNNER_STATE_DIR` | `/var/lib/nibrunner` | Where everything this host keeps lives |
| `NIBRUNNER_RUNTIME_DIR` | `/run/nibrunner` | Sockets and pidfiles that outlive the daemon |
| `NIBRUNNER_DESIRED_STATE_FILE` | `<state>/desired.json` | The document it watches |
| `NIBRUNNER_GUEST_IMAGE_DIR` | `<state>/guest` | `vmlinux`, `rootfs.ext4`, `manifest.json` |
| `NIBRUNNER_ARTIFACT_STORE_URL` | `<state>/artifact-store` | A directory, or `s3://bucket/prefix` |
| `NIBRUNNER_SNAPSHOT_DIR` | `<state>/snapshots` | Where a sleeping app's memory goes |
| `NIBRUNNER_PROXY_HTTP_PORT` | none | Serve plain HTTP on this port |
| `NIBRUNNER_PROXY_HTTPS_PORT` | none | With `NIBRUNNER_TLS_CERTIFICATE` and `NIBRUNNER_TLS_KEY` |
| `NIBRUNNER_CONTROL_PLANE_CIDRS_V4` | none | Ranges a guest is denied by name |
| `NIBRUNNER_CONTROL_PLANE_CIDRS_V6` | none | The same, where no blanket rule covers them |
| `NIBRUNNER_PORT_RELAY_PUBLIC_IPV4` | none | Where an app's own public port is reached |
| `NIBRUNNER_LOG` | `info` | `tracing` filter |

## The workspace

| Crate | What is in it |
| --- | --- |
| `crates/protocol` | The wire documents, field for field from nibrun's `packages/protocol` |
| `crates/guest-contract` | Drive order, kernel args, vsock ports, `instance.env`, the frame codecs |
| `crates/nft-render` | Slot arithmetic, the ruleset rendered whole, the parsers for what `nft` answers |
| `crates/nibrunnerd` | The daemon |
| `guest/` | `vmlinux` and `rootfs.ext4`, copied from nibrun with their manifest |

## Testing

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against recording services. The second is the only place a ruleset load, a real `mke2fs` or a tap
is ever considered proven.

### What phase 1 proved, and what it did not

Run on a real Linux kernel, as root, with `nft`, `mke2fs` and `/dev/net/tun`:

- **The isolation ruleset loads and the kernel holds it.** Read back from the kernel, not from
  what was sent: the metadata endpoint, guest-to-guest, guest-to-host and the named control-plane
  ranges are all `reject`; the DNAT reaches the guest's HTTP port; egress masquerades; nothing is
  `drop`. nibrun's own notes list this as rendered and asserted as text, never loaded.
- **A tap is created, addressed, brought up and given its neighbour entry**, and a second pass
  changes nothing. Also listed there as never run.
- **A volume is formatted by the real `mke2fs`** and a converged host does not reformat it.
- **The embedded hypervisor runs and names the version this build pins.**
- **A whole pass converges**: the watched document is noticed, the volume provisioned, the
  artifact fetched and verified against its digest, both squashfs images built, the config drive
  and machine description staged, the slot allocated, the ruleset applied, and the proxy answers
  503 with a sentence for a hostname it holds and 404 for one it does not.

### What phase 2 proved, and what it did not

The lane above has no `/dev/kvm`. The boot, the sleep and the wake were run instead on an
`m7i-flex.large` with `NestedVirtualization` enabled, which is an ordinary shared instance and not
a metal one, against a tenant written for the occasion: a static binary that listens on the port
it is handed, writes to both streams, and counts its own starts into a file on its data volume.

- **A tenant cold-boots.** The artifact is fetched, verified against its digest, packed into a
  squashfs, given a config drive and a data volume, and the guest comes up and answers.
- **An idle app sleeps.** After 84 seconds of no traffic the sweep paused the microVM and wrote a
  snapshot in 1.2 seconds; the memory file is the guest's whole 256 MiB.
- **The next request wakes it, from the snapshot and not from scratch.** A restore took 20 ms on
  the first burst and 37 ms on the second. The tenant's answer still named the boot it was
  snapshotted in, and that number is incremented once per process start and kept on the volume —
  so a cold boot would have named the next one. The same file is what says the disk persists.
- **A burst is one wake.** Ten concurrent requests to a sleeping app produced one restore and ten
  `200`s, and the nine behind the first waited on its wake rather than racing it.
- **A snapshot is restored at most once.** The snapshot directory is gone after the restore, so
  the invariant is enforced on disk and not only in the record.
- **A daemon restart does not disturb a tenant.** Stopping the daemon leaves every microVM
  running, and the daemon that replaces it adopts them from their pidfiles.
- **A quiet host costs nothing.** 50 ms of CPU over 60 idle seconds, with one app asleep.

Two defects survived the whole unit lane and were found only here. An app the document says is
`running` was refused with a connection error rather than answered, because its slot is allocated
by the same pass that starts it and the activator that owns its port had already been placed. And
the count of requests coalesced onto one wake was read before the wake rather than after it, so a
burst that entirely waited on one restore reported that none had. Both now have tests.

**The guest side of that run is not reproducible from this repository.** The image in `guest/` is
nibrun's stub build, whose manifest says `"init_is_stub": true`: it carries a throwaway `/init`
rather than the runtime that boots a tenant. That stub was replaced, for the run above, by a
stand-in `/init` that mounts the drives in the order this daemon writes them, exports the
`NIBRUN_*` environment and execs the artifact — and that stand-in's source was not kept. So what
phase 2 proves is this daemon's half of the guest contract, exercised by something that satisfies
the other half. Rebuilding a published image with a real init is what would make the lane
repeatable, and it is the first thing to do before trusting any of these numbers twice.

**What is otherwise still unknown.** Nothing here ran for longer than an hour, so nothing is known
about a host that has been up for a week. The vsock log and filesystem paths, the checkpoint and
export work, TLS, the port relay, and every second app on a host are untried: one app on one host
is what this proved.
