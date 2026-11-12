use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;
use crate::test_support::{app_hostname, app_id, http_worker};

async fn worker(root: &Path, name: &str) -> (HttpWorkerClient, tokio::task::JoinHandle<io::Result<()>>) {
    let config = http_worker::configuration(root, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let path = config.runtime_dir.join(format!("http/{name}.sock"));
    let client = HttpWorkerClient::new(&path);
    let serving = tokio::spawn(serve(config, path));
    tokio::time::timeout(Duration::from_secs(3), async {
        while client.active().await.is_err() {
            assert!(
                !serving.is_finished(),
                "the worker exited before its socket was ready"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    (client, serving)
}

async fn route(client: &HttpWorkerClient, port: u16) {
    client.apply(http_worker::routes(port)).await.unwrap();
}

async fn headers(stream: &mut TcpStream) -> String {
    String::from_utf8(http_worker::read_headers(stream).await.unwrap()).unwrap()
}

async fn request(address: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                app_hostname().hostname
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream
}

#[tokio::test]
async fn a_replacement_accepts_requests_while_the_old_worker_finishes_its_response() {
    tokio::time::timeout(Duration::from_secs(10), async {
        crate::install_crypto_provider();
        let directory = tempfile::tempdir().unwrap();
        let (old, old_worker) = worker(directory.path(), "old").await;
        let old_guest = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        route(&old, old_guest.local_addr().unwrap().port()).await;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        old.attach(listener).await.unwrap();
        let (finish, finished) = oneshot::channel();
        let old_guest = tokio::spawn(async move {
            let (mut stream, _) = old_guest.accept().await.unwrap();
            headers(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nbefore")
                .await
                .unwrap();
            finished.await.unwrap();
            stream.write_all(b"after").await.unwrap();
        });
        let mut existing = request(address).await;
        assert!(headers(&mut existing).await.starts_with("HTTP/1.1 200"));
        let mut first = [0; 6];
        existing.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"before");
        assert_eq!(old.active().await.unwrap().get(&app_id()), Some(&1));

        let shared = old.listener().await.unwrap();
        let (replacement, new_worker) = worker(directory.path(), "replacement").await;
        let new_guest = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        route(&replacement, new_guest.local_addr().unwrap().port()).await;
        replacement.attach(shared).await.unwrap();
        old.drain().await.unwrap();
        let new_guest = tokio::spawn(async move {
            let (mut stream, _) = new_guest.accept().await.unwrap();
            headers(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nnew")
                .await
                .unwrap();
        });
        let mut next = request(address).await;
        assert!(headers(&mut next).await.starts_with("HTTP/1.1 200"));
        let mut body = Vec::new();
        next.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"new");
        assert!(!old_worker.is_finished());
        finish.send(()).unwrap();
        let mut tail = Vec::new();
        existing.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, b"after");
        old_worker.await.unwrap().unwrap();
        replacement.drain().await.unwrap();
        new_worker.await.unwrap().unwrap();
        old_guest.await.unwrap();
        new_guest.await.unwrap();
    })
    .await
    .expect("worker handoff preserves an old response while serving the replacement's routes");
}

#[tokio::test]
async fn an_unfinished_control_request_does_not_block_another_controller() {
    let directory = tempfile::tempdir().unwrap();
    let (client, serving) = worker(directory.path(), "control").await;
    let path = directory.path().join("runtime/http/control.sock");
    let mut stalled = UnixStream::connect(path).await.unwrap();
    stalled.write_all(b"{").await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), client.active())
        .await
        .unwrap()
        .unwrap()
        .is_empty());
    drop(stalled);
    client.drain().await.unwrap();
    serving.await.unwrap().unwrap();
}
