use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use protocol::HostPort;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

use crate::adapters::proxy::forward::{forward, hostname_of, note_the_hop, say, ProxyBody};
use crate::domain::report::routes::RouteTarget;

const LOOPBACK: &str = "127.0.0.1";

// Nothing otherwise bounds how long a caller may take to finish a handshake or send a request
// line, and these listeners are open to the world: a connection opened and left unfinished is
// held for as long as its opener likes. The header alone, and the same ten seconds nibrun's edge
// gives, because a tenant's slow upload and its streaming response are the tenant's to take as
// long over as their own users need.
const GREETING_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteTable {
    by_hostname: BTreeMap<String, HostPort>,
}

impl RouteTable {
    pub fn from_targets(targets: &[RouteTarget]) -> Self {
        Self {
            by_hostname: targets
                .iter()
                .flat_map(|target| {
                    target
                        .hostnames
                        .iter()
                        .map(|entry| (entry.hostname.as_str().to_ascii_lowercase(), target.host_port))
                })
                .collect(),
        }
    }

    pub fn port_for(&self, hostname: &str) -> Option<HostPort> {
        self.by_hostname.get(hostname).copied()
    }

    pub fn hostnames(&self) -> Vec<&str> {
        self.by_hostname.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hostname.is_empty()
    }
}

// What a connection was before any request arrived on it: the name its handshake asked for, the
// address it came from, and whether it was encrypted. A tenant is dialled over loopback and can
// read none of it for itself.
#[derive(Debug, Clone)]
pub struct Arrival {
    pub server_name: Option<Arc<str>>,
    pub peer: IpAddr,
    pub secure: bool,
}

pub struct Router {
    routes: RwLock<Arc<RouteTable>>,
    client: Client<HttpConnector, Incoming>,
}

impl Router {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routes: RwLock::new(Arc::new(RouteTable::default())),
            client: crate::adapters::proxy::forward::upstream_client(),
        })
    }

    pub async fn apply(&self, table: RouteTable) {
        *self.routes.write().await = Arc::new(table);
    }

    pub async fn routes(&self) -> Arc<RouteTable> {
        self.routes.read().await.clone()
    }

    pub async fn handle(
        self: Arc<Self>,
        request: Request<Incoming>,
        arrival: Arrival,
    ) -> Response<ProxyBody> {
        let http2 = request.version() == hyper::Version::HTTP_2;
        let mut response = self.route(request, arrival).await;
        if http2 {
            // Illegal over HTTP/2, and hyper logs a warning for each one it has to strip.
            response.headers_mut().remove(hyper::header::CONNECTION);
        }
        response
    }

    async fn route(self: Arc<Self>, mut request: Request<Incoming>, arrival: Arrival) -> Response<ProxyBody> {
        let Some(hostname) = hostname_of(&request) else {
            return say(StatusCode::BAD_REQUEST, "This request names no host.\n");
        };
        // One certificate covers every app on this host, so the name in the handshake is the only
        // thing stopping a connection opened for one tenant from asking for another tenant's app.
        if arrival
            .server_name
            .as_deref()
            .is_some_and(|name| !name.eq_ignore_ascii_case(&hostname))
        {
            return say(
                StatusCode::MISDIRECTED_REQUEST,
                "This connection was opened for a different host.\n",
            );
        }
        let Some(port) = self.routes().await.port_for(&hostname) else {
            return say(
                StatusCode::NOT_FOUND,
                "No app on this host answers for that hostname.\n",
            );
        };
        note_the_hop(request.headers_mut(), arrival.peer, arrival.secure, &hostname);
        forward(&self.client, request, LOOPBACK, port.get(), true).await
    }
}

// A reply leaves this proxy as more than one write — the headers, then the body, and under TLS the
// records carrying them. With Nagle still on, the second write waits for an ACK the other end has
// already decided to delay, so a visitor pays about 40ms for an answer the tenant produced in two.
// The connector to the tenant is already nodelay; the leg the visitor is on was not.
fn without_nagle(stream: TcpStream) -> TcpStream {
    let _ = stream.set_nodelay(true);
    stream
}

pub async fn serve_http(router: Arc<Router>, address: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "the proxy is listening");
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let stream = without_nagle(stream);
        let router = router.clone();
        tokio::spawn(async move {
            let arrival = Arrival {
                server_name: None,
                peer: peer.ip(),
                secure: false,
            };
            let service = service_fn(move |request| {
                let (router, arrival) = (router.clone(), arrival.clone());
                async move { Ok::<_, std::convert::Infallible>(router.handle(request, arrival).await) }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(GREETING_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

pub async fn serve_https(
    router: Arc<Router>,
    address: SocketAddr,
    certificate: &Path,
    key: &Path,
    client_ca: Option<&Path>,
) -> std::io::Result<()> {
    let acceptor = tls_acceptor(certificate, key, client_ca)?;
    let listener = TcpListener::bind(address).await?;
    match client_ca {
        Some(pool) => tracing::info!(
            %address,
            trust_pool = %pool.display(),
            "the proxy is listening for TLS, and refuses a caller that presents no certificate of its own"
        ),
        None => tracing::info!(
            %address,
            "the proxy is listening for TLS, and serves whoever reaches this address"
        ),
    }
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let stream = without_nagle(stream);
        let router = router.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(Ok(stream)) = tokio::time::timeout(GREETING_TIMEOUT, acceptor.accept(stream)).await else {
                return;
            };
            let arrival = Arrival {
                server_name: stream.get_ref().1.server_name().map(Arc::from),
                peer: peer.ip(),
                secure: true,
            };
            let service = service_fn(move |request| {
                let (router, arrival) = (router.clone(), arrival.clone());
                async move { Ok::<_, std::convert::Infallible>(router.handle(request, arrival).await) }
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(GREETING_TIMEOUT)
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await;
        });
    }
}

// A trust pool that admits nobody is refused here rather than at the handshake it would refuse:
// the file is named to require a client certificate, and an empty one requires an impossible one.
fn client_verifier(client_ca: &Path) -> std::io::Result<Arc<dyn rustls::server::danger::ClientCertVerifier>> {
    let anchors: Vec<_> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(client_ca)?))
            .collect::<Result<_, _>>()?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(anchors);
    rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub fn tls_acceptor(
    certificate: &Path,
    key: &Path,
    client_ca: Option<&Path>,
) -> std::io::Result<tokio_rustls::TlsAcceptor> {
    let certificates: Vec<_> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(certificate)?))
            .collect::<Result<_, _>>()?;
    let private_key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(key)?))?
        .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the key file holds no private key",
        )
    })?;
    let builder = rustls::ServerConfig::builder();
    let builder = match client_ca {
        Some(client_ca) => builder.with_client_cert_verifier(client_verifier(client_ca)?),
        None => builder.with_no_client_auth(),
    };
    let mut config = builder
        .with_single_cert(certificates, private_key)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_visitor_never_waits_on_an_ack_the_other_end_is_delaying() {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let accepting = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let _visitor = TcpStream::connect(address).await.unwrap();

        let accepted = accepting.await.unwrap();
        assert!(
            !accepted.nodelay().unwrap(),
            "the kernel still hands it over with Nagle on"
        );
        assert!(without_nagle(accepted).nodelay().unwrap());
    }
    use crate::domain::report::routes::renderable_routes;
    use crate::test_support::{app_hostname, instance_record};
    use protocol::{AppHostname, AppHostnameKind, Hostname};

    #[test]
    fn a_route_is_rendered_for_every_hostname_an_app_holds() {
        let record = instance_record(|record| {
            record.hostnames = vec![
                app_hostname(),
                AppHostname {
                    hostname: Hostname::parse("Www.Example.Com")
                        .unwrap_or_else(|_| Hostname::parse("www.example.com").unwrap()),
                    kind: AppHostnameKind::Custom,
                },
            ];
        });
        let table = RouteTable::from_targets(&renderable_routes(std::slice::from_ref(&record)));
        assert_eq!(
            table.port_for(app_hostname().hostname.as_str()),
            Some(record.host_port)
        );
        assert_eq!(table.port_for("www.example.com"), Some(record.host_port));
        assert_eq!(table.port_for("nobody.example.com"), None);
        assert_eq!(table.hostnames().len(), 2);
    }

    #[test]
    fn the_table_is_identical_whether_the_app_is_up_or_down() {
        let up = RouteTable::from_targets(&renderable_routes(&[instance_record(|_| {})]));
        let down = RouteTable::from_targets(&renderable_routes(&[instance_record(|record| {
            record.state = protocol::InstanceState::Stopped;
        })]));
        assert_eq!(up, down);
        assert!(RouteTable::default().is_empty());
    }

    #[tokio::test]
    async fn a_router_that_has_been_told_nothing_answers_for_nothing() {
        let router = Router::new();
        assert!(router.routes().await.is_empty());
        assert!(router.routes().await.hostnames().is_empty());
        assert_eq!(router.routes().await.port_for("app-1.example.com"), None);
    }

    #[tokio::test]
    async fn a_new_table_replaces_the_old_one_rather_than_being_added_to_it() {
        let router = Router::new();
        router
            .apply(RouteTable::from_targets(&renderable_routes(&[instance_record(
                |_| {},
            )])))
            .await;
        assert_eq!(router.routes().await.hostnames(), vec!["app-1.apps.example.com"]);

        router.apply(RouteTable::default()).await;
        assert!(router.routes().await.is_empty());
    }

    fn arriving(server_name: Option<&'static str>, secure: bool) -> Arrival {
        Arrival {
            server_name: server_name.map(Arc::from),
            peer: IpAddr::from([203, 0, 113, 7]),
            secure,
        }
    }

    async fn serving(router: Arc<Router>) -> u16 {
        serving_as(router, arriving(None, false)).await
    }

    async fn serving_as(router: Arc<Router>, arrival: Arrival) -> u16 {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (router, arrival) = (router.clone(), arrival.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let (router, arrival) = (router.clone(), arrival.clone());
                        async move { Ok::<_, std::convert::Infallible>(router.handle(request, arrival).await) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        port
    }

    async fn asked(port: u16, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_to_string(&mut answer),
        )
        .await
        .expect("the proxy closes the connection it answered on")
        .unwrap();
        answer
    }

    #[tokio::test]
    async fn a_request_that_names_no_host_is_refused_before_any_table_is_read() {
        let port = serving(Router::new()).await;
        let answered = asked(port, "GET / HTTP/1.0\r\n\r\n").await;
        assert!(answered.starts_with("HTTP/1.0 400 "), "{answered}");
        assert!(answered.ends_with("This request names no host.\n"), "{answered}");
    }

    #[tokio::test]
    async fn a_hostname_no_app_on_this_host_holds_is_a_not_found_rather_than_a_gateway_failure() {
        let router = Router::new();
        router
            .apply(RouteTable::from_targets(&renderable_routes(&[instance_record(
                |_| {},
            )])))
            .await;
        let port = serving(router).await;
        let answered = asked(port, "GET / HTTP/1.0\r\nHost: nobody.example.com\r\n\r\n").await;
        assert!(answered.starts_with("HTTP/1.0 404 "), "{answered}");
        assert!(
            answered.ends_with("No app on this host answers for that hostname.\n"),
            "{answered}"
        );
    }

    #[tokio::test]
    async fn a_hostname_that_is_routed_is_carried_to_the_app_it_names() {
        let upstream = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let host_port = HostPort::new(upstream.local_addr().unwrap().port()).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = upstream.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let service = service_fn(|_request: Request<Incoming>| async move {
                        Ok::<_, std::convert::Infallible>(say(StatusCode::OK, "served by the tenant\n"))
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let router = Router::new();
        router
            .apply(RouteTable::from_targets(&renderable_routes(&[instance_record(
                |record| record.host_port = host_port,
            )])))
            .await;
        let port = serving(router).await;
        let answered = asked(port, "GET / HTTP/1.0\r\nHost: App-1.Apps.Example.Com\r\n\r\n").await;
        assert!(answered.starts_with("HTTP/1.0 200 "), "{answered}");
        assert!(answered.ends_with("served by the tenant\n"), "{answered}");
    }

    #[test]
    fn a_certificate_this_host_does_not_have_is_an_error_rather_than_a_proxy_that_serves_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let refusal = |certificate: &Path, key: &Path| match tls_acceptor(certificate, key, None) {
            Ok(_) => panic!("{} was accepted as TLS material", certificate.display()),
            Err(error) => error,
        };
        let absent = directory.path().join("absent.pem");
        assert_eq!(refusal(&absent, &absent).kind(), std::io::ErrorKind::NotFound);

        let empty = directory.path().join("empty.pem");
        std::fs::write(&empty, b"").unwrap();
        let error = refusal(&empty, &empty);
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("no private key"), "{error}");
    }

    async fn routed_to_one_app(server_name: Option<&'static str>) -> u16 {
        let router = Router::new();
        router
            .apply(RouteTable::from_targets(&renderable_routes(&[instance_record(
                |_| {},
            )])))
            .await;
        serving_as(router, arriving(server_name, false)).await
    }

    #[tokio::test]
    async fn a_connection_opened_for_one_hostname_may_not_ask_for_another() {
        let port = routed_to_one_app(Some("somewhere.else.example.com")).await;
        let answered = asked(port, "GET / HTTP/1.0\r\nHost: app-1.apps.example.com\r\n\r\n").await;
        assert!(answered.starts_with("HTTP/1.0 421 "), "{answered}");
        assert!(
            answered.ends_with("This connection was opened for a different host.\n"),
            "{answered}"
        );
    }

    #[tokio::test]
    async fn the_name_in_the_handshake_and_the_name_in_the_header_are_one_name_in_any_case() {
        let port = routed_to_one_app(Some("app-1.apps.example.com")).await;
        let answered = asked(port, "GET / HTTP/1.0\r\nHost: APP-1.Apps.Example.Com\r\n\r\n").await;
        assert!(
            !answered.starts_with("HTTP/1.0 421 "),
            "the request reached its route rather than being read as a misdirected one: {answered}"
        );
    }

    #[tokio::test]
    async fn a_request_that_names_no_host_is_refused_before_the_handshake_name_is_read() {
        let port = routed_to_one_app(Some("app-1.apps.example.com")).await;
        let answered = asked(port, "GET / HTTP/1.0\r\n\r\n").await;
        assert!(answered.starts_with("HTTP/1.0 400 "), "{answered}");
    }

    async fn echoing_the_forwarded_headers(secure: bool) -> u16 {
        let upstream = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let host_port = HostPort::new(upstream.local_addr().unwrap().port()).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = upstream.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let service = service_fn(|request: Request<Incoming>| async move {
                        let read = |name: &str| {
                            request
                                .headers()
                                .get(name)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("(none)")
                                .to_string()
                        };
                        let heard = format!(
                            "for={} proto={} host={}",
                            read("x-forwarded-for"),
                            read("x-forwarded-proto"),
                            read("x-forwarded-host")
                        );
                        Ok::<_, std::convert::Infallible>(say(StatusCode::OK, &heard))
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let router = Router::new();
        router
            .apply(RouteTable::from_targets(&renderable_routes(&[instance_record(
                |record| record.host_port = host_port,
            )])))
            .await;
        serving_as(router, arriving(None, secure)).await
    }

    #[tokio::test]
    async fn a_tenant_is_told_where_the_request_came_from_and_what_it_arrived_over() {
        let over_tls = echoing_the_forwarded_headers(true).await;
        let answered = asked(over_tls, "GET / HTTP/1.0\r\nHost: app-1.apps.example.com\r\n\r\n").await;
        assert!(
            answered.ends_with("for=203.0.113.7 proto=https host=app-1.apps.example.com"),
            "{answered}"
        );

        let in_the_clear = echoing_the_forwarded_headers(false).await;
        let answered = asked(
            in_the_clear,
            "GET / HTTP/1.0\r\nHost: app-1.apps.example.com\r\n\r\n",
        )
        .await;
        assert!(
            answered.ends_with("proto=http host=app-1.apps.example.com"),
            "{answered}"
        );
    }

    #[tokio::test]
    async fn what_an_edge_already_wrote_down_is_added_to_rather_than_written_over() {
        let port = echoing_the_forwarded_headers(true).await;
        let answered = asked(
            port,
            "GET / HTTP/1.0\r\nHost: app-1.apps.example.com\r\nx-forwarded-for: 198.51.100.9\r\nx-forwarded-proto: http\r\nx-forwarded-host: brought.example.com\r\n\r\n",
        )
        .await;
        assert!(
            answered.ends_with("for=198.51.100.9, 203.0.113.7 proto=http host=brought.example.com"),
            "the visitor's own leg outlives the hop this proxy made: {answered}"
        );
    }

    #[test]
    fn a_trust_pool_that_admits_nobody_is_refused_rather_than_a_listener_that_refuses_everyone() {
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("ca.pem");
        std::fs::write(&empty, b"").unwrap();
        let error = client_verifier(&empty).expect_err("an empty trust pool was accepted");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let absent = directory.path().join("absent.pem");
        assert_eq!(
            client_verifier(&absent)
                .expect_err("a trust pool this host does not have was accepted")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn two_apps_that_claim_the_same_hostname_do_not_both_get_the_traffic() {
        let mine = instance_record(|_| {});
        let theirs = instance_record(|record| {
            record.app_id = protocol::AppId::parse("app-2").unwrap();
            record.host_port = HostPort::new(mine.host_port.get() + 1).unwrap();
        });
        let table = RouteTable::from_targets(&renderable_routes(&[mine, theirs.clone()]));
        assert_eq!(table.hostnames().len(), 1);
        assert_eq!(
            table.port_for(app_hostname().hostname.as_str()),
            Some(theirs.host_port)
        );
    }
}
