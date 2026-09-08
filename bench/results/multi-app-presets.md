# Six real apps on one host, behind Cloudflare

The six presets `nibrun`'s deploy-link offers, run together on the AX41, artifacts fetched from
S3, served over TLS with a Cloudflare Origin certificate on a wildcard domain, with
`desired.json` moved underneath the daemon the way a control plane would move it.

Until this run no host had served a second app under TLS, no artifact had come from a real
bucket, and the two apps that need `${NIBRUN_DATA_DIR}` had never started.

| | |
| --- | --- |
| Host | Hetzner AX41, 12 threads, 64 GB, Ubuntu 26.04 |
| Apps | pocketbase, sharkord, boop, gitea, open-connector, context-use |
| Artifacts | `s3://…/artifacts/<slug>`, eu-west-2 |
| Volumes | `local-file`, 2 GiB each |
| Proxy | 80 and 443, Cloudflare Origin CA cert for `*.canister.site` |
| Per app | 1 vCPU, 512 MiB, TCP health probe |

## What it found first

**Two of the six could not start, and for the same reason.** `boop` and `open-connector` both
name `${NIBRUN_DATA_DIR}`, and both panicked their kernel 160 ms in:

```
[nibrun] BOOP_DATABASE_PATH names NIBRUN_DATA_DIR, which this runtime does not offer
[    0.163784] reboot: Restarting system
```

`NIBRUN_DATA_DIR` is in `protocol::RUNTIME_VALUE_NAMES`, so the control plane offers it and the
host accepts a value naming it. `tenant_environment()` hands it to every tenant. But nothing ever
wrote it into `instance.env`, and the guest resolves a reference only against what that file
carries — so the one thing the value could not do was be referenced. Phase 3 called the
`instance.env` contract proven end to end on the strength of `$NIBRUN_HTTP_PORT`, which the file
does carry.

Fixed by seeding the guest's expansion table from `paths::DATA_DIR`: where the volume is mounted
is the guest's own fact, and the host should not have to assert a path it does not own. The test
that came with it walks every name the protocol offers, so the next value added to that list
cannot be advertised without being reachable.

**An artifact is one file, and two of the six are not.** `pocketbase` ships a `.zip` and `boop` a
`.tar.gz`. `nibrunnerd` fetches one object, checks it against its digest and packs it into a
squashfs as `/server`; it has no idea what an archive is, and nibrun's deploy-link is what opens
them. So the digest a release publishes is of the archive, and the digest a host must be given is
of the binary inside it. `bench/fetch-presets.sh` is where that unpacking now lives.

## The run

Each step is `desired.json` rewritten whole and the daemon left to close the difference.

| # | Document says | Converged | Result |
| ---: | --- | ---: | --- |
| 1 | nothing | — | `reported.json` written, host `ready` |
| 2 | 1 app running | 6 s | cold S3 fetch, digest verified, 32.7 MB → 12.3 MB squashfs |
| 3 | 3 running | 39 s | three digests cached separately, all HTTP/2 |
| 4 | 6 running | 24 s | 400 MB of cold fetches, all six answering |
| 5 | 1 stopped, 1 removed, 2 on-request, 2 running | ~60 s | see below |
| 6 | — | 60 s idle | both on-request apps slept on their own |
| 7 | 1 running | 25 s | one microVM resident, the rest 503 |
| 8 | every volume absent | 65 s | nothing left but the taps |

**Stopped, removed and idle are three different answers.** A `stopped` app answers `This app is
not running.` with a 503 — the hostname is still the host's. A *removed* app answers
`No app on this host answers for that hostname.` with a 404, its volume file deleted from disk.
An `idle` app answers by waking.

**A volume leaves only when it is told to.** Dropping an instance from the document stops it and
keeps its data; deleting the data needs `desiredState: "absent"` on the volume. Both were
exercised, and the second is what emptied the host.

**Sleep and wake, at 512 MiB with real tenants.** gitea's snapshot took 4.16 s and wrote its whole
512 MiB; the restore took **8 ms** and the request that caused it waited 10 ms. Ten concurrent
requests to a sleeping `context-use` produced **one** restore of 9 ms and ten 200s — `coalesced: 9`
in the log, nine requests riding the first one's wake.

**A daemon restart under six tenants disturbs none of them.** Six firecracker processes before,
six after, all six still serving. The README claimed adoption on one app; this is six.

## TLS, and the edge in front of it

Against the origin directly, with the chain checked against Cloudflare's Origin CA ECC root
rather than with `-k`:

```
http=200  proto=2  tls=0  time=0.043s
```

- **HTTP/2 reaches a real tenant.** Every h2 request used to come back 502; `4c28f34` fixed it and
  this is the first time a real app has answered one. All six negotiated `h2`.
- A hostname no app holds gets the proxy's 404. A connection whose handshake named `gitea` while
  its request named `boop` got **421** and `This connection was opened for a different host.`
- Through the actual edge, on a wildcard `*.canister.site` record, all six answer publicly with
  `server: cloudflare` and a `cf-ray`. **Every Cloudflare connection arrived on 443 and none on
  80**, so the origin leg is encrypted and the Origin certificate is the one doing it.

One record and one wildcard certificate cover every app, because the hostname is the proxy's to
route on and nothing per-app exists in DNS.

## The S3 artifact store, for the first time

`artifacts.store_url = "s3://…"` had never been pointed at a bucket. It works: each artifact was
fetched once, checked against its digest, packed, and cached under its digest — six images, 195 MB,
and no second fetch for a redeploy. `AmazonS3Builder::from_env()` means the credentials are the
daemon's process environment, which on a machine with no instance role is a systemd
`EnvironmentFile`. Session credentials expire, and a fetch after they do will fail; a restart with
a fresh file costs a tenant nothing.

## Cost

Six apps: **2375 MiB**, load 0.57, 195 MB of artifact images, 430 MB actually occupied by 12 GiB of
sparse volumes. The AX41 held 752 of the benchmark tenant; these six are heavier and it is not
noticing them.

## What this did not establish

**Tap devices are never deleted.** After the host was emptied — no instances, no volumes, no
DNAT rules — `nbr0` through `nbr5` were still there, and there is no tap deletion anywhere in the
sources. It is bounded by peak concurrency rather than by churn, since a slot's tap is reused, but
a host that once ran 752 apps keeps 752 links forever. This box had 781 of them from an earlier
run, which is how it was noticed.

**A request during teardown hung** rather than being refused. One observation, in the window
between an instance being stopped and its route being withdrawn; the same request answered 404 a
few seconds later. Not chased down.

**An idle instance dropped from the document stayed `idle`** rather than moving to `stopped`.

**The extra public port was routed but not spoken.** sharkord got slot 3, and the ruleset carried
`tcp dport 22003 dnat to 10.201.0.14:22003` and the udp rule beside it. Nothing tested WebRTC
through it, and a TCP connect was refused because the tenant listens on UDP. The rule is right;
the traffic is untried. Cloudflare's proxy cannot carry it either — an extra public port needs a
grey-cloud record.

**No export was written and ZeroFS never ran.** Volumes were `local-file`, so a checkpoint is
refused and an export with it, which is the correct answer and as far as this went. The exports
bucket was created and is empty.

**Nothing ran for longer than about half an hour**, and no app was ever asked to do real work. What
this measures is a host holding six real applications, not those applications under load.
