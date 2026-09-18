use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::services::idle_service::IdleService;

// An app sleeps on the first pass that finds it quiet for long enough, so this interval is the
// overshoot: the least a timeout can be is 60 seconds, and waiting a minute to notice made an app
// sleep at twice the time it asked for. A pass reads what `ActivityController` last recorded
// rather than taking a reading of its own — at most one of its intervals old — so the snapshots
// a pass waits on hold up the next pass and no reading.
const IDLE_INTERVAL: Duration = Duration::from_secs(5);

pub struct IdleController {
    idle: Arc<dyn IdleService>,
}

impl IdleController {
    pub fn new(idle: Arc<dyn IdleService>) -> Arc<Self> {
        Arc::new(Self { idle })
    }

    pub async fn idle_once(&self) {
        self.idle.apply_sleep().await;
    }
}

#[async_trait]
impl Controller for IdleController {
    fn name(&self) -> &'static str {
        "idle"
    }

    async fn run(&self) {
        loop {
            tokio::time::sleep(IDLE_INTERVAL).await;
            self.idle_once().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::idle_service::MockIdleService;

    #[tokio::test]
    async fn a_pass_reads_the_latest_activity_rather_than_taking_a_reading_of_its_own() {
        let mut idle = MockIdleService::new();
        idle.expect_record_activity().never();
        idle.expect_apply_sleep().times(1).returning(|| ());

        IdleController::new(Arc::new(idle)).idle_once().await;
    }
}
