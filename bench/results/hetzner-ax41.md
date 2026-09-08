# Hetzner AX41 — 752 apps on one €45/month box

AX41-1-LTD, Helsinki, bare metal. AMD Ryzen 5 3600 (12 threads), 64 GB, 2× 512 GB NVMe
RAID1, Ubuntu 26.04, kernel 7.0.0-22. `systemd-detect-virt: none`. Swap disabled for the run —
installimage leaves 32 GB of it, and a density benchmark that swaps is measuring the disk.

`SLOT_COUNT` lifted from 63 to 1000 (see [slot-cap.md](../slot-cap.md)), tenant is the same
Bun + SQLite `/todos` server, 128 MiB declared per microVM, 64 MiB volumes.

## The ceiling

| apps | boot | running | POST | GET | mem used | free | load1 | OOM |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 63 | 5 s | 63 | 63/63 | 63/63 | 6622 MiB | 57596 | 1.36 | 0 |
| 128 | 10 s | 128 | 128/128 | 128/128 | 11894 MiB | 52323 | 3.68 | 0 |
| 256 | 15 s | 256 | 256/256 | 256/256 | 22555 MiB | 41663 | 8.31 | 0 |
| 384 | 26 s | 384 | 384/384 | 384/384 | 33384 MiB | 30834 | 13.03 | 0 |
| 512 | 36 s | 512 | 512/512 | 512/512 | 43897 MiB | 20321 | 11.13 | 0 |
| 640 | 40 s | 640 | 640/640 | 640/640 | 54531 MiB | 9687 | 13.78 | 0 |
| 704 | 40 s | 704 | 704/704 | 704/704 | 59808 MiB | 4409 | 9.95 | 0 |
| **752** | **45 s** | **752** | **752/752** | **752/752** | **63781 MiB** | **437** | 64.31 | **0** |
| 780 | 45 s | 780 | 202/780 | 202/780 | — | — | 88.91 | **3** |

**752 concurrent microVMs, every one serving both verbs**, with 437 MiB of 64 GB left. 780 is
past the wall: the OOM killer fires and the host collapses.

Marginal cost per microVM is **83 MiB** — `(63781 − 6622) ÷ (752 − 63)` — matching the 84 MiB
measured on AWS bare metal, and independent of the 128 MiB each guest declares, because
Firecracker backs guest memory lazily.

## Two things the service unit is missing

Both were invisible at 63 apps and are one-line fixes.

### `LimitNOFILE`

`deploy/nibrunnerd.service` sets none, so the daemon inherits systemd's **1024** soft default
against a 524288 hard limit. Each microVM costs at least one pidfd plus sockets:

```
Max open files            1024        524288
fds open:                 962
  256 anon_inode:[pidfd]              ← one per microVM
610 × "Too many open files (os error 24)"
"firewall apply failed": nft could not be run: Too many open files
```

The failure does not present as a resource problem. Health probes cannot open sockets and
`nft` cannot be spawned, so **DNAT rules go missing and healthy tenants are reported
`unhealthy` and served `503`** — while a direct probe to the guest returns `200` in 10 ms.
That split (`direct=200 viaproxy=503`) is the signature.

With `LimitNOFILE=524288` the host went from breaking at ~200 apps to 752, with zero fd errors.

### `OOMScoreAdjust`

At 780 apps the OOM killer chose **the daemon**:

```
Out of memory: Killed process 24432 (nibrunnerd)
```

`oom_score_adj:0` — the control plane is exactly as killable as anything else on the box, so
pushing a host past its memory takes out the thing that manages it rather than shedding a
tenant. `OOMScoreAdjust=-1000` in the unit would invert that.

Worth noting alongside: nibrunner's own memory admission (`memory_shortfall_mib`) is consulted
only by the waker, never on first boot, so nothing refused the 780th app. A boot-time check
would have turned this crash into a refusal.

## Against the other machines

| | m8id.large (nested) | z1d.metal (bare) | **AX41 (bare)** |
| --- | ---: | ---: | ---: |
| apps serving | 32 | 63 (slot-capped) | **752** |
| what stopped it | nested virt / 2 vCPU | the 63-slot constant | **RAM, at 437 MiB free** |
| `KVM_EXIT_FAIL_ENTRY` | 46 of 63 consoles | 0 | 0 |
| cost | $0.15/hr | $5.27/hr ≈ $3800/mo | **~€45/mo** |

The AWS metal run never found a hardware limit — it ran out of slots at 63 while using 2% of
its memory. Lifting the cap and moving to a machine an order of magnitude cheaper is what
turned the benchmark into an actual measurement.
