use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use protocol::CronJobDefinition;
use sha2::{Digest, Sha256};

use super::scheduler::JobKey;

pub fn cron_job_id(key: &JobKey, job: &CronJobDefinition) -> String {
    let definition = serde_json::to_vec(&(&key.app_id, &key.deployment_id, key.index, job))
        .expect("validated cron definitions serialize as JSON");
    format!("cron-{}", URL_SAFE_NO_PAD.encode(Sha256::digest(definition)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_id, deployment_id};

    fn key() -> JobKey {
        JobKey {
            app_id: app_id(),
            deployment_id: deployment_id(),
            index: 0,
        }
    }

    fn job() -> CronJobDefinition {
        CronJobDefinition {
            schedule: protocol::CronSchedule::parse("@daily").unwrap(),
            command: protocol::CronCommand::parse("echo tenant-secret").unwrap(),
            environment: None,
        }
    }

    #[test]
    fn a_registered_job_keeps_its_identity_across_runs_and_restarts() {
        let id = cron_job_id(&key(), &job());
        assert_eq!(id, "cron-mp9IEQFcHoMd3455mmb0Ns4z4GEyMthWViIRFWvri7Y");
        assert_eq!(cron_job_id(&key(), &job()), id);
        assert!(id.starts_with("cron-"));
        assert!(!id.contains("tenant-secret"));
    }

    #[test]
    fn a_changed_job_or_deployment_gets_a_distinct_identity() {
        let before = cron_job_id(&key(), &job());
        let mut changed = job();
        changed.command = protocol::CronCommand::parse("echo changed").unwrap();
        assert_ne!(before, cron_job_id(&key(), &changed));
        changed = job();
        changed.environment = Some(serde_json::from_str(r#"{"TOKEN":"tenant-secret"}"#).unwrap());
        assert_ne!(before, cron_job_id(&key(), &changed));
        let mut next = key();
        next.index = 1;
        assert_ne!(before, cron_job_id(&next, &job()));
        next.deployment_id = protocol::DeploymentId::parse("dep-2").unwrap();
        assert_ne!(before, cron_job_id(&next, &job()));
    }
}
