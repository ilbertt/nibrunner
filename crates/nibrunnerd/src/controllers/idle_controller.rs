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

    /// One tick: passes until one leaves nothing due. A pass takes a bite of what is due and
    /// counts back the rest, and the rest does not wait out an interval for its turn.
    pub async fn idle_once(&self) {
        loop {
            let left_due = self.idle.apply_sleep().await;
            if left_due == 0 {
                break;
            }
        }
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
        idle.expect_apply_sleep().times(1).returning(|| 0);

        IdleController::new(Arc::new(idle)).idle_once().await;
    }

    #[tokio::test]
    async fn a_pass_that_left_apps_due_is_followed_by_another_at_once_and_one_that_left_none_is_not() {
        let mut idle = MockIdleService::new();
        let mut left = [1, 0].into_iter();
        idle.expect_apply_sleep()
            .times(2)
            .returning(move || left.next().expect("no pass after one that left nothing"));

        IdleController::new(Arc::new(idle)).idle_once().await;
    }
}
