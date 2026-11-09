use std::fs::File;
use std::io;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::fd::AsRawFd;

    const SHA256: u32 = 1;

    #[repr(C)]
    #[derive(Default)]
    struct Enable {
        version: u32,
        hash_algorithm: u32,
        block_size: u32,
        salt_size: u32,
        salt_ptr: u64,
        sig_size: u32,
        reserved1: u32,
        sig_ptr: u64,
        reserved2: [u64; 11],
    }

    #[repr(C)]
    #[derive(Default)]
    struct DigestHeader {
        algorithm: u16,
        size: u16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Digest {
        header: DigestHeader,
        bytes: [u8; 32],
    }

    #[allow(unsafe_code, reason = "Linux exposes fs-verity through ioctls")]
    pub(super) fn seal(file: &File) -> io::Result<Option<[u8; 32]>> {
        let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .map_err(|_| io::Error::last_os_error())?;
        let block_size = nix::sys::statvfs::fstatvfs(file)?.fragment_size().min(page_size);
        let enable = Enable {
            version: 1,
            hash_algorithm: SHA256,
            block_size: block_size.try_into().expect("a verity block fits in u32"),
            ..Enable::default()
        };
        let request = nix::request_code_write!(b'f', 133, std::mem::size_of::<Enable>());
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                request as nix::sys::ioctl::ioctl_num_type,
                &enable,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENOTTY | libc::EOPNOTSUPP) => return Ok(None),
                Some(libc::EEXIST) => {}
                _ => return Err(error),
            }
        }
        measure(file).map(Some)
    }

    #[allow(unsafe_code, reason = "Linux exposes fs-verity through ioctls")]
    pub(super) fn measure(file: &File) -> io::Result<[u8; 32]> {
        let mut digest = Digest::default();
        digest.header.size = digest
            .bytes
            .len()
            .try_into()
            .expect("a SHA-256 digest fits in u16");
        // The ioctl encodes the flexible-array header's size, not the output buffer's size.
        let request = nix::request_code_readwrite!(b'f', 134, std::mem::size_of::<DigestHeader>());
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                request as nix::sys::ioctl::ioctl_num_type,
                &mut digest,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if u32::from(digest.header.algorithm) != SHA256
            || usize::from(digest.header.size) != digest.bytes.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected an fs-verity SHA-256 digest",
            ));
        }
        Ok(digest.bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_ioctl_arguments_match_the_linux_uapi_layout() {
            assert_eq!(std::mem::size_of::<Enable>(), 128);
            assert_eq!(std::mem::size_of::<DigestHeader>(), 4);
            assert_eq!(std::mem::size_of::<Digest>(), 36);
        }
    }
}

pub fn seal(file: &File) -> io::Result<Option<[u8; 32]>> {
    #[cfg(target_os = "linux")]
    return linux::seal(file);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Ok(None)
    }
}

pub fn measure(file: &File) -> io::Result<[u8; 32]> {
    #[cfg(target_os = "linux")]
    return linux::measure(file);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fs-verity requires Linux",
        ))
    }
}
