<div align="center">
  <img src="docs/public/logo.svg" width="96" alt="">
  <h1>nibrunner</h1>
  <p><em>MicroVM orchestrator for your VPS with built-in sleep/wake policies, backups, snapshots, HTTPS, custom image, logs and metrics</em></p>
</div>

nibrunner runs apps in Firecracker microVMs on one Linux machine. It is a single binary,
`nibrunnerd`. You list the apps you want in a JSON file. The daemon boots each one in its own
microVM and keeps it running.

- **Sleep/wake policies** — An app with no traffic is snapshotted and suspended. The next request
  wakes it. Which apps sleep, and after how long, is set per app, so a machine that runs a few
  dozen apps at once can hold hundreds.
- **Backups and snapshots** — Volumes can live in an object store, so they survive the machine. A
  checkpoint is a point-in-time copy of a volume. An export bundles a volume and the app's
  environment into an archive a new volume can start from.
- **HTTPS** — A built-in proxy routes each hostname to its app. It terminates TLS with the
  certificate you give it, or serves plain HTTP behind an edge that terminates TLS. Non-HTTP ports
  (ssh, DNS, WireGuard) are forwarded as-is, TCP or UDP, and wake a sleeping app like a request
  does.
- **Custom image** — An app's root filesystem is a stack of layers on top of the Debian guest
  image nibrunner ships: a bare executable the host packs into an image, or a squashfs or ext4 you
  built. A volume can start from an archive instead of empty.
- **Logs and metrics** — Each app's output goes to its own file. The host serves a Prometheus page
  built from the same state it reports.

## Quick start

You need a Linux x86_64 machine with `/dev/kvm` (bare metal, or a VPS with nested
virtualisation), running Debian or Ubuntu with systemd. As root:

```bash
curl -fsSL https://raw.githubusercontent.com/ilbertt/nibrunner/main/deploy/install.sh | sh
```

This installs the required packages and the release, verified against its published digests, and
sets up the host from `/etc/nibrunner/config.toml`. If the file does not exist, it writes one:
volumes as files on the local disk, plain HTTP on port 80, no object store. Nothing is started
yet. Edit the file if you want, then:

```bash
nibrunnerd start
```

Then put a binary in the artifact store and write the file `nibrunnerd` watches. The daemon boots
the app, checks its health, and routes its hostname to it:
**[deploy an app](https://nibrunner.dev/docs/getting-started/deploy-an-app)**.

## Documentation

[nibrunner.dev/docs](https://nibrunner.dev/docs). The source is under [`docs/`](docs).

- [Getting started](https://nibrunner.dev/docs/getting-started/installation) — install, deploy
  an app, configure the host.
- Guides — [layers](https://nibrunner.dev/docs/guides/layers),
  [volumes](https://nibrunner.dev/docs/guides/volumes),
  [sleep and wake](https://nibrunner.dev/docs/guides/sleep-and-wake),
  [health checks](https://nibrunner.dev/docs/guides/health-checks),
  [HTTPS](https://nibrunner.dev/docs/guides/https),
  [raw ports](https://nibrunner.dev/docs/guides/raw-ports),
  [logs and metrics](https://nibrunner.dev/docs/guides/logs-and-metrics),
  [host capacity](https://nibrunner.dev/docs/guides/host-capacity).
- Reference — [`config.toml`](https://nibrunner.dev/docs/reference/config),
  [`desired.json`](https://nibrunner.dev/docs/reference/desired-state) and
  [`reported.json`](https://nibrunner.dev/docs/reference/reported-state), every key, generated
  from their JSON Schemas.

## Contributing

[`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md) — the tools, the two test lanes, what CI
checks, and how a pull request lands.
