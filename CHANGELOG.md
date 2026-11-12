# Changelog

## [2026.11.11] - 2026-11-12

### Fixes

- Queue HTTP wakes when VM start slots are busy (#350)
- Defer health probes until guest initialization finishes (#351)

### Performance

- Use zstd fast mode for layer images (#352)

## [2026.11.10] - 2026-11-12

### Fixes

- Fail closed when tenant isolation cannot be verified (#348)

## [2026.11.9] - 2026-11-12

### Fixes

- Queue cron wakes while VM start slots are busy (#346)

## [2026.11.8] - 2026-11-12

### Features

- Run the HTTP proxy worker in its own process (#343)
- Serve HTTP through a transferable proxy worker (#340)
- Add the HTTP proxy worker control client (#337)
- **metrics:** Measure guest startup through readiness (#344)
- Expose guest startup checkpoints over vsock (#341)
- **metrics:** Measure guest clock handshake phases (#339)
- **metrics:** Measure cold VM boot phases (#334)
- **metrics:** Measure layer preparation phases (#332)
- Transfer HTTP listeners between processes (#329)
- **cron:** Preserve guest execution channels in a worker (#326)

### Fixes

- Coordinate HTTP log rotation across proxy workers (#333)
- **cron:** Keep running jobs alive across daemon restarts (#327)
- **logs:** Coordinate rotation across log writers (#325)

### Performance

- Move layer preparation to bounded blocking workers (#336)
- Detect guest control readiness sooner (#342)
- Write layer images directly to staged files (#335)
- Reuse downloaded artifact buffers (#338)
- Observe startup readiness every 50 ms (#331)
- Reduce executable layer packing cost (#330)

### Refactoring

- **proxy:** Allow listener ownership handoff (#328)
- **proxy:** Drain accepted HTTP connections (#323)
- **cron:** Inject the scheduled command executor (#324)

## [2026.11.7] - 2026-11-11

### Fixes

- **logs:** Preserve tenant line endings (#321)

## [2026.11.6] - 2026-11-10

### Features

- **vm:** Verify snapshot memory with fs-verity (#319)
- **storage:** Seal and measure fs-verity files (#315)
- **storage:** Enable verity on new ext4 host filesystems (#314)

### Fixes

- **vm:** Recover from hypervisor runtime failures (#318)
- **vm:** Preserve restart budgets when replacing snapshots (#317)

## [2026.11.5] - 2026-11-09

### Features

- **metrics:** Expose snapshot restore phase histograms (#311)
- **exports:** Select output format per export (#309)
- **exports:** Publish prepared directories locally (#307)

### Fixes

- **init:** Wait for guest control socket readiness (#310)
- **exports:** Preserve guest symlinks in bundles (#306)

## [2026.11.4] - 2026-11-08

### Features

- **api:** Tunnel deployment-scoped guest connections (#304)
- **config:** Configure a private guest service socket (#302)
- **guest:** Open deployment-fenced service connections (#301)

### Fixes

- **guest:** Backport the ext4 NULL folio correction (#298)

### Refactoring

- **ipc:** Share private host socket binding (#303)
- **state:** Share guest activity leases beyond cron (#299)

## [2026.11.3] - 2026-11-06

### Fixes

- **install:** Check storage in the host mount namespace (#296)

## [2026.11.2] - 2026-11-06

### Fixes

- **ci:** Stabilize integration checks (#294)
- **install:** Initialize signature-free host storage (#292)

## [2026.11.1] - 2026-11-04

### Features

- Report host CPU vulnerability status at startup (#286)
- **proxy:** Redirect the configured fallback hostname (#289)
- Reject hosts with active kernel samepage merging (#285)
- **Breaking:** Require host SMT to be disabled (#284)
- Publish structured tenant log records (#263)
- Report deployment cron registrations (#262)
- Initialize and mount configured host storage (#280)
- Include guest usage measurements in host reports (#261)

### Fixes

- **ci:** Elevate only integration test executables (#288)
- Reject snapshots from outdated guest images (#287)
- **start:** Bind applied inputs to service activations (#282)
- **start:** Persist unapplied service changes across invocations (#279)
- **install:** Allow default AWS credential providers (#274)

### Refactoring

- **config:** Name the fallback hostname origin_host (#290)

### Documentation

- Document host requirements for workload isolation (#283)

## [2026.11.0] - 2026-11-02

### Fixes

- **guest:** Update kernel and Debian base image (#272)

## [2026.10.7] - 2026-10-28

### Features

- Support configured JWKS URLs for JWT access policies (#258)
- **Breaking:** Configure per-app JWT access policies (#257)
- Verify JWT policies in the HTTP proxy (#255)

### Documentation

- Document Cloudflare Access setup (#259)

## [2026.10.6] - 2026-10-27

### Fixes

- Prevent external access to internal HTTP ports (#254)

## [2026.10.5] - 2026-10-22

### Fixes

- Mount devpts and send upgrade requests in origin form (#252)

## [2026.10.4] - 2026-10-14

### Fixes

- Forget reports for removed exports (#248)

### Maintenance

- Deploy docs on each release (#249)

## [2026.10.3] - 2026-10-07

### Features

- Derive VM CPU and memory limits from guest resources (#244)
- Bound concurrent VM boots and restores (#243)
- Bound HTTP concurrency across the host and per app (#242)
- Keep bounded HTTP request logs for each app (#239)

### Fixes

- Reserve VM memory for snapshots (#246)
- Report a start refused for want of a slot on a record, not only in a log (#241)
- Preserve USTAR paths when assembling OCI layers (#238)
- Synchronize guest clocks before resuming restored tenants (#237)

### Maintenance

- Cover popular remote OCI image unpacking in CI (#240)

## [2026.10.2] - 2026-10-04

### Features

- Pull digest-pinned images from public OCI registries (#234)
- Prepare OCI image layers on the host (#218)
- Assemble OCI image archives (#217)

### Documentation

- Consolidate OCI image guidance in layers (#235)

## [2026.10.1] - 2026-10-04

### Features

- Require the Firecracker jailer for every microVM (#232)
- Isolate VMM children in private mount namespaces (#227)
- Stage trusted VM inputs for jail rebuilding (#226)
- Reserve stable non-root identities during installation (#224)
- Readopt VMMs from their recorded root and identity (#222)
- Assign persistent TAPs to non-root VMMs (#220)
- **exports:** Include the app crontab in export bundles (#216)

### Fixes

- Verify and atomically publish microVM snapshots (#212)
- Publish snapshots through a private exchange (#228)
- Verify recorded VMM peers before API commands (#223)
- Reach guest services through their VMM directory (#219)
- Protect private guest sockets from path replacement (#214)
- Persist host identity from accepted desired state (#209)

### Refactoring

- Share guest channel attachment across boot and readoption (#225)
- Separate VMM command construction from supervision (#221)

### Documentation

- Explain automatic jailer isolation in host setup (#230)

### Maintenance

- Prove jailed VMM isolation across sleep and readoption (#229)
- Embed the matching Firecracker jailer (#211)

## [2026.10.0] - 2026-10-03

### Features

- Schedule and cancel cron jobs on the host (#202)
- Dispatch guest cron commands and collect their output (#201)
- Drain cron runs before stopping their deployment (#203)
- Track due cron jobs and keep active guests awake (#200)
- Receive crontabs from current guest deployments (#199)
- Execute cron commands inside the guest (#198)
- Persist deployment-scoped cron registrations (#197)
- Parse crontabs in the configured host time zone (#196)
- Define the guest cron execution transport (#195)
- Register guest cron jobs with crontab (#194)
- Define crontab registration and host policy (#192)

### Fixes

- Recheck sleep eligibility under the transition lock (#191)
- Release activator pools when their apps are removed (#189)

### Documentation

- Explain guest crontabs and host scheduling policy (#205)

### Maintenance

- Require fresh cron registration after deployment replacement (#207)
- Cover cron execution and scheduled guest lifecycle (#204)
- Remove CLAUDE.md symlink and clarify PR guidance (#193)
- Cancel superseded pull request runs (#190)
- Bound the drain of an expired app lifetime (#188)
- Keep an app awake while a quiet request is open (#183)
- Run the new artifact when replacing a sleeping deployment (#186)
- The ports an app names besides its HTTP one (#150)
- Isolate the new owner of a reused slot (#185)
- A tenant's output, its restarts and its silence (#149)
- Recover a killed microVM without disturbing its neighbour (#187)
- Isolation asked of the kernel rather than of the ruleset's text (#148)
- Concurrent callers restore one guest and all receive answers (#182)
- A volume belongs to one app and keeps what that app put in it (#147)
- Sleep and wake, from the caller's side (#146)
- What the document names, what it stops naming, and what the report claims (#143)
- A lane that boots real microVMs, and the tenant it boots (#141)

## [2026.9.1] - 2026-09-30

### Fixes

- Require complete braced environment references (#180)

## [2026.9.0] - 2026-09-29

Initial release.
