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

## The 40 milliseconds nobody was spending

The apps felt slower than they should, so the request path was taken apart on the host. Twenty
samples per layer, gitea, median:

| Layer | Before | After |
| --- | ---: | ---: |
| the guest directly, app and tap | 2.0 ms | 1.4 ms |
| its host port, adding the loopback DNAT | 1.8 ms | 1.4 ms |
| through the proxy over HTTP | 1.8 ms | 1.3 ms |
| through the proxy over **HTTPS** | **43.4 ms** | **4.0 ms** |

**The proxy costs nothing and TLS cost 41 ms**, which is not what a handshake costs on loopback.
Splitting the request said where it went:

```
connect=0.000093s   tls_done=0.002629s   first_byte=0.043177s
```

The handshake finished at 2.6 ms. Then forty milliseconds passed before the first byte of a reply
the tenant had already produced. That is not work; that is Nagle waiting on an ACK the other end
had decided to delay.

A reply leaves the proxy as more than one write, and the socket the visitor arrived on came
straight out of `accept()` with Nagle still on — `set_nodelay` appeared once in the whole proxy,
on the connector *to the tenant*. Plain HTTP hid it, because those replies left in a single write;
under TLS the records around them do not. And because Cloudflare reaches an origin over TLS, this
was every real request: through the edge, the same page went from a ~280 ms first sample to a
132 ms median.

Setting it on both accept loops is the whole fix. The test that came with it asserts the thing
that makes the bug possible — that the kernel hands the stream over with Nagle on — so the
assertion fails if anyone removes the call.

## Cost

Six apps: **2375 MiB**, load 0.57, 195 MB of artifact images, 430 MB actually occupied by 12 GiB of
sparse volumes. The AX41 held 752 of the benchmark tenant; these six are heavier and it is not
noticing them.

## Sleeping near the timeout that was asked for

Recording what moved and measuring every guest shared a loop, so they shared its interval, and the
interval belonged to the expensive half. An app sleeps on the first pass that finds it quiet, so
the smallest timeout the protocol allows was served at up to twice its length: an app asking for 60
seconds could stay resident for 120.

Split into two loops — counters every 5s, guests every 60s — and measured here, six apps woken at
once and then left alone:

| app | asked for | slept after | over |
| --- | ---: | ---: | ---: |
| context-use | 60s | 60.4s | 0.4s |
| boop | 60s | 64.5s | 4.5s |
| gitea | 60s | 64.5s | 4.5s |

Overshoot is now bounded by the tick rather than by the measurement, and the daemon's own cost
went from 14 CPU ticks per minute to 15, holding six apps.

**What that last figure does not establish.** The counters come from one look at the ruleset, and
the ruleset is as big as the host has apps. Six of them is not where this gets expensive, and the
752-app case has not been measured at the new interval. A host that big is the one that sets the
floor under this interval, not this one.

## What this did not establish

**Tap devices were never deleted, and now are.** After the host was emptied — no instances, no
volumes, no DNAT rules — `nbr0` through `nbr5` were still there, and there was no tap deletion
anywhere in the sources. The bound is churn rather than peak concurrency: slots are handed out by
a cursor rather than lowest-free, so a delete followed by a deploy takes a fresh slot and strands
the old device. Six apps torn down and redeployed left twelve taps behind. This box had 781 from
the density run, which is how it was noticed.

Fixed in two parts: a tap goes back when its slot is released, and every tap no slot claims is
taken back at startup, which is the only thing that helps a host already carrying them. Measured
here afterwards — tearing down six apps took the count from 12 to 6, and the next restart took the
six historical orphans to 0 (`stranded=6 held=0`). Restarting under three live tenants left their
taps alone and all three serving, which is the property that actually matters.

**A request during teardown hung** rather than being refused. One observation, in the window
between an instance being stopped and its route being withdrawn; the same request answered 404 a
few seconds later. Not chased down.

**An idle instance dropped from the document stayed `idle`** rather than moving to `stopped`.

**The extra public port was routed but not spoken, and has since been removed.** sharkord got
slot 3, and the ruleset carried `tcp dport 22003 dnat to 10.201.0.14:22003` and the udp rule beside
it. Nothing tested WebRTC through it, and a TCP connect was refused because the tenant listens on
UDP. Cloudflare's proxy could not have carried it either — that needs a grey-cloud record. The
feature left the codebase in `75542c8` afterwards, so sharkord runs here without the half of its
preset that wanted one.

**No export was written and ZeroFS never ran.** Volumes were `local-file`, so a checkpoint is
refused and an export with it, which is the correct answer and as far as this went. The exports
bucket was created and is empty.

**Nothing ran for longer than about half an hour**, and no app was ever asked to do real work. What
this measures is a host holding six real applications, not those applications under load.
