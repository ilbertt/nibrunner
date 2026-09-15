<div align="center">
  <h1>nibrunner</h1>
  <p><em>MicroVM orchestrator for your VPS with built-in sleep/wake policies, backups, snapshots, HTTPS, custom image, logs and metrics</em></p>
</div>

nibrunner is one binary, `nibrunnerd`, that turns a Linux machine with `/dev/kvm` into a microVM
host. You describe the apps you want in a JSON document; it boots each one in its own Firecracker
microVM and keeps them that way.

- **Sleep/wake policies** — an app nobody is visiting is snapshotted and suspended; the next
  request wakes it. Which apps sleep, and after how long, is the document's to say, so a machine
  that runs a few dozen apps at once holds hundreds.
- **Backups and snapshots** — a volume in an object store outlives the machine it ran on. A
  checkpoint cuts it at a point in time; an export bundles it, and the app's environment, as an
  archive you can start another volume from.
- **HTTPS** — a proxy routes each hostname to its app, terminating TLS with the certificate you
  give it or serving plain HTTP behind an edge that does. Ports beside HTTP — ssh, DNS,
  WireGuard — are carried unread, tcp or udp, and wake a sleeping app like a request would.
- **Custom image** — an app's root is a stack of layers over the Debian guest image nibrunner
  ships: a bare executable the host packs into an image, or a squashfs or ext4 you built yourself.
  A volume can start from an archive rather than empty.
- **Logs and metrics** — every app's output in a file of its own, and a Prometheus page for the
  host rendered from the same state the daemon reports.

## Quick start

A Linux x86_64 machine with `/dev/kvm` — bare metal, or a VPS whose provider passes hardware
virtualisation through — running Debian or Ubuntu with systemd. As root:

```bash
curl -fsSL https://raw.githubusercontent.com/ilbertt/nibrunner/main/deploy/install.sh | sh
```

That installs the packages and the release, checked against the digests it publishes, and lays
the host out from `/etc/nibrunner/config.toml` — writing one if there is none: volumes as files
on this disk, plain HTTP on `:80`, nothing in an object store. It starts nothing. Edit the file
if you want to, then:

```bash
nibrunnerd start
```

Then put a binary in the artifact store, write the document `nibrunnerd` watches, and the daemon
boots it, watches its health, and routes its hostname to it. The
**[quick start](https://nibrunner.dev/docs/quick-start)** walks through all four steps, first app
included.

## Documentation

[nibrunner.dev/docs](https://nibrunner.dev/docs) — the source is under [`docs/`](docs).

- [Quick start](https://nibrunner.dev/docs/quick-start) — prerequisites, install, configure,
  start, and a first app.
- [The document](https://nibrunner.dev/docs/desired-state) — everything an instance may say:
  layers, what a volume starts with, ports, what healthy means, and when an app sleeps.
- [Configuring a host](https://nibrunner.dev/docs/config) — every key in `config.toml`.
- [Reference](https://nibrunner.dev/docs/reference) — `config.toml`, `desired.json` and
  `reported.json` key by key, rendered from their JSON Schemas.

## Contributing

[`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md) — the tools, the two test lanes, what CI
checks, and how a pull request lands.
