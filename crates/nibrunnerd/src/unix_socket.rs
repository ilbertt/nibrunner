use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_SOCKET_MODE: u32 = 0o600;

struct StagingDirectory(PathBuf);

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("s"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

pub(crate) fn bind(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // The socket's parent may belong to a live VMM; its parent belongs to the host.
    let parent = path
        .parent()
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("a private socket needs a host-owned staging parent"))?;
    let metadata = std::fs::metadata(parent)?;
    #[allow(
        unsafe_code,
        reason = "checking the staging parent's owner requires the effective UID"
    )]
    let uid = unsafe { libc::geteuid() };
    if (metadata.uid() != 0 && metadata.uid() != uid)
        || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
    {
        return Err(std::io::Error::other(
            "the socket staging parent is not protected from other users",
        ));
    }
    let staging = loop {
        let directory = parent.join(format!(
            ".s-{}",
            hex::encode(&uuid::Uuid::new_v4().as_bytes()[..6])
        ));
        match std::fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&directory)
        {
            Ok(()) => break StagingDirectory(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };
    let source = staging.0.join("s");
    let listener = UnixListener::bind(&source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(PRIVATE_SOCKET_MODE))?;
    std::fs::rename(&source, path)?;
    Ok(listener)
}

#[allow(
    unsafe_code,
    reason = "a VMM may replace a socket with a symlink; ownership must never follow it"
)]
pub(crate) fn own(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    if unsafe { libc::lchown(name.as_ptr(), uid, gid) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) async fn connect(path: &Path) -> std::io::Result<UnixStream> {
    use std::os::unix::fs::MetadataExt;

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("a guest socket has no parent"))?;
    let expected_uid = std::fs::metadata(parent)?.uid();
    let stream = UnixStream::connect(path).await?;
    verify_peer(&stream, expected_uid)?;
    Ok(stream)
}

fn verify_peer(stream: &UnixStream, expected_uid: u32) -> std::io::Result<()> {
    let actual_uid = stream.peer_cred()?.uid();
    if actual_uid != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the guest socket belongs to UID {actual_uid}, not its jail's UID {expected_uid}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn a_connected_peer_with_another_identity_is_refused() {
        let (client, _server) = UnixStream::pair().unwrap();
        let uid = client.peer_cred().unwrap().uid();
        verify_peer(&client, uid).unwrap();
        let error = verify_peer(&client, uid ^ 1).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn a_socket_published_by_rename_accepts_connections_at_its_final_path() {
        let directory = tempfile::tempdir().unwrap();
        let jail = directory.path().join("root");
        std::fs::create_dir(&jail).unwrap();
        let path = jail.join("guest.sock");
        let listener = bind(&path).unwrap();
        let client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            PRIVATE_SOCKET_MODE
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        drop((client, server));
    }

    #[tokio::test]
    async fn publishing_a_socket_over_a_symlink_leaves_its_target_permissions_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let jail = directory.path().join("root");
        std::fs::create_dir(&jail).unwrap();
        let victim = directory.path().join("keep");
        std::fs::write(&victim, b"operator data").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o640)).unwrap();
        let path = jail.join("guest.sock");
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        let listener = bind(&path).unwrap();
        let client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"operator data");
        drop((client, server));
    }
}
