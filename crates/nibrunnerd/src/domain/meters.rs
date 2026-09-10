use std::collections::BTreeMap;

use guest_contract::filesystem::MeasuredCompute;
use nft_render::AppTraffic;
use protocol::{AppId, InstanceState, UsageMeters};

use crate::domain::report::InstanceRecord;
use crate::ports::GuestReading;

// A microVM exists and is holding the memory it was promised. Pending has not been given any yet,
// Idle handed it back to a snapshot, and Stopped and Failed are holding none.
const HOLDS_MEMORY: [InstanceState; 4] = [
    InstanceState::Starting,
    InstanceState::Running,
    InstanceState::Unhealthy,
    InstanceState::Stopping,
];

// `/proc/stat` counts in USER_HZ, which Linux fixes at 100 for anything reading it from userspace
// whatever the kernel was built to tick at.
const MILLIS_PER_TICK: u64 = 10;

// What a pass that ran late may bill for, as a multiple of the interval it meant to run at. A host
// that was suspended and a daemon that was stopped and started again both come back to a clock
// that moved further than they watched, and time nobody observed is not time this host will claim
// an app was using.
const LATE_PASS_ALLOWANCE: u64 = 2;

/// What a counter that only grows has grown by. A reading below the one before it is a counter
/// that restarted — nftables forgets on a reload, a guest forgets on a reboot — so what it holds
/// now is all of it this host can still account for. A first reading is not usage: nothing is
/// known about what came before it.
pub fn advanced(before: Option<u64>, after: u64) -> u64 {
    match before {
        None => 0,
        Some(before) if after >= before => after - before,
        Some(_) => after,
    }
}

pub fn elapsed_since(before: Option<i64>, now_ms: i64, interval_ms: u64) -> u64 {
    let Some(before) = before else { return 0 };
    u64::try_from(now_ms.saturating_sub(before))
        .unwrap_or(0)
        .min(interval_ms * LATE_PASS_ALLOWANCE)
}

/// The pass that meters wall time and what arrived at each guest. Both are the host's own
/// observations, so this holds for a guest that answers nothing at all.
pub fn metered_after(
    previous: &BTreeMap<AppId, UsageMeters>,
    records: &BTreeMap<AppId, InstanceRecord>,
    traffic_before: &BTreeMap<AppId, AppTraffic>,
    traffic_after: &BTreeMap<AppId, AppTraffic>,
    elapsed_ms: u64,
) -> BTreeMap<AppId, UsageMeters> {
    records
        .iter()
        .map(|(app_id, record)| {
            let mut meter = previous.get(app_id).copied().unwrap_or_default();
            if HOLDS_MEMORY.contains(&record.state) {
                meter.running_ms += elapsed_ms;
            } else if record.is_idle() {
                meter.idle_ms += elapsed_ms;
            }
            if let Some(after) = traffic_after.get(app_id) {
                let before = traffic_before.get(app_id);
                meter.rx_bytes += advanced(before.map(|before| before.received.bytes), after.received.bytes);
                meter.tx_bytes += advanced(before.map(|before| before.sent.bytes), after.sent.bytes);
            }
            (app_id.clone(), meter)
        })
        .collect()
}

/// The pass that meters what the guests said they spent, which only a guest that answers can. It
/// runs on the measurement interval rather than the activity one, so most passes over an app add
/// nothing and the pass after a reading adds the whole stretch since the last one.
pub fn cpu_metered_after(
    previous: &BTreeMap<AppId, UsageMeters>,
    previous_ticks: &BTreeMap<AppId, MeasuredCompute>,
    taken: &[(AppId, GuestReading)],
) -> BTreeMap<AppId, UsageMeters> {
    let mut metered = previous.clone();
    for (app_id, reading) in taken {
        let Some(measured) = reading.compute else {
            continue;
        };
        let spent = advanced(
            previous_ticks.get(app_id).map(|before| before.cpu_busy_ticks),
            measured.cpu_busy_ticks,
        );
        metered.entry(app_id.clone()).or_default().cpu_ms += spent * MILLIS_PER_TICK;
    }
    metered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use nft_render::Counted;
    use protocol::INSTANCE_STATES;

    const TICK_MS: u64 = 5_000;

    fn compute(busy: u64) -> MeasuredCompute {
        MeasuredCompute {
            memory_total_bytes: 268_435_456,
            memory_used_bytes: 1024,
            cpu_total_ticks: busy * 4,
            cpu_busy_ticks: busy,
        }
    }

    fn traffic(received: u64, sent: u64) -> BTreeMap<AppId, AppTraffic> {
        BTreeMap::from([(
            app_id(),
            AppTraffic {
                received: Counted {
                    packets: 1,
                    bytes: received,
                },
                sent: Counted {
                    packets: 1,
                    bytes: sent,
                },
            },
        )])
    }

    fn records(state: InstanceState) -> BTreeMap<AppId, InstanceRecord> {
        BTreeMap::from([(app_id(), instance_record(|record| record.state = state))])
    }

    fn held(metered: &BTreeMap<AppId, UsageMeters>) -> UsageMeters {
        metered.get(&app_id()).copied().unwrap_or_default()
    }

    #[test]
    fn a_counter_that_grew_is_metered_by_what_it_grew_by() {
        assert_eq!(advanced(Some(1_000), 1_500), 500);
        assert_eq!(advanced(Some(1_000), 1_000), 0);
    }

    #[test]
    fn a_counter_that_restarted_is_metered_by_what_it_holds_rather_than_read_as_a_fall() {
        assert_eq!(
            advanced(Some(9_000_000), 16),
            16,
            "the ruleset was reloaded; 16 bytes have moved since"
        );
    }

    #[test]
    fn a_first_reading_is_not_usage_because_nothing_is_known_of_what_came_before_it() {
        assert_eq!(advanced(None, 4_096), 0);
        assert_eq!(elapsed_since(None, 10_000, TICK_MS), 0);
    }

    #[test]
    fn a_pass_that_ran_late_bills_the_interval_it_meant_to_rather_than_the_clock_it_woke_to() {
        assert_eq!(elapsed_since(Some(1_000), 6_000, TICK_MS), TICK_MS);
        assert_eq!(
            elapsed_since(Some(0), 86_400_000, TICK_MS),
            TICK_MS * LATE_PASS_ALLOWANCE,
            "a host that was suspended for a day watched none of it"
        );
        assert_eq!(
            elapsed_since(Some(9_000), 1_000, TICK_MS),
            0,
            "a clock that went backwards is not time an app spent"
        );
    }

    #[test]
    fn time_is_metered_against_what_the_app_was_holding_while_it_passed() {
        let running = metered_after(
            &BTreeMap::new(),
            &records(InstanceState::Running),
            &BTreeMap::new(),
            &BTreeMap::new(),
            TICK_MS,
        );
        assert_eq!(held(&running).running_ms, TICK_MS);
        assert_eq!(held(&running).idle_ms, 0);

        let idle = metered_after(
            &BTreeMap::new(),
            &records(InstanceState::Idle),
            &BTreeMap::new(),
            &BTreeMap::new(),
            TICK_MS,
        );
        assert_eq!(idle.get(&app_id()).map(|meter| meter.idle_ms), Some(TICK_MS));
        assert_eq!(held(&idle).running_ms, 0);
    }

    #[test]
    fn every_state_meters_against_exactly_one_of_the_two_or_neither() {
        for state in INSTANCE_STATES {
            let metered = metered_after(
                &BTreeMap::new(),
                &records(state),
                &BTreeMap::new(),
                &BTreeMap::new(),
                TICK_MS,
            );
            let meter = held(&metered);
            assert!(
                meter.running_ms == 0 || meter.idle_ms == 0,
                "{state:?} was metered as holding memory and as asleep at once"
            );
            let counted = meter.running_ms + meter.idle_ms;
            assert!(counted == 0 || counted == TICK_MS, "{state:?} billed {counted}");
        }
    }

    #[test]
    fn a_meter_carries_what_it_already_held_forward() {
        let before = BTreeMap::from([(
            app_id(),
            UsageMeters {
                running_ms: 60_000,
                rx_bytes: 4_096,
                tx_bytes: 8_192,
                ..UsageMeters::default()
            },
        )]);
        let after = metered_after(
            &before,
            &records(InstanceState::Running),
            &traffic(1_000, 20_000),
            &traffic(1_500, 90_000),
            TICK_MS,
        );
        assert_eq!(held(&after).running_ms, 60_000 + TICK_MS);
        assert_eq!(held(&after).rx_bytes, 4_096 + 500);
        assert_eq!(held(&after).tx_bytes, 8_192 + 70_000);
    }

    #[test]
    fn an_app_nothing_could_be_read_about_keeps_its_bytes_rather_than_losing_them() {
        let before = BTreeMap::from([(
            app_id(),
            UsageMeters {
                rx_bytes: 4_096,
                ..UsageMeters::default()
            },
        )]);
        let after = metered_after(
            &before,
            &records(InstanceState::Running),
            &traffic(1_000, 1_000),
            &BTreeMap::new(),
            TICK_MS,
        );
        assert_eq!(held(&after).rx_bytes, 4_096);
    }

    #[test]
    fn an_app_the_host_no_longer_holds_a_record_of_is_metered_no_further() {
        let before = BTreeMap::from([(
            app_id(),
            UsageMeters {
                running_ms: 60_000,
                ..UsageMeters::default()
            },
        )]);
        let after = metered_after(
            &before,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &traffic(9_999, 9_999),
            TICK_MS,
        );
        assert!(after.is_empty());
    }

    #[test]
    fn what_a_guest_spent_is_metered_in_milliseconds_of_the_vcpus_it_was_given() {
        let taken = vec![(
            app_id(),
            GuestReading {
                filesystem: None,
                compute: Some(compute(1_800)),
            },
        )];
        let ticks = BTreeMap::from([(app_id(), compute(1_000))]);

        let after = cpu_metered_after(&BTreeMap::new(), &ticks, &taken);
        assert_eq!(held(&after).cpu_ms, 8_000, "800 ticks of 10ms each");
    }

    #[test]
    fn a_guest_that_rebooted_is_metered_from_what_it_has_spent_since() {
        let taken = vec![(
            app_id(),
            GuestReading {
                filesystem: None,
                compute: Some(compute(30)),
            },
        )];
        let ticks = BTreeMap::from([(app_id(), compute(500_000))]);

        let after = cpu_metered_after(&BTreeMap::new(), &ticks, &taken);
        assert_eq!(held(&after).cpu_ms, 300);
    }

    #[test]
    fn a_guest_that_answered_nothing_leaves_every_meter_where_it_was() {
        let before = BTreeMap::from([(
            app_id(),
            UsageMeters {
                cpu_ms: 12_000,
                running_ms: 60_000,
                ..UsageMeters::default()
            },
        )]);
        let taken = vec![(app_id(), GuestReading::default())];

        let after = cpu_metered_after(&before, &BTreeMap::new(), &taken);
        assert_eq!(held(&after), held(&before));
    }
}
