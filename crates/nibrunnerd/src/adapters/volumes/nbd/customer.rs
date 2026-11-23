use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{NbdDevices, NbdTarget, ProvenDevice, NBD_CLIENT, PROBE_TIMEOUT, RELEASE_POLL};
use crate::adapters::volumes::VolumeError;
use crate::install::customer_writer::SOCKET_ROOT;

const UNIT_PREFIX: &str = "nibrunner-customer-nbd-";

fn unavailable() -> VolumeError {
    VolumeError::Unusable("the NBD device is held by a different or unidentified writer".into())
}

fn owner(pid_file: &Path) -> Result<Option<u32>, VolumeError> {
    match std::fs::read_to_string(pid_file) {
        Ok(value) => value
            .trim()
            .parse()
            .ok()
            .filter(|pid| *pid > 0)
            .map(Some)
            .ok_or_else(unavailable),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(unavailable()),
    }
}

impl NbdTarget<'_> {
    pub(super) fn dedicated(&self) -> bool {
        Path::new(self.socket_path).starts_with(SOCKET_ROOT)
    }

    fn client_unit(&self) -> String {
        let identity = format!("{}\0{}\0{}", self.device_path, self.socket_path, self.volume_id);
        format!("{UNIT_PREFIX}{:x}.service", Sha256::digest(identity.as_bytes()))
    }

    fn client_args(&self) -> Vec<&str> {
        vec![
            "-unix",
            self.socket_path,
            self.device_path,
            "-N",
            self.volume_id.as_str(),
            "-nofork",
            "-nonetlink",
            "-timeout",
            "600",
            "-connections",
            "1",
            "-block-size",
            "4096",
        ]
    }
}

impl NbdDevices {
    fn owner_file(&self, device: &str) -> Result<std::path::PathBuf, VolumeError> {
        Ok(self
            .sysfs_block
            .join(Path::new(device).file_name().ok_or_else(unavailable)?)
            .join("pid"))
    }

    pub(super) fn customer_owned(&self, target: &NbdTarget<'_>) -> Result<bool, VolumeError> {
        let pid_file = self.owner_file(target.device_path)?;
        let Some(pid) = owner(&pid_file)? else {
            return Ok(false);
        };
        let process = Path::new("/proc").join(pid.to_string());
        let group = std::fs::read_to_string(process.join("cgroup")).map_err(|_| unavailable())?;
        let cmd = std::fs::read(process.join("cmdline")).map_err(|_| unavailable())?;
        let mut args = cmd.strip_suffix(&[0]).ok_or_else(unavailable)?.split(|c| *c == 0);
        let _binary = args.next();
        if !group
            .lines()
            .any(|line| line == format!("0::/system.slice/{}", target.client_unit()))
            || !args.eq(target.client_args().iter().map(|arg| arg.as_bytes()))
            || owner(&pid_file)? != Some(pid)
        {
            return Err(unavailable());
        }
        Ok(true)
    }

    pub(super) fn is_customer_client(&self, device: &str) -> bool {
        self.attribute(device, "pid")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok())
            .is_some_and(|groups| {
                groups
                    .lines()
                    .any(|line| line.starts_with("0::/system.slice/nibrunner-customer-nbd-"))
            })
    }

    pub(super) async fn ready_customer(&self, target: &NbdTarget<'_>) -> Result<ProvenDevice, VolumeError> {
        if self.customer_owned(target)? {
            if self.is_usable(target.device_path).await {
                return Ok(Self::proof(target));
            }
            self.stop_customer(target).await?;
        }
        let unit = format!("--unit={}", target.client_unit());
        let mut command = vec![
            "systemd-run",
            "--quiet",
            "--collect",
            "--service-type=exec",
            &unit,
            "--",
            NBD_CLIENT,
        ];
        command.extend(target.client_args());
        self.client("start the customer NBD client", &command).await?;
        let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
        loop {
            if self.customer_owned(target)? && self.is_usable(target.device_path).await {
                return Ok(Self::proof(target));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(VolumeError::Unusable(
                    "the customer NBD client did not acquire a readable device".into(),
                ));
            }
            tokio::time::sleep(RELEASE_POLL).await;
        }
    }

    pub(crate) async fn flush_customer(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        if !self.customer_owned(target)? {
            return Ok(());
        }
        // Firecracker can leave writes in the host block cache. ZeroFS's RPC can seal only
        // writes that have already reached its NBD server.
        let device = target.device_path.to_owned();
        let sync = tokio::task::spawn_blocking(move || std::fs::File::open(device)?.sync_all());
        tokio::time::timeout(std::time::Duration::from_secs(120), sync)
            .await
            .map_err(|_| VolumeError::Unusable("flushing the customer block device timed out".into()))?
            .map_err(|error| VolumeError::Unusable(error.to_string()))?
            .map_err(|error| {
                VolumeError::Unusable(format!("the customer block device could not be flushed: {error}"))
            })
    }

    pub(crate) async fn stop_customer(&self, target: &NbdTarget<'_>) -> Result<(), VolumeError> {
        // A client may still be negotiating before the kernel publishes a PID. Stopping its
        // specific unit also cancels that attempt without disconnecting a foreign device.
        let owned = self.customer_owned(target)?;
        let unit = target.client_unit();
        if !owned
            && !Path::new("/sys/fs/cgroup/system.slice")
                .join(&unit)
                .try_exists()
                .map_err(|_| unavailable())?
        {
            return Ok(());
        }
        if owned {
            self.client(
                "disconnect the customer NBD device",
                &[NBD_CLIENT, "-d", target.device_path],
            )
            .await?;
        }
        // Disconnect can already have made the transient unit exit and be collected.
        if Path::new("/sys/fs/cgroup/system.slice")
            .join(&unit)
            .try_exists()
            .map_err(|_| unavailable())?
        {
            self.client("stop the customer NBD client", &["systemctl", "stop", &unit])
                .await?;
        }
        if self.customer_owned(target)? {
            Err(unavailable())
        } else {
            Ok(())
        }
    }
}
