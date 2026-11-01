use chrono::{DateTime, Utc};
use protocol::{CronListing, DesiredInstanceState, RegisteredCronJob, Timestamp};

use crate::domain::cron::jobs::cron_job_id;
use crate::domain::cron::registry::CronRegistryError;
use crate::domain::cron::schedule::ParsedSchedule;
use crate::domain::cron::scheduler::JobKey;
use crate::host::Host;

pub async fn read(host: &Host, after: DateTime<Utc>) -> Result<Vec<CronListing>, CronRegistryError> {
    let desired = host.cache.lock().await.latest().cloned();
    let Some(desired) = desired else {
        return Ok(Vec::new());
    };
    let tables = host.cron.tables().await?;
    let mut listings = Vec::new();
    for instance in desired.instances {
        let Some(table) = tables
            .iter()
            .find(|table| table.app_id == instance.app_id && table.deployment_id == instance.deployment_id)
        else {
            continue;
        };
        let enabled = instance.desired_state != DesiredInstanceState::Stopped;
        let jobs = table
            .jobs
            .iter()
            .enumerate()
            .map(|(index, job)| {
                let next_run_at = if enabled {
                    let next = ParsedSchedule::parse(job.schedule.as_str())
                        .and_then(|schedule| schedule.next_after(after, host.config.cron.time_zone))
                        .map_err(|_| CronRegistryError::Restore)?;
                    Some(Timestamp::from_epoch_ms(next.timestamp_millis()))
                } else {
                    None
                };
                Ok(RegisteredCronJob {
                    job_id: cron_job_id(
                        &JobKey {
                            app_id: instance.app_id.clone(),
                            deployment_id: instance.deployment_id.clone(),
                            index,
                        },
                        job,
                    ),
                    definition: job.clone(),
                    next_run_at,
                })
            })
            .collect::<Result<_, CronRegistryError>>()?;
        listings.push(CronListing {
            app_id: instance.app_id,
            deployment_id: instance.deployment_id,
            enabled,
            time_zone: host.config.cron.time_zone.name().into(),
            jobs,
        });
    }
    Ok(listings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn sleeping_and_stopped_apps_list_their_durable_jobs_without_waking_a_guest() {
        let host = test_host().await;
        let after = "2026-11-01T12:00:00Z".parse().unwrap();
        host.cron
            .begin_deployment(&app_id(), &deployment_id())
            .await
            .unwrap();
        host.cron
            .replace(
                &app_id(),
                &deployment_id(),
                "TOKEN=secret\n* * * * * echo task\n",
                after,
            )
            .await
            .unwrap();
        for state in [DesiredInstanceState::OnRequest, DesiredInstanceState::Stopped] {
            host.cache.lock().await.accept(desired_state(|desired| {
                desired.instances = vec![desired_instance(|instance| instance.desired_state = state)];
            }));
            let listings = read(&host, after).await.unwrap();
            assert_eq!(listings.len(), 1);
            let listing = &listings[0];
            assert_eq!(listing.app_id, app_id());
            assert_eq!(listing.deployment_id, deployment_id());
            assert_eq!(listing.time_zone, "UTC");
            assert_eq!(listing.jobs[0].definition.command.expose(), "echo task");
            assert_eq!(
                listing.jobs[0].next_run_at.is_some(),
                state != DesiredInstanceState::Stopped
            );
            assert_eq!(listing.enabled, state != DesiredInstanceState::Stopped);
            let serialized = serde_json::to_value(listing).unwrap();
            assert_eq!(serialized["jobs"][0]["command"], "echo task");
            assert_eq!(serialized["jobs"][0]["environment"]["TOKEN"], "secret");
        }
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_old_deployments_registrations_are_not_reported_for_its_replacement() {
        let host = test_host().await;
        host.cron
            .begin_deployment(&app_id(), &deployment_id())
            .await
            .unwrap();
        host.cache.lock().await.accept(desired_state(|desired| {
            desired.instances = vec![desired_instance(|instance| {
                instance.deployment_id = protocol::DeploymentId::parse("replacement").unwrap();
            })];
        }));
        assert!(read(&host, Utc::now()).await.unwrap().is_empty());
    }
}
