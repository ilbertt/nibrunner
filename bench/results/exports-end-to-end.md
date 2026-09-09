# The export path, end to end

Every earlier attempt at an export failed, and the run before this one failed without saying
why. This is the run where a bundle reached the store, on a Hetzner host with six real apps and
volumes backed by ZeroFS over S3.

## It works

```
exp-real   pocketbase  state=ready  12794839 bytes  ~13s
exp-quiet  sharkord    state=ready  55910412 bytes  ~18s
```

and the bundle holds what it is supposed to:

```
data/pb_data/data.db          the tenant's live database
pocketbase                    sha256 77478434…, the digest the manifest names
.env                          the environment it was given
```

Four things stood between the code as it was and that result. Three were bugs, one was a host
that nothing had ever told the operator to configure.

## The daemon needs the store password, and nothing said so

The checkpoint server reads the writer's store, so it needs the writer's encryption password.
That lived in `zerofs.env`, which only the `zerofs` unit was given. The daemon spawns the
checkpoint server with its own environment, so the server exited immediately:

```
the volume could not be made ready: /opt/nibrun/bin/zerofs/zerofs exited 1:
    1: environment variable not found
```

Nothing checks for it at startup and nothing documents it, so a host can run for months and
only discover it the first time somebody asks for an export. This is the same shape as the
`[servers.rpc]` requirement found earlier: a deployment fact that only the export path needs,
and so the only one that finds it. Fixed on the host by giving the unit `zerofs.env` too.

## The server was killed by being ignored

With the password in place the checkpoint was cut and the reader still could not attach:

```
the volume could not be made ready: nbd-client exited 1: Error: CONNECT failed
```

The server was alive for the whole sixty-second attach window and its socket was on disk, so
the obvious readings — server dead, socket missing — were both wrong. What settled it was
connecting to the socket twice:

```
14:10:29  connect OK, magic = b'NBDMAGICIHAVEOPT'
14:10:35  connect FAILED: ConnectionRefusedError [Errno 111] Connection refused
```

Same socket, same inode, six seconds apart. A unix socket whose listener has died refuses
exactly like that, and six seconds is one of this server's logging intervals.

Its stderr was piped and read only on the failure path, so on the path where it came up the
reading end closed as soon as `start()` returned. The next line it logged went to a pipe with
nobody on it. The mechanism added to capture the server's account of itself was what killed it.

Proved by sending the server's stderr to a file instead, changing nothing else: the export
succeeded on the first attempt.

## Checkpoint names were never read

`parse_checkpoint_names` took the first whitespace-separated word of each line. The CLI prints
a table drawn in box characters, so the first word of a row is `│`:

```
│ export-exp-g ┆ 4eaaf567-f923-4a67-b3ff-388487cd884e ┆ 2026-09-09 16:15:37 │
```

Nothing survived being parsed as an id, so `observe_checkpoints()` returned an empty list on a
host that held checkpoints. Two things rest on that list and both were quietly off: the reap
that lets a half-finished export be retried never ran, so a retry was refused with
`A checkpoint with name 'export-exp-g' already exists`; and the reaper that drops checkpoints
nothing is waiting on saw nothing to drop, so they accumulate, each holding storage that cannot
be reclaimed while it exists.

The unit tests asserted against a plain whitespace-column format the CLI has never emitted.

## Ten seconds is not long enough on a host doing its job

A server opens by reading its checkpoint out of the object store. The attach that follows
waits sixty seconds for exactly that reason; the wait for the socket was ten. On a host busy
with its own tenants, every export asked for failed the same way, about ten and a half seconds
in, three times running:

```
14:33:21  export checkpoint cut while the tenant was frozen  (export-exp-shark)
14:33:32  the server for export-exp-shark did not answer … in time
14:34:05  cut … 14:34:16  same
14:34:51  cut … 14:35:02  same
```

The same host, the same request, once it was quiet: ready in eighteen seconds with a 55 MB
bundle. The export was never wrong, only hurried — and each hurried attempt still froze the
tenant and cut a checkpoint first, so the cost of being early is paid by the tenant.
