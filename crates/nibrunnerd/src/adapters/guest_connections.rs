use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{header, Method, Request, Response, StatusCode};
use protocol::{AppId, DeploymentId};

use crate::domain::guest_connections::{connect, GuestConnectionError};
use crate::host::Host;

struct Requested {
    app_id: AppId,
    deployment_id: DeploymentId,
    port: u32,
}

fn requested<B>(request: &Request<B>) -> Result<Requested, StatusCode> {
    if request.method() != Method::CONNECT {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    if request.uri().scheme().is_some() || request.uri().path_and_query().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let authority = request.uri().authority().ok_or(StatusCode::BAD_REQUEST)?;
    let app_id = AppId::parse(authority.host()).map_err(|_| StatusCode::BAD_REQUEST)?;
    let port = authority
        .port()
        .and_then(|port| port.as_str().parse::<u32>().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let matched = request
        .headers()
        .get(header::IF_MATCH)
        .ok_or(StatusCode::PRECONDITION_REQUIRED)?;
    let named = matched
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'))
        .ok_or(StatusCode::BAD_REQUEST)?;
    let deployment_id = DeploymentId::parse(named).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Requested {
        app_id,
        deployment_id,
        port,
    })
}

fn response(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .expect("a status with an empty body is a response")
}

fn refused(error: &GuestConnectionError) -> StatusCode {
    match error {
        GuestConnectionError::DeploymentChanged => StatusCode::PRECONDITION_FAILED,
        GuestConnectionError::Unavailable => StatusCode::CONFLICT,
        GuestConnectionError::WakeFailed | GuestConnectionError::Unreachable => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        GuestConnectionError::TimedOut => StatusCode::GATEWAY_TIMEOUT,
    }
}

async fn answer(host: Arc<Host>, mut request: Request<Incoming>) -> Response<Full<Bytes>> {
    let wanted = match requested(&request) {
        Ok(wanted) => wanted,
        Err(status) => return response(status),
    };
    let mut guest = match connect(&host, &wanted.app_id, &wanted.deployment_id, wanted.port).await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(app_id = %wanted.app_id, %error, "guest service connection refused");
            return response(refused(&error));
        }
    };
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        if let Ok(upgraded) = upgraded.await {
            let mut client = hyper_util::rt::TokioIo::new(upgraded);
            let _ = tokio::io::copy_bidirectional(&mut client, guest.stream()).await;
        }
        host.state
            .mark_active(&wanted.app_id, crate::clock::now_ms())
            .await;
    });
    response(StatusCode::OK)
}

pub async fn serve(host: &Arc<Host>, path: &Path) -> io::Result<tokio::task::JoinHandle<()>> {
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let listener = crate::unix_socket::bind_private(path).await?;
    let host = host.clone();
    Ok(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let host = host.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let host = host.clone();
                    async move { Ok::<_, std::convert::Infallible>(answer(host, request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use hyper::Uri;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    const PORT: u32 = 51099;

    fn request() -> Request<()> {
        Request::builder()
            .method(Method::CONNECT)
            .uri(
                Uri::builder()
                    .authority(format!("{}:{PORT}", app_id()))
                    .build()
                    .unwrap(),
            )
            .header(header::IF_MATCH, format!("\"{}\"", deployment_id()))
            .body(())
            .unwrap()
    }

    #[test]
    fn a_tunnel_names_one_app_port_and_exact_deployment() {
        let asked = requested(&request()).unwrap();
        assert_eq!(asked.app_id, app_id());
        assert_eq!(asked.deployment_id, deployment_id());
        assert_eq!(asked.port, PORT);
        let mut missing = request();
        missing.headers_mut().remove(header::IF_MATCH);
        assert!(matches!(
            requested(&missing),
            Err(StatusCode::PRECONDITION_REQUIRED)
        ));
        let mut wildcard = request();
        wildcard
            .headers_mut()
            .insert(header::IF_MATCH, "*".parse().unwrap());
        assert!(matches!(requested(&wildcard), Err(StatusCode::BAD_REQUEST)));
        let mut wrong_method = request();
        *wrong_method.method_mut() = Method::POST;
        assert!(matches!(
            requested(&wrong_method),
            Err(StatusCode::METHOD_NOT_ALLOWED)
        ));
    }

    #[tokio::test]
    async fn a_local_tunnel_forwards_bytes_and_releases_activity_after_disconnect() {
        let (host, directory, listener) = crate::test_support::guest_connections::host().await;
        let path = directory.path().join("connections.sock");
        let server = serve(host.arc(), &path).await.unwrap();
        let guest = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = BufReader::new(stream);
            wire.read_line(&mut String::new()).await.unwrap();
            wire.write_all(b"OK 1234\nwelcome").await.unwrap();
            let mut message = [0; 5];
            wire.read_exact(&mut message).await.unwrap();
            assert_eq!(&message, b"hello");
            wire.write_all(b"reply").await.unwrap();
        });
        let mut client = UnixStream::connect(&path).await.unwrap();
        client
            .write_all(
                format!(
                    "CONNECT {}:{PORT} HTTP/1.1\r\nHost: localhost\r\nIf-Match: \"{}\"\r\n\r\n",
                    app_id(),
                    deployment_id()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut wire = BufReader::new(client);
        let mut status = String::new();
        wire.read_line(&mut status).await.unwrap();
        assert!(status.contains(" 200 "), "{status}");
        loop {
            let mut line = String::new();
            wire.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            assert!(!line.to_lowercase().starts_with("content-length:"));
            assert!(!line.to_lowercase().starts_with("transfer-encoding:"));
        }
        let mut welcome = [0; 7];
        wire.read_exact(&mut welcome).await.unwrap();
        assert_eq!(&welcome, b"welcome");
        assert!(host.state.has_guest_activity(&app_id()));
        wire.write_all(b"hello").await.unwrap();
        let mut reply = [0; 5];
        wire.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");
        drop(wire);
        guest.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while host.state.has_guest_activity(&app_id()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }
}
