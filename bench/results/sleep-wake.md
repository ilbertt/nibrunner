# Sleep and wake across 400 apps

Same AX41 (12 threads, 64 GB). 400 apps, `desiredState: "on-request"`, `idleTimeoutMs` at the
60 s minimum, 128 MiB declared each. The tenant counts its process starts onto its volume, so a
wake can be told apart from a restart from outside: a restore resumes the frozen process and
leaves the count alone, a cold boot moves it.

## A fleet nobody visits costs nothing

Deployed, each app boots once and then puts itself away. With no traffic at all:

| | |
| --- | ---: |
| states | **idle: 400** |
| resident microVMs | **0** |
| memory | **1890 MiB** of 64218 |
| snapshots on disk | **50 GB**, 399 files |

Four hundred apps exist, none of them is running, and the host has spent 1.9 GB. The same
fleet held awake costs about 33 GB. **The trade is RAM for disk**: each sleeping app parks its
declared guest memory, 128 MiB of it, on the filesystem.

That inverts which resource runs out first. Awake, this box holds ~752 apps before RAM ends.
Asleep, 436 GB of disk would hold roughly 3400 — so the slot table, not the hardware, is what
a sleeping fleet hits first.

## Under a realistic invocation mix

Traffic shaped to what Azure Functions reported in production ([Shahrad et al., ATC '20](https://www.usenix.org/system/files/atc20-shahrad.pdf)):
45% of apps invoked at most hourly, 36% at most once a minute, the rest busier, ~3% more than
once a second — and the busiest 19% carrying over 99.5% of all invocations. Ten minutes,
`drive.py`, one thread per app with jittered periods so the fleet does not synchronise.

| class | apps | calls | p50 | p95 | max | calls over 10 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| hourly | 180 | 180 | **36.04 ms** | 42.80 ms | 1049 ms | 180 of 180 |
| minutely | 144 | 987 | 1.20 ms | 38.85 ms | 265 ms | 341 of 987 |
| busy | 64 | 3418 | 0.95 ms | 2.34 ms | 44.39 ms | 134 of 3418 |
| hot | 12 | 6470 | 0.87 ms | 1.03 ms | 55.95 ms | 37 of 6470 |

**Every hourly call was a wake, and cost 36 ms at the median, 43 ms at p95.** Those are the
apps that sleep between every visitor, and by the same distribution they carry well under 1% of
all invocations. The classes that carry the traffic never sleep and answer under a millisecond.

The minutely class shows the boundary: a 90 s period against a 60 s idle timeout, so about a
third of its calls (341 of 987) find the app away and pay the wake, which is what pulls its p95
to 38.85 ms while its median stays at 1.2 ms.

## The fleet settles at a third resident

Sampled while the traffic ran:

| elapsed | resident | idle | memory | load |
| ---: | ---: | ---: | ---: | ---: |
| 150 s | 400 | 0 | 6405 MiB | 0.70 |
| 270 s | 269 | 131 | 5019 MiB | 8.45 |
| 390 s | 208 | 191 | 4405 MiB | 9.43 |
| 570 s | 186 | 214 | 3442 MiB | 1.50 |
| end | **139** | **259** | **3113 MiB** | — |

The initial burst wakes everything, then the fleet drains to about a third resident. **400 apps
under production-shaped traffic cost 3.1 GB of RAM and 139 running microVMs** — against roughly
33 GB to hold all four hundred awake.

Snapshot disk fell from 50 GB to 33 GB over the run, and 259 idle apps × 128 MiB is 33 GB
exactly: a snapshot exists for a sleeping app and for no one else, which is the on-disk half of
"a snapshot is restored at most once".

## What did not go perfectly

The hourly class reported `max_boots: 2`, so at least one of those 180 apps cold-booted instead
of restoring. 179 of them resumed with their process intact. One outlier call took 1049 ms
against a 43 ms p95, most likely during the opening burst when all 400 woke together. Neither
was chased down, and neither is explained here.
