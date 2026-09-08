# Density benchmark

How many apps one nibrunner host actually holds. Each app is the same Bun binary serving
`GET`/`POST /todos` against SQLite on its own volume, so the number is about the host rather
than about the tenant.

| Run | Machine | Apps serving | Stopped by |
| --- | --- | ---: | --- |
| [aws-m8id-large-nested](results/aws-m8id-large-nested.md) | m8id.large, nested virt, 2 vCPU | 32 | nested virtualisation, unsupported by Firecracker |
| [z1d-metal-baremetal](results/z1d-metal-baremetal.md) | z1d.metal, 48 vCPU, 377 GiB | 63 | the 63-slot cap, at 2% memory use |
| [hetzner-ax41](results/hetzner-ax41.md) | AX41, 12 threads, 64 GB, ~€45/mo | **752** | **RAM, with 437 MiB left** |
| [contention](results/contention.md) | the same AX41, tenants made to work | see below | what they cost each other |

The 752 is idle tenants. `contention.md` is what happens when they work: memory costs a flat
82-87 MiB per microVM plus exactly what the tenant touches, so holding 128 MiB drops the host
to ~289 apps; CPU degrades linearly with oversubscription, within 15%, with no cliff.

Two defects came out of it, both invisible below a hundred apps and both now fixed: the service
unit set no `LimitNOFILE`, and no `OOMScoreAdjust`. A third is written up but not applied —
`slot-cap.md` has the reasoning behind bounding slots by the port layout.

## Running it

The host builds everything itself; only these files are copied to it.

```bash
scp provision-hetzner.sh install-host-hetzner.sh \
    ramp2.sh gen-desired.py tenant/server.ts root@host:/root/stage/

ssh root@host 'bash /root/stage/provision-hetzner.sh'   # toolchain, daemon, init, tenant, image
ssh root@host 'bash /root/stage/install-host-hetzner.sh' # state dir, config, unit, start
ssh root@host 'MEMORY_MIB=128 VOLUME_MIB=64 STEPS="63 256 512 752" bash /root/ramp2.sh'
```

The guest image is not built here: `just guest-image` does it, from the pins in
`guest/manifest.json`. A host cannot boot a tenant without it, because the image in `guest/`
is nibrun's stub, whose `/init` exits and panics the guest kernel.

`provision.sh`, `install-host.sh` and `ramp.sh` are the AWS variants, kept because the results
above were produced with them.
