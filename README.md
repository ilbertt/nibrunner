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
daemon that is not running still leaves the last thing it observed behind. Whatever writes
`desired.json` is not this daemon's concern.

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

One TOML file, read once at startup and validated whole. `/etc/nibrunner/config.toml` unless
`NIBRUNNER_CONFIG` names another — the difference being that a file named deliberately must exist,
where the default is allowed to be absent. A host that names nothing gets a working default, so an
empty file and no file are the same host.

A key this daemon does not have is an error that names the key, which is the thing an environment
could never do: a mistyped variable and one nobody set are the same absence, so the typo silently
takes the default. The same goes for the values — a relative path, a CIDR `nft` would choke on, an
`s3://` URL with no bucket, or a proxy port that an app slot is going to want are all refused
while an operator is still watching rather than on the pass that first needed them.

`deploy/config.toml` is a copy with every key at its default. What each one is:

| Key | Default | What it is |
| --- | --- | --- |
| `paths.state_dir` | `/var/lib/nibrunner` | Where everything this host keeps lives, `state.db` included |
| `paths.runtime_dir` | `/run/nibrunner` | Sockets and pidfiles that outlive the daemon |
| `paths.desired_state_file` | `<state>/desired.json` | The document it watches |
| `paths.guest_image_dir` | `<state>/guest` | `vmlinux`, `rootfs.ext4`, `manifest.json` |
| `paths.snapshot_dir` | `<state>/snapshots` | Where a sleeping app's memory goes |
| `artifacts.store_url` | `<state>/artifact-store` | A directory, or `s3://bucket/prefix` |
| `volumes.backend` | `local-file` | `local-file` or `zerofs` |
| `volumes.store_url` | none | Where a volume's blocks live, for an object-store backend |
| `volumes.storage_prefix` | `volumes` | Prepended to every key this host writes |
| `volumes.zerofs.*` | see below | Only read when the backend is `zerofs` |
| `proxy.http_port` | none | Serve plain HTTP on this port |
| `proxy.https_port` | none | With `proxy.tls_certificate` and `proxy.tls_key` |
| `proxy.tls_client_ca` | none | A PEM trust pool. Naming one makes a caller's own certificate the price of the handshake |
| `proxy.port_relay_public_ipv4` | none | Where an app's own public port is reached |
| `metrics.port` | none | Serve Prometheus metrics on this port |
| `metrics.listen_address` | `127.0.0.1` | Where to bind them, when a port is named |
| `network.control_plane_cidrs_v4` | `[]` | Ranges a guest is denied by name |
| `network.control_plane_cidrs_v6` | `[]` | The same, where no blanket rule covers them |
| `exports.store_url` | `<state>/export-store` | Where a finished bundle goes |
| `exports.staging_dir` | `<state>/exports` | Where one is assembled, and removed after |

### The loops

Three, on every host:

| Loop | What it does |
| --- | --- |
| converge | Watches the document and runs a reconcile pass when it moves |
| status | Probes health, applies the ruleset and the routes, writes `reported.json` |
| measurement | Records activity, lets quiet apps sleep, then measures every guest |

There is one input and it is the file. Nothing polls anything, and nothing may tell this daemon
what to do except by writing that document.

`NIBRUNNER_LOG` is still an environment variable, and the only one besides `NIBRUNNER_CONFIG`: it
is a `tracing` filter an operator changes to debug one restart, not a property of the host.

### Browsing a tenant's files

A listing comes from a `readdir` *inside* the microVM, against the filesystem the tenant has
mounted — so it shows what is there rather than what had reached the block device by the last
flush. The slot table is what scopes it: an app resolves to the single microVM this host runs for
it, so a path is only ever resolved inside the filesystem its own app owns.

Nothing on the wire is text. A path goes out behind its own length and a name comes back behind
one, because a tenant's binary created those names and ext4 allows anything in them but `/` and
NUL — a space, a quote, a newline, a leading dash. Length prefixes are what make that restriction
unnecessary rather than merely relaxed.

A failure is answered rather than swallowed: a host that stays quiet about a guest it could not
reach turns a refusal somebody could act on into a timeout they cannot. The refusal reads as a
sentence and never names the path — what a tenant keeps in their own filesystem is theirs to know.

### Exports

An export hands a tenant their data back: their filesystem, the binary that was running on it, and
the environment it ran under, as one `bundle.tar.gz` in `exports.store_url`.

The order is the guarantee. The guest is frozen over its control vsock, because only its kernel can
checkpoint the ext4 journal and `debugfs` never replays one — an unfrozen filesystem is missing
recent metadata however durable the storage under it is. Then the checkpoint, which captures *now*.
Then the freeze is released, and everything whose cost scales with the tenant's data — the read,
the archive, the upload — runs against that pinned view while they are writing again. The bundle is
still of the moment it always was, because the cut happened inside the freeze.

The filesystem is read with `debugfs`, which walks inodes in userspace. Mounting it — even
read-only — would mean asking this host's kernel to interpret tenant-controlled metadata, which is
the one thing it never does.

Checkpoints are named after the export they belong to, so a daemon killed mid-export comes back to
a name it recognises: the reap is derived from what the store says exists rather than from anything
this process remembers, which makes retrying the same code path as the first attempt. That matters
more than it sounds — while any checkpoint exists, ZeroFS pauses segment deletion, compaction and
metadata reclamation for *every* tenant on the host.

### Volumes that outlive the host

`volumes.backend = "zerofs"` puts a volume's blocks in an object store and reaches them from the
guest over NBD. The device file lives at `<mount>/.nbd/<volume-id>`, and the minor it is attached
on comes from the same slot integer as the app's tap, ports and addresses.

**ZeroFS is a service this daemon does not own and never starts.** There must be exactly one
read-write `zerofs run` per storage prefix, fleet-wide — a second writer is fenced by SlateDB's
epoch only *after* a window of acknowledging writes it then discards, so it loses a tenant's data
rather than failing to start. Whatever supervises the host is the lock; a single-instance unit is
how it is held. This daemon only ever runs its admin CLI, and there is a test asserting so.

A host on this backend needs three tools the local-file one does not: `nbd-client`, `zerofs`, and
`debugfs` for exports — that last one ships in `e2fsprogs` beside `mke2fs`, so it is no new
package. It
also holds ZeroFS's configured `[cache]` back from both the memory a guest may be given and the
disk a snapshot may go on — read from ZeroFS's own config file rather than written down twice, and
assumed rather than treated as zero when it cannot be read, because a host that promises memory
the cache will take back kills tenants.

## The workspace

| Crate | What is in it |
| --- | --- |
| `crates/protocol` | The wire documents, field for field from nibrun's `packages/protocol` |
| `crates/guest-contract` | Drive order, kernel args, vsock ports, boot paths, `instance.env`, the frame codecs — **both halves of each**, so the host and the guest share one definition rather than agreeing by review |
| `crates/nft-render` | Slot arithmetic, the ruleset rendered whole, the parsers for what `nft` answers |
| `crates/nibrunnerd` | The daemon |
| `crates/init` | The guest's PID 1: mounts, config, supervision, and the three vsock channels |
| `guest/` | `vmlinux` and `rootfs.ext4`, copied from nibrun with their manifest |

### Inside `crates/nibrunnerd`

| Folder | What may live there |
| --- | --- |
| `controllers/` | `*_controller.rs` only. One per loop, plus `lifecycle_controller.rs` for start and stop. Each is a trait, its implementation and its tests. |
| `services/` | `*_service.rs` only. Same rule: trait, implementation, tests. |
| `repositories/` | `*_repository.rs` only. Same rule. One per table. |
| `domain/` | Everything the three layers above reason with: the planner, the health machine, the backoff, the report builder, the export and browse code, the SQLite pool. |
| `adapters/` | What fills the ports on a real machine. |
| `ports.rs` | The outbound traits. |

`main.rs` is a composition of controllers and nothing else: build the host, `lifecycle.start()`, spawn every controller `lifecycle.controllers()` hands back, wait for a signal, `lifecycle.stop()`. Anything it used to do itself — loading the host, adopting microVMs, binding the activators, serving the proxy, writing the last report — is in `lifecycle_controller.rs`.

The folder is `domain/` rather than `lib/` because rustc refuses a module named `lib` beside a crate root called `lib.rs` (E0761, plus a `special_module_name` warning).

Each loop splits into a `*_once` that does one pass and a `run` that owns the timing, so what a loop decides is a unit test and the `loop {}` around it carries no logic.

## Testing

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against mocked collaborators. The second is the only place a ruleset load, a real `mke2fs` or a tap
is ever considered proven.

### How the doubles work

Every seam in this daemon is a trait, and every trait is mocked by `mockall`:

| Layer | Traits |
| --- | --- |
| `ports` | `CommandRunner`, `Vmm`, `ArtifactStore`, `LogSink` |
| `adapters` | `VolumeBackend`, `HostNetwork`, `Waker` |
| `repositories` | `InstanceRepository`, `SlotRepository`, `ActivityRepository`, `DeletedVolumeRepository`, `HostIdentityRepository` |
| `services` | `ReconcileService`, `ReportService`, `IdleService`, `UsageService`, `FilesystemService`, `ControlPlaneService`, `ExportStore` |

The mocks are generated under `#[cfg_attr(any(test, feature = "testing"), mockall::automock)]`, so
they exist for this crate's own tests and for the integration lane — which depends on the crate
with `features = ["testing"]` and therefore builds its fixtures from the same place rather than
keeping doubles of its own.

`crates/nibrunnerd/src/test_support/` is that place. `mod.rs` holds the domain fixtures, each
taking a closure so a test names only the field it is about; `mocks.rs` holds the pre-programmed
mocks and the spies that read back what they were asked. A test that only needs a working host
calls `test_host()`; one that wants to assert on what a repository was handed calls
`test_host_with(...)` with mocks of its own.

Each loop is split into a `*_once` that does one pass and a `*_loop` that owns the timing, so what
a loop decides is a unit test and the `loop {}` around it carries no logic.

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

### What phase 3 proved: the guest is ours

The image in `guest/` is nibrun's stub build, whose manifest says `"init_is_stub": true`. Phase 3
replaced that stub with `crates/init` — this repository's own PID 1 — and ran the lane again on an
`m7i-flex.large` with nested virtualisation.

- **It boots.** Mounts, config drive, DNS, artifact drive, data drive, and a tenant started as uid
  65534. Cold boot to first answer: 603 ms.
- **The whole `instance.env` contract holds end to end.** The tenant was handed
  `DERIVED=port 3000` — a `$NIBRUN_HTTP_PORT` reference the host wrote and this guest expanded —
  along with its own variables, the platform's, and the `PORT` alias.
- **Tenant output reaches the host over vsock**, both streams, correctly labelled.
- **Every filesystem verb answers**: `list`, `stat`, `read`, `write`, `mkdir`, `remove`, `usage`,
  `compute`. So do the refusals — removing a directory that still holds something is refused, and
  a path that is not there is `there is nothing at that path` rather than a failure.
- **The freeze holds and lets go.** A write blocks while the lease is held and succeeds once the
  connection drops. There is no path out of answering that connection which leaves a tenant frozen,
  which an export that failed *after* freezing then demonstrated by accident: it refused, and the
  tenant was writing again a second later.
- **All of it survives a snapshot and restore.** Slept after going quiet, 1.2-second snapshot,
  restored in 27 ms, ten concurrent requests served by one restore — and the browse, the write and
  the freeze all still answered afterwards.

Booting it found four things the port had dropped, all of them in the mounts: `EBUSY` treated as
fatal where the reference tolerates it for a pseudo-filesystem and refuses it for a drive, a
missing `noatime` on the data drive, `/dev/shm` never created, and the tmpfs modes left off. The
first of those is why the first boot rebooted at 0.17 s with nothing on the console.

**What the guest still has not done.** No export has been written, because that needs a checkpoint
and this host kept its volumes as local files — the refusal is the correct answer and is as far as
the lane went. Nothing ran for longer than an hour, no second app has ever been on a host, and the
ZeroFS backend has still never met a real ZeroFS.

**What is otherwise still unknown.** Nothing here ran for longer than an hour, so nothing is known
about a host that has been up for a week. The vsock log and filesystem paths, the checkpoint and
export work, the port relay, and every second app on a host are untried: one app on one host
is what this proved.

**What TLS has now done.** On an AX41 serving three tenants, against a self-signed CA: a real
tenant answered over HTTPS on both HTTP/2 and HTTP/1.1, ALPN negotiated `h2` where it was offered,
a hostname no app holds got the proxy's own 404, and a connection whose handshake named one app
while its request named another got a 421. With `proxy.tls_client_ca` set, a caller with no
certificate and a caller holding one from a CA the host was never told about were both refused
during the handshake — `tlsv13 alert certificate required`, before any request was read — and the
edge's own certificate was served. What that run found is the only thing the unit tests could not:
every request that arrived over HTTP/2 was answered `502`, because the version and the authority of
an h2 request were being carried onto a leg that speaks HTTP/1.1.

A fourth tenant, deployed for the run and speaking RFC 6455, closed the two remaining gaps. A
websocket opened through the TLS proxy handshook — the `Sec-WebSocket-Accept` token checked against
the key the client sent — and echoed its payload back byte for byte; the same connection with no
client certificate never got that far. And the tenant reported what it had been told: an
`x-forwarded-for` appended to rather than replaced, a leg that was `http` on the plain listener and
`https` on the TLS one, and an `x-forwarded-proto: http` written by a caller left alone even where
the connection carrying it was encrypted — which is the whole of the header policy above, read back
from inside a microVM.

Three tenants stayed up across all five daemon restarts it took.
