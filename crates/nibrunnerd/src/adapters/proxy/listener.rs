use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::sync::Arc;

use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::adapters::proxy::router::{without_nagle, Arrival, Router, GREETING_TIMEOUT};
use crate::domain::metrics::proxy::Handshake;

pub struct HttpListener {
    listener: TcpListener,
    router: Arc<Router>,
    tls: Option<tokio_rustls::TlsAcceptor>,
}

impl HttpListener {
    pub async fn bind(
        router: Arc<Router>,
        address: SocketAddr,
        tls: Option<tokio_rustls::TlsAcceptor>,
    ) -> std::io::Result<Self> {
        Self::from_listener(router, TcpListener::bind(address).await?, tls)
    }

    pub fn from_listener(
        router: Arc<Router>,
        listener: TcpListener,
        tls: Option<tokio_rustls::TlsAcceptor>,
    ) -> std::io::Result<Self> {
        tracing::info!(address = %listener.local_addr()?, secure = tls.is_some(), "the proxy is listening");
        Ok(Self {
            listener,
            router,
            tls,
        })
    }

    pub fn duplicate_listener(&self) -> std::io::Result<std::net::TcpListener> {
        Ok(self.listener.as_fd().try_clone_to_owned()?.into())
    }

    pub fn send_listener(&self, channel: &std::os::unix::net::UnixStream) -> std::io::Result<()> {
        super::listener_handoff::send(channel, &self.duplicate_listener()?)
    }

    pub fn receive_listener(
        channel: &std::os::unix::net::UnixStream,
    ) -> std::io::Result<std::net::TcpListener> {
        super::listener_handoff::receive(channel)
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn serve(self, stop_accepting: impl Future<Output = ()>) -> std::io::Result<()> {
        let (drain, draining) = watch::channel(false);
        let mut connections = JoinSet::new();
        tokio::pin!(stop_accepting);
        loop {
            tokio::select! {
                biased;
                () = &mut stop_accepting => break,
                _ = connections.join_next(), if !connections.is_empty() => {},
                accepted = self.listener.accept() => {
                    let Ok((stream, peer)) = accepted else { continue; };
                    let router = self.router.clone();
                    let tls = self.tls.clone();
                    let draining = draining.clone();
                    connections.spawn(serve_connection(stream, peer, router, tls, draining));
                }
            }
        }
        drop(self.listener);
        let _ = drain.send(true);
        while connections.join_next().await.is_some() {}
        // Upgraded connections leave hyper's connection future; their request guard lives
        // until the bidirectional copy finishes, which may be long after the HTTP handshake.
        self.router.metrics.proxy.wait_for_requests().await;
        Ok(())
    }
}

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    router: Arc<Router>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    mut draining: watch::Receiver<bool>,
) {
    // Nagle delays a response body behind the header's ACK on the client-facing leg.
    let stream = without_nagle(stream);
    let Some(tls) = tls else {
        let service = service_fn(move |request| {
            let router = router.clone();
            async move {
                Ok::<_, Infallible>(
                    router
                        .handle(
                            request,
                            Arrival {
                                server_name: None,
                                peer: peer.ip(),
                                secure: false,
                            },
                        )
                        .await,
                )
            }
        });
        let mut builder = hyper::server::conn::http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(GREETING_TIMEOUT);
        let connection = builder
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        tokio::pin!(connection);
        tokio::select! {
            _ = &mut connection => return,
            _ = draining.wait_for(|draining| *draining) => connection.as_mut().graceful_shutdown(),
        }
        let _ = connection.await;
        return;
    };
    let handshake = tokio::time::timeout(GREETING_TIMEOUT, tls.accept(stream));
    let stream = tokio::select! {
        _ = draining.wait_for(|draining| *draining) => return,
        result = handshake => match result {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) => { router.metrics.proxy.handshake(Handshake::Failed); return; },
            Err(_) => { router.metrics.proxy.handshake(Handshake::TimedOut); return; },
        },
    };
    router.metrics.proxy.handshake(Handshake::Completed);
    let arrival = Arrival {
        server_name: stream.get_ref().1.server_name().map(Arc::from),
        peer: peer.ip(),
        secure: true,
    };
    let service = service_fn(move |request| {
        let router = router.clone();
        let arrival = arrival.clone();
        async move { Ok::<_, Infallible>(router.handle(request, arrival).await) }
    });
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(GREETING_TIMEOUT);
    let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        _ = &mut connection => return,
        _ = draining.wait_for(|draining| *draining) => connection.as_mut().graceful_shutdown(),
    }
    let _ = connection.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use crate::adapters::proxy::RouteTable;
    use crate::domain::metrics::HostMetrics;
    use crate::domain::report::routes::renderable_routes;
    use crate::test_support::{app_id, instance_record};

    async fn header(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(stream.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }

    async fn router(upstream: SocketAddr) -> (Arc<Router>, Arc<HostMetrics>) {
        let metrics = Arc::new(HostMetrics::new());
        let router = Router::new(metrics.clone(), None);
        router
            .apply(RouteTable::from_targets(&renderable_routes(&[instance_record(
                |record| {
                    record.host_port = Some(protocol::HostPort::new(upstream.port()).unwrap());
                },
            )])))
            .await;
        (router, metrics)
    }

    async fn proxy(upstream: SocketAddr) -> (HttpListener, Arc<HostMetrics>) {
        let (router, metrics) = router(upstream).await;
        let listener = HttpListener::bind(router, SocketAddr::from(([127, 0, 0, 1], 0)), None)
            .await
            .unwrap();
        (listener, metrics)
    }

    async fn wait_for_listener_to_close(address: SocketAddr) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while TcpStream::connect(address).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_replacement_accepts_queued_connections_while_the_old_response_keeps_streaming() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let old_upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let new_upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let (old, _) = proxy(old_upstream.local_addr().unwrap()).await;
            let address = old.local_addr().unwrap();
            let shared = old.duplicate_listener().unwrap();
            let (new_router, _) = router(new_upstream.local_addr().unwrap()).await;
            let replacement =
                HttpListener::from_listener(new_router, TcpListener::from_std(shared).unwrap(), None)
                    .unwrap();
            assert_eq!(replacement.local_addr().unwrap(), address);
            let (finish, finished) = oneshot::channel();
            let old_tenant = tokio::spawn(async move {
                let (mut stream, _) = old_upstream.accept().await.unwrap();
                header(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello")
                    .await
                    .unwrap();
                finished.await.unwrap();
                stream.write_all(b"world").await.unwrap();
            });
            let new_tenant = tokio::spawn(async move {
                let (mut stream, _) = new_upstream.accept().await.unwrap();
                header(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nnew")
                    .await
                    .unwrap();
            });
            let (stop, stopped) = oneshot::channel();
            let (accepted_stop, accepting_stopped) = oneshot::channel();
            let old_server = tokio::spawn(old.serve(async {
                let _ = stopped.await;
                accepted_stop.send(()).unwrap();
            }));
            let mut existing = TcpStream::connect(address).await.unwrap();
            existing
                .write_all(b"GET / HTTP/1.1\r\nHost: app-1.apps.example.com\r\n\r\n")
                .await
                .unwrap();
            assert!(header(&mut existing).await.starts_with("HTTP/1.1 200"));
            let mut first = [0; 5];
            existing.read_exact(&mut first).await.unwrap();
            assert_eq!(&first, b"hello");
            stop.send(()).unwrap();
            accepting_stopped.await.unwrap();
            let mut queued = TcpStream::connect(address).await.unwrap();
            queued
                .write_all(b"GET / HTTP/1.1\r\nHost: app-1.apps.example.com\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let (stop_new, stopped_new) = oneshot::channel();
            let new_server = tokio::spawn(replacement.serve(async {
                let _ = stopped_new.await;
            }));
            assert!(header(&mut queued).await.starts_with("HTTP/1.1 200"));
            let mut answer = Vec::new();
            queued.read_to_end(&mut answer).await.unwrap();
            assert_eq!(answer, b"new");
            assert!(!old_server.is_finished());
            finish.send(()).unwrap();
            let mut tail = Vec::new();
            existing.read_to_end(&mut tail).await.unwrap();
            assert_eq!(tail, b"world");
            old_server.await.unwrap().unwrap();
            stop_new.send(()).unwrap();
            new_server.await.unwrap().unwrap();
            old_tenant.await.unwrap();
            new_tenant.await.unwrap();
        })
        .await
        .expect("a shared listening socket preserves queued and accepted requests during handoff");
    }

    #[tokio::test]
    async fn draining_stops_accepting_while_a_streaming_response_finishes() {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (listener, metrics) = proxy(upstream.local_addr().unwrap()).await;
        let address = listener.local_addr().unwrap();
        let (finish, finished) = oneshot::channel();
        let tenant = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            header(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello")
                .await
                .unwrap();
            finished.await.unwrap();
            stream.write_all(b"world").await.unwrap();
        });
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(listener.serve(async {
            let _ = stopped.await;
        }));
        let mut visitor = TcpStream::connect(address).await.unwrap();
        visitor
            .write_all(b"GET / HTTP/1.1\r\nHost: app-1.apps.example.com\r\n\r\n")
            .await
            .unwrap();
        assert!(header(&mut visitor).await.starts_with("HTTP/1.1 200"));
        let mut first = [0; 5];
        visitor.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"hello");
        stop.send(()).unwrap();
        wait_for_listener_to_close(address).await;
        assert!(!server.is_finished());
        assert_eq!(metrics.proxy.open_requests_for(&app_id()), 1);
        finish.send(()).unwrap();
        let mut rest = Vec::new();
        visitor.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"world");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tenant.await.unwrap();
        assert!(metrics.proxy.open_requests().is_empty());
    }

    #[tokio::test]
    async fn draining_finishes_an_http2_response_over_tls() {
        crate::install_crypto_provider();
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (mut listener, _) = proxy(upstream.local_addr().unwrap()).await;
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/test_support/jwt");
        listener.tls = Some(
            crate::adapters::proxy::router::tls_acceptor(
                &fixtures.join("jwks-server-cert.pem"),
                &fixtures.join("jwks-server-key.pem"),
                None,
            )
            .unwrap(),
        );
        let address = listener.local_addr().unwrap();
        let (finish, finished) = oneshot::channel();
        let tenant = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            header(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello")
                .await
                .unwrap();
            finished.await.unwrap();
            stream.write_all(b"world").await.unwrap();
        });
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(listener.serve(async {
            let _ = stopped.await;
        }));
        let client = reqwest::Client::builder()
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .resolve("app-1.apps.example.com", address)
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let mut response = client
            .get(format!("https://app-1.apps.example.com:{}/", address.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), hyper::Version::HTTP_2);
        assert_eq!(response.chunk().await.unwrap().unwrap(), b"hello"[..]);
        stop.send(()).unwrap();
        wait_for_listener_to_close(address).await;
        assert!(!server.is_finished());
        finish.send(()).unwrap();
        assert_eq!(response.bytes().await.unwrap(), b"world"[..]);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tenant.await.unwrap();
    }

    #[tokio::test]
    async fn draining_finishes_an_upload_once_without_replaying_it() {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (listener, _) = proxy(upstream.local_addr().unwrap()).await;
        let address = listener.local_addr().unwrap();
        let (began, begun) = oneshot::channel();
        let tenant = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            assert!(header(&mut stream).await.starts_with("POST /"));
            let mut body = [0; 10];
            stream.read_exact(&mut body[..5]).await.unwrap();
            began.send(()).unwrap();
            stream.read_exact(&mut body[5..]).await.unwrap();
            assert_eq!(&body, b"helloworld");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            assert_eq!(
                upstream.into_std().unwrap().accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(listener.serve(async {
            let _ = stopped.await;
        }));
        let mut visitor = TcpStream::connect(address).await.unwrap();
        visitor
            .write_all(b"POST / HTTP/1.1\r\nHost: app-1.apps.example.com\r\nContent-Length: 10\r\n\r\nhello")
            .await
            .unwrap();
        begun.await.unwrap();
        stop.send(()).unwrap();
        wait_for_listener_to_close(address).await;
        visitor.write_all(b"world").await.unwrap();
        assert!(header(&mut visitor).await.starts_with("HTTP/1.1 200"));
        let mut body = Vec::new();
        visitor.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"ok");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tenant.await.unwrap();
    }

    #[tokio::test]
    async fn draining_keeps_an_upgraded_connection_alive_until_its_peer_closes() {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (listener, metrics) = proxy(upstream.local_addr().unwrap()).await;
        let address = listener.local_addr().unwrap();
        let tenant = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            header(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                )
                .await
                .unwrap();
            let mut data = [0; 5];
            stream.read_exact(&mut data).await.unwrap();
            stream.write_all(&data).await.unwrap();
            let mut end = [0; 1];
            assert_eq!(stream.read(&mut end).await.unwrap(), 0);
        });
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(listener.serve(async {
            let _ = stopped.await;
        }));
        let mut visitor = TcpStream::connect(address).await.unwrap();
        visitor.write_all(b"GET / HTTP/1.1\r\nHost: app-1.apps.example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
        assert!(header(&mut visitor).await.starts_with("HTTP/1.1 101"));
        stop.send(()).unwrap();
        wait_for_listener_to_close(address).await;
        assert!(!server.is_finished());
        assert_eq!(metrics.proxy.open_requests_for(&app_id()), 1);
        visitor.write_all(b"hello").await.unwrap();
        let mut echoed = [0; 5];
        visitor.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        drop(visitor);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tenant.await.unwrap();
    }
}
