// What decides that an instance should sleep now lives in `domain::activation`, which reads a
// policy rather than a timeout. The constant stays reachable from here because it is the default
// this pass applies to a document that named none.
pub use protocol::DEFAULT_IDLE_TIMEOUT_MS;

use std::collections::{BTreeMap, BTreeSet};

use nft_render::AppTraffic;
use protocol::{AppId, Timestamp};

use crate::domain::activation::{should_sleep, ActivitySignals, SleepReason};
use crate::host::Host;

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
        if before.is_some_and(|before| after.bytes > before.bytes) {
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
            snapshot.app_traffic = traffic;
            snapshot.last_active_at_ms = last_active_at_ms;
        })
        .await;
}

pub async fn apply_sleep(host: &std::sync::Arc<Host>) {
    let policies: BTreeMap<AppId, protocol::ActivationPolicy> = {
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

    let letting_go: Vec<_> = snapshot
        .records
        .values()
        .filter_map(|record| {
            let policy = policies.get(&record.app_id)?;
            let signals = ActivitySignals {
                last_active_at_ms: snapshot.last_active_at_ms.get(&record.app_id).copied(),
                started_at_ms: record.started_at.as_ref().map(Timestamp::epoch_ms),
            };
            let reason = should_sleep(policy, record, &signals, now)?;
            Some((record.clone(), signals, reason))
        })
        .collect();
    if letting_go.is_empty() {
        return;
    }
    for (record, signals, reason) in letting_go {
        let since = match reason {
            SleepReason::Quiet => signals.last_active_at_ms,
            SleepReason::LivedLongEnough => signals.started_at_ms,
        };
        tracing::info!(
            app_id = %record.app_id,
            reason = reason.as_str(),
            for_ms = now - since.unwrap_or(now),
            "letting an app sleep"
        );
        crate::domain::reconcile::instances::suspend_instance(host, &record.app_id, reason.as_str()).await;
    }
    crate::domain::reconcile::network::apply_network(host).await;
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
            ready_when: protocol::ReadinessPolicy::PortAnswers,
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

    #[tokio::test]
    async fn an_app_still_inside_its_lifetime_is_left_up_however_quiet_it_has_been() {
        let host = host_under(max_lifetime(TTL_MS)).await;
        started(&host, TTL_MS as i64 - 1).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 * 10).await;

        apply_sleep(host.arc()).await;

        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_told_never_to_sleep_is_kept_up_where_a_timeout_would_have_let_it_go() {
        let never = protocol::ActivationPolicy {
            sleep_when: protocol::SleepPolicy::Never,
            ready_when: protocol::ReadinessPolicy::PortAnswers,
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
}
