use std::sync::Arc;

use async_trait::async_trait;
use protocol::AppId;

use crate::domain::meters::cpu_metered_after;
use crate::domain::usage::{compute_usage_after, volume_usage_after, MEASUREMENT_CONCURRENCY};
use crate::host::Host;
use crate::ports::{GuestMeasurements, GuestReading};

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait]
pub trait UsageService: Send + Sync {
    async fn measure(&self);
}

pub struct HostUsage {
    host: Arc<Host>,
    measurements: Arc<dyn GuestMeasurements>,
}

impl HostUsage {
    pub fn new(host: Arc<Host>, measurements: Arc<dyn GuestMeasurements>) -> Arc<Self> {
        Arc::new(Self { host, measurements })
    }

    async fn take_readings(&self) -> Vec<(AppId, GuestReading)> {
        let slots = self.host.slots().await;
        let mut taken = Vec::with_capacity(slots.len());
        for batch in slots.chunks(MEASUREMENT_CONCURRENCY) {
            let asked = batch
                .iter()
                .map(|slot| async { (slot.app_id.clone(), self.measurements.measure(&slot.app_id).await) });
            taken.extend(futures::future::join_all(asked).await);
        }
        taken
    }
}

#[async_trait]
impl UsageService for HostUsage {
    async fn measure(&self) {
        let taken = self.take_readings().await;
        let at = crate::clock::now_timestamp();
        let snapshot = self.host.state.snapshot().await;
        let volumes = volume_usage_after(&taken, &snapshot.volume_usage, at.clone());
        let compute = compute_usage_after(
            &taken,
            &snapshot.records,
            &snapshot.compute_usage,
            &snapshot.compute_ticks,
            at,
        );
        self.host
            .state
            .modify(move |snapshot| {
                // Under the write lock, and off the ticks this pass is about to replace: the
                // activity pass accumulates into the same meters from a task of its own, and what
                // a guest spent is the distance from the last reading to this one.
                let metered = cpu_metered_after(&snapshot.meters, &snapshot.compute_ticks, &taken);
                snapshot.meters = metered;
                snapshot.volume_usage = volumes;
                snapshot.compute_usage = compute.usage;
                snapshot.compute_ticks = compute.ticks;
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::MockGuestMeasurements;
    use crate::test_support::*;
    use guest_contract::filesystem::{MeasuredBytes, MeasuredCompute};
    fn compute(total: u64, busy: u64) -> MeasuredCompute {
        MeasuredCompute {
            memory_total_bytes: 268_435_456,
            memory_used_bytes: 1024,
            cpu_total_ticks: total,
            cpu_busy_ticks: busy,
        }
    }
    fn measuring(reading: GuestReading) -> Arc<MockGuestMeasurements> {
        let mut measurements = MockGuestMeasurements::new();
        measurements.expect_measure().returning(move |_| reading);
        Arc::new(measurements)
    }
    #[tokio::test]
    async fn a_host_with_no_apps_on_it_asks_no_guest_anything() {
        let host = test_host().await;
        let mut measurements = MockGuestMeasurements::new();
        measurements.expect_measure().never();

        HostUsage::new(host.arc().clone(), Arc::new(measurements))
            .measure()
            .await;
        assert!(host.state.snapshot().await.volume_usage.is_empty());
    }
    #[tokio::test]
    async fn every_app_with_a_slot_is_asked_exactly_once_a_sweep() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        let mut measurements = MockGuestMeasurements::new();
        measurements
            .expect_measure()
            .times(1)
            .withf(|asked| *asked == app_id())
            .returning(|_| GuestReading::default());

        HostUsage::new(host.arc().clone(), Arc::new(measurements))
            .measure()
            .await;
    }
    #[tokio::test]
    async fn what_a_guest_answered_is_what_the_host_reports_holding() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;
        let reading = GuestReading {
            filesystem: Some(MeasuredBytes {
                total_bytes: 2_000,
                used_bytes: 500,
            }),
            compute: Some(compute(2_000, 600)),
        };

        HostUsage::new(host.arc().clone(), measuring(reading))
            .measure()
            .await;

        let snapshot = host.state.snapshot().await;
        assert_eq!(
            snapshot.volume_usage.get(&app_id()).map(|usage| usage.used_bytes),
            Some(500)
        );
        assert_eq!(snapshot.compute_ticks.get(&app_id()), Some(&compute(2_000, 600)));
    }

    #[tokio::test]
    async fn what_a_guest_has_spent_since_the_last_sweep_is_added_to_what_it_had_already_spent() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;
        host.state
            .modify(|snapshot| {
                snapshot.compute_ticks.insert(app_id(), compute(1_000, 100));
                snapshot.meters.insert(
                    app_id(),
                    protocol::UsageMeters {
                        cpu_ms: 1_000,
                        ..Default::default()
                    },
                );
            })
            .await;
        let reading = GuestReading {
            filesystem: None,
            compute: Some(compute(2_000, 400)),
        };

        HostUsage::new(host.arc().clone(), measuring(reading))
            .measure()
            .await;

        let metered = host.state.snapshot().await.meters;
        assert_eq!(
            metered.get(&app_id()).map(|meter| meter.cpu_ms),
            Some(1_000 + 3_000),
            "300 busy ticks of 10ms each, on top of the second already spent"
        );
    }
    #[tokio::test]
    async fn a_share_appears_on_the_second_sweep_because_the_first_has_no_interval_behind_it() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;

        let usage = HostUsage::new(
            host.arc().clone(),
            measuring(GuestReading {
                filesystem: None,
                compute: Some(compute(1_000, 100)),
            }),
        );
        usage.measure().await;
        assert_eq!(
            host.state
                .snapshot()
                .await
                .compute_usage
                .get(&app_id())
                .and_then(|usage| usage.cpu_share),
            None
        );

        let usage = HostUsage::new(
            host.arc().clone(),
            measuring(GuestReading {
                filesystem: None,
                compute: Some(compute(2_000, 400)),
            }),
        );
        usage.measure().await;
        assert_eq!(
            host.state
                .snapshot()
                .await
                .compute_usage
                .get(&app_id())
                .and_then(|usage| usage.cpu_share),
            Some(0.3)
        );
    }
}
