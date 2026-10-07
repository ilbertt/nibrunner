# Changelog

## [2026.10.3] - 2026-10-07

### Features

- Derive VM CPU and memory limits from guest resources (#244)
- Bound concurrent VM boots and restores (#243)
- Bound HTTP concurrency across the host and per app (#242)
- Keep bounded HTTP request logs for each app (#239)

### Fixes

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
