# Restarting the daemon under a fleet

The property the daemon is built around, from the README: *"Nothing this daemon does stops a
tenant. Each microVM runs in a session of its own, adopted from a pidfile on the way back up.
Restarting the daemon, or killing it, leaves every app serving."* Phase 2 proved that with one
app. This asks it of a host under systemd.

The tenant counts its process starts onto its volume, and firecracker PIDs are recorded either
side, so an adoption can be told from a restart two independent ways.

## As shipped, a restart killed every tenant

```
their cgroup: 0::/system.slice/nibrunnerd.service
systemctl stop nibrunnerd
microVMs still alive: 0 of 5
```

A microVM is a child of the daemon, so it lands in the unit's cgroup. systemd's default
`KillMode=control-group` empties that cgroup on stop, and takes every tenant with it. The
`setsid()` in `adapters/vm/process.rs` gives each microVM its own **session**, which is a
process-group idea and buys nothing against a cgroup kill.

So on a host running under the shipped unit, `systemctl restart nibrunnerd` — upgrading the
binary, say — stopped all 300 apps at once. The property held everywhere except where it is
actually deployed.

## With `KillMode=process` it holds exactly as documented

```
systemctl stop nibrunnerd
  microVMs alive:  5 of 5
  daemon:          gone
  app-1 -> http 200 {"boots":4,"uptimeS":36}     served with no daemon at all
  app-3 -> http 200 {"boots":3,"uptimeS":36}
  app-5 -> http 200 {"boots":3,"uptimeS":37}

systemctl start nibrunnerd
  running:     5
  pids before: 132229 132231 132235 132238 132242
  pids after:  132229 132231 132235 132238 132242
  SAME PROCESSES - adopted, not restarted
```

Tenants kept answering on their own addresses while nothing supervised them, and the daemon
that replaced the old one took them back without touching a single process. The adoption logic
was never at fault; the unit was.

## What was not a bug

Partway through I thought a `failed` instance was never restarted: five apps sat `failed` with
`desired.json` saying `running`, and only a new `deploymentId` moved them. That was wrong.

A brand new app, never churned, recovers by itself:

```
killing its firecracker pid 132665
  [5s] state=running restartCount=0 firecracker=1   RECOVERED
```

The five stuck apps had spent their `maxRestarts: 5` across a morning of being killed, which is
the restart policy doing its job. What is fair to say is narrower: **when the budget is spent,
the report does not say so.** The instance reads `state: failed`, `restartCount: 0`,
`lastExitCode: -1`, and a `message` left over from its last successful boot — "starting the
tenant as uid 65534 with data at /app/data". Nothing there tells an operator the instance is
terminally out of restarts, and `restartCount` is not the counter the decision is made on.

**Since fixed.** A refused start now carries which of the two things refused it. One waiting out
its backoff is left alone as before; one that will not start again says so, with both numbers the
decision was actually made on:

```
out of restarts: 6 starts attempted against a budget of 5, and this instance will not be
started again until it is deployed afresh
```

`restartCount` still counts the starts that worked, because that is the field's meaning on the
wire. The attempts go in the sentence instead, which is the number the startability check was
reading all along.
