//! A stopped ZeroFS image can be streamed through its two mounted filesystems.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use nix::fcntl::{openat, AtFlags, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{linkat, unlinkat, UnlinkatFlags};
use protocol::VolumeId;
use uuid::Uuid;

use super::images::directory;
use super::profile::{failed, io_error};
use crate::adapters::volumes::VolumeError;

const BUFFER: usize = 1024 * 1024;

fn image(dir: &File, name: &str, flags: OFlag) -> io::Result<File> {
    let fd = openat(
        dir,
        name,
        flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?;
    let file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("migration requires regular disk images"));
    }
    Ok(file)
}

fn verify(source: &mut File, target: &mut File, size: u64) -> Result<(), VolumeError> {
    if source.metadata().map_err(io_error)?.len() != size
        || target.metadata().map_err(io_error)?.len() != size
    {
        return Err(failed("migrated image length differs from the source"));
    }
    source.rewind().map_err(io_error)?;
    target.rewind().map_err(io_error)?;
    let mut left = vec![0; BUFFER];
    let mut right = vec![0; BUFFER];
    let mut remaining = size;
    while remaining != 0 {
        let n = usize::try_from(remaining.min(BUFFER as u64)).expect("bounded buffer");
        source.read_exact(&mut left[..n]).map_err(io_error)?;
        target.read_exact(&mut right[..n]).map_err(io_error)?;
        if left[..n] != right[..n] {
            return Err(failed("migrated image differs from the source"));
        }
        remaining -= n as u64;
    }
    Ok(())
}

fn remove_stage(dir: &File, stage: &str) -> Result<(), VolumeError> {
    match unlinkat(dir, stage, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(nix::errno::Errno::ENOENT) => Ok(()),
        Err(error) => Err(io_error(error.into())),
    }
}

/// Both writers must be exclusively held and the source detached for the entire transfer.
/// The caller flushes the destination writer's RPC before committing placement.
pub fn copy_image(source: &Path, target: &Path, volume: &VolumeId, attempt: Uuid) -> Result<(), VolumeError> {
    let to = directory(target)?;
    copy_between(&directory(source)?, &to, volume.as_str(), attempt)?;
    remove_stage(&to, &format!(".migration-{attempt}"))
}

pub(super) fn stage_image(
    source: &Path,
    target: &Path,
    volume: &VolumeId,
    attempt: Uuid,
) -> Result<(), VolumeError> {
    copy_between(&directory(source)?, &directory(target)?, volume.as_str(), attempt)
}

pub(super) fn finish_attempt(
    mount: &Path,
    volume: &VolumeId,
    attempt: Uuid,
    committed: bool,
) -> Result<(), VolumeError> {
    clean_links(&directory(mount)?, volume.as_str(), attempt, committed)
}

fn clean_links(to: &File, name: &str, attempt: Uuid, committed: bool) -> Result<(), VolumeError> {
    let stage = format!(".migration-{attempt}");
    let stat = |path: &str| match nix::sys::stat::fstatat(to, path, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(st)),
        Err(nix::errno::Errno::ENOENT) => Ok(None),
        Err(e) => Err(io_error(e.into())),
    };
    if !committed {
        if let (Some(owned), Some(published)) = (stat(&stage)?, stat(name)?) {
            // A pre-existing volume can have identical contents. Only our exclusive hard link proves ownership.
            if owned.st_mode & libc::S_IFMT == libc::S_IFREG
                && (owned.st_dev, owned.st_ino) == (published.st_dev, published.st_ino)
            {
                remove_stage(to, name)?;
            }
        }
    }
    remove_stage(to, &stage)
}

fn copy_between(from: &File, to: &File, name: &str, attempt: Uuid) -> Result<(), VolumeError> {
    let mut source = image(from, name, OFlag::O_RDONLY).map_err(io_error)?;
    let size = source.metadata().map_err(io_error)?.len();
    if size == 0 || !size.is_multiple_of(super::super::SECTOR_SIZE_BYTES) {
        return Err(failed("source is not a sector-aligned disk image"));
    }
    let stage = format!(".migration-{attempt}");
    match image(to, name, OFlag::O_RDONLY) {
        Ok(mut target) => {
            verify(&mut source, &mut target, size)?;
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    remove_stage(to, &stage)?;
    let mut target = image(to, &stage, OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR).map_err(io_error)?;
    let mut buffer = vec![0; BUFFER];
    let mut remaining = size;
    while remaining != 0 {
        let n = usize::try_from(remaining.min(BUFFER as u64)).expect("bounded buffer");
        source.read_exact(&mut buffer[..n]).map_err(io_error)?;
        if buffer[..n].iter().all(|&byte| byte == 0) {
            target
                .seek(SeekFrom::Current(i64::try_from(n).expect("bounded buffer")))
                .map_err(io_error)?;
        } else {
            target.write_all(&buffer[..n]).map_err(io_error)?;
        }
        remaining -= n as u64;
    }
    target
        .set_len(size)
        .and_then(|()| target.sync_all())
        .map_err(io_error)?;
    verify(&mut source, &mut target, size)?;
    // Hard-link publication is exclusive even on 9P mounts without RENAME_NOREPLACE.
    linkat(to, stage.as_str(), to, name, AtFlags::empty()).map_err(|e| io_error(e.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_unlinks_only_the_image_published_by_this_attempt() {
        let dst = tempfile::tempdir().unwrap();
        let dir = File::open(dst.path()).unwrap();
        let attempt = Uuid::new_v4();
        let stage = dst.path().join(format!(".migration-{attempt}"));
        let disk = dst.path().join("vol");
        std::fs::write(&stage, "old").unwrap();
        std::fs::hard_link(&stage, &disk).unwrap();
        clean_links(&dir, "vol", attempt, false).unwrap();
        assert!(!disk.exists());
        std::fs::write(&stage, "old").unwrap();
        std::fs::write(&disk, "old").unwrap();
        clean_links(&dir, "vol", attempt, false).unwrap();
        assert!(disk.exists());
    }

    #[test]
    fn retries_replace_only_private_scratch_and_never_overwrite_a_different_destination() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let source = File::open(src.path()).unwrap();
        let target = File::open(dst.path()).unwrap();
        let attempt = Uuid::new_v4();
        let stage = dst.path().join(format!(".migration-{attempt}"));
        let mut bytes = vec![0u8; BUFFER * 3];
        bytes[BUFFER + 7] = 0xff;
        std::fs::write(src.path().join("vol"), &bytes).unwrap();
        std::fs::write(&stage, "interrupted").unwrap();
        copy_between(&source, &target, "vol", attempt).unwrap();
        assert_eq!(std::fs::read(dst.path().join("vol")).unwrap(), bytes);
        assert!(stage.exists());
        clean_links(&target, "vol", attempt, true).unwrap();
        assert!(!stage.exists());
        copy_between(&source, &target, "vol", attempt).unwrap();
        bytes[BUFFER + 7] = 1;
        std::fs::write(dst.path().join("vol"), &bytes).unwrap();
        assert!(copy_between(&source, &target, "vol", attempt).is_err());
        assert_eq!(std::fs::read(dst.path().join("vol")).unwrap(), bytes);
    }
}
