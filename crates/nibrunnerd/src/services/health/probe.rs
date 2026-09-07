use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use protocol::{HealthCheck, HttpPort, Ipv4Address};

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

async fn probe_tcp(address: SocketAddr, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await,
        Ok(Ok(_))
    )
}

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

    #[tokio::test]
    async fn the_default_probe_asks_only_whether_the_tenant_accepts_a_connection() {
        let port = listening(hyper::StatusCode::OK).await;
        assert!(probe_instance(&loopback(), port, &DEFAULT_HEALTH_CHECK).await);
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
        let unwell = listening(hyper::StatusCode::INTERNAL_SERVER_ERROR).await;
        assert!(!probe_instance(&loopback(), unwell, &with_path).await);
    }

    #[tokio::test]
    async fn a_tenant_that_answers_a_path_with_anything_but_success_is_not_healthy() {
        let with_path = HealthCheck {
            path: Some("/health".into()),
            ..DEFAULT_HEALTH_CHECK
        };
        for answer in [
            hyper::StatusCode::NOT_FOUND,
            hyper::StatusCode::UNAUTHORIZED,
            hyper::StatusCode::BAD_GATEWAY,
            hyper::StatusCode::MOVED_PERMANENTLY,
        ] {
            let port = listening(answer).await;
            assert!(
                !probe_instance(&loopback(), port, &with_path).await,
                "{answer} was read as a healthy tenant"
            );
        }
        let accepted = listening(hyper::StatusCode::NO_CONTENT).await;
        assert!(probe_instance(&loopback(), accepted, &with_path).await);
    }

    #[tokio::test]
    async fn a_tenant_that_takes_the_connection_and_says_nothing_runs_out_of_time() {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = HttpPort::new(listener.local_addr().unwrap().port()).unwrap();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                open.push(stream);
            }
        });

        let silent = HealthCheck {
            path: Some("/health".into()),
            timeout_ms: 100,
            ..DEFAULT_HEALTH_CHECK
        };
        assert!(!probe_instance(&loopback(), port, &silent).await);
        assert!(probe_instance(&loopback(), port, &DEFAULT_HEALTH_CHECK).await);
        held.abort();
    }
}
