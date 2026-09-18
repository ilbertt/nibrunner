use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::controllers::Controller;
use crate::services::idle_service::IdleService;

// It is the counters this reads that set the floor — they come from the ruleset, whose size is
// the number of apps on the host. A reading is taken on a clock of its own rather than at the
// head of each sleep pass, because a pass is as long as its snapshots: a couple of hundred apps
// due at once is minutes, and a reading that waits for them leaves the meters standing still and
// an app taking hundreds of requests a second unrecorded, and so quiet.
const ACTIVITY_INTERVAL: Duration =
    Duration::from_millis(crate::domain::reconcile::idle::ACTIVITY_INTERVAL_MS);

pub struct ActivityController {
    idle: Arc<dyn IdleService>,
}

impl ActivityController {
    pub fn new(idle: Arc<dyn IdleService>) -> Arc<Self> {
        Arc::new(Self { idle })
    }

    pub async fn activity_once(&self) {
        self.idle.record_activity().await;
    }
}

#[async_trait]
impl Controller for ActivityController {
    fn name(&self) -> &'static str {
        "activity"
    }

    async fn run(&self) {
        loop {
            tokio::time::sleep(ACTIVITY_INTERVAL).await;
            self.activity_once().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::controllers::idle_controller::IdleController;
    use crate::services::idle_service::MockIdleService;

    #[tokio::test]
    async fn a_tick_takes_a_reading_and_decides_nothing() {
        let mut idle = MockIdleService::new();
        idle.expect_record_activity().times(1).returning(|| ());
        idle.expect_apply_sleep().never();

        ActivityController::new(Arc::new(idle)).activity_once().await;
    }

    /// An idle service that counts the readings taken of it and holds every sleep pass for a
    /// stretch, so that a reading taken while a pass is under way is something a test can see.
    struct HeldPasses {
        readings: AtomicUsize,
        holds_for: Duration,
        in_flight: AtomicUsize,
        begun: tokio::sync::Notify,
    }

    impl HeldPasses {
        fn holding_sleeps_for(holds_for: Duration) -> Arc<Self> {
            Arc::new(Self {
                readings: AtomicUsize::new(0),
                holds_for,
                in_flight: AtomicUsize::new(0),
                begun: tokio::sync::Notify::new(),
            })
        }

        fn readings(&self) -> usize {
            self.readings.load(Ordering::SeqCst)
        }

        fn sleep_in_flight(&self) -> bool {
            self.in_flight.load(Ordering::SeqCst) > 0
        }

        async fn sleep_begun(&self) {
            self.begun.notified().await;
        }
    }

    #[async_trait]
    impl IdleService for HeldPasses {
        async fn record_activity(&self) {
            self.readings.fetch_add(1, Ordering::SeqCst);
        }

        async fn apply_sleep(&self) {
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            self.begun.notify_one();
            tokio::time::sleep(self.holds_for).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reading_is_taken_every_interval_with_nothing_sleeping() {
        let idle = HeldPasses::holding_sleeps_for(Duration::ZERO);
        let controller = ActivityController::new(idle.clone());
        let reading = tokio::spawn(async move { controller.run().await });
        tokio::time::sleep(ACTIVITY_INTERVAL * 3 + Duration::from_millis(100)).await;
        reading.abort();
        let _ = reading.await;

        assert_eq!(idle.readings(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sleep_pass_that_takes_long_does_not_hold_up_the_readings() {
        let idle = HeldPasses::holding_sleeps_for(ACTIVITY_INTERVAL * 4);
        let sleeping = IdleController::new(idle.clone());
        let reading = ActivityController::new(idle.clone());
        let sleeping = tokio::spawn(async move { sleeping.run().await });
        let reading = tokio::spawn(async move { reading.run().await });

        idle.sleep_begun().await;
        let taken = idle.readings();
        tokio::time::sleep(ACTIVITY_INTERVAL + ACTIVITY_INTERVAL / 2).await;

        assert!(idle.sleep_in_flight(), "the pass is still on its snapshots");
        assert!(
            idle.readings() > taken,
            "no reading was taken while the pass held"
        );
        sleeping.abort();
        reading.abort();
        let _ = sleeping.await;
        let _ = reading.await;
    }
}
