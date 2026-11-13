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

#[tokio::test]
#[ignore = "subprocess fixture; the daemon restart test invokes this"]
async fn a_daemon_process_owns_the_http_proxy_controller() {
    let path = std::path::PathBuf::from(std::env::var_os("NIBRUNNER_HTTP_FIXTURE").unwrap());
    let config = nibrunnerd::config::HostConfig::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let port = std::env::var("NIBRUNNER_HTTP_GUEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let proxy = nibrunnerd::adapters::proxy::worker::process::DetachedHttpProxy::new(
        &config,
        env!("CARGO_BIN_EXE_nibrunnerd").into(),
    );
    let mut host = nibrunnerd::test_support::test_host().await;
    std::sync::Arc::get_mut(&mut host.host).unwrap().http_proxy = Some(std::sync::Arc::new(proxy));
    host.state
        .put_record(nibrunnerd::test_support::instance_record(|record| {
            record.host_port = Some(protocol::HostPort::new(port).unwrap());
        }))
        .await;
    nibrunnerd::run::serve_proxy(host.arc()).await.unwrap();
    std::fs::write(path.with_extension("ready"), "ready").unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn killing_the_daemon_controller_keeps_the_listener_and_its_response_for_the_replacement() {
    use nibrunnerd::adapters::proxy::worker::process::DetachedHttpProxy;
    tokio::time::timeout(Duration::from_secs(20), async {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let config = http_worker::configuration(directory.path(), address);
        let path = directory.path().join("fixture.toml");
        std::fs::write(&path, config.to_toml()).unwrap();
        let guest = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut owner = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_daemon_process_owns_the_http_proxy_controller",
                "--ignored",
                "--nocapture",
            ])
            .env("NIBRUNNER_HTTP_FIXTURE", &path)
            .env(
                "NIBRUNNER_HTTP_GUEST_PORT",
                guest.local_addr().unwrap().port().to_string(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        while !path.with_extension("ready").exists() {
            assert!(owner.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let replacement = DetachedHttpProxy::new(&config, env!("CARGO_BIN_EXE_nibrunnerd").into());
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
        http_worker::read_headers(&mut request).await.unwrap();
        let mut first = [0; 6];
        request.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"before");
        owner.kill().await.unwrap();
        owner.wait().await.unwrap();
        drop(TcpStream::connect(address).await.unwrap());
        let new_guest = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        replacement
            .apply(http_worker::routes(new_guest.local_addr().unwrap().port()))
            .await
            .unwrap();
        assert_eq!(
            replacement
                .metrics()
                .await
                .unwrap()
                .iter()
                .map(|sample| sample.apps.get(&app_id()).map_or(0, |app| app.open))
                .sum::<u64>(),
            1
        );
        let new_guest = tokio::spawn(async move {
            let (mut stream, _) = new_guest.accept().await.unwrap();
            http_worker::read_headers(&mut stream).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nnew")
                .await
                .unwrap();
        });
        let mut next = TcpStream::connect(address).await.unwrap();
        next.write_all(
            format!(
                "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                app_hostname().hostname
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        http_worker::read_headers(&mut next).await.unwrap();
        let mut body = Vec::new();
        next.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"new");
        finish.send(()).unwrap();
        let mut tail = Vec::new();
        request.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, b"after");
        replacement.drain().await.unwrap();
        guest.await.unwrap();
        new_guest.await.unwrap();
    })
    .await
    .expect("the controller can be killed and replaced without losing a response or listener");
}
