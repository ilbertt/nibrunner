#[cfg(target_os = "linux")]
use std::path::Path;

#[derive(Clone)]
pub(crate) struct Mount {
    #[cfg(target_os = "linux")]
    source: std::ffi::CString,
    #[cfg(target_os = "linux")]
    target: std::ffi::CString,
    #[cfg(target_os = "linux")]
    read_only: bool,
}

#[cfg(target_os = "linux")]
pub(crate) fn configure(command: &mut tokio::process::Command, mounts: Vec<Mount>) {
    #[cfg(target_os = "linux")]
    {
        #[allow(unsafe_code, reason = "a VMM needs private bind mounts before exec")]
        unsafe {
            command.pre_exec(move || enter(&mounts));
        }
    }
}

#[cfg(target_os = "linux")]
impl Mount {
    pub(crate) fn new(source: &Path, target: &Path, read_only: bool) -> std::io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        Ok(Self {
            source: std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(std::io::Error::other)?,
            target: std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(std::io::Error::other)?,
            read_only,
        })
    }
}

#[cfg(target_os = "linux")]
#[allow(
    unsafe_code,
    reason = "mount namespaces and bind mounts have no safe libc wrapper here"
)]
fn enter(mounts: &[Mount]) -> std::io::Result<()> {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    for mount in mounts {
        if unsafe {
            libc::mount(
                mount.source.as_ptr(),
                mount.target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if mount.read_only
            && unsafe {
                libc::mount(
                    std::ptr::null(),
                    mount.target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                )
            } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
