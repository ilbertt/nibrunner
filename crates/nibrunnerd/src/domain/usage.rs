use std::collections::BTreeMap;

use guest_contract::filesystem::MeasuredCompute;
use protocol::{AppId, ComputeUsage, FilesystemUsage, Timestamp};

use crate::domain::report::instance_record::InstanceRecord;
use crate::ports::GuestReading;

pub const MEASUREMENT_CONCURRENCY: usize = 4;

const FULLY_BUSY: f64 = 1.0;

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
    pub ticks: BTreeMap<AppId, MeasuredCompute>,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use guest_contract::filesystem::MeasuredBytes;
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

    #[test]
    fn a_first_reading_has_no_share_rather_than_a_share_of_nothing() {
        assert_eq!(share_between(None, compute(1_000, 100)), None);
    }

    #[test]
    fn a_guest_that_restarted_its_counters_has_no_share_until_it_has_two_readings() {
        assert_eq!(share_between(Some(compute(5_000, 900)), compute(100, 10)), None);
        assert_eq!(
            share_between(Some(compute(1_000, 100)), compute(1_000, 100)),
            None
        );
    }

    #[test]
    fn a_fully_busy_guest_reads_as_one_however_many_vcpus_it_has() {
        let share = share_between(Some(compute(1_000, 100)), compute(2_000, 3_100));
        assert_eq!(share, Some(1.0));
    }

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
