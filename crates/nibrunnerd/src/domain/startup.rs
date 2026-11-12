use std::sync::Arc;
use std::time::{Duration, Instant};

use guest_contract::startup::StartupMilestone;

use crate::domain::metrics::startup::GuestStartupPhase;
use crate::domain::report::InstanceRecord;
use crate::host::Host;
use crate::ports::GuestStartupSample;

pub(crate) fn phases(
    sample: &GuestStartupSample,
    ready_at: Instant,
) -> [(GuestStartupPhase, Duration); GuestStartupPhase::ALL.len()] {
    let init = sample.guest.at(StartupMilestone::InitStarted);
    let root = sample.guest.at(StartupMilestone::RootReady);
    let exec = sample.guest.at(StartupMilestone::TenantExec);
    let sampled_since_ready = sample
        .received_at
        .saturating_duration_since(ready_at)
        .saturating_sub(sample.round_trip / 2);
    let exec_to_sample = Duration::from_micros(sample.guest.sampled_at_us - exec);
    [
        (GuestStartupPhase::BootToInit, Duration::from_micros(init)),
        (GuestStartupPhase::RootSetup, Duration::from_micros(root - init)),
        (GuestStartupPhase::Launch, Duration::from_micros(exec - root)),
        (
            GuestStartupPhase::ExecToReady,
            exec_to_sample.saturating_sub(sampled_since_ready),
        ),
        (GuestStartupPhase::SampleRoundTrip, sample.round_trip),
    ]
}

pub(crate) fn observed_ready(host: Arc<Host>, observed: InstanceRecord, ready_at: Instant) {
    tokio::spawn(async move {
        let app_id = observed.app_id.clone();
        let Some(sample) = host.vms.startup_sample(&app_id).await else {
            return;
        };
        if !host
            .state
            .record(&app_id)
            .await
            .is_some_and(|latest| latest.has_same_start(&observed))
        {
            return;
        }
        for (phase, took) in phases(&sample, ready_at) {
            host.metrics.startup.observed(phase, took);
            tracing::info!(%app_id, phase = phase.as_str(), took_us = took.as_micros(), "guest startup measured");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use guest_contract::startup::StartupSnapshot;

    #[test]
    fn delayed_sampling_is_removed_from_the_tenant_readiness_estimate() {
        let origin = Instant::now();
        let sample = GuestStartupSample {
            guest: StartupSnapshot::new(1_000, [100, 200, 300]).unwrap(),
            received_at: origin + Duration::from_micros(1_010),
            round_trip: Duration::from_micros(20),
        };
        let measurements = phases(&sample, origin + Duration::from_micros(700));
        assert_eq!(
            measurements[3],
            (GuestStartupPhase::ExecToReady, Duration::from_micros(400))
        );
        assert_eq!(
            measurements[4],
            (GuestStartupPhase::SampleRoundTrip, Duration::from_micros(20))
        );
    }

    #[test]
    fn sampling_uncertainty_never_produces_a_negative_readiness_duration() {
        let origin = Instant::now();
        let sample = GuestStartupSample {
            guest: StartupSnapshot::new(301, [100, 200, 300]).unwrap(),
            received_at: origin + Duration::from_micros(1_000),
            round_trip: Duration::from_micros(20),
        };
        assert_eq!(phases(&sample, origin)[3].1, Duration::ZERO);
    }
}
