# Configuring a host

`nibrunnerd install` laid this host out and left you a `config.toml` to make its own. This is what
every key in that file is.

The file is at `/etc/nibrunner/config.toml` unless `NIBRUNNER_CONFIG` names another. It is read
once, at startup, and validated whole — so a host that starts is a host whose configuration was
right, and one that is wrong says which key and why before it does anything.

Re-run `nibrunnerd install` after editing it. It re-renders what it wrote, leaves what it did not
alone, and tells you which was which.

## Three rules that explain every refusal

**No key has a default.** A key its section declares but the file omits is refused by name. There
is no second copy of the answer compiled in for an absence to fall back to, so what a running host
does is what this file says.

**An unknown key is refused too.** A mistyped key and a missing one are different errors, which is
the thing an environment variable could never do for you.

**A whole *section* is what may be absent** — `[proxy.http.tls]`, `[metrics]`, `[volumes.zerofs]`. A
section that is present is filled in completely, so there is no half-configured listener to warn
about at startup because there is no way to write one.

```
volumes.backend is not specified, and nothing here is optional
volumes.zerofs is not read by the local-file backend, which is what volumes.backend says
proxy.http.port is not free, because 21000-21999 is what a slot takes for an app's loopback port
```

## Every host has these

| Key | Type | Must be |
| --- | --- | --- |
| `paths.state_dir` | string | absolute path — everything this host keeps, `state.db` included |
| `paths.runtime_dir` | string | absolute path — sockets and pidfiles that outlive the daemon |
| `paths.snapshot_dir` | string | absolute path — where a sleeping app's memory goes |
| `paths.guest_image_dir` | string | absolute path — `vmlinux`, `rootfs.ext4`, `manifest.json`, put there by `install` |
| `paths.desired_state_file` | string | absolute path — the document it watches |
| `paths.api_socket` | string | absolute path |
| `paths.versions_file` | string | absolute path — what `install` stamped what it laid down into |
| `artifacts.store_url` | string | `s3://bucket[/prefix]`, or an absolute path |
| `volumes.backend` | string | `local-file` or `zerofs` |
| `volumes.storage_prefix` | string | 1–512 bytes, no leading or trailing `/`, no empty or `.`/`..` segment |
| `exports.store_url` | string | same rule as `artifacts.store_url` |
| `exports.staging_dir` | string | absolute path — a bundle is assembled here and removed after |
| `network.control_plane_cidrs_v4` | array of string | each `a.b.c.d/n`, `n` ≤ 32 |
| `network.control_plane_cidrs_v6` | array of string | each `addr/n`, `n` ≤ 128 |

Both CIDR arrays may be empty, but the keys must be there. What they hold is the ranges a guest is
denied by name, on top of the blanket rules — a control plane a tenant must not reach.

**`volumes.storage_prefix` is one host, not one app.** Every tenant placed here shares it. Deleting
it destroys all of them.

## Where volumes live

### `backend = "local-file"`

Volumes are sparse files under the state directory. Nothing else to configure, and
**`[volumes.zerofs]` is refused** rather than ignored.

What this costs is the property the other backend exists for: a volume that outlives the machine.
The `flush` that is the durability point becomes the host's page cache rather than a service that
has to be asked, and an export is **refused** rather than written, because a checkpoint is
something only an object store can cut.

What it buys is density. A host is bounded by the 1000 loopback ports a slot takes, and one AX41
has held 752 tenants on it.

### `backend = "zerofs"`

Blocks live in an object store and are reached from the guest over NBD. `install` renders ZeroFS's
two configuration files and the two units that supervise it from the keys below, so these are the
only place any of it is said.

| Key | Type | Must be |
| --- | --- | --- |
| `binary` | string | absolute path — where `install` puts ZeroFS |
| `config_file` | string | absolute path — **rendered by `install`** |
| `mount_path` | string | absolute path — this host's own view of the filesystem |
| `nbd_socket_path` | string | absolute path |
| `ninep_socket_path` | string | absolute path |
| `rpc_socket_path` | string | absolute path |
| `storage_url` | string | `s3://bucket/prefix`, or an absolute path |
| `cache_dir` | string | absolute path |
| `cache_disk_gib` | integer | > 0, whole gibibytes |
| `cache_memory_gib` | integer | > 0, whole gibibytes |
| `checkpoint_runtime_dir` | string | absolute path |
| `checkpoint_config_file` | string | absolute path — **rendered by `install`** |
| `checkpoint_cache_dir` | string | absolute path |

**Size `cache_disk_gib` against the disk it is on.** It is a cache of the object store, and it
shares that disk with the snapshots in `paths.snapshot_dir`. A snapshot lost costs one cold boot; a
full disk breaks the filesystem every app on the host runs from, asleep or not.

**Whole gibibytes, not fractions.** The daemon holds this cache back from what any guest may be
promised, and reads it back out of the file it rendered — which truncates. A fraction would reserve
against a number ZeroFS is not taking, and a host that promises memory the cache will take back
kills tenants.

**This backend caps a host at 63 apps.** Slot *N* takes `/dev/nbdN` and the export reader holds the
last of them.

## What this host serves

Every way in is a section under `[proxy]`, and each is absent or complete.

| Section | Key | What |
| --- | --- | --- |
| `[proxy]` | `listen_address` | where every listener below binds |
| `[proxy.http]` | `port` | the one HTTP listener |
| `[proxy.http.tls]` | `certificate`, `key` | serve that port encrypted, with this material |
| `[proxy.http.tls.client_ca]` | `certificate` | a PEM trust pool |
| `[proxy.tcp]` | `max_extra_ports_per_app` | how many raw TCP ports an app may name **beside** its HTTP one |

**One HTTP listener, not a plain one and a TLS one.** Nothing here redirects, so two would serve
every app unencrypted and encrypted at once, forever, with nothing moving a visitor from the first
to the second. `[proxy.http.tls]` absent is plain HTTP, which is what a host behind an edge that
terminates TLS wants.

**This daemon does not obtain certificates.** It serves what is at the path it was given. ACME,
renewal and rate limits belong to certbot, or to the edge. It is read once, at startup, so a
renewed certificate needs `systemctl restart nibrunnerd` — safe, because nothing this daemon does
stops a tenant, and neither does its death.

**One certificate covers the whole host.** There is no SNI selection, so every hostname a tenant
holds has to be covered by this one — a wildcard, in practice, which matches one label deep:
`app.example.com` but not `a.b.example.com`.

**Naming a `client_ca` makes a caller's own certificate the price of the handshake.** On an origin
whose IP is discoverable, that is what keeps it reachable only through the edge. It also means you
cannot reach it yourself without one, so turn it on after the plain path is proven.

A connection whose handshake named one app and whose request names another gets a **421**. That is
the only thing standing between two tenants that share a certificate.

**`[proxy.tcp]` is the way in for a protocol this host does not read** — ssh, in practice. Such a
port carries bytes and nothing else, so nothing can route it by name and it is reached at a port
of its own. `max_extra_ports_per_app` is bounded by what a slot reserves beside the HTTP port, which is
seven. Absent offers none.

**A document asking for what this host does not serve is refused by name**, and the instance is
reported `failed` saying so, rather than started somewhere nothing could reach it: a hostname on a
host with no `[proxy.http]`, or more ports than `[proxy.tcp]` allows.

## Metrics

| Key | Type | Must be |
| --- | --- | --- |
| `port` | integer | a free port |
| `listen_address` | string | an IP address to bind |

The page is rendered from the same builder that writes `reported.json`, so a scraper and the file
cannot disagree. Nothing here is an input: there is no route that changes anything.

**Not 9091 on a zerofs host** — ZeroFS holds that one, and it is refused by name here rather than
becoming a startup failure over there.

## Ports, across every section

**21000–28999 is refused everywhere.** Each slot reserves eight consecutive ports from 21000, and
a listener inside that range would be taken out from under you by the next app deployed. A slot
hands out only as many as the document asked for — one, or two — and the rest are reserve, so that
raising the limit later moves nobody's ports. `0` is refused separately: that is the kernel picking
one, and a host should say what it serves on.

`metrics.port` must differ from `proxy.http.port`.

## What is not in this file

**No secret.** The AWS credentials and the ZeroFS encryption password live in `host.env` beside
this file, which `install` creates empty and never writes into. The rendered ZeroFS configuration
references `${ZEROFS_ENCRYPTION_PASSWORD}` and `${AWS_REGION}`; the daemon resolves the rest from
its own environment. So this file can be read over someone's shoulder.

**Nothing about what runs here.** Which apps this host serves is `desired.json`, which is watched
rather than read once, and which whatever writes it is not this daemon's concern.

**`NIBRUNNER_LOG`** stays in the environment — it is a `tracing` filter an operator changes to
debug one restart, not a property of the host.

## Two worked examples

`deploy/config.toml` is the smallest file this daemon accepts: volumes as files on this machine's
own disk, stores as directories on it, nothing served. It is what `install` writes when a host has
no configuration at all.

`deploy/config.zerofs.toml` is the same file for a host whose volumes live in an object store,
whose artifacts and exports are in S3, and which serves TLS behind an edge.
