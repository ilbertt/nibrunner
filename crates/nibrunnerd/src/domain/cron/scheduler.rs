use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use protocol::{AppId, CronJobDefinition, CronTable, DeploymentId};

use super::schedule::{InvalidCronSchedule, ParsedSchedule};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobKey {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub index: usize,
}

#[derive(Debug, Clone)]
pub struct ScheduledRun {
    pub key: JobKey,
    pub job: CronJobDefinition,
    pub scheduled_at: DateTime<Utc>,
}

#[derive(Clone)]
struct ScheduledJob {
    job: CronJobDefinition,
    schedule: ParsedSchedule,
    next: DateTime<Utc>,
}

#[derive(Default)]
pub struct CronScheduler {
    jobs: BTreeMap<JobKey, ScheduledJob>,
    tables: BTreeMap<AppId, CronTable>,
}

impl CronScheduler {
    pub fn synchronize(
        &mut self,
        tables: &[CronTable],
        now: DateTime<Utc>,
        time_zone: Tz,
    ) -> Result<(), InvalidCronSchedule> {
        let mut jobs = BTreeMap::new();
        let mut next_tables = BTreeMap::new();
        for table in tables {
            let unchanged = self.tables.get(&table.app_id) == Some(table);
            for (index, job) in table.jobs.iter().enumerate() {
                let key = JobKey {
                    app_id: table.app_id.clone(),
                    deployment_id: table.deployment_id.clone(),
                    index,
                };
                let held = self.jobs.get(&key).filter(|_| unchanged).cloned();
                let held = if let Some(held) = held {
                    held
                } else {
                    let schedule = ParsedSchedule::parse(job.schedule.as_str())?;
                    let next = schedule.next_after(now, time_zone)?;
                    ScheduledJob {
                        job: job.clone(),
                        schedule,
                        next,
                    }
                };
                jobs.insert(key, held);
            }
            next_tables.insert(table.app_id.clone(), table.clone());
        }
        self.jobs = jobs;
        self.tables = next_tables;
        Ok(())
    }

    pub fn next_run(&self) -> Option<DateTime<Utc>> {
        self.jobs.values().map(|job| job.next).min()
    }

    pub fn due(&mut self, now: DateTime<Utc>, time_zone: Tz) -> Vec<ScheduledRun> {
        let mut runs = Vec::new();
        self.jobs.retain(|key, held| {
            if held.next > now {
                return true;
            }
            let scheduled_at = held.next;
            let Ok(next) = held.schedule.next_after(now, time_zone) else {
                return false;
            };
            held.next = next;
            runs.push(ScheduledRun {
                key: key.clone(),
                job: held.job.clone(),
                scheduled_at,
            });
            true
        });
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_id, deployment_id};

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }
    fn table() -> CronTable {
        CronTable {
            app_id: app_id(),
            deployment_id: deployment_id(),
            jobs: vec![CronJobDefinition {
                schedule: protocol::CronSchedule::parse("* * * * *").unwrap(),
                command: protocol::CronCommand::parse("echo ok").unwrap(),
                environment: None,
            }]
            .into(),
            crontab: None,
        }
    }

    #[test]
    fn a_restart_schedules_only_occurrences_after_it_started() {
        let mut scheduler = CronScheduler::default();
        scheduler
            .synchronize(&[table()], at("2026-01-01T00:05:30Z"), chrono_tz::UTC)
            .unwrap();
        assert!(scheduler
            .due(at("2026-01-01T00:05:59Z"), chrono_tz::UTC)
            .is_empty());
        assert_eq!(scheduler.due(at("2026-01-01T00:06:00Z"), chrono_tz::UTC).len(), 1);
    }

    #[test]
    fn a_delayed_pass_runs_once_and_never_replays_missed_minutes() {
        let mut scheduler = CronScheduler::default();
        scheduler
            .synchronize(&[table()], at("2026-01-01T00:00:00Z"), chrono_tz::UTC)
            .unwrap();
        let runs = scheduler.due(at("2026-01-01T00:10:00Z"), chrono_tz::UTC);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].scheduled_at, at("2026-01-01T00:01:00Z"));
        assert!(scheduler
            .due(at("2026-01-01T00:10:00Z"), chrono_tz::UTC)
            .is_empty());
    }

    #[test]
    fn unchanged_tables_keep_the_due_instant_and_replacements_start_afresh() {
        let mut scheduler = CronScheduler::default();
        scheduler
            .synchronize(&[table()], at("2026-01-01T00:00:00Z"), chrono_tz::UTC)
            .unwrap();
        scheduler
            .synchronize(&[table()], at("2026-01-01T00:01:00Z"), chrono_tz::UTC)
            .unwrap();
        assert_eq!(scheduler.due(at("2026-01-01T00:01:00Z"), chrono_tz::UTC).len(), 1);
        let mut replacement = table();
        replacement.deployment_id = DeploymentId::parse("dep-2").unwrap();
        scheduler
            .synchronize(&[replacement], at("2026-01-01T00:02:00Z"), chrono_tz::UTC)
            .unwrap();
        assert!(scheduler
            .due(at("2026-01-01T00:02:00Z"), chrono_tz::UTC)
            .is_empty());
        scheduler
            .synchronize(&[], at("2026-01-01T00:03:00Z"), chrono_tz::UTC)
            .unwrap();
        assert!(scheduler
            .due(at("2026-01-01T00:10:00Z"), chrono_tz::UTC)
            .is_empty());
    }

    #[test]
    fn editing_the_table_reschedules_even_jobs_whose_definition_did_not_change() {
        let mut scheduler = CronScheduler::default();
        let original = table();
        scheduler
            .synchronize(
                std::slice::from_ref(&original),
                at("2026-01-01T00:00:00Z"),
                chrono_tz::UTC,
            )
            .unwrap();
        let edited = CronTable {
            crontab: Some(protocol::Crontab::parse("# edited\n* * * * * echo ok").unwrap()),
            ..original
        };
        scheduler
            .synchronize(&[edited], at("2026-01-01T00:01:00Z"), chrono_tz::UTC)
            .unwrap();
        assert!(scheduler
            .due(at("2026-01-01T00:01:00Z"), chrono_tz::UTC)
            .is_empty());
        assert_eq!(scheduler.next_run(), Some(at("2026-01-01T00:02:00Z")));
    }
}
