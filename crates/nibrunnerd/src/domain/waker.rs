use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{AppId, DesiredInstanceState};
use tokio::sync::{broadcast, Mutex};

use crate::domain::health::probe::probe_instance;
use crate::domain::report::capacity::{committed_resources, memory_shortfall_mib};
use crate::host::Host;
use crate::ports::{WakeFailure, WakeRefusal, Waker};

const PROBE_INTERVAL: Duration = Duration::from_millis(5);

type Outcome = Result<(), WakeRefusal>;

pub struct AppWaker {
    host: Arc<Host>,
    in_flight: Mutex<BTreeMap<AppId, (broadcast::Sender<Outcome>, u64)>>,
}

impl AppWaker {
    pub fn new(host: Arc<Host>) -> Arc<Self> {
        Arc::new(Self {
            host,
            in_flight: Mutex::new(BTreeMap::new()),
        })
    }

    async fn refusal_for_room(
        &self,
        app_id: &AppId,
        wanted: &protocol::InstanceResources,
    ) -> Option<WakeRefusal> {
        let others: Vec<_> = self
            .host
            .state
            .records()
            .await
            .into_iter()
            .filter(|record| &record.app_id != app_id)
            .collect();
        let shortfall =
            memory_shortfall_mib(self.host.guest_memory_mib, &committed_resources(&others), wanted);
        (shortfall > 0).then_some(WakeRefusal::NoRoom {
            shortfall_mib: shortfall,
        })
    }

    async fn boot(&self, app_id: &AppId) -> Outcome {
        let started = Instant::now();
        let wanted = {
            let cache = self.host.cache.lock().await;
            cache
                .latest()
                .and_then(|desired| {
                    desired
                        .instances
                        .iter()
                        .find(|instance| &instance.app_id == app_id)
                })
                .cloned()
        };
        let Some(wanted) = wanted else {
            return Err(WakeRefusal::Failed {
                kind: WakeFailure::NotNamed,
                reason: "the control plane no longer names it".into(),
            });
        };
        if wanted.desired_state != DesiredInstanceState::OnRequest {
            return Err(WakeRefusal::Failed {
                kind: WakeFailure::NotOnRequest,
                reason: format!("it is {}", wanted.desired_state.as_str()),
            });
        }
        if !self.host.state.snapshot().await.isolated {
            return Err(WakeRefusal::Failed {
                kind: WakeFailure::NotIsolated,
                reason: "the isolation ruleset is not applied".into(),
            });
        }
        if let Some(refusal) = self.refusal_for_room(app_id, &wanted.config.resources).await {
            if let WakeRefusal::NoRoom { shortfall_mib } = &refusal {
                let message = format!(
                    "{app_id} could not be woken: its host is {shortfall_mib} MiB short of the memory it needs"
                );
                self.host
                    .state
                    .update_record(app_id, |record| {
                        record.message = Some(protocol::StateMessage::new(message));
                    })
                    .await;
            }
            return Err(refusal);
        }

        let outcome = crate::domain::reconcile::instances::resume_instance(&self.host, &wanted)
            .await
            .map_err(|reason| WakeRefusal::Failed {
                kind: WakeFailure::WouldNotStart,
                reason,
            })?;

        let Some(record) = self.host.state.record(app_id).await else {
            return Err(WakeRefusal::Failed {
                kind: WakeFailure::WouldNotStart,
                reason: "the microVM would not start".into(),
            });
        };
        if record.started_at.is_none() {
            let reason = record.message.as_ref().map_or_else(
                || "the microVM would not start".to_string(),
                |message| message.as_str().to_string(),
            );
            return Err(WakeRefusal::Failed {
                kind: WakeFailure::WouldNotStart,
                reason,
            });
        }
        let restored = started.elapsed();

        // A caller is handed on once the guest is ready for it, and what ready means is the
        // instance's to say: a port that answers, or a microVM that started at all.
        if record.health_check.probes_a_port() {
            let deadline =
                Instant::now() + Duration::from_millis(record.health_check.probe().grace_period_ms);
            loop {
                if probe_instance(&record.guest_ipv4, record.http_port, &record.health_check).await {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(WakeRefusal::Failed {
                        kind: WakeFailure::NeverAnswered,
                        reason: format!("nothing answered on port {} inside the guest", record.http_port),
                    });
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
        }
        let ready = started.elapsed() - restored;

        self.host.state.signal_refresh();
        self.host
            .metrics
            .sleep_wake
            .woken(app_id, outcome, started.elapsed(), ready);
        let joined = self
            .in_flight
            .lock()
            .await
            .get(app_id)
            .map_or(0, |(_, count)| *count);
        tracing::info!(
            %app_id,
            outcome = outcome.as_str(),
            waited_ms = started.elapsed().as_millis(),
            restored_ms = restored.as_millis(),
            ready_ms = ready.as_millis(),
            coalesced = joined,
            "app woken by a request"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl Waker for AppWaker {
    async fn wake(&self, app_id: &AppId) -> Result<(), WakeRefusal> {
        let started = Instant::now();
        let joined = {
            let mut in_flight = self.in_flight.lock().await;
            match in_flight.get_mut(app_id) {
                Some((sender, count)) => {
                    *count += 1;
                    Some(sender.subscribe())
                }
                None => {
                    let (sender, _) = broadcast::channel(1);
                    in_flight.insert(app_id.clone(), (sender, 0));
                    None
                }
            }
        };
        let metrics = &self.host.metrics.sleep_wake;
        if let Some(mut waiting) = joined {
            let outcome = match waiting.recv().await {
                Ok(outcome) => outcome,
                Err(_) => Err(WakeRefusal::Failed {
                    kind: WakeFailure::Abandoned,
                    reason: "the wake was abandoned".into(),
                }),
            };
            metrics.woke(started.elapsed(), true);
            return outcome;
        }

        let outcome = self.boot(app_id).await;
        if let Err(refusal) = &outcome {
            metrics.wake_refused(refusal);
        }
        metrics.woke(started.elapsed(), false);
        let mut in_flight = self.in_flight.lock().await;
        if let Some((sender, _)) = in_flight.remove(app_id) {
            let _ = sender.send(outcome.clone());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protocol::InstanceState;

    #[tokio::test]
    async fn concurrent_requests_to_one_app_cause_one_wake() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
                record.health_check.probe_mut().unwrap().grace_period_ms = 50;
            }))
            .await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        let waker = AppWaker::new(host.arc().clone());
        let outcomes = futures::future::join_all((0..10).map(|_| {
            let waker = waker.clone();
            let app_id = app_id();
            async move { waker.wake(&app_id).await }
        }))
        .await;

        assert_eq!(outcomes.len(), 10);
        assert_eq!(
            host.vms
                .calls()
                .iter()
                .filter(|call| **call == crate::ports::VmCall::Wake)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn the_requests_that_waited_on_a_wake_are_counted_on_it() {
        use std::net::SocketAddr;
        use tracing_subscriber::layer::SubscriberExt;

        let _woken = WOKEN_LOG.lock();
        let counted = CountsCoalesced::default();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(counted.clone()));
        // Interest in a callsite is cached for the whole process by whichever thread reaches it
        // first, and a thread with no subscriber of its own caches it as never. This subscriber is
        // this thread's alone, so the cache is told the question is worth asking again — and the
        // lock above keeps the other test that reaches the same callsite from answering it first.
        tracing::callsite::rebuild_interest_cache();

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });

        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
                record.guest_ipv4 = crate::domain::health::probe::loopback();
                record.http_port = protocol::HttpPort::try_from(u32::from(port)).unwrap();
            }))
            .await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        let waker = AppWaker::new(host.arc().clone());
        let outcomes = futures::future::join_all((0..10).map(|_| {
            let waker = waker.clone();
            let app_id = app_id();
            async move { waker.wake(&app_id).await }
        }))
        .await;

        assert!(outcomes.iter().all(Result::is_ok));
        assert_eq!(counted.taken(), vec![9]);

        let page = page(&host).await;
        assert!(
            page.contains("nibrunner_wake_requests_coalesced_total 9\n"),
            "{page}"
        );
        assert!(
            page.contains("nibrunner_wake_duration_seconds_count 10\n"),
            "every request waited"
        );
        assert!(page.contains("nibrunner_wake_phase_seconds_count{phase=\"total\",outcome=\"restored\"} 1\n"));
        assert!(page.contains("nibrunner_wake_phase_seconds_count{phase=\"ready\",outcome=\"restored\"} 1\n"));
        assert!(page.contains("nibrunner_instance_wakes_total{app=\"app-1\",outcome=\"restored\"} 1\n"));
        assert!(
            page.contains("nibrunner_instance_last_wake_seconds{app=\"app-1\"} 0."),
            "{page}"
        );
    }

    /// Both tests below reach the log line a finished wake writes, and only one of them may be the
    /// thread that first decides whether anything is listening for it.
    static WOKEN_LOG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[derive(Clone, Default)]
    struct CountsCoalesced(Arc<std::sync::Mutex<Vec<u64>>>);

    impl CountsCoalesced {
        fn taken(&self) -> Vec<u64> {
            self.0.lock().unwrap().clone()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountsCoalesced {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            struct Pick(Option<u64>);
            impl tracing::field::Visit for Pick {
                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    if field.name() == "coalesced" {
                        self.0 = Some(value);
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut pick = Pick(None);
            event.record(&mut pick);
            if let Some(value) = pick.0 {
                self.0.lock().unwrap().push(value);
            }
        }
    }

    #[tokio::test]
    async fn a_host_with_no_memory_left_refuses_rather_than_evicting_a_neighbour() {
        let host = test_host().await;
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        for index in 0..4 {
            host.state
                .put_record(instance_record(|record| {
                    record.app_id = AppId::parse(format!("neighbour-{index}")).unwrap();
                }))
                .await;
        }
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        let waker = AppWaker::new(host.arc().clone());
        assert_eq!(
            waker.wake(&app_id()).await.unwrap_err(),
            WakeRefusal::NoRoom {
                shortfall_mib: u64::from(protocol::DEFAULT_INSTANCE_RESOURCES.memory_mib)
            }
        );
    }

    #[tokio::test]
    async fn a_tenant_refused_for_want_of_memory_is_told_how_much_was_missing() {
        let host = test_host().await;
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state.put_record(instance_record(|_| {})).await;
        for index in 0..4 {
            host.state
                .put_record(instance_record(|record| {
                    record.app_id = AppId::parse(format!("neighbour-{index}")).unwrap();
                }))
                .await;
        }
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        let refusal = AppWaker::new(host.arc().clone())
            .wake(&app_id())
            .await
            .unwrap_err();
        assert!(matches!(refusal, WakeRefusal::NoRoom { .. }), "{refusal:?}");
        let message = host
            .state
            .record(&app_id())
            .await
            .and_then(|record| record.message)
            .expect("a refused tenant is told why");
        assert!(
            message.as_str().contains("MiB short of the memory it needs"),
            "{message:?}"
        );
        assert!(message.as_str().contains(app_id().as_str()), "{message:?}");
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn a_guest_that_never_answers_is_refused_rather_than_reported_as_woken() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
                record.guest_ipv4 = crate::domain::health::probe::loopback();
                record.http_port = protocol::HttpPort::new(1).unwrap();
                let probe = record.health_check.probe_mut().unwrap();
                probe.grace_period_ms = 20;
                probe.timeout_ms = 50;
            }))
            .await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        let Err(WakeRefusal::Failed { kind, reason }) =
            AppWaker::new(host.arc().clone()).wake(&app_id()).await
        else {
            panic!("a guest that never answered was reported as woken");
        };
        assert!(reason.contains("nothing answered on port 1"), "{reason}");
        assert_eq!(kind, WakeFailure::NeverAnswered);
        assert!(
            page(&host)
                .await
                .contains("nibrunner_wake_refusals_total{reason=\"never_answered\"} 1\n"),
            "the refusal is counted under its reason"
        );
    }

    async fn page(host: &TestHost) -> String {
        crate::domain::metrics::tests::page(
            &crate::domain::metrics::tests::report(),
            &host.metrics,
            &host.state.snapshot().await,
            0,
        )
    }

    #[tokio::test]
    async fn a_guest_that_answers_no_port_is_woken_when_starting_is_all_it_promised() {
        let _woken = WOKEN_LOG.lock();
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
                record.health_check = protocol::HealthCheck::BootCompleted;
                record.guest_ipv4 = crate::domain::health::probe::loopback();
                record.http_port = protocol::HttpPort::new(1).unwrap();
            }))
            .await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        AppWaker::new(host.arc().clone())
            .wake(&app_id())
            .await
            .expect("a microVM that started is the whole of what it promised");
        assert!(host.vms.calls().contains(&crate::ports::VmCall::Wake));
    }

    #[tokio::test]
    async fn a_wake_is_refused_for_an_app_this_host_has_no_record_of_starting() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));

        let refusal = AppWaker::new(host.arc().clone())
            .wake(&app_id())
            .await
            .unwrap_err();
        assert_eq!(
            refusal,
            WakeRefusal::Failed {
                kind: WakeFailure::WouldNotStart,
                reason: "the microVM would not start".into()
            }
        );
    }

    #[tokio::test]
    async fn a_wake_is_refused_for_an_app_the_document_no_longer_names() {
        let host = test_host().await;
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        let waker = AppWaker::new(host.arc().clone());
        let refusal = waker.wake(&app_id()).await.unwrap_err();
        assert_eq!(
            refusal,
            WakeRefusal::Failed {
                kind: WakeFailure::NotNamed,
                reason: "the control plane no longer names it".into()
            }
        );
    }

    #[tokio::test]
    async fn a_wake_is_refused_while_the_isolation_ruleset_is_not_applied() {
        let host = test_host().await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![on_request]));
        let waker = AppWaker::new(host.arc().clone());
        let refusal = waker.wake(&app_id()).await.unwrap_err();
        assert_eq!(
            refusal,
            WakeRefusal::Failed {
                kind: WakeFailure::NotIsolated,
                reason: "the isolation ruleset is not applied".into()
            }
        );
    }

    #[tokio::test]
    async fn a_wake_is_refused_for_an_app_the_document_says_is_stopped() {
        let host = test_host().await;
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        let stopped = desired_instance(|instance| instance.desired_state = DesiredInstanceState::Stopped);
        host.cache
            .lock()
            .await
            .accept(desired_state(|state| state.instances = vec![stopped]));
        let waker = AppWaker::new(host.arc().clone());
        assert_eq!(
            waker.wake(&app_id()).await.unwrap_err(),
            WakeRefusal::Failed {
                kind: WakeFailure::NotOnRequest,
                reason: "it is stopped".into()
            }
        );
    }
}
