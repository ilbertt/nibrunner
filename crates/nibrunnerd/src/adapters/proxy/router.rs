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
use crate::services::report::routes::RouteTarget;

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

    pub async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<ProxyBody> {
        let Some(hostname) = hostname_of(&request) else {
            return say(StatusCode::BAD_REQUEST, "This request names no host.\n");
        };
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
                async move { Ok::<_, std::convert::Infallible>(router.handle(request).await) }
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
) -> std::io::Result<()> {
    let acceptor = tls_acceptor(certificate, key)?;
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "the proxy is listening for TLS");
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
            let service = service_fn(move |request| {
                let router = router.clone();
                async move { Ok::<_, std::convert::Infallible>(router.handle(request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

pub fn tls_acceptor(certificate: &Path, key: &Path) -> std::io::Result<tokio_rustls::TlsAcceptor> {
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
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::report::routes::renderable_routes;
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
}
