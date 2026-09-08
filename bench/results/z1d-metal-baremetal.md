# z1d.metal, eu-west-2 — bare metal, the supported configuration

48 vCPU Xeon Platinum 8151, 377 GiB, `systemd-detect-virt: none`, native `/dev/kvm`.
Guest image built on the box from the manifest pins, carrying this repo's own `crates/init`:

```
version: 6.1.180-98db6df338f0+nibrunner-init
init_is_stub: False
ca-certificates 20250419   libc6 2.41-12+deb13u3
libgcc-s1 14.2.0-19        libstdc++6 14.2.0-19
```

All four package versions match `manifest.json` exactly — the pinned `debian:trixie-slim`
digest already carries three of them, so only `ca-certificates` is added from the pinned
Debian snapshot.

## Ramp, 256 MiB per microVM, empty host between steps

| apps | converged | running | mem used | load1 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 5 s | 1 | 3833 MiB | 0.21 |
| 8 | 5 s | 8 | 4417 MiB | 0.20 |
| 16 | 5 s | 16 | 5175 MiB | 0.50 |
| 32 | 5 s | 32 | 6508 MiB | 0.50 |
| 48 | 5 s | 48 | 7794 MiB | 2.94 |
| **63** | **5 s** | **63** | **9015 MiB** | **2.70** |
| 64 | 201 s | 63 | 9003 MiB | 0.33 |

The `answered` column of the ramp is not meaningful: it curls the moment `reported.json`
says N are running, before the tenants have warmed. The verification below is the real number.

## Verification at 63, with settle

```
all 63 reported running after 4s
POST 201: 63/63
GET 200 with rows: 63/63
states: running:63
memory used: 8714 MiB of 386576
firecracker processes: 63
```

**Every one of nibrunner's 63 slots runs and serves both verbs.** 63 concurrent Firecracker
microVMs, each a Bun process writing SQLite to its own volume, on 8.7 GiB and load 2.70.

## The 64th app

Asking for 64 leaves 63 running and the 64th unplaced — `SlotExhausted` from
`crates/nft-render/src/slot.rs` (64 NBD minors less one reserved for the export reader).
The refusal is clean: the other 63 kept serving, `63/64` answered, nothing crashed, and the
daemon stayed up. The cap is a deliberate design limit, correctly enforced.

## Against the nested-virtualization run

| | m8id.large, nested | z1d.metal, bare metal |
| --- | ---: | ---: |
| apps serving | 32 | **63** |
| consoles ending in `KVM_EXIT_FAIL_ENTRY` | 46 of 63 | **0 of 63** |
| host `kvm_intel` VMCS errors | 36 | 0 |
| OOM kills | 0 | 0 |
| load at 63 apps | 28.74 | 2.70 |

Nested virtualization is strongly implicated: the identical workload produced 46 hypervisor
entry failures there and none here. Both variables moved at once, though — bare metal also
brought 48 cores instead of 2 — so the cleanest honest statement is that the supported
configuration has no ceiling below nibrunner's own 63-slot cap, and the unsupported one does.

## Cost

z1d.metal is $5.27/hr in London. The whole run above is well under an hour of it.
