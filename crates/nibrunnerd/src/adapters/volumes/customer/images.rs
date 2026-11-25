use std::fs::File;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use nix::fcntl::{openat, OFlag};
use nix::sys::stat::Mode;
use protocol::VolumeId;

use super::profile::{failed, io_error};
use crate::adapters::volumes::{align_to_sector, VolumeError};

pub(super) fn mounted_root(mount: &Path) -> Result<File, VolumeError> {
    // Pin the mount across lookup. Losing a FUSE mount must never turn a disk operation into
    // an operation on the underlying host directory, or follow a customer's symlink as root.
    let root = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(mount)
        .map_err(io_error)?;
    #[cfg(target_os = "linux")]
    if nix::sys::statfs::fstatfs(&root)
        .map_err(|e| io_error(e.into()))?
        .filesystem_type()
        != nix::sys::statfs::FUSE_SUPER_MAGIC
    {
        return Err(failed("customer disk images require an active ZeroFS mount"));
    }
    Ok(root)
}

fn image_directory(root: &File) -> nix::Result<File> {
    openat(
        root,
        super::super::zerofs::NBD_DIRECTORY,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
}

pub(super) fn directory(mount: &Path) -> Result<File, VolumeError> {
    image_directory(&mounted_root(mount)?).map_err(|e| io_error(e.into()))
}

pub(in crate::adapters::volumes) fn image_size(
    mount: &Path,
    id: &VolumeId,
) -> Result<Option<u64>, VolumeError> {
    let dir = match image_directory(&mounted_root(mount)?) {
        Ok(dir) => dir,
        Err(nix::errno::Errno::ENOENT) => return Ok(None),
        Err(e) => return Err(io_error(e.into())),
    };
    match nix::sys::stat::fstatat(&dir, id.as_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(meta) if meta.st_mode & libc::S_IFMT == libc::S_IFREG => u64::try_from(meta.st_size)
            .map(Some)
            .map_err(|_| failed("invalid disk image length")),
        Ok(_) => Err(failed("the customer volume image must be a regular file")),
        Err(nix::errno::Errno::ENOENT) => Ok(None),
        Err(e) => Err(io_error(e.into())),
    }
}

pub(in crate::adapters::volumes) fn ensure(
    mount: &Path,
    id: &VolumeId,
    size: u64,
) -> Result<u64, VolumeError> {
    let dir = directory(mount)?;
    let fd = openat(
        &dir,
        id.as_str(),
        OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .map_err(|e| io_error(e.into()))?;
    let file = File::from(fd);
    let meta = file.metadata().map_err(io_error)?;
    if !meta.is_file() {
        return Err(failed("the customer disk image must be a regular file"));
    }
    let target = align_to_sector(size);
    if meta.len() > target {
        return Err(VolumeError::ShrinkRefused {
            current: meta.len(),
            requested: target,
        });
    }
    if meta.len() < target {
        file.set_len(target).map_err(io_error)?;
    }
    Ok(target)
}

pub(super) fn remove(mount: &Path, id: &VolumeId) -> Result<(), VolumeError> {
    let dir = directory(mount)?;
    match nix::unistd::unlinkat(&dir, id.as_str(), nix::unistd::UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(nix::errno::Errno::ENOENT) => Ok(()),
        Err(e) => Err(io_error(io::Error::from(e))),
    }
}
