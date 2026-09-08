use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioIo;
use protocol::HostPort;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::adapters::proxy::forward::{forward, hostname_of, say, ProxyBody};
use crate::domain::report::routes::RouteTarget;

const LOOPBACK: &str = "127.0.0.1";

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
        server_name: Option<Arc<str>>,
    ) -> Response<ProxyBody> {
        let Some(hostname) = hostname_of(&request) else {
            return say(StatusCode::BAD_REQUEST, "This request names no host.\n");
        };
        // One certificate covers every app on this host, so the name in the handshake is the only
        // thing stopping a connection opened for one tenant from asking for another tenant's app.
        if server_name
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
        forward(&self.client, request, LOOPBACK, port.get(), true).await
    }
}

pub async fn serve_http(router: Arc<Router>, address: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "the proxy is listening");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let router = router.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let router = router.clone();
                async move { Ok::<_, std::convert::Infallible>(router.handle(request, None).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
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
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let router = router.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(stream).await else {
                return;
            };
            let server_name: Option<Arc<str>> = stream.get_ref().1.server_name().map(Arc::from);
            let service = service_fn(move |request| {
                let router = router.clone();
                let server_name = server_name.clone();
                async move { Ok::<_, std::convert::Infallible>(router.handle(request, server_name).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
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
    let config = builder
        .with_single_cert(certificates, private_key)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    async fn serving(router: Arc<Router>) -> u16 {
        serving_as(router, None).await
    }

    async fn serving_as(router: Arc<Router>, server_name: Option<&'static str>) -> u16 {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let router = router.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let router = router.clone();
                        let server_name = server_name.map(Arc::from);
                        async move { Ok::<_, std::convert::Infallible>(router.handle(request, server_name).await) }
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
        serving_as(router, server_name).await
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
