//! Whether the tenant is accepting connections.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use protocol::{HealthCheck, HttpPort, Ipv4Address};

/// Straight to the guest address rather than through the forwarded host port, so a failure means
/// the tenant is down and never that a NAT rule is missing.
pub async fn probe_instance(
    guest_ipv4: &Ipv4Address,
    http_port: HttpPort,
    health_check: &HealthCheck,
) -> bool {
    let timeout = Duration::from_millis(health_check.timeout_ms);
    let address = SocketAddr::from((guest_ipv4.addr(), http_port.get()));
    match &health_check.path {
        None => probe_tcp(address, timeout).await,
        Some(path) => probe_http(address, path, timeout).await,
    }
}

/// That the connection opened at all is the whole question; nothing the tenant sends is read.
///
/// The deadline is the moment this stops waiting rather than the moment the *socket* gives up: an
/// address nothing answers on is otherwise bounded by the kernel's connect timeout, and a guest
/// whose tap has gone is exactly that address.
async fn probe_tcp(address: SocketAddr, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await,
        Ok(Ok(_))
    )
}

/// A declared path upgrades the probe to an HTTP GET that must answer 2xx.
async fn probe_http(address: SocketAddr, path: &str, timeout: Duration) -> bool {
    use http_body_util::Empty;
    use hyper_util::rt::TokioIo;

    let attempt = async {
        let stream = tokio::net::TcpStream::connect(address).await.ok()?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        let pump = tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = hyper::Request::builder()
            .uri(path)
            .header("host", address.to_string())
            .body(Empty::<bytes::Bytes>::new())
            .ok()?;
        let response = sender.send_request(request).await.ok()?;
        pump.abort();
        Some(response.status().is_success())
    };
    matches!(tokio::time::timeout(timeout, attempt).await, Ok(Some(true)))
}

/// The loopback, for a probe whose target is this machine rather than a guest.
pub fn loopback() -> Ipv4Address {
    Ipv4Address::from(Ipv4Addr::LOCALHOST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::DEFAULT_HEALTH_CHECK;

    async fn listening(answer: hyper::StatusCode) -> HttpPort {
        use http_body_util::Full;
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |_request| async move {
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(answer)
                                .body(Full::new(bytes::Bytes::from_static(b"ok")))
                                .unwrap(),
                        )
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        HttpPort::new(port).unwrap()
    }

    /// Against a real socket rather than a stub: what this asks of the platform is exactly the
    /// part that cannot be checked by asserting a call shape.
    #[tokio::test]
    async fn the_default_probe_asks_only_whether_the_tenant_accepts_a_connection() {
        let port = listening(hyper::StatusCode::OK).await;
        assert!(probe_instance(&loopback(), port, &DEFAULT_HEALTH_CHECK).await);
        // Nothing listens here, so a probe against it must fail rather than hang.
        let closed = HttpPort::new(1).unwrap();
        assert!(!probe_instance(&loopback(), closed, &DEFAULT_HEALTH_CHECK).await);
    }

    #[tokio::test]
    async fn a_declared_path_upgrades_the_probe_to_an_http_get() {
        let with_path = HealthCheck {
            path: Some("/health".into()),
            ..DEFAULT_HEALTH_CHECK
        };
        let healthy = listening(hyper::StatusCode::OK).await;
        assert!(probe_instance(&loopback(), healthy, &with_path).await);
        // A listening tenant answering 500 is up and not well, which is not healthy.
        let unwell = listening(hyper::StatusCode::INTERNAL_SERVER_ERROR).await;
        assert!(!probe_instance(&loopback(), unwell, &with_path).await);
    }
}
