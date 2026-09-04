//! The other end of an app's loopback port.
//!
//! The output-hook DNAT rewrites that port to the guest before local delivery, so while the
//! microVM is up nothing here is reached — and while it is down this is what the proxy finds
//! instead of a refused connection.
//!
//! Bound for the life of the slot rather than the life of the microVM: the rule is what switches
//! between the two, so there is no bind to race the reconciler and no window the port is nobody's.
//!
//! For an app that runs on request this is the front door: the request that finds no microVM is
//! what starts one, and it is held here and answered from the guest once that guest is up. For
//! every other app a stopped microVM is somebody's decision or somebody's bug, and a request is
//! not the thing that resolves either — so it is told so.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioIo;
use protocol::{AppId, HostPort};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::proxy::forward::{forward, say, ProxyBody};
use crate::state::SharedState;

const LOOPBACK: &str = "127.0.0.1";

/// It says the app is down rather than unknown: the router's 404 is the answer for a hostname
/// this host serves nothing on, and a suspended app is not that.
fn app_is_down() -> Response<ProxyBody> {
    say(StatusCode::SERVICE_UNAVAILABLE, "This app is not running.\n")
}

fn app_would_not_start() -> Response<ProxyBody> {
    say(
        StatusCode::SERVICE_UNAVAILABLE,
        "This app could not be started.\n",
    )
}

/// Its own sentence rather than the one above. An app that could not be woken because its host
/// had no memory left is not a broken app, and telling its visitor otherwise would have its owner
/// reading a binary that is fine — while the repair, moving the app, is not something either of
/// them can bring about by asking again.
fn host_is_full() -> Response<ProxyBody> {
    say(
        StatusCode::SERVICE_UNAVAILABLE,
        "This app could not be started: its machine is out of memory.\n",
    )
}

/// A connection the proxy wants to upgrade cannot be carried across: what comes back from the
/// guest here is one HTTP message, and a websocket is the opposite of that. The wake still
/// happens, so the client that reconnects finds the app up and reaches it through the forward
/// rule rather than through this.
fn come_back() -> Response<ProxyBody> {
    let mut response = say(
        StatusCode::SERVICE_UNAVAILABLE,
        "This app is starting. Please reconnect.\n",
    );
    response
        .headers_mut()
        .insert("retry-after", hyper::header::HeaderValue::from_static("2"));
    response
}

/// What a wake ended as, so the activator can say the right sentence without knowing how waking
/// works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeRefusal {
    NoRoom { shortfall_mib: u64 },
    Failed { reason: String },
}

/// Waking is a reflex rather than a verb: nothing calls it, a request causes it. Behind a trait
/// so the activator can be tested against a host with no hypervisor on it.
#[async_trait::async_trait]
pub trait Waker: Send + Sync {
    async fn wake(&self, app_id: &AppId) -> Result<(), WakeRefusal>;
}

struct Listener {
    host_port: HostPort,
    task: tokio::task::JoinHandle<()>,
}

pub struct AppActivator {
    state: SharedState,
    waker: Arc<dyn Waker>,
    client: Client<HttpConnector, Incoming>,
    listeners: Mutex<BTreeMap<AppId, Listener>>,
}

impl AppActivator {
    pub fn new(state: SharedState, waker: Arc<dyn Waker>) -> Arc<Self> {
        Arc::new(Self {
            state,
            waker,
            client: crate::proxy::forward::upstream_client(),
            listeners: Mutex::new(BTreeMap::new()),
        })
    }

    /// A listener on every port this host holds a slot for, so one with no forward still answers.
    pub async fn serve(self: &Arc<Self>, slots: &[(AppId, HostPort)]) {
        let wanted: BTreeMap<AppId, HostPort> = slots.iter().cloned().collect();
        let mut listeners = self.listeners.lock().await;
        listeners.retain(|app_id, listener| {
            let keep = wanted.get(app_id) == Some(&listener.host_port);
            if !keep {
                listener.task.abort();
            }
            keep
        });
        for (app_id, host_port) in wanted {
            if listeners.contains_key(&app_id) {
                continue;
            }
            let address = SocketAddr::from(([127, 0, 0, 1], host_port.get()));
            match TcpListener::bind(address).await {
                Ok(listener) => {
                    tracing::info!(%app_id, %host_port, "app activator listening");
                    let task = tokio::spawn(accept(listener, self.clone(), app_id.clone()));
                    listeners.insert(app_id, Listener { host_port, task });
                }
                Err(error) => {
                    tracing::warn!(%app_id, %host_port, %error, "app activator bind failed");
                }
            }
        }
    }

    pub async fn listening_for(&self) -> Vec<AppId> {
        self.listeners.lock().await.keys().cloned().collect()
    }

    /// A request that finds no microVM. For an app that runs on request it is the thing that
    /// brings one back, and it waits here until the guest answers — which is a snapshot restore
    /// where there is one to restore and a cold boot where there is not, and the second is the
    /// reason the request is held rather than refused.
    ///
    /// The record is read again after the wake because the wake is what wrote it: the port and
    /// address to forward to are the ones the microVM that just came up is on.
    async fn handle(self: Arc<Self>, app_id: AppId, request: Request<Incoming>) -> Response<ProxyBody> {
        let Some(record) = self.state.record(&app_id).await else {
            return app_is_down();
        };
        if !record.on_request || !record.desired_running {
            return app_is_down();
        }
        self.state.mark_active(&app_id, crate::clock::now_ms()).await;

        let started = std::time::Instant::now();
        if let Err(refusal) = self.waker.wake(&app_id).await {
            return match refusal {
                WakeRefusal::NoRoom { shortfall_mib } => {
                    tracing::warn!(%app_id, shortfall_mib, "a request could not be given an app");
                    host_is_full()
                }
                WakeRefusal::Failed { reason } => {
                    tracing::warn!(%app_id, reason, "a request could not be given an app");
                    app_would_not_start()
                }
            };
        }
        let woke_ms = started.elapsed().as_millis();

        let Some(woken) = self.state.record(&app_id).await else {
            return app_would_not_start();
        };
        if request.headers().get(hyper::header::UPGRADE).is_some() {
            return come_back();
        }
        let served = std::time::Instant::now();
        let response = forward(
            &self.client,
            request,
            woken.guest_ipv4.as_str(),
            woken.http_port.get(),
            // `connection: close` is what keeps a resume immediate. The proxy pools its upstream
            // connections, and one opened while the microVM was down was accepted *here* rather
            // than forwarded to a guest — so the forward rule appearing later cannot redirect it.
            false,
        )
        .await;
        // Both halves, because a wake ends at the guest's first TCP accept and a guest accepts
        // long before it answers: `woke_ms` alone reads as the whole cost and is the smaller part
        // of it. Which half moved is the only way to tell a slower host from a tenant that takes
        // longer to come back.
        tracing::info!(
            %app_id,
            woke_ms,
            served_ms = served.elapsed().as_millis(),
            "app answered the request that woke it"
        );
        response
    }
}

async fn accept(listener: TcpListener, activator: Arc<AppActivator>, app_id: AppId) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let activator = activator.clone();
        let app_id = app_id.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let activator = activator.clone();
                let app_id = app_id.clone();
                async move { Ok::<_, std::convert::Infallible>(activator.handle(app_id, request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Where a request the activator answers is sent when the app is up: the guest itself, not the
/// loopback port it arrived on.
pub const GUEST_HOST: &str = LOOPBACK;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::HostState;
    use crate::test_support::{app_id, instance_record};
    use protocol::{HttpPort, InstanceState, Ipv4Address};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingWaker {
        woken: AtomicUsize,
        refusal: Option<WakeRefusal>,
    }

    impl CountingWaker {
        fn allowing() -> Arc<Self> {
            Arc::new(Self {
                woken: AtomicUsize::new(0),
                refusal: None,
            })
        }

        fn refusing(refusal: WakeRefusal) -> Arc<Self> {
            Arc::new(Self {
                woken: AtomicUsize::new(0),
                refusal: Some(refusal),
            })
        }

        fn count(&self) -> usize {
            self.woken.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Waker for CountingWaker {
        async fn wake(&self, _app_id: &AppId) -> Result<(), WakeRefusal> {
            self.woken.fetch_add(1, Ordering::SeqCst);
            match &self.refusal {
                None => Ok(()),
                Some(refusal) => Err(refusal.clone()),
            }
        }
    }

    /// A port the activator is actually listening on, rather than one that was free a moment
    /// ago: a test must not need 21000 to be free on whatever machine runs it, and the window
    /// between letting a probe port go and binding it again is one another test can take.
    async fn serving(activator: &Arc<AppActivator>, app_id: &AppId) -> HostPort {
        for _ in 0..50 {
            let probe = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
            let port = HostPort::new(probe.local_addr().unwrap().port()).unwrap();
            drop(probe);
            activator.serve(&[(app_id.clone(), port)]).await;
            if activator.listening_for().await.contains(app_id) {
                return port;
            }
        }
        panic!("the activator could not be given a port to listen on");
    }

    async fn get(port: HostPort) -> reqwest::Response {
        crate::install_crypto_provider();
        reqwest::Client::builder()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("the activator answers")
    }

    /// A real listener on the address the record names, because the thing being checked is that a
    /// request came out of the far side.
    async fn guest(state: &SharedState, body: &'static str) -> HostPort {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let service = service_fn(move |_request: Request<Incoming>| async move {
                        Ok::<_, std::convert::Infallible>(say(StatusCode::OK, body))
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
                record.guest_ipv4 = Ipv4Address::parse("127.0.0.1").unwrap();
                record.http_port = HttpPort::new(port).unwrap();
            }))
            .await;
        HostPort::new(port).unwrap()
    }

    #[tokio::test]
    async fn a_request_the_microvm_is_not_there_to_take_is_answered_rather_than_refused() {
        let state = HostState::shared();
        state
            .put_record(instance_record(|record| record.on_request = false))
            .await;
        let activator = AppActivator::new(state, CountingWaker::allowing());
        let host_port = serving(&activator, &app_id()).await;
        let response = get(host_port).await;
        assert_eq!(response.status(), 503);
        assert!(response.text().await.unwrap().contains("not running"));
    }

    #[tokio::test]
    async fn the_request_waits_for_the_wake_and_is_answered_by_the_guest_that_comes_up() {
        let state = HostState::shared();
        guest(&state, "served by the tenant\n").await;
        let waker = CountingWaker::allowing();
        let activator = AppActivator::new(state, waker.clone());
        let host_port = serving(&activator, &app_id()).await;

        let response = get(host_port).await;
        assert_eq!(response.status(), 200);
        // Not reusable, so the proxy takes the forward rule rather than asking this again.
        assert_eq!(
            response
                .headers()
                .get("connection")
                .map(|value| value.to_str().unwrap()),
            Some("close")
        );
        assert_eq!(response.text().await.unwrap(), "served by the tenant\n");
        assert_eq!(waker.count(), 1);
    }

    #[tokio::test]
    async fn a_wake_refused_for_want_of_memory_says_so_rather_than_blaming_the_app() {
        let state = HostState::shared();
        state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
            }))
            .await;
        let activator = AppActivator::new(
            state,
            CountingWaker::refusing(WakeRefusal::NoRoom { shortfall_mib: 256 }),
        );
        let host_port = serving(&activator, &app_id()).await;
        assert!(get(host_port)
            .await
            .text()
            .await
            .unwrap()
            .contains("out of memory"));
    }

    #[tokio::test]
    async fn a_microvm_that_would_not_start_is_said_to_have_not_started() {
        let state = HostState::shared();
        state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
            }))
            .await;
        let activator = AppActivator::new(
            state,
            CountingWaker::refusing(WakeRefusal::Failed {
                reason: "no slots left".into(),
            }),
        );
        let host_port = serving(&activator, &app_id()).await;
        assert!(get(host_port)
            .await
            .text()
            .await
            .unwrap()
            .contains("could not be started"));
    }

    /// Suspending is an answer, not a question. A request is not the thing that reverses it.
    #[tokio::test]
    async fn a_suspended_app_is_not_woken_by_somebody_finding_its_hostname() {
        let state = HostState::shared();
        state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Stopped;
                record.desired_running = false;
            }))
            .await;
        let waker = CountingWaker::allowing();
        let activator = AppActivator::new(state, waker.clone());
        let host_port = serving(&activator, &app_id()).await;
        assert_eq!(get(host_port).await.status(), 503);
        assert_eq!(waker.count(), 0);
    }

    #[tokio::test]
    async fn a_slot_the_host_no_longer_holds_stops_being_answered_for() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        let activator = AppActivator::new(state, CountingWaker::allowing());
        let host_port = serving(&activator, &app_id()).await;
        // A sync that changes nothing keeps the port, so nothing goes out under it.
        activator.serve(&[(app_id(), host_port)]).await;
        assert_eq!(activator.listening_for().await, vec![app_id()]);
        assert_eq!(get(host_port).await.status(), 503);

        activator.serve(&[]).await;
        assert!(activator.listening_for().await.is_empty());
        // The listener is gone, so the port refuses rather than answering for an app that left.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", host_port.get()))
            .await
            .is_err());
    }
}
