# Changelog

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
