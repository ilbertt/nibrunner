//! What each guest on this host is holding and spending.
//!
//! Ported from `apps/agent/src/lib/agent/usage.ts`. Measured on the sweep that already decides
//! what sleeps rather than on the report it feeds: measuring is a round trip into every guest on
//! the host, and a report that waited for those would be a report a hung tenant could stop —
//! while what the report carries otherwise is what a deploy converges on.

use std::collections::BTreeMap;

use guest_contract::filesystem::{MeasuredBytes, MeasuredCompute};
use protocol::{AppId, ComputeUsage, FilesystemUsage, Timestamp};

use crate::host::Host;
use crate::services::report::instance_record::InstanceRecord;

/// Enough that one guest which has stopped answering does not hold up the rest, and low enough
/// that a packed host is not opening a connection into every tenant it runs at once.
const MEASUREMENT_CONCURRENCY: usize = 4;

/// Every vCPU the app was given, busy. Two saturated vCPUs is this, not twice it.
const FULLY_BUSY: f64 = 1.0;

/// What one guest answered, and either half may be missing while the other arrived: they are two
/// exchanges, and a guest whose image predates one of the verbs refuses that one and answers the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuestReading {
    pub filesystem: Option<MeasuredBytes>,
    pub compute: Option<MeasuredCompute>,
}

/// The share of the vCPUs spent computing between two readings.
///
/// Missing rather than zero where there is nothing to compare against. The counters are cumulative
/// since the guest booted, so the first reading after this daemon starts has no interval behind
/// it, and one standing *behind* the reading before it is a guest that has rebooted since. Both
/// would divide by a difference that is not an interval, and a made-up nought is the reading an
/// owner would act on.
pub fn share_between(before: Option<MeasuredCompute>, after: MeasuredCompute) -> Option<f64> {
    let before = before?;
    let total = after.cpu_total_ticks.checked_sub(before.cpu_total_ticks)?;
    let busy = after.cpu_busy_ticks.checked_sub(before.cpu_busy_ticks)?;
    if total == 0 {
        return None;
    }
    Some((busy as f64 / total as f64).min(FULLY_BUSY))
}

fn as_compute_usage(
    measured: MeasuredCompute,
    before: Option<MeasuredCompute>,
    at: Timestamp,
) -> ComputeUsage {
    ComputeUsage {
        memory_total_bytes: measured.memory_total_bytes,
        memory_used_bytes: measured.memory_used_bytes,
        cpu_share: share_between(before, measured),
        measured_at: at,
    }
}

/// What every app with a slot keeps after a sweep, given what each one answered.
///
/// A slot outlives the microVM, so this is also what a *suspended* app keeps: it stopped, its
/// guest went with it, and the honest answer about it is what was true when it was last running
/// rather than nothing at all. Each reading carries the moment it was taken, which is what lets
/// whoever reads it tell the two apart.
pub fn volume_usage_after(
    taken: &[(AppId, GuestReading)],
    previous: &BTreeMap<AppId, FilesystemUsage>,
    at: Timestamp,
) -> BTreeMap<AppId, FilesystemUsage> {
    taken
        .iter()
        .filter_map(|(app_id, reading)| {
            let measured = match reading.filesystem {
                Some(measured) => FilesystemUsage {
                    total_bytes: measured.total_bytes,
                    used_bytes: measured.used_bytes,
                    measured_at: at.clone(),
                },
                None => previous.get(app_id)?.clone(),
            };
            Some((app_id.clone(), measured))
        })
        .collect()
}

pub struct ComputeAfter {
    pub usage: BTreeMap<AppId, ComputeUsage>,
    /// The raw counters, kept only so the next sweep has something to subtract.
    pub ticks: BTreeMap<AppId, MeasuredCompute>,
}

/// An app asleep between requests forgets, where a suspended one remembers.
///
/// What a suspended app last spent answers a question its owner asked by taking it offline; an
/// `on-request` app sleeps and wakes on its own all day, and a figure carried across every sleep
/// would have an app that is holding nothing go on reporting what it held when it last ran. Not
/// costing anything while asleep is the whole of the policy, so the reading goes with the microVM
/// — and so do the counters, because a wake is a cold boot and the next reading has nothing
/// behind it to subtract.
pub fn compute_usage_after(
    taken: &[(AppId, GuestReading)],
    records: &BTreeMap<AppId, InstanceRecord>,
    previous_usage: &BTreeMap<AppId, ComputeUsage>,
    previous_ticks: &BTreeMap<AppId, MeasuredCompute>,
    at: Timestamp,
) -> ComputeAfter {
    let mut usage = BTreeMap::new();
    let mut ticks = BTreeMap::new();
    for (app_id, reading) in taken {
        if records.get(app_id).is_some_and(InstanceRecord::is_idle) {
            continue;
        }
        let before = previous_ticks.get(app_id).copied();
        match reading.compute {
            Some(measured) => {
                usage.insert(app_id.clone(), as_compute_usage(measured, before, at.clone()));
                ticks.insert(app_id.clone(), measured);
            }
            // A guest that could not be asked is not a guest that answered nothing: what it last
            // said stands, and the counters behind it stay so the next answer has an interval.
            None => {
                if let Some(kept) = previous_usage.get(app_id) {
                    usage.insert(app_id.clone(), kept.clone());
                }
                if let Some(before) = before {
                    ticks.insert(app_id.clone(), before);
                }
            }
        }
    }
    ComputeAfter { usage, ticks }
}

/// Asks every guest with a slot, and writes what came back into the host's own picture.
pub async fn measure(host: &Host) {
    let slots = host.slots().await;
    let mut taken: Vec<(AppId, GuestReading)> = Vec::with_capacity(slots.len());
    for batch in slots.chunks(MEASUREMENT_CONCURRENCY) {
        let asked = batch.iter().map(|slot| async {
            (
                slot.app_id.clone(),
                crate::services::filesystem::reader::measure(host, &slot.app_id).await,
            )
        });
        taken.extend(futures::future::join_all(asked).await);
    }

    let at = crate::clock::now_timestamp();
    let snapshot = host.state.snapshot().await;
    let volumes = volume_usage_after(&taken, &snapshot.volume_usage, at.clone());
    let compute = compute_usage_after(
        &taken,
        &snapshot.records,
        &snapshot.compute_usage,
        &snapshot.compute_ticks,
        at,
    );
    host.state
        .modify(move |snapshot| {
            snapshot.volume_usage = volumes;
            snapshot.compute_usage = compute.usage;
            snapshot.compute_ticks = compute.ticks;
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protocol::InstanceState;

    fn compute(total: u64, busy: u64) -> MeasuredCompute {
        MeasuredCompute {
            memory_total_bytes: 268_435_456,
            memory_used_bytes: 1024,
            cpu_total_ticks: total,
            cpu_busy_ticks: busy,
        }
    }

    fn at() -> Timestamp {
        observed_at()
    }

    #[test]
    fn a_share_is_the_busy_ticks_over_the_ticks_between_two_readings() {
        let share = share_between(Some(compute(1_000, 100)), compute(2_000, 400));
        assert_eq!(share, Some(0.3), "300 busy of 1000 elapsed");
    }

    /// The first reading after this daemon starts has no interval behind it, and a made-up nought
    /// is the reading an owner would act on.
    #[test]
    fn a_first_reading_has_no_share_rather_than_a_share_of_nothing() {
        assert_eq!(share_between(None, compute(1_000, 100)), None);
    }

    /// A counter standing behind the one before it is a guest that rebooted since, and the
    /// difference is not an interval.
    #[test]
    fn a_guest_that_restarted_its_counters_has_no_share_until_it_has_two_readings() {
        assert_eq!(share_between(Some(compute(5_000, 900)), compute(100, 10)), None);
        assert_eq!(
            share_between(Some(compute(1_000, 100)), compute(1_000, 100)),
            None
        );
    }

    /// Every vCPU the app was given, busy. Two saturated vCPUs is one, not two.
    #[test]
    fn a_fully_busy_guest_reads_as_one_however_many_vcpus_it_has() {
        let share = share_between(Some(compute(1_000, 100)), compute(2_000, 3_100));
        assert_eq!(share, Some(1.0));
    }

    /// It stopped, its guest went with it, and what was true when it was last running is a better
    /// answer than nothing at all.
    #[test]
    fn a_suspended_app_keeps_what_its_volume_last_measured() {
        let previous = BTreeMap::from([(
            app_id(),
            FilesystemUsage {
                total_bytes: 1_000,
                used_bytes: 400,
                measured_at: at(),
            },
        )]);
        let taken = vec![(app_id(), GuestReading::default())];
        let after = volume_usage_after(&taken, &previous, at());
        assert_eq!(after.get(&app_id()).map(|usage| usage.used_bytes), Some(400));
    }

    /// Not costing anything while asleep is the whole of the policy, so the reading goes with the
    /// microVM rather than being carried across every wake.
    #[test]
    fn an_app_asleep_between_requests_forgets_what_it_last_spent() {
        let records = BTreeMap::from([(
            app_id(),
            instance_record(|record| record.state = InstanceState::Idle),
        )]);
        let previous_usage = BTreeMap::from([(
            app_id(),
            ComputeUsage {
                memory_total_bytes: 1,
                memory_used_bytes: 1,
                cpu_share: Some(0.5),
                measured_at: at(),
            },
        )]);
        let taken = vec![(app_id(), GuestReading::default())];

        let after = compute_usage_after(&taken, &records, &previous_usage, &BTreeMap::new(), at());
        assert!(
            after.usage.is_empty(),
            "an asleep app reports nothing rather than the past"
        );
        assert!(
            after.ticks.is_empty(),
            "and keeps no counters a wake would subtract from"
        );
    }

    /// A guest that could not be asked is not a guest that answered nothing.
    #[test]
    fn a_running_app_that_would_not_answer_keeps_what_it_last_said() {
        let records = BTreeMap::from([(app_id(), instance_record(|_| {}))]);
        let previous_usage = BTreeMap::from([(
            app_id(),
            ComputeUsage {
                memory_total_bytes: 8,
                memory_used_bytes: 4,
                cpu_share: Some(0.25),
                measured_at: at(),
            },
        )]);
        let previous_ticks = BTreeMap::from([(app_id(), compute(1_000, 100))]);
        let taken = vec![(app_id(), GuestReading::default())];

        let after = compute_usage_after(&taken, &records, &previous_usage, &previous_ticks, at());
        assert_eq!(
            after.usage.get(&app_id()).and_then(|usage| usage.cpu_share),
            Some(0.25)
        );
        assert_eq!(
            after.ticks.get(&app_id()),
            Some(&compute(1_000, 100)),
            "the counters stay, so the next answer has an interval behind it"
        );
    }

    #[test]
    fn a_running_app_that_answered_reports_what_it_measured() {
        let records = BTreeMap::from([(app_id(), instance_record(|_| {}))]);
        let previous_ticks = BTreeMap::from([(app_id(), compute(1_000, 100))]);
        let taken = vec![(
            app_id(),
            GuestReading {
                filesystem: Some(MeasuredBytes {
                    total_bytes: 2_000,
                    used_bytes: 500,
                }),
                compute: Some(compute(2_000, 600)),
            },
        )];

        let after = compute_usage_after(&taken, &records, &BTreeMap::new(), &previous_ticks, at());
        assert_eq!(
            after.usage.get(&app_id()).and_then(|usage| usage.cpu_share),
            Some(0.5)
        );
        let volumes = volume_usage_after(&taken, &BTreeMap::new(), at());
        assert_eq!(volumes.get(&app_id()).map(|usage| usage.used_bytes), Some(500));
    }
}
