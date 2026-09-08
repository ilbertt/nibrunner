# What microVMs cost each other

Same AX41: Ryzen 5 3600, **12 threads**, 64 GB. The tenant is `tenant/loadgen.ts` — it holds
`HOLD_MIB` of memory it has actually written to, spins a duty cycle at `SPIN_PCT`, and times a
fixed unit of work (`sha256` chained `rounds` times) on `/work`. Fixed work, so the time is
contention rather than a different amount of work.

Both knobs arrive through `config.environment` in the document, which the guest reports as
`instance configured: port 3000, 2 environment variables`.

## Compute: they share fairly

Every app spinning flat out, 200k rounds timed inside one of them. Each microVM has one vCPU,
so `apps ÷ 12` is how oversubscribed the host is.

| apps | oversubscribed | work | vs. alone | cpu busy | load1 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 0.08× | 92.84 ms | 1.0× | 9% | 0.74 |
| 6 | 0.5× | 111.53 ms | 1.2× | 51% | 3.66 |
| 12 | 1× | 200.92 ms | 2.2× | 100% | 8.27 |
| 24 | 2× | 424.76 ms | 4.6× | 100% | 17.5 |
| 48 | 4× | 930.79 ms | 10.0× | 100% | 31.0 |
| 96 | 8× | 1644.83 ms | 17.7× | 100% | 79.2 |
| 192 | 16× | 3352.34 ms | 36.1× | 100% | 167.7 |

**Degradation is linear in oversubscription, within about 15%.** Past saturation the cost is
`2.2 × (apps ÷ 12)`: predicted 4.4× at 24 apps against 4.6× measured, 17.6× at 96 against 17.7×,
35.2× at 192 against 36.1×. Nothing pathological happens — no collapse, no thrash, no cliff. A
host four times oversubscribed is four times slower, and that is all.

The 2.2× at exactly one vCPU per thread is not host contention: it is the tenant competing with
itself. A guest has a single vCPU, and `/work` shares it with the app's own spinner.

The worst sample never ran far from the median — 1881 ms against 1645 at 96 apps, 3617 against
3352 at 192 — so the scheduler is not starving anyone to favour others.

## Memory: a flat 84 MiB, plus whatever the tenant touches

128 apps, each holding an amount it has written to. Declared memory is `hold + 256` so the guest
can carry it.

| held | host per app | overhead | a 64 GB host holds |
| ---: | ---: | ---: | ---: |
| 0 MiB | 82 MiB | 82 | ~758 |
| 32 MiB | 116 MiB | 84 | ~536 |
| 64 MiB | 150 MiB | 86 | ~414 |
| 128 MiB | 215 MiB | 87 | ~289 |
| 256 MiB | 342 MiB | 86 | ~181 |

**The overhead is constant at 82–87 MiB whatever the tenant does with its memory**, and the rest
is exactly what the tenant touched. There is no amplification: a microVM costs its guest's real
working set plus a fixed Firecracker-and-Bun tax.

Declared memory still costs nothing on its own. Every row above declares more than the one before
and the host only pays for pages the tenant wrote, which is why the 752-app result was reached
with 128 MiB declared per guest and 83 MiB actually spent.

## What this means for the 752

The 752 figure is for **idle** tenants. It is the best case, not a capacity plan:

| tenant | apps on this box |
| --- | ---: |
| idle | ~752 |
| holding 32 MiB | ~536 |
| holding 128 MiB | ~289 |
| holding 256 MiB | ~181 |
| busy on CPU | 12 before each one is sharing a thread |

Memory sets how many can exist; CPU sets how many can be busy at once. On a 12-thread box those
are two very different numbers, and only the first one is 752.

## A caveat on the harness

The sampler in `contention.sh` takes one shot per sample and silently drops a reply it cannot
parse, which is why the 12 and 48-app rows came back empty on the first pass and were measured
by hand afterwards. The apps were healthy throughout — a direct probe answered `200` in 30 ms —
so the gaps are the harness, not the host.
