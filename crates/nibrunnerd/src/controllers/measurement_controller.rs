use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::services::idle_service::IdleService;
use crate::services::usage_service::UsageService;

const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

pub struct MeasurementController {
    idle: Arc<dyn IdleService>,
    usage: Arc<dyn UsageService>,
}

impl MeasurementController {
    pub fn new(idle: Arc<dyn IdleService>, usage: Arc<dyn UsageService>) -> Arc<Self> {
        Arc::new(Self { idle, usage })
    }

    pub async fn measurement_once(&self) {
        self.idle.record_activity().await;
        self.idle.apply_sleep().await;
        self.usage.measure().await;
    }
}

#[async_trait]
impl Controller for MeasurementController {
    fn name(&self) -> &'static str {
        "measurement"
    }

    async fn run(&self) {
        loop {
            tokio::time::sleep(MEASUREMENT_INTERVAL).await;
            self.measurement_once().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::idle_service::MockIdleService;
    use crate::services::usage_service::MockUsageService;

    #[tokio::test]
    async fn a_measurement_sweep_measures_after_it_has_let_the_quiet_apps_go() {
        let mut sequence = mockall::Sequence::new();
        let mut idle = MockIdleService::new();
        let mut usage = MockUsageService::new();
        idle.expect_record_activity()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());
        idle.expect_apply_sleep()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());
        usage
            .expect_measure()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| ());

        MeasurementController::new(Arc::new(idle), Arc::new(usage))
            .measurement_once()
            .await;
    }
}
