# ZeroFS for real, and how far an export gets

The README has said since the beginning that "the ZeroFS backend has still never met a real
ZeroFS" and that no export has ever been written. This is the run that stopped both being true
of the backend, and did not manage to make the second one false.

Same AX41. ZeroFS v2.3.3, volume blocks in `s3://…-volumes/volumes`, `nbds_max=1024`, the
filesystem mounted over its own 9P export by the FUSE client it ships, and one `zerofs run` —
the single writer the README requires.

## What now works that never had

**A tenant runs on a volume whose blocks are in S3.** pocketbase provisioned `vol-pocketbase`,
`nbd-client` attached it at `/dev/nbd0`, ext4 was formatted onto it, the guest booted and served.

**The cache reservation is real.** The daemon read `memory_size_gb = 1.0` out of ZeroFS's own
config and reported `memoryMib` 62554 against 63578 on the same host with `local-file` — the
1 GiB ZeroFS is going to take, held back from what a guest may be promised, exactly as documented.

**A guest freezes and a checkpoint is cut while it is frozen.** Both are firsts:

```
INFO the guest froze its filesystem                       app_id=pocketbase
INFO export checkpoint cut while the tenant was frozen    checkpoint_id=export-exp-3
```

## Three defects between the checkpoint and the bundle

**A multipart part was whatever one read returned.** `AsyncReadExt::read` hands back what is to
hand, not what was asked for. Off a warm page cache that was 2 MiB against an 8 MiB buffer, and
S3 refuses any part but the last below 5 MiB:

```
EntityTooSmall: ProposedSize 2097152, MinSizeAllowed 5242880, PartNumber 1
```

So no bundle larger than a single part could ever have reached a bucket. The tests upload to a
directory, which has no such floor, so they passed either way.

**A checkpoint server was ready when the last one's socket was still there.** Readiness is the
socket appearing, and a server that dies leaves its socket behind, so the second export of a
checkpoint was answered instantly by the first one's socket and the attach was refused by a
server that had not started yet — `nbd-client exited 1: Error: CONNECT failed`.

**A retried export could not cut the checkpoint it had already named.** Checkpoints are named
after the export that owns them precisely so a retry is the same code path as the first attempt.
It was not: `A checkpoint with name 'export-exp-2' already exists`, so the second attempt failed
where the first had merely not finished.

All three are fixed. None of them is reachable on a `local-file` host, which is why a lane that
correctly refuses to checkpoint found none of them.

## Where it still stops

Attaching the checkpoint's volume to the reader device, with nothing to say why:

```
export not written  reason=the volume could not be made ready: nbd-client exited 1: Exiting.
```

**No bundle has reached the exports bucket.** The checkpoint reader spawns its ZeroFS with
`stdout` and `stderr` on `/dev/null`, so the one process that could explain this says nothing at
all, and `zerofs checkpoint list` does not show the checkpoint the daemon has just been told it
cut. That is where the next session starts.

## Two things worth knowing before running this

**A failed export leaves its checkpoint behind.** `export-exp-2` was still in the store long
after the export that named it had failed, and the README is explicit about the cost: while any
checkpoint exists, ZeroFS pauses segment deletion, compaction and metadata reclamation *for every
tenant on the host*. A host that retries exports accumulates that.

**The zerofs config needs `[servers.rpc]`, and nothing says so.** Every admin command the adapter
runs — `flush`, `checkpoint list`, `checkpoint create` — goes over ZeroFS's RPC server. Without
that section every one of them fails, and the failure does not read as a configuration problem:

```
WARN stopping a guest whose disk would not flush  reason=Error: RPC server not configured
```

A flush that fails **stops the tenant**. A host configured exactly as nibrunner's own table
describes loses every app on it, and the message names ZeroFS rather than the missing key.

## What this run cannot tell you

**Temporary credentials are the wrong shape for this.** ZeroFS was given AWS SSO session
credentials, and when they expired mid-run every S3 request became a 400, NBD writes failed, and
the guest's volume went read-error:

```
I/O error, dev vdd, sector 0 op 0x1:(WRITE) ... lost sync page write
```

The tenant kept answering a health probe on its own address while its disk was gone, and the
instance still reported `running` — the probe is a TCP connect and a broken disk is not something
it can see. Nothing here is nibrunner's fault, but it is the shape of the failure, and a real
deployment wants a durable credential rather than a session one.

**The freeze ceiling is sized for a local file.** The first freeze of a cold ZeroFS volume
exceeded `REPLY_TIMEOUT` of 30 seconds more than once — `logs.vsock took the request and never
answered` — and the attempt after it succeeded in under a second. Freezing an ext4 flushes its
journal to the device, and here the device is an object store. Whether 30 seconds is simply too
short, or the first freeze is paying for something warm afterwards, was not established.
