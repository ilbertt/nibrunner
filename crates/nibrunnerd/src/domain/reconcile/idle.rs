// What decides that an instance should sleep now lives in `domain::activation`, which reads a
// policy rather than a timeout. The constant stays reachable from here because it is the default
// this pass applies to a document that named none.
pub use protocol::DEFAULT_IDLE_TIMEOUT_MS;

// What `ActivityController` runs this pass at. Only the meters read it, and only to decide how
// much of a late pass is time this host actually watched.
pub const ACTIVITY_INTERVAL_MS: u64 = 5_000;

// How many apps a pass puts to sleep at once. A snapshot is disk-bound — pausing the guest is
// instant, and the rest is writing its memory out to the one NVMe every app on the host runs
// from — so four keep that disk busy without forty of them queueing on it, and the last app of a
// batch that went quiet together is asleep a few snapshots after the first rather than forty.
pub const SLEEP_CONCURRENCY: usize = 4;

// How many sleeps one pass takes on before handing back to its caller. A pass acts on one reading
// of the state, and with two hundred apps due at once the last of them was snapshotted minutes
// after that reading, having been up and busy for most of them. Two rounds of the disk keep a
// pass short; what it leaves it counts back, so the caller comes straight back for it.
pub const SLEEPS_PER_PASS: usize = SLEEP_CONCURRENCY * 2;

use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt;
use nft_render::AppTraffic;
use protocol::{ActivationPolicy, AppId, Timestamp};

use crate::domain::activation::{should_sleep, ActivitySignals, SleepReason};
use crate::domain::metrics::sleep_wake::SleepOutcome;
use crate::domain::report::InstanceRecord;
use crate::host::Host;
use crate::state::HostSnapshot;

pub struct Activity {
    pub traffic: BTreeMap<AppId, AppTraffic>,
    pub last_active_at_ms: BTreeMap<AppId, i64>,
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
        // Inbound only. What a guest sends of its own accord — a poll outward, a heartbeat to
        // something else — is metered but is not somebody asking for the app, and reading it as
        // activity would keep an app that talks to itself awake for ever.
        if before.is_some_and(|before| after.received.bytes > before.received.bytes) {
            moved.insert(app_id.clone());
        }
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

    // Twelve times a minute, and all but the ones where something moved say the same thing.
    if next.moved.is_empty() {
        tracing::debug!(
            measured = traffic.len(),
            tracked = last_active_at_ms.len(),
            "app activity measured"
        );
    } else {
        tracing::info!(
            measured = traffic.len(),
            moved = next.moved.len(),
            tracked = last_active_at_ms.len(),
            "app activity measured"
        );
    }
    host.state
        .modify(|snapshot| {
            // Under the write lock rather than off the reading above, because the measurement pass
            // accumulates into the same meters from a task of its own and whichever of them wrote
            // second would otherwise carry away a copy taken before the other had added anything.
            // Metered off the counters this pass is about to store, so what is billed and what is
            // read as activity are the same reading.
            let metered = crate::domain::meters::metered_after(
                &snapshot.meters,
                &crate::domain::meters::MeterInputs {
                    records: &snapshot.records,
                    traffic_before: &snapshot.app_traffic,
                    traffic_after: &traffic,
                    volumes: &snapshot.volume_reports,
                    volume_usage: &snapshot.volume_usage,
                    elapsed_ms: crate::domain::meters::elapsed_since(
                        snapshot.metered_at_ms,
                        now,
                        ACTIVITY_INTERVAL_MS,
                    ),
                },
            );
            snapshot.meters = metered;
            snapshot.metered_at_ms = Some(now);
            snapshot.app_traffic = traffic;
            snapshot.last_active_at_ms = last_active_at_ms;
        })
        .await;
}

/// How long a policy lets an app be before it may sleep; nothing for one that never lets it.
fn allowance_ms(policy: protocol::SleepPolicy) -> Option<u64> {
    match policy {
        protocol::SleepPolicy::Never => None,
        protocol::SleepPolicy::TrafficIdle { timeout_ms } => Some(timeout_ms.get()),
        protocol::SleepPolicy::MaxLifetime { ttl_ms } => Some(ttl_ms.get()),
    }
}

/// The policy's case for letting this app go now.
struct Due {
    reason: SleepReason,
    /// How long the app has been what the policy sleeps it for: quiet, or up.
    for_ms: i64,
    /// How far past what the policy allowed.
    late_ms: i64,
}

fn due(policy: &ActivationPolicy, record: &InstanceRecord, snapshot: &HostSnapshot, now: i64) -> Option<Due> {
    let signals = ActivitySignals {
        last_active_at_ms: snapshot.last_active_at_ms.get(&record.app_id).copied(),
        started_at_ms: record.started_at.as_ref().map(Timestamp::epoch_ms),
    };
    let reason = should_sleep(policy, record, &signals, now)?;
    let since = match reason {
        SleepReason::Quiet => signals.last_active_at_ms,
        SleepReason::LivedLongEnough => signals.started_at_ms,
    };
    let for_ms = now - since.unwrap_or(now);
    let late_ms = allowance_ms(policy.sleep_when)
        .map_or(0, |allowed| for_ms - allowed as i64)
        .max(0);
    Some(Due {
        reason,
        for_ms,
        late_ms,
    })
}

/// Puts the app to sleep if it is still due, and says whether it went.
async fn let_sleep(host: &Host, app_id: &AppId, policy: &ActivationPolicy) -> bool {
    // Decided again, against the state and the clock as they are now: an app picked for the
    // batch may have been asked for while it waited its turn, and how late the policy is acted on
    // is measured from when it is, not from when the batch was picked. Decided before the
    // transition lock rather than under it, so the lock is held for the move and nothing else.
    let now = crate::clock::now_ms();
    let snapshot = host.state.snapshot().await;
    let Some(record) = snapshot.records.get(app_id) else {
        return false;
    };
    let Some(due) = due(policy, record, &snapshot, now) else {
        let active_ms_ago = snapshot.last_active_at_ms.get(app_id).map(|at| now - at);
        tracing::info!(
            %app_id,
            state = record.state.as_str(),
            active_ms_ago,
            "no longer quiet; left up"
        );
        return false;
    };
    host.metrics
        .sleep_wake
        .sleep_due(due.reason, std::time::Duration::from_millis(due.late_ms as u64));
    tracing::info!(
        %app_id,
        reason = due.reason.as_str(),
        for_ms = due.for_ms,
        late_ms = due.late_ms,
        "letting an app sleep"
    );
    // Held across the flush and the snapshot, and taken by a wake for as long as it is bringing
    // the guest back: a request that reaches an app mid-snapshot waits for the snapshot and then
    // restores what it wrote, rather than finding a guest that is paused or half written out.
    let _transition = host.state.transition(app_id).await;
    crate::domain::reconcile::instances::suspend_instance(host, app_id, due.reason).await
        == Some(SleepOutcome::Slept)
}

/// Puts the due apps to sleep, the most overdue first and `SLEEPS_PER_PASS` of them at most, and
/// returns how many due apps it left for the next pass. A caller handed more than none runs again
/// at once rather than at the interval: the bound keeps a pass short, not the host slow. A pass
/// that put nothing to sleep returns none whatever was due, since run again at once it would try
/// the same apps against the same full disk.
pub async fn apply_sleep(host: &std::sync::Arc<Host>) -> usize {
    let policies: BTreeMap<AppId, ActivationPolicy> = {
        let cache = host.cache.lock().await;
        cache
            .latest()
            .map(|desired| {
                desired
                    .instances
                    .iter()
                    .map(|instance| (instance.app_id.clone(), instance.activation()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let snapshot = host.state.snapshot().await;
    let now = crate::clock::now_ms();

    let mut letting_go: Vec<(Due, AppId, ActivationPolicy)> = snapshot
        .records
        .values()
        .filter_map(|record| {
            let policy = policies.get(&record.app_id)?;
            due(policy, record, &snapshot, now).map(|due| (due, record.app_id.clone(), *policy))
        })
        .collect();
    if letting_go.is_empty() {
        return 0;
    }
    letting_go.sort_by_key(|(due, ..)| std::cmp::Reverse(due.late_ms));
    let left = letting_go.len().saturating_sub(SLEEPS_PER_PASS);
    letting_go.truncate(SLEEPS_PER_PASS);
    let slept = futures::stream::iter(letting_go)
        .map(|(_, app_id, policy)| async move { let_sleep(host, &app_id, &policy).await })
        .buffer_unordered(SLEEP_CONCURRENCY)
        .filter(|slept| std::future::ready(*slept))
        .count()
        .await;
    crate::domain::reconcile::network::apply_network(host).await;
    if slept == 0 {
        0
    } else {
        left
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::test_support::*;
    use nft_render::Counted;

    const EARLIER: i64 = 1_000;
    const NOW: i64 = 60_000;

    fn other() -> AppId {
        AppId::parse("app-2").unwrap()
    }

    fn inbound(bytes: u64) -> AppTraffic {
        AppTraffic {
            received: Counted { packets: 1, bytes },
            sent: Counted::default(),
        }
    }

    fn reading(bytes: u64) -> BTreeMap<AppId, AppTraffic> {
        BTreeMap::from([(app_id(), inbound(bytes))])
    }

    fn previously(bytes: u64, at: i64) -> (BTreeMap<AppId, AppTraffic>, BTreeMap<AppId, i64>) {
        (
            BTreeMap::from([(app_id(), inbound(bytes))]),
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
        assert_eq!(after.traffic.get(&app_id()).map(|t| t.received.bytes), Some(4096));
        assert!(!after.moved.contains(&app_id()));
    }

    #[test]
    fn a_rewritten_ruleset_is_not_an_app_going_quiet() {
        let (traffic, moments) = previously(9_000_000, EARLIER);
        let after = activity_after(reading(16), &traffic, &moments, NOW);
        assert_eq!(after.last_active_at_ms.get(&app_id()), Some(&EARLIER));
        assert!(!after.moved.contains(&app_id()));
        assert_eq!(after.traffic.get(&app_id()).map(|t| t.received.bytes), Some(16));
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
        let taken = BTreeMap::from([(app_id(), inbound(10)), (other(), inbound(20))]);
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

    #[tokio::test]
    async fn a_pass_meters_the_stretch_since_the_last_one_against_what_the_app_was_holding() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;
        host.state
            .modify(|snapshot| {
                snapshot.metered_at_ms = Some(crate::clock::now_ms() - ACTIVITY_INTERVAL_MS as i64);
            })
            .await;

        record_activity(&host).await;

        let metered = host.state.snapshot().await.meters;
        let held = metered.get(&app_id()).copied().unwrap_or_default();
        assert!(
            held.running_ms >= ACTIVITY_INTERVAL_MS,
            "a running app was metered {} ms of the {ACTIVITY_INTERVAL_MS} it was up for",
            held.running_ms
        );
        assert_eq!(held.idle_ms, 0);
    }

    #[tokio::test]
    async fn the_first_pass_a_daemon_makes_bills_nothing_for_the_time_it_was_not_watching() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;

        record_activity(&host).await;

        let snapshot = host.state.snapshot().await;
        assert_eq!(
            snapshot.meters.get(&app_id()).copied().unwrap_or_default(),
            protocol::UsageMeters::default()
        );
        assert!(
            snapshot.metered_at_ms.is_some(),
            "and the pass after it has somewhere to measure from"
        );
    }

    #[tokio::test]
    async fn a_counter_table_that_could_not_be_read_leaves_what_the_host_already_knew_alone() {
        let mut host = test_host().await;
        let (commands, _log) = mocks::commands_answering(|_| {
            Err(crate::ports::CommandError::Unstartable {
                executable: "nft".to_string(),
                reason: "it is not installed".to_string(),
            })
        });
        std::sync::Arc::get_mut(&mut host.host)
            .expect("nothing else holds this host yet")
            .firewall = std::sync::Arc::new(crate::adapters::net::firewall::HostFirewall::new(commands));
        host.slot_for(&app_id()).await.unwrap();
        host.state
            .modify(|snapshot| {
                snapshot.last_active_at_ms.insert(app_id(), EARLIER);
            })
            .await;

        record_activity(&host).await;

        let snapshot = host.state.snapshot().await;
        assert_eq!(snapshot.last_active_at_ms.get(&app_id()), Some(&EARLIER));
        assert!(snapshot.app_traffic.is_empty());
    }
}

#[cfg(test)]
mod sleep_tests {
    use super::*;
    use crate::ports::VmCall;
    use crate::test_support::*;
    use protocol::{DesiredInstanceState, IdleTimeoutMs, InstanceState};

    fn quiet_record() -> crate::domain::report::InstanceRecord {
        instance_record(|record| {
            record.on_request = true;
            record.desired_running = true;
            record.state = InstanceState::Running;
        })
    }

    async fn on_request_host(idle_timeout_ms: Option<IdleTimeoutMs>) -> TestHost {
        let host = test_host().await;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::OnRequest;
                instance.idle_timeout_ms = idle_timeout_ms;
            })]
        }));
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(quiet_record()).await;
        host
    }

    async fn last_reached(host: &TestHost, ms_ago: i64) {
        let moment = crate::clock::now_ms() - ms_ago;
        host.state
            .modify(|snapshot| {
                snapshot.last_active_at_ms.insert(app_id(), moment);
            })
            .await;
    }

    #[tokio::test]
    async fn an_app_nobody_has_asked_for_since_its_timeout_is_put_to_sleep() {
        let host = on_request_host(None).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 + 1).await;

        apply_sleep(host.arc()).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Sleep]);
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        assert!(record.stop_requested);
    }

    #[tokio::test]
    async fn how_late_the_policy_was_acted_on_is_measured_against_what_it_allowed() {
        let host = on_request_host(None).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 + 4_000).await;

        apply_sleep(host.arc()).await;

        let page = crate::domain::metrics::tests::page(
            &crate::domain::metrics::tests::report(),
            &host.metrics,
            &host.state.snapshot().await,
            0,
        );
        assert!(page.contains("nibrunner_sleep_phase_seconds_count{phase=\"late\",reason=\"idle\"} 1\n"));
        // Four seconds over, and so in the bucket that ends at five and not the one before it.
        assert!(page
            .contains("nibrunner_sleep_phase_seconds_bucket{phase=\"late\",reason=\"idle\",le=\"2.5\"} 0\n"));
        assert!(page
            .contains("nibrunner_sleep_phase_seconds_bucket{phase=\"late\",reason=\"idle\",le=\"5\"} 1\n"));
    }

    #[tokio::test]
    async fn one_asked_for_a_moment_ago_is_left_up() {
        let host = on_request_host(None).await;
        last_reached(&host, 0).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Running
        );
    }

    #[tokio::test]
    async fn the_timeout_the_document_names_is_the_one_that_is_waited_out() {
        let longer = IdleTimeoutMs::try_from(DEFAULT_IDLE_TIMEOUT_MS * 2).unwrap();
        let host = on_request_host(Some(longer)).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 + 1).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_the_document_keeps_up_is_never_let_go_however_quiet_it_is() {
        let host = test_host().await;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|_| {})]
        }));
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(quiet_record()).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 * 10).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn a_host_that_has_been_given_no_document_lets_nothing_go() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(quiet_record()).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 * 10).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_nothing_has_been_measured_about_is_left_up_rather_than_read_as_quiet() {
        let host = on_request_host(None).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    async fn host_under(policy: protocol::ActivationPolicy) -> TestHost {
        let host = test_host().await;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::OnRequest;
                instance.activation = Some(policy);
            })]
        }));
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(quiet_record()).await;
        host
    }

    fn max_lifetime(ttl_ms: u64) -> protocol::ActivationPolicy {
        protocol::ActivationPolicy {
            sleep_when: protocol::SleepPolicy::MaxLifetime {
                ttl_ms: protocol::MaxLifetimeMs::try_from(ttl_ms).unwrap(),
            },
        }
    }

    async fn started(host: &TestHost, ms_ago: i64) {
        let moment = protocol::Timestamp::from_epoch_ms(crate::clock::now_ms() - ms_ago);
        host.state
            .update_record(&app_id(), |record| record.started_at = Some(moment.clone()))
            .await;
    }

    const TTL_MS: u64 = 3_600_000;

    #[tokio::test]
    async fn an_app_that_has_lived_its_lifetime_sleeps_though_it_was_asked_for_a_moment_ago() {
        let host = host_under(max_lifetime(TTL_MS)).await;
        started(&host, TTL_MS as i64 + 1).await;
        last_reached(&host, 0).await;

        apply_sleep(host.arc()).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Sleep]);
        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Idle
        );
    }

    // Half way through, not a millisecond short of the end: the clock is read once to stamp the
    // start and again to decide, and a runner that takes a millisecond between the two would have
    // put an app started one millisecond inside its lifetime past it.
    #[tokio::test]
    async fn an_app_still_inside_its_lifetime_is_left_up_however_quiet_it_has_been() {
        let host = host_under(max_lifetime(TTL_MS)).await;
        started(&host, TTL_MS as i64 / 2).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 * 10).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_told_never_to_sleep_is_kept_up_where_a_timeout_would_have_let_it_go() {
        let never = protocol::ActivationPolicy {
            sleep_when: protocol::SleepPolicy::Never,
        };
        let host = host_under(never).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 * 10).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_that_has_never_started_has_no_lifetime_to_have_run_out() {
        let host = host_under(max_lifetime(protocol::MIN_MAX_LIFETIME_MS)).await;
        host.state
            .update_record(&app_id(), |record| record.started_at = None)
            .await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    fn nth(n: usize) -> AppId {
        AppId::parse(format!("app-{n}")).unwrap()
    }

    /// This many on-request apps, each in a slot and quiet for longer than its timeout, on a
    /// host whose VMM holds every snapshot at a gate until the test lets it through.
    async fn quiet_batch(count: usize) -> (TestHost, std::sync::Arc<mocks::HeldSleeps>) {
        let mut host = test_host().await;
        let (held, spy) = mocks::vmm_holding_sleeps();
        std::sync::Arc::get_mut(&mut host.host)
            .expect("nothing else holds this host yet")
            .vms = held.clone();
        host.vms = spy;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = (1..=count)
                .map(|n| {
                    desired_instance(|instance| {
                        instance.app_id = nth(n);
                        instance.desired_state = DesiredInstanceState::OnRequest;
                    })
                })
                .collect()
        }));
        let quiet_since = crate::clock::now_ms() - DEFAULT_IDLE_TIMEOUT_MS as i64 - 1;
        for n in 1..=count {
            host.slot_for(&nth(n)).await.unwrap();
            let mut record = quiet_record();
            record.app_id = nth(n);
            host.state.put_record(record).await;
            host.state.mark_active(&nth(n), quiet_since).await;
        }
        (host, held)
    }

    fn a_pass_over(host: &TestHost) -> tokio::task::JoinHandle<usize> {
        let host = host.arc().clone();
        tokio::spawn(async move { apply_sleep(&host).await })
    }

    #[tokio::test]
    async fn a_batch_of_quiet_apps_sleeps_four_at_a_time_and_every_one_of_them_ends_asleep() {
        let count = SLEEP_CONCURRENCY * 2;
        let (host, held) = quiet_batch(count).await;
        let pass = a_pass_over(&host);

        held.held_up(SLEEP_CONCURRENCY).await;
        // Nothing has been let through, so anything more at the gate now is a fifth beside four.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(held.in_flight(), SLEEP_CONCURRENCY);

        held.let_through(count);
        pass.await.unwrap();

        assert_eq!(held.most_in_flight(), SLEEP_CONCURRENCY);
        assert_eq!(host.vms.calls(), vec![VmCall::Sleep; count]);
        for n in 1..=count {
            let record = host.state.record(&nth(n)).await.unwrap();
            assert_eq!(record.state, InstanceState::Idle, "{}", nth(n));
        }
    }

    #[tokio::test]
    async fn an_app_asked_for_while_it_waited_its_turn_is_left_up_and_the_pass_says_so() {
        let (said, _listening) = Said::listening();
        let count = SLEEP_CONCURRENCY + 1;
        let (host, held) = quiet_batch(count).await;
        let pass = a_pass_over(&host);

        held.held_up(SLEEP_CONCURRENCY).await;
        let waiting_its_turn = nth(count);
        host.state
            .mark_active(&waiting_its_turn, crate::clock::now_ms())
            .await;

        held.let_through(count);
        pass.await.unwrap();

        assert_eq!(host.vms.calls(), vec![VmCall::Sleep; SLEEP_CONCURRENCY]);
        let record = host.state.record(&waiting_its_turn).await.unwrap();
        assert_eq!(record.state, InstanceState::Running);
        assert_eq!(
            said.lines()
                .iter()
                .filter(|line| line == &"INFO no longer quiet; left up")
                .count(),
            1,
            "{:?}",
            said.lines()
        );
    }

    #[tokio::test]
    async fn a_pass_takes_its_share_of_what_is_due_and_says_how_many_it_left() {
        let count = SLEEPS_PER_PASS + 3;
        let (host, held) = quiet_batch(count).await;
        held.let_through(count);

        let left = a_pass_over(&host).await.unwrap();

        assert_eq!(left, 3);
        assert_eq!(host.vms.calls(), vec![VmCall::Sleep; SLEEPS_PER_PASS]);

        let left = a_pass_over(&host).await.unwrap();

        assert_eq!(left, 0);
        assert_eq!(host.vms.calls(), vec![VmCall::Sleep; count]);
        for n in 1..=count {
            let record = host.state.record(&nth(n)).await.unwrap();
            assert_eq!(record.state, InstanceState::Idle, "{}", nth(n));
        }
    }

    #[tokio::test]
    async fn the_most_overdue_app_is_in_the_share_a_pass_takes() {
        let count = SLEEPS_PER_PASS + 1;
        let (host, held) = quiet_batch(count).await;
        held.let_through(count);
        let longest_quiet = nth(count);
        let quiet_since = host.state.snapshot().await.last_active_at_ms[&longest_quiet];
        host.state.mark_active(&longest_quiet, quiet_since - 1).await;

        let left = a_pass_over(&host).await.unwrap();

        assert_eq!(left, 1);
        let record = host.state.record(&longest_quiet).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
    }

    #[tokio::test]
    async fn a_pass_that_put_nothing_to_sleep_leaves_nothing_to_come_straight_back_for() {
        let count = SLEEPS_PER_PASS + 1;
        let (host, held) = quiet_batch(count).await;
        held.let_through(count);
        host.vms.refuse_sleep(crate::ports::VmError::SleepRefused {
            reason: "the disk has no room for another snapshot".into(),
        });

        let left = a_pass_over(&host).await.unwrap();

        assert_eq!(left, 0);
        assert_eq!(host.vms.calls(), vec![VmCall::Sleep; SLEEPS_PER_PASS]);
        for n in 1..=count {
            let record = host.state.record(&nth(n)).await.unwrap();
            assert_eq!(record.state, InstanceState::Running, "{}", nth(n));
        }
    }
}
