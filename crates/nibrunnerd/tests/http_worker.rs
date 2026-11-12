#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::net::Ipv4Addr;
use std::process::Stdio;
use std::time::Duration;

use nibrunnerd::adapters::proxy::worker::HttpWorkerClient;
use nibrunnerd::test_support::{app_hostname, app_id, http_worker};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[tokio::test]
async fn the_worker_process_finishes_a_response_after_its_control_client_is_dropped() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let config = http_worker::configuration(directory.path(), address);
        let path = config.runtime_dir.join("http/process.sock");
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_nibrunnerd"))
            .args(["http-worker", "--socket"])
            .arg(&path)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(config.to_toml().as_bytes()).await.unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        let client = HttpWorkerClient::new(&path);
        while client.active().await.is_err() {
            assert!(child.try_wait().unwrap().is_none(), "the worker could not start");
            tokio::task::yield_now().await;
        }
        let guest = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        client
            .apply(http_worker::routes(guest.local_addr().unwrap().port()))
            .await
            .unwrap();
        client.attach(listener).await.unwrap();
        let (finish, finished) = oneshot::channel();
        let guest = tokio::spawn(async move {
            let (mut stream, _) = guest.accept().await.unwrap();
            http_worker::read_headers(&mut stream).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nbefore")
                .await
                .unwrap();
            finished.await.unwrap();
            stream.write_all(b"after").await.unwrap();
        });
        let mut request = TcpStream::connect(address).await.unwrap();
        request
            .write_all(
                format!(
                    "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    app_hostname().hostname
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert!(http_worker::read_headers(&mut request)
            .await
            .unwrap()
            .starts_with(b"HTTP/1.1 200"));
        let mut first = [0; 6];
        request.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"before");
        drop(client);

        let replacement = HttpWorkerClient::new(&path);
        assert_eq!(replacement.active().await.unwrap().get(&app_id()), Some(&1));
        replacement.drain().await.unwrap();
        assert!(child.try_wait().unwrap().is_none());
        finish.send(()).unwrap();
        let mut remaining = Vec::new();
        request.read_to_end(&mut remaining).await.unwrap();
        assert_eq!(remaining, b"after");
        assert!(child.wait().await.unwrap().success());
        guest.await.unwrap();
        assert!(!path.exists());
    })
    .await
    .expect("a standalone HTTP worker owns its listener and unfinished requests");
}
