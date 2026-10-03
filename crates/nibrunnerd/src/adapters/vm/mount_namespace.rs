pub(crate) fn configure(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    {
        #[allow(unsafe_code, reason = "a VMM needs private mounts before exec")]
        unsafe {
            command.pre_exec(enter);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "mount namespaces have no safe libc wrapper here")]
fn enter() -> std::io::Result<()> {
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
    Ok(())
}
