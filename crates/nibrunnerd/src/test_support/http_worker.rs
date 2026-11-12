use std::net::SocketAddr;
use std::path::Path;

use crate::config::{HostConfig, HttpListener};
use crate::domain::report::routes::{renderable_routes, RouteTarget};

pub fn configuration(root: &Path, address: SocketAddr) -> HostConfig {
    let mut config = HostConfig::example();
    config.runtime_dir = root.join("runtime");
    config.state_dir = root.join("state");
    config.proxy.http = Some(HttpListener {
        listen_address: address.ip(),
        port: address.port(),
        tls: None,
        fallback: None,
        concurrency: None,
    });
    config
}

pub fn routes(port: u16) -> Vec<RouteTarget> {
    renderable_routes(&[super::instance_record(|record| {
        record.host_port = Some(protocol::HostPort::new(port).unwrap());
    })])
}

pub async fn read_headers(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await?);
    }
    Ok(bytes)
}
