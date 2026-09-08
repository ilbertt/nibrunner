# The vsock paths, and what a local-file host says about exports

Same AX41. The README lists "the vsock log and filesystem paths, the checkpoint and export work"
as untried beyond a single app.

## The measurement path holds at 200 apps

Every measurement tick asks each guest for `Usage` and `Compute` over its own vsock. At 200 apps:

| | |
| --- | ---: |
| instances reporting compute | **200 of 200** |
| volumes reporting filesystem usage | **200 of 200** |
| vsock errors in the log | **0** |

```
app-1  memoryTotalBytes 109309952  memoryUsedBytes 25174016  cpuShare 0.0005
vol-1  totalBytes 58675200         usedBytes 159744
```

Two hundred guests, each answering two verbs on a private vsock every tick, with nothing
dropped. The numbers are also sane: 104 MiB visible to a guest declared 128 MiB, and 56 MiB
usable on a 64 MiB volume once ext4 has taken its cut.

## The browse verbs are unreachable, not merely untested

`guest-contract` still defines `List`, `Stat`, `Read`, `Write`, `Remove`, `Usage` and `Compute`,
and the guest still answers all of them. On the host side `domain::filesystem::reader` exposes
`list` and `measure` — but since the control plane came out of the daemon, **`reader::list` has
no caller outside its own tests**. The only thing reaching into a guest is `measure`.

So the browse half of the contract cannot be exercised from a running host at all. That is a
sharper statement than "untried": there is nothing to try it with until something drives it.

## A local-file host refuses both, in a sentence

Asked for a checkpoint and an export on `volumes.backend = "local-file"`:

```json
{"checkpointId": "cp-1", "state": "failed",
 "message": "a volume kept as a file on this host's own disk cannot be checkpointed"}

{"exportId": "exp-1", "checkpointId": "export-exp-1", "state": "failed",
 "message": "a volume kept as a file on this host's own disk cannot be checkpointed"}
```

The refusal is the correct answer and it reads as a sentence. Nothing was written to
`export-store`, the other 199 apps never noticed, and app-1 kept serving.

## The freeze lets go, even when the export fails

This is the part worth having done deliberately. An export freezes the guest's filesystem
*before* it discovers the backend cannot checkpoint:

```
INFO  the guest froze its filesystem   app_id=app-1
WARN  export not written               reason=a volume kept as a file ... cannot be checkpointed
```

The README says of this: *"There is no path out of answering that connection which leaves a
tenant frozen, which an export that failed after freezing then demonstrated by accident."*
A `/health` probe cannot tell the difference, because a frozen filesystem still serves reads.
A write can. Probing app-1 once a second with app-2 as a control:

| | app-1 | app-2 (control) |
| --- | ---: | ---: |
| before the export | 0.35 ms | — |
| **at the freeze** | **197.23 ms** | 0.23 ms |
| 1 s later | 0.06 ms | 0.07 ms |
| for the next 28 s | 0.06–0.25 ms | 0.06–0.24 ms |

The write blocked for 197 ms, the export failed, the freeze was released, and the tenant was
writing again on the next probe. **The invariant holds.** 197 ms is also the measured cost of
the freeze window to a tenant whose export is going to fail.

## One thing worth changing

The order is freeze-then-discover. On a local-file host the backend can never checkpoint, and
that is knowable before touching the guest — yet every export request still freezes a tenant for
about 200 ms to learn it. Checking the backend's capability first would refuse just as correctly
without reaching into a guest that was never going to be exported.
