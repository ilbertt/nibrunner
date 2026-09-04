//! When an app that runs on request has gone quiet enough to sleep.

use protocol::InstanceState;

use crate::report::InstanceRecord;

/// What an `on-request` app is given when the control plane names no timeout of its own — an api
/// that predates the field, or one that could not read it. Long enough not to stop an app out
/// from under somebody reading a page, short enough that most of a quiet day is reclaimed.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Only a microVM that is up can be put down, and only one that is not on its way up.
///
/// `unhealthy` is not among them, though it is up: an app failing its probes has gone quiet
/// *because* it is broken, so the silence is a symptom of the fault rather than evidence nobody
/// wants it. Sleeping it would turn a failure the owner can see into one they cannot, and take it
/// out of the restart and backoff machinery that exists to answer exactly this.
const SLEEPABLE_STATES: [InstanceState; 1] = [InstanceState::Running];

/// An app with no activity recorded is never quiet: it is one this daemon has not watched long
/// enough to say anything about, and the first counter reading gives it a starting point. Erring
/// towards awake is the only safe direction — the cost is memory, and the cost of the other
/// mistake is somebody's request meeting a microVM being shut down underneath it.
pub fn has_gone_quiet(
    record: &InstanceRecord,
    timeout_ms: Option<u64>,
    last_active_at_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    let (Some(timeout_ms), Some(last_active_at_ms)) = (timeout_ms, last_active_at_ms) else {
        return false;
    };
    record.on_request
        && record.desired_running
        && SLEEPABLE_STATES.contains(&record.state)
        && now_ms - last_active_at_ms >= timeout_ms as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::instance_record;
    use protocol::INSTANCE_STATES;

    const TIMEOUT_MS: u64 = 900_000;
    const NOW_MS: i64 = 10_000_000;
    const QUIET_SINCE_MS: i64 = NOW_MS - TIMEOUT_MS as i64;
    const BUSY_SINCE_MS: i64 = QUIET_SINCE_MS + 1;

    fn quiet(record: &InstanceRecord, timeout_ms: Option<u64>, last_active_at_ms: Option<i64>) -> bool {
        has_gone_quiet(record, timeout_ms, last_active_at_ms, NOW_MS)
    }

    fn on_request(state: InstanceState) -> InstanceRecord {
        instance_record(|record| {
            record.on_request = true;
            record.state = state;
        })
    }

    #[test]
    fn a_quiet_serving_app_has_gone_quiet_and_one_asked_for_a_moment_ago_has_not() {
        assert!(quiet(
            &on_request(InstanceState::Running),
            Some(TIMEOUT_MS),
            Some(QUIET_SINCE_MS)
        ));
        assert!(!quiet(
            &on_request(InstanceState::Running),
            Some(TIMEOUT_MS),
            Some(BUSY_SINCE_MS)
        ));
    }

    #[test]
    fn an_unhealthy_app_is_not_a_quiet_one_however_long_nobody_has_asked_for_it() {
        assert!(!quiet(
            &on_request(InstanceState::Unhealthy),
            Some(TIMEOUT_MS),
            Some(QUIET_SINCE_MS)
        ));
    }

    #[test]
    fn an_app_that_is_kept_up_or_suspended_is_never_let_go() {
        let always_up = instance_record(|record| {
            record.on_request = false;
            record.state = InstanceState::Running;
        });
        assert!(!quiet(&always_up, Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)));
        let suspended = instance_record(|record| {
            record.on_request = true;
            record.state = InstanceState::Running;
            record.desired_running = false;
        });
        assert!(!quiet(&suspended, Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)));
    }

    #[test]
    fn every_other_state_has_no_microvm_to_stop() {
        for state in INSTANCE_STATES
            .iter()
            .filter(|state| **state != InstanceState::Running && **state != InstanceState::Unhealthy)
        {
            assert!(
                !quiet(&on_request(*state), Some(TIMEOUT_MS), Some(QUIET_SINCE_MS)),
                "{state:?}"
            );
        }
    }

    /// The two ways of knowing nothing. A restarted daemon has watched no traffic yet, and an app
    /// desired state no longer calls `on-request` has no timeout to be measured against.
    #[test]
    fn an_app_nothing_has_been_observed_about_is_left_alone() {
        assert!(!quiet(
            &on_request(InstanceState::Running),
            Some(TIMEOUT_MS),
            None
        ));
        assert!(!quiet(
            &on_request(InstanceState::Running),
            None,
            Some(QUIET_SINCE_MS)
        ));
    }
}

use std::collections::{BTreeMap, BTreeSet};

use nft_render::AppTraffic;
use protocol::AppId;

use crate::host::Host;

/// When each app was last reached by something that was not this host.
///
/// The counters are cumulative and the table they live in is replaced rather than edited, so a
/// count standing below the one before it is the ruleset having been rewritten — which every
/// health flip on every app does — and not an app that has gone quiet. Read as a difference it
/// would be a negative one; read as evidence it is none at all, and the moment already recorded
/// stands.
///
/// A slot with no counter is an app that is not forwarded: stopped, unhealthy, or never started.
/// Nothing can have reached it, so its moment stands too rather than being reset by its own
/// absence — which is what stops a restart looking like use.
pub struct Activity {
    pub traffic: BTreeMap<AppId, AppTraffic>,
    pub last_active_at_ms: BTreeMap<AppId, i64>,
    /// Which apps the count actually moved for, which is not the same as which moments equal now:
    /// a first reading starts the clock at now without having observed anything.
    pub moved: BTreeSet<AppId>,
}

pub fn activity_after(
    taken: BTreeMap<AppId, AppTraffic>,
    previous_traffic: &BTreeMap<AppId, AppTraffic>,
    previous_moments: &BTreeMap<AppId, i64>,
    now_ms: i64,
) -> Activity {
    let mut traffic = BTreeMap::new();
    let mut last_active_at_ms = BTreeMap::new();
    let mut moved = BTreeSet::new();

    for (app_id, after) in taken {
        let before = previous_traffic.get(&app_id);
        let recorded = previous_moments.get(&app_id).copied();
        if before.is_some_and(|before| after.bytes > before.bytes) {
            moved.insert(app_id.clone());
        }
        // No interval behind the first reading of a counter, so it starts the clock rather than
        // answering it: an app whose counter has just appeared has not been idle since the epoch.
        let moment = if moved.contains(&app_id) {
            now_ms
        } else {
            recorded.unwrap_or(now_ms)
        };
        last_active_at_ms.insert(app_id.clone(), moment);
        traffic.insert(app_id, after);
    }
    for (app_id, recorded) in previous_moments {
        last_active_at_ms.entry(app_id.clone()).or_insert(*recorded);
    }
    Activity {
        traffic,
        last_active_at_ms,
        moved,
    }
}

pub async fn record_activity(host: &Host) {
    let now = crate::clock::now_ms();
    let snapshot = host.state.snapshot().await;
    let taken = match host.firewall.traffic().await {
        Ok(taken) => taken,
        Err(error) => {
            tracing::debug!(error = %error.message(), "app traffic could not be read");
            BTreeMap::new()
        }
    };
    let next = activity_after(taken, &snapshot.app_traffic, &snapshot.last_active_at_ms, now);

    // A slot this host no longer holds takes its history with it: the app is somewhere else or
    // nowhere, and either way what it last did here is not something to keep answering about.
    let held: BTreeSet<AppId> = host.slots().await.into_iter().map(|slot| slot.app_id).collect();
    let traffic: BTreeMap<_, _> = next
        .traffic
        .into_iter()
        .filter(|(app_id, _)| held.contains(app_id))
        .collect();
    let last_active_at_ms: BTreeMap<_, _> = next
        .last_active_at_ms
        .into_iter()
        .filter(|(app_id, _)| held.contains(app_id))
        .collect();

    // One line a tick, because the counts being read at all is the thing worth seeing: `measured:
    // 0` on a host running apps is a counter that never appeared, which reads the same as a quiet
    // host in every other respect.
    tracing::info!(
        measured = traffic.len(),
        moved = next.moved.len(),
        tracked = last_active_at_ms.len(),
        "app activity measured"
    );
    host.state
        .modify(|snapshot| {
            snapshot.app_traffic = traffic;
            snapshot.last_active_at_ms = last_active_at_ms;
        })
        .await;
}

/// Runs on the measurement tick rather than the status one: the moment it reads is only written
/// there, so asking sixty times more often would be sixty readings of the same answer — and a
/// sleep flushes a filesystem and snapshots a microVM, which is not work to put in front of the
/// health probes of every other app on the host.
pub async fn apply_sleep(host: &std::sync::Arc<Host>) {
    // Read from desired state rather than from the record, so an owner shortening it takes effect
    // on the next document rather than on the app's next boot.
    let timeouts: BTreeMap<AppId, u64> = {
        let cache = host.cache.lock().await;
        cache
            .latest()
            .map(|desired| {
                desired
                    .instances
                    .iter()
                    .filter(|instance| instance.desired_state == protocol::DesiredInstanceState::OnRequest)
                    .map(|instance| {
                        (
                            instance.app_id.clone(),
                            instance
                                .idle_timeout_ms
                                .map_or(DEFAULT_IDLE_TIMEOUT_MS, |timeout| timeout.get()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let snapshot = host.state.snapshot().await;
    let now = crate::clock::now_ms();

    let quiet: Vec<_> = snapshot
        .records
        .values()
        .filter(|record| {
            has_gone_quiet(
                record,
                timeouts.get(&record.app_id).copied(),
                snapshot.last_active_at_ms.get(&record.app_id).copied(),
                now,
            )
        })
        .cloned()
        .collect();
    if quiet.is_empty() {
        return;
    }
    for record in quiet {
        let quiet_for_ms = now
            - snapshot
                .last_active_at_ms
                .get(&record.app_id)
                .copied()
                .unwrap_or(now);
        tracing::info!(app_id = %record.app_id, quiet_for_ms, "app has gone quiet; letting it sleep");
        crate::reconcile::instances::suspend_instance(host, &record.app_id, "idle").await;
    }
    // Here rather than on the next status tick: the record already says the app is not running,
    // so until this runs the forward rule points at a guest that has gone, and a request arriving
    // in that second is refused rather than answered by the activator.
    crate::reconcile::network::apply_network(host).await;
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::test_support::*;

    const EARLIER: i64 = 1_000;
    const NOW: i64 = 60_000;

    fn other() -> AppId {
        AppId::parse("app-2").unwrap()
    }

    fn reading(bytes: u64) -> BTreeMap<AppId, AppTraffic> {
        BTreeMap::from([(app_id(), AppTraffic { packets: 1, bytes })])
    }

    fn previously(bytes: u64, at: i64) -> (BTreeMap<AppId, AppTraffic>, BTreeMap<AppId, i64>) {
        (
            BTreeMap::from([(app_id(), AppTraffic { packets: 1, bytes })]),
            BTreeMap::from([(app_id(), at)]),
        )
    }

    #[test]
    fn an_app_is_active_when_its_counter_has_moved() {
        let (traffic, moments) = previously(1024, EARLIER);
        let after = activity_after(reading(2048), &traffic, &moments, NOW);
        assert_eq!(after.last_active_at_ms.get(&app_id()), Some(&NOW));
        assert!(after.moved.contains(&app_id()));

        let same = activity_after(reading(1024), &traffic, &moments, NOW);
        assert_eq!(same.last_active_at_ms.get(&app_id()), Some(&EARLIER));
        assert!(!same.moved.contains(&app_id()));
    }

    #[test]
    fn a_first_reading_is_not_an_app_that_was_just_used() {
        let after = activity_after(reading(4096), &BTreeMap::new(), &BTreeMap::new(), NOW);
        assert_eq!(after.last_active_at_ms.get(&app_id()), Some(&NOW));
        assert_eq!(after.traffic.get(&app_id()).map(|t| t.bytes), Some(4096));
        assert!(!after.moved.contains(&app_id()));
    }

    #[test]
    fn a_rewritten_ruleset_is_not_an_app_going_quiet() {
        let (traffic, moments) = previously(9_000_000, EARLIER);
        let after = activity_after(reading(16), &traffic, &moments, NOW);
        assert_eq!(after.last_active_at_ms.get(&app_id()), Some(&EARLIER));
        assert!(!after.moved.contains(&app_id()));
        assert_eq!(after.traffic.get(&app_id()).map(|t| t.bytes), Some(16));
    }

    #[test]
    fn an_app_with_no_counter_keeps_the_moment_it_was_last_reached() {
        let (traffic, moments) = previously(1024, EARLIER);
        let after = activity_after(BTreeMap::new(), &traffic, &moments, NOW);
        assert_eq!(after.last_active_at_ms.get(&app_id()), Some(&EARLIER));
        assert!(!after.traffic.contains_key(&app_id()));
    }

    #[test]
    fn every_app_in_the_table_is_read_not_just_the_first() {
        let taken = BTreeMap::from([
            (
                app_id(),
                AppTraffic {
                    packets: 1,
                    bytes: 10,
                },
            ),
            (
                other(),
                AppTraffic {
                    packets: 1,
                    bytes: 20,
                },
            ),
        ]);
        let after = activity_after(taken, &BTreeMap::new(), &BTreeMap::new(), NOW);
        assert_eq!(after.traffic.len(), 2);
    }

    #[tokio::test]
    async fn what_the_host_stops_holding_it_stops_answering_about() {
        let host = test_host().await;
        host.state
            .modify(|snapshot| {
                snapshot.last_active_at_ms.insert(app_id(), EARLIER);
                snapshot.last_active_at_ms.insert(other(), EARLIER);
            })
            .await;
        host.slot_for(&app_id()).await.unwrap();

        record_activity(&host).await;

        let snapshot = host.state.snapshot().await;
        assert!(snapshot.last_active_at_ms.contains_key(&app_id()));
        assert!(!snapshot.last_active_at_ms.contains_key(&other()));
    }
}
