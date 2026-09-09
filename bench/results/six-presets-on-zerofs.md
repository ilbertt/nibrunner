# Six real apps, churned, on ZeroFS volumes

The six `deploy-link` presets on one Hetzner host, volumes on ZeroFS over S3, TLS on
`*.canister.site` through a Cloudflare origin certificate. The document was rewritten as the run
went — one app, then three, then six, then a mixture, then none — because a host is only ever
told what it should be holding, never what to do about it.

## What held

| Change to the document | What the host did |
|---|---|
| 1 app → 3 | new apps started; the first kept slot, port and device |
| 3 → 6 | five up in under a minute; gitea below |
| `running` → `on-request` | went to `idle`; woke on the next request |
| `running` → `stopped` | stopped, volume and slot kept |
| named → `absent` | volume deleted, slot released |
| dropped from the document | tenant stopped, volume kept |
| daemon restarted under load | every tenant adopted, none restarted |

Waking an idle app, measured from outside over TLS:

```
pocketbase  cold 0.229s   warm 0.180s
boop        cold 0.201s
```

Serving, warm, through the proxy — the 40ms Nagle stall found earlier stays gone:

```
pocketbase /api/health   ttfb 0.084s
gitea /                  ttfb 0.208s
```

## Gitea's first boot takes sixteen minutes, and only its first

```
first boot   14:22:31 started → 14:38:12 serving      15m 41s
second boot  serving after                                12s
```

The whole cost is a one-time schema migration, and on this storage it is brutal: individual
`CREATE` statements taking twenty to forty seconds each, logged by gitea itself as
`[Slow SQL Query]`. SQLite DDL is many small synchronous writes, and each one is a round trip
the volume has to make.

Nothing is wrong with the host or the app — but the default 120s grace period cannot cover it,
so the app is declared `failed` while it is still legitimately working:

```
nothing answered on port 3000 inside the guest: 219 health probes failed
after the 120000ms grace period
```

At 600s it still failed, at 1356 probes. The microVM was never killed and the migration ran on
to completion underneath, so the report was wrong about the app rather than the app being
wrong. An app whose first boot migrates a database needs a grace period set for that migration,
and there is no way to ask for one that applies only to the first boot.

## An app dropped from the document keeps its slot until the daemon restarts

Not fixed here, and the sharpest open thread from this run. With an empty document, every
tenant stopped and every tap and nft rule went, but three of five instance records stayed put
across many converge ticks, still holding their host ports:

```
[('gitea', 'stopped'), ('pocketbase', 'idle'), ('sharkord', 'stopped')]   × many ticks
```

Restarting the daemon cleared them instantly. So the records are droppable and the loop is not
dropping them: `InstancePlan::Forget` reaches `discard` + `drop_record`, and after a restart
`started_this_boot` is false, which is the difference between the two paths. `discard` failing
while the pidfile stays adopted would produce exactly this, since `observe` rebuilds `app_ids`
from `adopted_app_ids()` every tick — but that is where to look, not a diagnosis.

Two NBD attachments are also left over: after the restart the daemon reports three volumes
while five devices are still attached and five device files remain in the store.
