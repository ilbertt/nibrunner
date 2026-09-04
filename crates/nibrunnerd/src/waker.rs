//! Bringing an `on-request` app's microVM back because something asked for it.
//!
//! A restore where there is a snapshot to restore and a cold boot where there is not, which is
//! the difference between tens of milliseconds and a second or so. Which one happened is the
//! reconcile's to decide; what is decided here is whether it may happen at all.
//!
//! One wake per app at a time. A cold page load is a burst of requests, and every one of them
//! arrives to find no microVM: without this the first would bring it up and the rest would each
//! spend the app's restart budget racing it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{AppId, DesiredInstanceState};
use tokio::sync::{broadcast, Mutex};

use crate::health::probe::probe_instance;
use crate::host::Host;
use crate::proxy::activator::{WakeRefusal, Waker};
use crate::report::capacity::{committed_resources, memory_shortfall_mib};

/// Tighter than the startup grid the status loop probes on, because somebody is holding a browser
/// tab open on this one — and tighter than it needs to be to find out, because on average half an
/// interval is added to every wake purely by asking on a grid. A restore reaches its first accept
/// in ~90ms, so 25ms spent a measured ~12ms of that on nothing; the probe is one loopback connect
/// to a guest that either has the port open or does not.
const PROBE_INTERVAL: Duration = Duration::from_millis(5);

type Outcome = Result<(), WakeRefusal>;

pub struct AppWaker {
    host: Arc<Host>,
    /// The joiners are counted beside the sender rather than derived afterwards: a cold page load
    /// is one wake and a burst of requests, and without the count every reading of how often apps
    /// wake is really a reading of how many requests arrived while one was waking.
    in_flight: Mutex<BTreeMap<AppId, (broadcast::Sender<Outcome>, u64)>>,
}

impl AppWaker {
    pub fn new(host: Arc<Host>) -> Arc<Self> {
        Arc::new(Self {
            host,
            in_flight: Mutex::new(BTreeMap::new()),
        })
    }

    /// Whether this host can find the memory for one more microVM, asked of everything *else* it
    /// is holding: the app being woken is idle, so it commits nothing until it is up, and one
    /// that is somehow already running would otherwise be counted twice and refused its own room.
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
                reason: "the control plane no longer names it".into(),
            });
        };
        if wanted.desired_state != DesiredInstanceState::OnRequest {
            return Err(WakeRefusal::Failed {
                reason: format!("it is {}", wanted.desired_state.as_str()),
            });
        }
        if !self.host.state.snapshot().await.isolated {
            return Err(WakeRefusal::Failed {
                reason: "the isolation ruleset is not applied".into(),
            });
        }
        if let Some(refusal) = self.refusal_for_room(app_id, &wanted.config.resources).await {
            // The record is the only account of this its owner will ever see, and left to say
            // nothing it reads as an app asleep between requests — which is the one thing it is not.
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

        let outcome = crate::reconcile::instances::resume_instance(&self.host, &wanted)
            .await
            .map_err(|reason| WakeRefusal::Failed { reason })?;

        let Some(record) = self.host.state.record(app_id).await else {
            return Err(WakeRefusal::Failed {
                reason: "the microVM would not start".into(),
            });
        };
        if record.started_at.is_none() {
            // Neither half wrote a time, so neither reached the VMM: the artifact would not
            // fetch, the restore was refused, or the restart budget was spent on boots that
            // already failed.
            let reason = record.message.as_ref().map_or_else(
                || "the microVM would not start".to_string(),
                |message| message.as_str().to_string(),
            );
            return Err(WakeRefusal::Failed { reason });
        }

        // The grace period is the deadline: past it the health loop would call the instance
        // failed anyway, so holding the request longer only delays telling whoever sent it.
        let deadline = Instant::now() + Duration::from_millis(record.health_check.grace_period_ms);
        loop {
            if probe_instance(&record.guest_ipv4, record.http_port, &record.health_check).await {
                break;
            }
            if Instant::now() >= deadline {
                return Err(WakeRefusal::Failed {
                    reason: format!("nothing answered on port {} inside the guest", record.http_port),
                });
            }
            tokio::time::sleep(PROBE_INTERVAL).await;
        }

        // The guest is answering, which is the one thing the forward rule waits on — so the loop
        // is told now rather than on its next tick. Until it refreshes, the record still says this
        // app is coming up and every request behind this one is answered by the activator.
        self.host.state.signal_refresh();
        // Counted here rather than when the wake started, because when it started is before any
        // of them had arrived: a burst reaches a sleeping app over the same tens of milliseconds
        // this wake spends, so reading the count first reports nothing waited on a wake that the
        // whole burst waited on. The requests that join between this line and the removal below
        // go uncounted, which is a window measured against a boot rather than against a burst.
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
            coalesced = joined,
            "app woken by a request"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl Waker for AppWaker {
    async fn wake(&self, app_id: &AppId) -> Result<(), WakeRefusal> {
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
        if let Some(mut waiting) = joined {
            return match waiting.recv().await {
                Ok(outcome) => outcome,
                // The wake that was leading ended without saying so, which is the daemon shutting
                // down: the request is told the app did not start rather than left holding.
                Err(_) => Err(WakeRefusal::Failed {
                    reason: "the wake was abandoned".into(),
                }),
            };
        }

        let outcome = self.boot(app_id).await;
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
                // Nothing is listening for these to reach, so the wake gives up at its own
                // deadline rather than holding the test for the grace period a real app gets.
                record.health_check.grace_period_ms = 50;
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
                .filter(|call| **call == crate::services::VmCall::Wake)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn the_requests_that_waited_on_a_wake_are_counted_on_it() {
        use std::net::SocketAddr;
        use tracing_subscriber::layer::SubscriberExt;

        let counted = CountsCoalesced::default();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(counted.clone()));

        // The wake only reaches its log once something answers inside the guest, so the probe is
        // pointed at a socket on this machine that accepts and says nothing.
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
                record.guest_ipv4 = crate::health::probe::loopback();
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
        // One wake, and the nine behind the one that led it.
        assert_eq!(counted.taken(), vec![9]);
    }

    /// A count that is only ever logged is only testable through the log.
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

    /// Refusing converges: the app stays down, its owner is told why, and the repair is to move
    /// it — which is placement's to make and not something a request can bring about. Evicting a
    /// neighbour would make one tenant's traffic a reason to take another tenant's app offline,
    /// and on a host already over its memory every wake would evict somebody whose next visitor
    /// evicts somebody else.
    #[tokio::test]
    async fn a_host_with_no_memory_left_refuses_rather_than_evicting_a_neighbour() {
        let host = test_host().await;
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        // Every app the host can hold, running, so the one being woken does not fit.
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
    async fn a_wake_is_refused_for_an_app_the_document_no_longer_names() {
        let host = test_host().await;
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        let waker = AppWaker::new(host.arc().clone());
        let refusal = waker.wake(&app_id()).await.unwrap_err();
        assert_eq!(
            refusal,
            WakeRefusal::Failed {
                reason: "the control plane no longer names it".into()
            }
        );
    }

    /// A tenant boots onto a host whose isolation ruleset is in the kernel, and a request is not
    /// a reason to make an exception.
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
                reason: "the isolation ruleset is not applied".into()
            }
        );
    }

    /// Suspending is an answer, not a question: a request is not the thing that reverses it.
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
                reason: "it is stopped".into()
            }
        );
    }
}
