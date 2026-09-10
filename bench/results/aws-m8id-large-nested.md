# Density benchmark: how many apps fit on one host

Each app is a Bun binary serving `GET`/`POST /todos` against SQLite on its own persistent
volume. One binary, one digest, 63 apps — the artifact squashfs is cached per digest, so the
host stores it once.

## The blocker that had to be cleared first

**The repository cannot boot a tenant as shipped.** `guest/rootfs.ext4` is nibrun's stub image
(`"init_is_stub": true`), whose `/init` prints `not the real /init` and exits, panicking the
guest kernel at 0.19 s:

```
nibrun guest-image stub init: not the real /init
[    0.192626] Kernel panic - not syncing: Attempted to kill init! exitcode=0x00000000
```

`crates/init` is the real PID 1 and builds fine, but the image carrying it was never committed.
A host that has to boot a tenant needs a published image rather than this one, so the fix is to
build `nibrunner-init` and install it as `/init`:

```bash
cargo zigbuild -p nibrunner-init --target x86_64-unknown-linux-musl --release
mount -o loop,rw rootfs.ext4 /mnt/rootfs
cp nibrunner-init /mnt/rootfs/init && umount /mnt/rootfs
```

The real init (539,808 bytes) is smaller than the stub (758,544), so the image needs no resize.
Nothing verifies the guest image against `manifest.json` — `read_guest_image_version` reads only
the `version` string — so patching the image in place is enough.

## m8id.large, eu-west-2, nested virtualization

2 vCPU, 8 GiB, `CpuOptions.NestedVirtualization=enabled`, Xeon 6975P-C. `/dev/kvm` present.
nibrunnerd reported `guest_memory_mib=7136`.

### 256 MiB per microVM, apps added cumulatively

| apps | converged | running | answered | mem used | load1 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 0 s | 1 | 1/1 | 629 MiB | 0.00 |
| 8 | 5 s | 8 | 8/8 | 1169 MiB | 0.32 |
| 16 | 5 s | 16 | 16/16 | 1805 MiB | 0.94 |
| 24 | 5 s | 24 | 24/24 | 2434 MiB | 1.62 |
| **32** | **5 s** | **32** | **32/32** | **3077 MiB** | **1.73** |
| 40 | 240 s | 21 | 21/40 | 2245 MiB | 0.33 |

### 128 MiB per microVM, each step from an empty host

| apps | converged | booted | still running | answered | load1 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 32 | 20 s | 32 | 32 | 31/32 | 6.83 |
| 40 | 25 s | 40 | 40 | 9/40 | 14.74 |
| 48 | 30 s | 48 | 28 | 29/48 | 16.26 |
| 56 | 35 s | 56 | 48 | 45/56 | 25.56 |
| 63 | 40 s | **63** | 55 | 16/63 | 28.74 |

## What this establishes

**nibrunner drove its full 63-slot table.** Every microVM booted at 128 MiB — slot allocation,
the nftables ruleset, per-app volumes, artifact sharing by digest and Host-header routing all
held at 63 apps, on a codebase whose README says no host had ever run a *second* app. The
63-slot cap itself is `SLOT_COUNT` in `crates/nft-render/src/slot.rs`: 64 NBD minors less one
reserved for the export reader.

**The honest density figure for this machine is 32 concurrent apps**, every one serving `GET`
and `POST` reliably, at 3077 MiB and load 1.73.

**Memory was never the constraint.** Zero OOM kills across every run. A microVM's real RSS is
about 74 MiB whether it declares 128 or 256 MiB, because Firecracker backs guest memory lazily —
32 apps cost 2972 MiB at 128 MiB/VM and 3077 MiB at 256 MiB/VM. Note that nibrunner's own
accounting (`memory_shortfall_mib`) is consulted only by the waker, never on first boot, so a
host will happily overcommit: 32 apps at 256 MiB commits 8192 MiB against a 7136 MiB budget.

## What this does NOT establish

**Why apps failed past 32.** Two explanations fit the data and this machine cannot separate them:

- *Nested virtualization.* 46 of 63 guest consoles ended in `KVM_EXIT_FAIL_ENTRY`, with 36
  matching `kvm_intel` VMCS complaints in the host kernel log. That is the signature in
  [firecracker#668](https://github.com/firecracker-microvm/firecracker/issues/668), where the
  maintainers state plainly: "Firecracker can only run on physical machines. We do not support
  nested virtualization." [#751](https://github.com/firecracker-microvm/firecracker/issues/751),
  which would have investigated it, is closed with no resolution.
- *Two vCPUs.* Load average reached 28.7 with 63 Bun runtimes on 2 cores. Health checks allow
  2000 ms with an unhealthy threshold of 3; under that much starvation they fail on timing alone.

An attempt to isolate the two by running 63 apps with no traffic at all was inconclusive:
26 failed within 60 s, but load was still 18.5, because booting 63 Bun runtimes saturates
2 cores by itself. **Bare metal is the only way to settle it** — no nesting, and enough cores
that 63 microVMs are not CPU-starved.

## Reproducing

```bash
python3 gen-desired.py 63 --digest <sha256> --size <bytes> --memory-mib 128 --state running \
  --out /var/lib/nibrunner/desired.json
MEMORY_MIB=128 STEPS="32 40 48 56 63" bash ramp2.sh
curl -s http://127.0.0.1:8080/todos -H 'Host: app-1.bench.local'
```

`provision.sh` builds the tenant with bun on the host and lays out the state directory;
`ramp2.sh` resets to an empty host between steps so no step is judged on the previous one's
wreckage.
