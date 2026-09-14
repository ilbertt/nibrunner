use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use protocol::{HealthCheck, HttpPort, Ipv4Address};
use serde::{Deserialize, Serialize};

/// How a probe came back without finding the tenant well. Kept on the record, so that when the
/// check flips to unhealthy the sentence on it can say what the last probe ran into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProbeFailure {
    /// The port took no connection: nothing is listening on it.
    Refused,
    /// The connection could not be made, for a reason other than a closed port.
    Unreachable { error: String },
    /// The path answered, with anything but a success.
    Answered { status: u16 },
    /// The connection was taken and then dropped, or what came back was not HTTP.
    Dropped { error: String },
    /// Nothing came back inside the check's timeout.
    TimedOut { after_ms: u64 },
}

impl ProbeFailure {
    /// The clause that finishes "the last probe ...".
    pub fn describe(&self) -> String {
        match self {
            ProbeFailure::Refused => "tcp connect refused".to_string(),
            ProbeFailure::Unreachable { error } => format!("could not connect: {error}"),
            ProbeFailure::Answered { status } => {
                let reason = hyper::StatusCode::from_u16(*status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                    .map_or(String::new(), |reason| format!(" {reason}"));
                format!("answered {status}{reason}")
            }
            ProbeFailure::Dropped { error } => format!("connected but got no answer: {error}"),
            ProbeFailure::TimedOut { after_ms } => format!("timed out after {after_ms} ms"),
        }
    }

    fn connect(error: &std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::ConnectionRefused {
            return ProbeFailure::Refused;
        }
        ProbeFailure::Unreachable {
            error: error.to_string(),
        }
    }
}

pub async fn probe_instance(
    guest_ipv4: &Ipv4Address,
    http_port: HttpPort,
    health_check: &HealthCheck,
) -> Result<(), ProbeFailure> {
    let timeout = Duration::from_millis(health_check.probe().timeout_ms);
    let address = SocketAddr::from((guest_ipv4.addr(), http_port.get()));
    let attempt = match health_check {
        HealthCheck::Http { path, .. } => probe_http(address, path, timeout).await,
        // Whether the tenant is listening is all a boot-completed check asks; that it is asked
        // once, and never after the port has answered, is the caller's to keep.
        HealthCheck::Tcp { .. } | HealthCheck::BootCompleted => probe_tcp(address, timeout).await,
    };
    attempt.unwrap_or(Err(ProbeFailure::TimedOut {
        after_ms: health_check.probe().timeout_ms,
    }))
}

/// `None` is the timeout running out before the attempt came back either way.
async fn probe_tcp(address: SocketAddr, timeout: Duration) -> Option<Result<(), ProbeFailure>> {
    let connected = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address))
        .await
        .ok()?;
    Some(connected.map(drop).map_err(|error| ProbeFailure::connect(&error)))
}

async fn probe_http(address: SocketAddr, path: &str, timeout: Duration) -> Option<Result<(), ProbeFailure>> {
    use http_body_util::Empty;
    use hyper_util::rt::TokioIo;

    let attempt = async {
        let stream = tokio::net::TcpStream::connect(address)
            .await
            .map_err(|error| ProbeFailure::connect(&error))?;
        let dropped = |error: hyper::Error| ProbeFailure::Dropped {
            error: error.to_string(),
        };
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(dropped)?;
        let pump = tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = hyper::Request::builder()
            .uri(path)
            .header("host", address.to_string())
            .body(Empty::<bytes::Bytes>::new())
            .map_err(|error| ProbeFailure::Dropped {
                error: error.to_string(),
            })?;
        let response = sender.send_request(request).await.map_err(dropped)?;
        pump.abort();
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(ProbeFailure::Answered {
                status: status.as_u16(),
            })
        }
    };
    tokio::time::timeout(timeout, attempt).await.ok()
}

pub fn loopback() -> Ipv4Address {
    Ipv4Address::from(Ipv4Addr::LOCALHOST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TCP_HEALTH_CHECK;
    use protocol::Probe;

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
    async fn a_tcp_check_asks_only_whether_the_tenant_accepts_a_connection() {
        let port = listening(hyper::StatusCode::OK).await;
        assert_eq!(probe_instance(&loopback(), port, &TCP_HEALTH_CHECK).await, Ok(()));
        let closed = HttpPort::new(1).unwrap();
        assert_eq!(
            probe_instance(&loopback(), closed, &TCP_HEALTH_CHECK).await,
            Err(ProbeFailure::Refused)
        );
    }

    // Nothing is asked of a guest this host did not build about its health; whether its tenant
    // has bound the port yet is not about its health.
    #[tokio::test]
    async fn a_boot_completed_check_asks_whether_the_tenant_is_listening() {
        let closed = HttpPort::new(1).unwrap();
        assert_eq!(
            probe_instance(&loopback(), closed, &HealthCheck::BootCompleted).await,
            Err(ProbeFailure::Refused)
        );
        let listening = listening(hyper::StatusCode::INTERNAL_SERVER_ERROR).await;
        assert_eq!(
            probe_instance(&loopback(), listening, &HealthCheck::BootCompleted).await,
            Ok(()),
            "what the tenant answers is not asked, only that it is there"
        );
    }

    fn http(path: &str, timeout_ms: u64) -> HealthCheck {
        HealthCheck::Http {
            path: path.to_string(),
            probe: Probe {
                timeout_ms,
                ..TCP_HEALTH_CHECK.probe().clone()
            },
        }
    }

    #[tokio::test]
    async fn an_http_check_asks_its_path_and_takes_a_success_for_an_answer() {
        let with_path = http("/health", 2_000);
        let healthy = listening(hyper::StatusCode::OK).await;
        assert_eq!(probe_instance(&loopback(), healthy, &with_path).await, Ok(()));
        let unwell = listening(hyper::StatusCode::INTERNAL_SERVER_ERROR).await;
        assert_eq!(
            probe_instance(&loopback(), unwell, &with_path).await,
            Err(ProbeFailure::Answered { status: 500 })
        );
        let closed = HttpPort::new(1).unwrap();
        assert_eq!(
            probe_instance(&loopback(), closed, &with_path).await,
            Err(ProbeFailure::Refused),
            "a closed port is the same failure whichever way it is asked"
        );
    }

    #[tokio::test]
    async fn a_tenant_that_answers_a_path_with_anything_but_success_is_not_healthy() {
        let with_path = http("/health", 2_000);
        for answer in [
            hyper::StatusCode::NOT_FOUND,
            hyper::StatusCode::UNAUTHORIZED,
            hyper::StatusCode::BAD_GATEWAY,
            hyper::StatusCode::MOVED_PERMANENTLY,
        ] {
            let port = listening(answer).await;
            assert_eq!(
                probe_instance(&loopback(), port, &with_path).await,
                Err(ProbeFailure::Answered {
                    status: answer.as_u16()
                }),
                "{answer} was read as a healthy tenant"
            );
        }
        let accepted = listening(hyper::StatusCode::NO_CONTENT).await;
        assert_eq!(probe_instance(&loopback(), accepted, &with_path).await, Ok(()));
    }

    #[test]
    fn a_failure_says_what_the_probe_ran_into() {
        assert_eq!(ProbeFailure::Refused.describe(), "tcp connect refused");
        assert_eq!(
            ProbeFailure::Answered { status: 404 }.describe(),
            "answered 404 Not Found"
        );
        assert_eq!(
            ProbeFailure::Answered { status: 599 }.describe(),
            "answered 599",
            "a status with no name is still a status"
        );
        assert_eq!(
            ProbeFailure::TimedOut { after_ms: 2_000 }.describe(),
            "timed out after 2000 ms"
        );
        assert_eq!(
            ProbeFailure::Unreachable {
                error: "No route to host (os error 65)".into()
            }
            .describe(),
            "could not connect: No route to host (os error 65)"
        );
        assert_eq!(
            ProbeFailure::Dropped {
                error: "connection closed before message completed".into()
            }
            .describe(),
            "connected but got no answer: connection closed before message completed"
        );
    }

    #[test]
    fn a_failure_round_trips_through_the_record_it_is_kept_on() {
        for failure in [
            ProbeFailure::Refused,
            ProbeFailure::Answered { status: 404 },
            ProbeFailure::TimedOut { after_ms: 2_000 },
        ] {
            let written = serde_json::to_value(&failure).unwrap();
            assert_eq!(serde_json::from_value::<ProbeFailure>(written).unwrap(), failure);
        }
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

        let silent = http("/health", 100);
        assert_eq!(
            probe_instance(&loopback(), port, &silent).await,
            Err(ProbeFailure::TimedOut { after_ms: 100 })
        );
        assert_eq!(probe_instance(&loopback(), port, &TCP_HEALTH_CHECK).await, Ok(()));
        held.abort();
    }
}
