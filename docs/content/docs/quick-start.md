---
title: Quick start
description: From a bare Linux machine to a microVM answering on a hostname, in four steps.
---

nibrunner is one binary, `nibrunnerd`, that turns a Linux machine into a microVM host. You
describe the apps you want in a JSON document; it boots each one in its own Firecracker microVM,
puts it to sleep when nobody is visiting and wakes it on the next request, backs its volume up,
and serves it over HTTPS with logs and metrics. This is the shortest path from a fresh machine to
an app answering on a hostname.

## Prerequisites

**A Linux x86_64 machine with `/dev/kvm`.** Every app is a microVM, and a microVM needs hardware
virtualisation. A bare-metal server has it; a VPS has it only if the provider passes nested
virtualisation through. A machine that has it shows the device:

```bash
ls -l /dev/kvm
```

`nibrunnerd install` refuses a machine without it, by name.

**Debian or Ubuntu, with `systemd`.** The installer puts its packages on with `apt-get` and
`nibrunnerd` runs the host as systemd units. It needs to be run as root: it installs a system
daemon.

**Outbound HTTPS.** The installer fetches the release from GitHub, and a host whose volumes live
in an object store reaches that store from here.

The packages the installer needs are ones it installs itself: `nftables`, `e2fsprogs`, `kmod`,
`curl`, `ca-certificates`, plus `nbd-client`, `fuse3` and `passwd` for the object-store volume
backend. On a machine without `apt-get`, install those and re-run — the script says so.

For an app to be reachable from the world you will also want **a hostname pointing at this
machine** and, for HTTPS, either an edge that terminates TLS in front of it or a certificate on
it. Neither is needed for the first run: the host the installer writes serves plain HTTP on
`:80`, and a request that carries the hostname reaches the app.

## 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/ilbertt/nibrunner/main/deploy/install.sh | sh
```

The script installs the packages, downloads the newest release — the daemon, the guest kernel
and the guest image — checks each against the digests it publishes, puts `nibrunnerd` in
`/usr/local/bin`, and hands over to `nibrunnerd install`. That lays the host out from
`/etc/nibrunner/config.toml`: the guest image, the kernel settings, ZeroFS if that file asks for
it, and every systemd unit. A host with no such file is given one. It starts nothing, and ends
with what is left:

```
Laid out; nothing started. What is left:

  1. edit /etc/nibrunner/config.toml
     written just now: volumes as files on this disk, plain HTTP on :80, nothing in an
     object store. Every key: https://nibrunner.dev/docs/config
  2. edit /etc/nibrunner/host.env
     the secrets that configuration needs, each named in the file
  3. nibrunnerd start
```

`NIBRUNNER_VERSION=<tag>` in front of the command pins a release; the newest one otherwise.

## 2. Configure

The configuration the installer wrote is a complete host: volumes as sparse files on this disk,
artifacts and exports in directories under `/var/lib/nibrunner`, plain HTTP on every address at
`:80`, and `max_apps` set to what it measured this machine can hold. It runs as it is, and for a
first app there is nothing to change.

What you will change before putting anything real on it is the volume backend — `zerofs` puts
volumes in an object store, which is what makes them outlive the machine and what a backup is cut
from — and the listener, `[proxy.http.tls]` for a certificate on this host, or none behind an edge
that terminates TLS. [Configuring a host](/docs/config) is every key, and
[`deploy/config.example.toml`](https://github.com/ilbertt/nibrunner/blob/main/deploy/config.example.toml)
is a host with every section filled in.

Secrets — object-store credentials, the ZeroFS encryption password — go in
`/etc/nibrunner/host.env`, which `install` created with every variable named and none set. The
starter configuration needs none of them.

## 3. Start

```bash
nibrunnerd start
```

It lays the host out again from the file as it is now, refuses while a secret the configuration
needs is still empty, and then starts the units that file names. Every later change to the
configuration is the same two steps: edit, `nibrunnerd start`. It restarts only what read
something that changed.

```bash
systemctl status nibrunnerd
journalctl -u nibrunnerd -f
```

## 4. Deploy an app

An app is a program the guest runs in a Debian root, as uid 65534, listening on the port the
document names. A statically linked Linux x86_64 binary — Go, Rust, Zig, a compiled Bun
executable — needs nothing from the image; a dynamically linked one has what Debian's slim image
ships — glibc, libstdc++, CA certificates — and nothing else.

Put the binary in the artifact store under a key of your choosing, and take its digest. The
starter configuration's store is a directory:

```bash
install -D -m 0644 ./my-server /var/lib/nibrunner/artifact-store/my-server
sha256sum ./my-server
```

Then write the document `nibrunnerd` watches, `/var/lib/nibrunner/desired.json`, naming that key
and that digest:

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
        "healthCheck": { "kind": "http", "path": "/healthz", "intervalMs": 5000, "timeoutMs": 2000, "gracePeriodMs": 30000, "healthyThreshold": 1, "unhealthyThreshold": 3 },
        "restartPolicy": { "maxRestarts": 5, "initialBackoffMs": 500, "maxBackoffMs": 30000, "backoffFactor": 2, "resetAfterMs": 60000 }
      },
      "hostnames": [{ "hostname": "app-1.example.com", "kind": "platform" }]
    }
  ],
  "checkpoints": [],
  "exports": []
}
```

The daemon hears the file change and converges: it formats the volume, packs the binary into an
image at `/app/server`, boots the microVM, and reports the instance `running` once `/healthz` on
port 3000 answers 2xx. `on-request` is the sleep/wake policy: the app comes up for this first
deploy, sleeps once nothing has reached it for five minutes, and the next request wakes it.
`running` keeps it up instead.

The proxy routes on the hostname the request carries, so the app is reachable before any DNS
exists:

```bash
curl -H 'Host: app-1.example.com' http://127.0.0.1/
```

Point `app-1.example.com` at this machine and the same request arrives from anywhere.

## What the host says back

`/var/lib/nibrunner/reported.json` is the daemon's account of every volume, instance, checkpoint
and export the document names — its state, and why, when the why is not obvious. An instance
that cannot be started says so there rather than in a log line.

Each app's output is in `/var/lib/nibrunner/logs/<appId>.log`. The daemon's own is in the
journal.

A `[metrics]` section in the configuration serves a Prometheus page rendered from the same state
`reported.json` is written from, so a scraper and the file cannot disagree.

## Where next

- [The document](/docs/desired-state) — everything an instance may say: layers and what runs in
  them, what a volume starts with, ports beside the HTTP one, what healthy means, and when an app
  sleeps.
- [Configuring a host](/docs/config) — every key in `config.toml`: the object-store backend, TLS,
  raw ports, metrics, and how many apps a host is laid out for.
