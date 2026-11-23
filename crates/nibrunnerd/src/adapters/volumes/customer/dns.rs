use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::profile::{failed, io_error};
use crate::adapters::volumes::VolumeError;

const REFRESH: Duration = Duration::from_secs(60);

pub(super) struct EndpointHosts {
    hostname: String,
    path: PathBuf,
    refreshed: tokio::sync::Mutex<Option<Instant>>,
}

impl EndpointHosts {
    pub(super) fn new(endpoint: &str, directory: &Path) -> Result<Self, VolumeError> {
        let url = reqwest::Url::parse(endpoint).map_err(|_| failed("invalid staged customer endpoint"))?;
        let hostname = url
            .host_str()
            .ok_or_else(|| failed("customer endpoint has no hostname"))?;
        Ok(Self {
            hostname: hostname.trim_matches(['[', ']']).to_string(),
            path: directory.join("s3.hosts"),
            refreshed: Default::default(),
        })
    }

    pub(super) async fn prepare(&self) -> Result<(), VolumeError> {
        let mut last = self.refreshed.lock().await;
        if last.is_some_and(|time| time.elapsed() < REFRESH) {
            return Ok(());
        }
        self.resolve().await?;
        *last = Some(Instant::now());
        Ok(())
    }

    pub(super) fn refresh(self: &Arc<Self>) {
        let Ok(mut last) = self.refreshed.try_lock() else {
            return;
        };
        if !last.is_some_and(|time| time.elapsed() >= REFRESH) {
            return;
        }
        *last = Some(Instant::now());
        let hosts = self.clone();
        tokio::spawn(async move {
            let _guard = hosts.refreshed.lock().await;
            if let Err(error) = hosts.resolve().await {
                tracing::warn!(error = %error.message(), "could not refresh the customer writer's S3 DNS answers");
            }
        });
    }

    async fn resolve(&self) -> Result<(), VolumeError> {
        let mut hosts = String::from("127.0.0.1 localhost\n::1 localhost\n");
        if self.hostname.parse::<IpAddr>().is_err() {
            let addresses = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((self.hostname.as_str(), 443)),
            )
            .await
            .map_err(|_| failed("customer S3 endpoint DNS timed out"))?
            .map_err(|_| failed("customer S3 endpoint DNS failed"))?
            .map(|addr| addr.ip())
            .collect::<std::collections::BTreeSet<_>>();
            if addresses.is_empty() {
                return Err(failed("customer S3 endpoint has no DNS answers"));
            }
            for address in addresses {
                use std::fmt::Write as _;
                writeln!(hosts, "{address} {}", self.hostname).expect("writing to a string");
            }
        }
        // The writer bind-mounts this inode as /etc/hosts. Renaming a replacement would leave
        // an active writer using stale addresses. The host resolver can reach private DNS while
        // the writer's network policy still applies to every actual S3 connection.
        let mut file = std::fs::File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o644)
            .open(&self.path)
            .map_err(io_error)?;
        file.write_all(hosts.as_bytes())
            .and_then(|()| file.set_len(hosts.len() as u64))
            .map_err(io_error)?;
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(io_error)
    }
}
