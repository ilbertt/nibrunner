use std::fs::File;
use std::io::{Read, Write};

use nix::fcntl::{openat, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{unlinkat, UnlinkatFlags};
use protocol::AppId;
use uuid::Uuid;

use super::images::mounted_root;
use super::profile::{failed, io_error};
use crate::adapters::volumes::zerofs::ZerofsVolumes;
use crate::adapters::volumes::{VolumeBackend, VolumeError};

struct ProbeFile<'a> {
    root: &'a File,
    name: &'a str,
    removed: bool,
}

impl ProbeFile<'_> {
    fn remove(&mut self) -> Result<(), VolumeError> {
        unlinkat(self.root, self.name, UnlinkatFlags::NoRemoveDir).map_err(|e| io_error(e.into()))?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for ProbeFile<'_> {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.remove();
        }
    }
}

/// The customer mount ignores fsync. Its writer RPC is the durability barrier for both changes.
pub(super) async fn check(writer: &ZerofsVolumes, app: &AppId) -> Result<(), VolumeError> {
    let root = mounted_root(&writer.filesystem().mount_path)?;
    let name = format!(".nibrunner-preflight-{}", Uuid::new_v4());
    let challenge = Uuid::new_v4();
    let fd = openat(
        &root,
        name.as_str(),
        OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .map_err(|e| io_error(e.into()))?;
    let mut probe = ProbeFile {
        root: &root,
        name: &name,
        removed: false,
    };
    let checked = async {
        File::from(fd).write_all(challenge.as_bytes()).map_err(io_error)?;
        writer.flush(app).await?;
        let read = openat(
            &root,
            name.as_str(),
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io_error(e.into()))?;
        let mut echoed = [0; 16];
        let mut file = File::from(read);
        if !file.metadata().map_err(io_error)?.is_file() {
            return Err(failed("customer writer probe is not a regular file"));
        }
        file.read_exact(&mut echoed).map_err(io_error)?;
        if echoed != *challenge.as_bytes() {
            return Err(failed("customer writer probe did not round-trip"));
        }
        Ok(())
    }
    .await;
    probe.remove()?;
    writer.flush(app).await?;
    checked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_check_unlinks_its_probe_from_the_pinned_directory() {
        let scratch = tempfile::tempdir().unwrap();
        let root = File::open(scratch.path()).unwrap();
        let path = scratch.path().join("probe");
        std::fs::write(&path, "challenge").unwrap();
        drop(ProbeFile {
            root: &root,
            name: "probe",
            removed: false,
        });
        assert!(!path.exists());
    }
}
