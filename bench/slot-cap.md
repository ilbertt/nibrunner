# Lifting the 63-slot cap

Not applied — this is the proposal, for review.

## Why 63 is not a real limit

`SLOT_COUNT` is derived from the NBD minor count:

```rust
const NBD_DEVICE_COUNT: u32 = 64;
pub const SLOT_COUNT: u32 = NBD_DEVICE_COUNT - 1;   // one reserved for the export reader
```

But `nbd_device_path` is consumed only by `adapters/volumes/zerofs.rs` and by the export
reader. **A `local-file` host never touches an NBD minor**, so it is paying a ceiling set by
a backend it does not run.

## What binds after it

| Limit | Ceiling | Why |
| --- | ---: | --- |
| **Port layout** | **1000** | `host_port = 21_000 + slot`, `extra_public_port = 22_000 + slot`. The ranges are 1000 apart, so slot 1000 is handed a host port that is already slot 0's extra public port. |
| Guest network | 16,384 | `10.201.0.0/16`, a `/30` per slot |
| Host ports in a `u16` | 44,536 | `65_536 - 21_000` |

The port collision is **latent rather than immediate**: `extra_public_port` is only bound when
the app asks for one — `reconcile/network.rs` uses
`record.wants_extra_public_port().then_some(slot.extra_public_port)`. A host whose tenants
never set `hasExtraPublicPort` would run past 1000 without noticing, and break the first time
one did. That makes it worth fixing *before* it is reachable, not after.

## The change

```diff
-pub const SLOT_COUNT: u32 = NBD_DEVICE_COUNT - 1;
-
 pub const HOST_PORT_BASE: u16 = 21_000;
 
 pub const EXTRA_PUBLIC_PORT_BASE: u16 = 22_000;
+
+/// A slot's host port is `HOST_PORT_BASE + slot` and its extra public port is
+/// `EXTRA_PUBLIC_PORT_BASE + slot`. The ranges are only their difference apart, so the slot
+/// at that distance would be handed a host port already spoken for as slot 0's extra public
+/// port. Nothing above it can be laid out, whatever the machine has room for.
+pub const SLOT_COUNT: u32 = (EXTRA_PUBLIC_PORT_BASE - HOST_PORT_BASE) as u32;
```

**This is only correct for `local-file`.** A zerofs host would be handed `/dev/nbd500` for
slot 500, which does not exist unless the module was loaded as `modprobe nbd nbds_max=1000`.
So the zerofs backend needs to start refusing what it cannot address:

```rust
// in adapters/volumes/zerofs.rs, where a slot becomes a device
if slot.slot >= NBD_DEVICE_COUNT {
    return Err(/* this backend addresses one nbd minor per slot, and the module was
                  loaded with fewer than this host has slots */);
}
```

A refusal that names the reason is the point: today the arithmetic silently caps every host
at 63 whether or not it runs zerofs, and after the change a zerofs host would silently get a
device path that is not there. Neither is something an operator can act on.

## What it buys on the machine we are about to rent

Measured on z1d.metal: **84 MiB of host memory per Bun microVM** — `(9015 − 3833) ÷ 62` — with
declared memory barely mattering, because Firecracker backs guest memory lazily.

| Machine | RAM | apps before RAM binds | first limit hit |
| --- | ---: | ---: | --- |
| **AX41** | 64 GB | **~750** | RAM |
| AX102 | 128 GB | ~1500 | the 1000-slot port layout |
| AX162 | 512 GB | ~6000 | the 1000-slot port layout |

On an AX41 the port ceiling is never reached, so **the slot bump is the only change needed** —
raise `SLOT_COUNT` and the run finds a real hardware limit at roughly 750 apps.
