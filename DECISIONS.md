# Decisions

Where this daemon does something the nibrun agent does differently, and why. The rule for the
port was to keep the TypeScript's choice; each entry below is either a place where that was not
possible, or a place where it was kept and the reason for wanting otherwise is worth writing down.

## Kept, with a note

**The nftables table is still called `nibrun`, and the counters `app_<id>`.** They are this
daemon's own names and nothing outside reads them, so renaming them would have been free. Keeping
them means a host can be handed from the agent to this daemon and back without two tables
fighting over the same hooks — the first apply replaces what the other left. Rename them the day
the two are meant to run side by side, which is the day they would need different names anyway.

**The guest-facing spellings are all still `NIBRUN_`.** The config drive keys, the vsock ports,
the frame magics and the `x-nibrun-protocol-version` header are contracts with the C runtime, the
guest image and the control plane. None of them is this project's to rename.

**A start is refused while its backoff still has time to run, and the budget is the tenant's own
restart policy.** The guest's supervisor already has that policy and applies it to the process; the
host applies the same numbers to the microVM. Two budgets from one set of numbers is confusing to
read, and I would give the host its own — but the field is in the protocol and the guest reads it.

**`hasExtraPublicPort` is a boolean and the port is derived from the slot.** Which means an app's
public port changes if it ever moves slot, and nothing tells its users. The alternative is a port
in desired state, which is what I would do. It is the control plane's field to change.

## Changed

**No CLI, and no local socket.** The brief asked for `hostctl` over a unix socket with
`PUT /desired-state`, `GET /reported-state` and an event stream. Under a file-watching core all
three collapse: applying is writing `desired.json`, status is reading `reported.json`, waking is
what a request already does, and logs are a file per app. A command surface that only restates
what a file already says is a second way to be wrong about the same state. Removed at the user's
request, and the seam it leaves is the poller below.

**Desired state is watched, not polled.** The agent long-polls the control plane on an interval
the control plane sets. This daemon holds an inotify watch on the directory holding the document —
on the directory, because a document is replaced by a rename and a watch on the old inode watches
a file nothing will write to again — and re-reads on a thirty-second backstop in case a watch was
never established or was silently dropped. A poll is a decision about how stale a host may be,
paid on every tick of every quiet host; a watch costs nothing until the file moves.

**The remote control plane is an addon that writes that file.** Rather than a second source the
reconciler has to arbitrate between. It keeps the reconciler's one-source property literally true,
and it means a host that loses the control plane goes on converging on the last document it was
given rather than on nothing. `crates/nibrunnerd/src/control.rs` holds the client and the poll;
the session renewal, the report upload and the filesystem-query channel are the part still to write.

**ZeroFS is spawned, not linked.** The brief said not to link it: AGPL, and a private server
API. Reading nibrun's own agent shows it does not link it either. ZeroFS is a long-running service
the agent never starts and only ever talks to over its admin CLI — `zerofs flush`, `zerofs
checkpoint create|delete|list`, plus a read of the `[cache]` sizes out of the config file the
service was itself started with. So the licence question does not arise: nothing here is derived
from it, and the interface is a command line rather than a private API. What it does need is
permission to spawn two host tools beyond the three the brief allowed — `zerofs` for that CLI, and
`nbd-client`, whose attach is a fork that holds `NBD_DO_IT` for the life of the device rather than
a call this daemon could make and return from. Granted at the user's request. The one thing this
daemon must never do is start a read-write `zerofs run`: a second writer per storage prefix is
fenced by SlateDB's epoch only after a window of acknowledging writes it then discards, so it
loses tenant data rather than failing to start. There is a test asserting nothing here ever runs
one.

**`dd` is not spawned for the liveness probe.** nibrun reads a device's first block with `dd
iflag=direct` in a subprocess, because a read the kernel has accepted cannot be cancelled and the
process therefore has to be *abandoned* rather than waited for. The same property holds for a
`spawn_blocking` thread doing an `O_DIRECT` read, so this daemon does that instead: one fewer host
tool, the same bound on what is left behind — the repair the failed probe triggers is a detach,
which errors every queued request on that device and frees the reader. If that turns out to differ
on a real wedged host, the change is one function.

**Configuration is a file, not the environment.** The brief did not say either way, and the agent
reads environment variables. A file can refuse a setting that does not exist; an environment
cannot, because a variable nobody set and one whose name was mistyped are the same absence. Every
value is validated at startup — paths absolute, CIDRs parseable, ports outside the range slots
take, storage prefixes that will not become a key nobody can find. `NIBRUNNER_LOG` stays in the
environment because it is a thing an operator changes to debug one restart.

**A checkpoint server is started by this daemon, not by systemd.** nibrun starts one through a
templated unit and waits for `systemctl start` to return, which works only because that unit ends
with an `ExecStartPost` polling for the socket. This daemon has no systemd, so it spawns the
process and does that same wait itself — the readiness signal is the socket appearing either way,
because ZeroFS execs well before it answers on anything. It is killed with the daemon rather than
outliving it, unlike a tenant's microVM: it holds nothing a restart would want back.

**`tar` is not spawned.** The archive is written in-process with the `tar` and `flate2` crates. An
export already needs `debugfs`, and an archive whose entry names come from a tenant's own filenames
is one more surface than a library that takes them as data.

**systemd is not the supervisor.** The agent runs each microVM as a `nibrun-vm@<app>.service` and
reads its state out of `systemctl show`. This daemon spawns Firecracker itself into a session of
its own — `setsid`, no `kill_on_drop` — and keeps a pidfile carrying the pid, the host boot id and
the start time. `loaded` is that record, `active` is `kill(pid, 0)`, `failed` is a recorded
non-zero exit nobody asked for, and `startedThisBoot` is the boot id comparing equal, which is
what `InactiveExitTimestampMonotonic` was standing in for. The daemon is the VMM's parent while it
lives, so an exit code is readable; once it is gone the VMM is init's and the next daemon adopts it
from the pidfile. What this loses is systemd's own restart accounting, which the agent did not use.

**`vm_launch.sh` is gone.** Its whole job was to decide, before exec, whether a start is a cold
boot or a restore — because `PUT /snapshot/load` is refused by a Firecracker that has been given a
config file. This daemon makes that decision where it spawns the process, and deletes the stamp
itself immediately before the restore. Same invariant, one fewer file that has to agree about a
path with something else.

**The guest's verdict is read from a console file rather than from the journal.** The agent bounds
a `journalctl` read by the run's start time, because a redeploy reuses the unit name and the
journal holds every earlier release's console. This daemon redirects each Firecracker's output to
`vm-<app>.console` and truncates it on every start, so the whole file belongs to the run in
progress and there is nothing older in it to exclude.

**No `ip`, no `mksquashfs`, no `nbd-client`.** The tap is one ioctl on `/dev/net/tun` and the rest
is netlink through `rtnetlink`; the two read-only images are packed with `backhand`. `nft` and
`mke2fs` are the only host tools left, which is what the brief allowed.

**The artifact and config images are gzip-compressed.** The agent passes `-noI -noD -noF -noX` to
store everything uncompressed, buying a deploy the compressor's seconds. `backhand` has no
equivalent flag. squashfs already stores a block whose compressed form is no smaller, which covers
the artifact — the bytes are a binary. What is left is the config drive: a few hundred bytes of
text, compressed once per boot. The superblock names gzip either way and the guest kernel has
`CONFIG_SQUASHFS_ZLIB=y`, so nothing about mounting changes.

**Volumes are sparse files, not NBD devices over an object store.** This is phase 3's work, and
the `VolumeBackend` trait is the seam for it: `provision`, `attach`, `detach`, `teardown`, `flush`,
`checkpoint`, `observe`. `LocalFile` implements all but `checkpoint`, which it refuses rather than
faking. What that costs today is the property the object-store backend exists for — a volume that
outlives the machine — and the `flush` that is the durability point becomes the host's page cache
rather than a service that has to be asked.

**ring rather than aws-lc-rs.** `object_store`'s `aws` feature forces the aws-lc-rs backend, whose
`aws-lc-sys` needs cmake and a C toolchain for the target. That is the one thing that would stop
this being a static cross-compiled binary, so the feature set is `aws-base` + `ring`, reqwest gets
`rustls-no-provider`, and `main` installs ring's provider before anything dials.

## Not done, and named as such

- **Exports and the vsock filesystem browse.** The codecs for the filesystem channel are written
  and tested against the C headers (`crates/guest-contract/src/filesystem.rs`); nothing calls them.
  Exports are a checkpoint server started per checkpoint, an NBD attach against it and a read of
  the filesystem it pins — the attach half is written (`NbdDevices::attach_checkpoint`), the
  server and the reader are not.
- **The vsock filesystem browse.** The codecs are written and tested against the C headers
  (`crates/guest-contract/src/filesystem.rs`); nothing calls them.
- **Usage reporting.** A non-goal for v1.
- **ACME.** Phase 5. The proxy serves a certificate and key from disk, or plain HTTP, or nothing.
- **The `guestImage` version in a report** is read from the manifest beside the image. The image in
  `guest/` is nibrun's own `dist/`, whose manifest says `"init_is_stub": true` — it carries a
  throwaway `/init`, not the real guest runtime. A host that has to boot a tenant needs a
  published image rather than this one.
