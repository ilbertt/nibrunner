use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::services::usage_service::UsageService;

// Every guest on the host is asked what it is using, so this is the expensive half and keeps the
// interval it always had. Letting a quiet app sleep used to ride along with it and no longer does,
// because the two want very different intervals.
const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

pub struct MeasurementController {
    usage: Arc<dyn UsageService>,
}

impl MeasurementController {
    pub fn new(usage: Arc<dyn UsageService>) -> Arc<Self> {
        Arc::new(Self { usage })
    }

    pub async fn measurement_once(&self) {
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
    use crate::services::usage_service::MockUsageService;

    #[tokio::test]
    async fn a_measurement_sweep_asks_every_guest_and_decides_nothing() {
        let mut usage = MockUsageService::new();
        usage.expect_measure().times(1).returning(|| ());
        MeasurementController::new(Arc::new(usage))
            .measurement_once()
            .await;
    }
}
