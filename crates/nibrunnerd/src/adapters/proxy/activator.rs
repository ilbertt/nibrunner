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

use crate::adapters::proxy::forward::{forward, say, ProxyBody};
use crate::domain::metrics::sleep_wake::Answer;
use crate::domain::metrics::HostMetrics;
use crate::ports::{WakeRefusal, Waker};
use crate::state::SharedState;

const LOOPBACK: &str = "127.0.0.1";

fn app_is_down() -> Response<ProxyBody> {
    say(StatusCode::SERVICE_UNAVAILABLE, "This app is not running.\n")
}

fn app_would_not_start() -> Response<ProxyBody> {
    say(
        StatusCode::SERVICE_UNAVAILABLE,
        "This app could not be started.\n",
    )
}

fn host_is_full() -> Response<ProxyBody> {
    say(
        StatusCode::SERVICE_UNAVAILABLE,
        "This app could not be started: its machine is out of memory.\n",
    )
}

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

struct Listener {
    host_port: HostPort,
    task: tokio::task::JoinHandle<()>,
}

pub struct AppActivator {
    state: SharedState,
    waker: Arc<dyn Waker>,
    metrics: Arc<HostMetrics>,
    client: Client<HttpConnector, Incoming>,
    listeners: Mutex<BTreeMap<AppId, Listener>>,
}

impl AppActivator {
    pub fn new(state: SharedState, waker: Arc<dyn Waker>, metrics: Arc<HostMetrics>) -> Arc<Self> {
        Arc::new(Self {
            state,
            waker,
            metrics,
            client: crate::adapters::proxy::forward::upstream_client(),
            listeners: Mutex::new(BTreeMap::new()),
        })
    }

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

    async fn handle(self: Arc<Self>, app_id: AppId, request: Request<Incoming>) -> Response<ProxyBody> {
        let (response, answer) = self.answer(app_id, request).await;
        self.metrics.sleep_wake.answered(answer);
        response
    }

    async fn answer(&self, app_id: AppId, request: Request<Incoming>) -> (Response<ProxyBody>, Answer) {
        let Some(record) = self.state.record(&app_id).await else {
            return (app_is_down(), Answer::Down);
        };
        if !record.on_request || !record.desired_running {
            return (app_is_down(), Answer::Down);
        }
        self.state.mark_active(&app_id, crate::clock::now_ms()).await;

        let started = std::time::Instant::now();
        if let Err(refusal) = self.waker.wake(&app_id).await {
            return match refusal {
                WakeRefusal::NoRoom { shortfall_mib } => {
                    tracing::warn!(%app_id, shortfall_mib, "a request could not be given an app");
                    (host_is_full(), Answer::HostFull)
                }
                WakeRefusal::Failed { reason, .. } => {
                    tracing::warn!(%app_id, reason, "a request could not be given an app");
                    (app_would_not_start(), Answer::WouldNotStart)
                }
            };
        }
        let woke_ms = started.elapsed().as_millis();

        let Some(woken) = self.state.record(&app_id).await else {
            return (app_would_not_start(), Answer::WouldNotStart);
        };
        if request.headers().get(hyper::header::UPGRADE).is_some() {
            return (come_back(), Answer::ComeBack);
        }
        let served = std::time::Instant::now();
        let response = forward(
            &self.client,
            request,
            woken.guest_ipv4.as_str(),
            woken.http_port.get(),
            false,
        )
        .await;
        self.metrics.sleep_wake.first_response(served.elapsed());
        tracing::info!(
            %app_id,
            woke_ms,
            served_ms = served.elapsed().as_millis(),
            "app answered the request that woke it"
        );
        (response, Answer::Served)
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
        let activator = AppActivator::new(state, CountingWaker::allowing(), Arc::default());
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
        let activator = AppActivator::new(state, waker.clone(), Arc::default());
        let host_port = serving(&activator, &app_id()).await;

        let response = get(host_port).await;
        assert_eq!(response.status(), 200);
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
            Arc::default(),
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
                kind: crate::ports::WakeFailure::WouldNotStart,
                reason: "no slots left".into(),
            }),
            Arc::default(),
        );
        let host_port = serving(&activator, &app_id()).await;
        assert!(get(host_port)
            .await
            .text()
            .await
            .unwrap()
            .contains("could not be started"));
    }

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
        let activator = AppActivator::new(state, waker.clone(), Arc::default());
        let host_port = serving(&activator, &app_id()).await;
        assert_eq!(get(host_port).await.status(), 503);
        assert_eq!(waker.count(), 0);
    }

    #[tokio::test]
    async fn a_slot_the_host_no_longer_holds_stops_being_answered_for() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        let activator = AppActivator::new(state, CountingWaker::allowing(), Arc::default());
        let host_port = serving(&activator, &app_id()).await;
        activator.serve(&[(app_id(), host_port)]).await;
        assert_eq!(activator.listening_for().await, vec![app_id()]);
        assert_eq!(get(host_port).await.status(), 503);

        activator.serve(&[]).await;
        assert!(activator.listening_for().await.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", host_port.get()))
            .await
            .is_err());
    }

    async fn raw(port: HostPort, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port.get()))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_to_string(&mut answer),
        )
        .await
        .expect("the activator closes the connection it answered on")
        .unwrap();
        answer
    }

    #[tokio::test]
    async fn a_connection_that_would_be_upgraded_is_told_to_come_back_once_the_app_is_up() {
        let state = HostState::shared();
        guest(&state, "served by the tenant\n").await;
        let waker = CountingWaker::allowing();
        let activator = AppActivator::new(state, waker.clone(), Arc::default());
        let host_port = serving(&activator, &app_id()).await;

        let answered = raw(
            host_port,
            "GET /ws HTTP/1.0\r\nHost: app-1.apps.example.com\r\nupgrade: websocket\r\n\r\n",
        )
        .await;
        assert!(answered.starts_with("HTTP/1.0 503 "), "{answered}");
        assert!(answered.contains("retry-after: 2"), "{answered}");
        assert!(
            answered.ends_with("This app is starting. Please reconnect.\n"),
            "{answered}"
        );
        assert_eq!(waker.count(), 1, "the app is still woken for the reconnect");
    }

    #[tokio::test]
    async fn a_request_that_woke_an_app_counts_as_the_app_having_been_used() {
        let state = HostState::shared();
        guest(&state, "served by the tenant\n").await;
        let before = crate::clock::now_ms();
        let activator = AppActivator::new(state.clone(), CountingWaker::allowing(), Arc::default());
        let host_port = serving(&activator, &app_id()).await;
        assert_eq!(get(host_port).await.status(), 200);
        let stamped = state.snapshot().await.last_active_at_ms.get(&app_id()).copied();
        assert!(stamped.is_some_and(|at| at >= before), "{stamped:?}");
    }

    #[tokio::test]
    async fn an_app_that_was_never_asked_for_on_request_is_not_marked_as_used_by_a_refusal() {
        let state = HostState::shared();
        state
            .put_record(instance_record(|record| record.on_request = false))
            .await;
        let activator = AppActivator::new(state.clone(), CountingWaker::allowing(), Arc::default());
        let host_port = serving(&activator, &app_id()).await;
        assert_eq!(get(host_port).await.status(), 503);
        assert!(state.snapshot().await.last_active_at_ms.is_empty());
    }

    #[tokio::test]
    async fn a_slot_that_moved_to_another_port_stops_being_answered_for_on_the_old_one() {
        let state = HostState::shared();
        state.put_record(instance_record(|_| {})).await;
        let activator = AppActivator::new(state, CountingWaker::allowing(), Arc::default());
        let first = serving(&activator, &app_id()).await;
        let second = serving(&activator, &app_id()).await;
        assert_ne!(first, second);
        assert_eq!(activator.listening_for().await, vec![app_id()]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", first.get()))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_port_nothing_can_bind_leaves_the_activator_answering_for_nothing() {
        let held = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let taken = HostPort::new(held.local_addr().unwrap().port()).unwrap();
        let activator = AppActivator::new(HostState::shared(), CountingWaker::allowing(), Arc::default());
        activator.serve(&[(app_id(), taken)]).await;
        assert!(activator.listening_for().await.is_empty());
    }

    #[test]
    fn a_guest_is_reached_on_the_loopback_the_relay_sits_on() {
        assert_eq!(GUEST_HOST, LOOPBACK);
    }
}
