//! The kernel and root filesystem every microVM on this host boots from.
//!
//! Taken from a release somebody already fetched rather than fetched here: whatever bootstraps
//! this binary has to reach that release anyway, to get this binary out of it, and two things
//! reaching the same URL means two implementations of the same digest check. What this knows is
//! the one thing that side cannot — that the files go wherever `paths.guest_image_dir` says.

use std::path::Path;

use super::InstallError;

/// The three the daemon checks before it boots anything, and the list their digests are in.
const ARTIFACTS: [&str; 3] = ["vmlinux", "rootfs.ext4", "manifest.json"];
const CHECKSUMS: &str = "checksums.txt";
const READABLE_FILE_MODE: u32 = 0o644;

#[derive(Debug)]
pub enum Laid {
    AlreadyThere(String),
    Taken(String),
}

/// Idempotent by asking the same question the daemon asks on the way up: a directory that already
/// verifies against its own manifest is one nothing needs to be done to.
pub fn ensure(directory: &Path, release: &Path) -> Result<Laid, InstallError> {
    if let Ok(version) = super::prerequisites::verify(directory) {
        return Ok(Laid::AlreadyThere(version));
    }

    let published = std::fs::read_to_string(release.join(CHECKSUMS)).map_err(|error| {
        InstallError::Refused(format!(
            "{} could not be read, so nothing in {} can be checked against it: {error}",
            release.join(CHECKSUMS).display(),
            release.display()
        ))
    })?;

    crate::json_store::make_directory(directory, 0o755).map_err(|error| {
        InstallError::Refused(format!("{} could not be made: {error}", directory.display()))
    })?;
    for artifact in ARTIFACTS {
        let source = release.join(artifact);
        let bytes = std::fs::read(&source).map_err(|error| {
            InstallError::Refused(format!("{} is not in that release: {error}", source.display()))
        })?;
        let expected = digest_of(&published, artifact).ok_or_else(|| {
            InstallError::Refused(format!("{CHECKSUMS} publishes no digest for {artifact}"))
        })?;
        let found = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        if found != expected {
            return Err(InstallError::Refused(format!(
                "{artifact} did not hash to what {CHECKSUMS} publishes: expected {expected}, got {found}"
            )));
        }
        write_bytes(&directory.join(artifact), &bytes)?;
    }

    // The same check the daemon makes before it boots anything, run here so a directory that is
    // going to refuse at startup refuses now instead.
    let version = super::prerequisites::verify(directory).map_err(InstallError::Refused)?;
    Ok(Laid::Taken(version))
}

/// `sha256sum` format: the digest, two spaces, the name.
fn digest_of(published: &str, artifact: &str) -> Option<String> {
    published
        .lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, name)| name.trim() == artifact)
        .map(|(digest, _)| digest.trim().to_string())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let unwritable = |error: std::io::Error| {
        InstallError::Refused(format!("{} could not be written: {error}", path.display()))
    };
    std::fs::write(path, bytes).map_err(unwritable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(READABLE_FILE_MODE))
            .map_err(unwritable)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(directory: &Path, files: &[(&str, &[u8])]) {
        let mut published = String::new();
        for (name, bytes) in files {
            std::fs::write(directory.join(name), bytes).unwrap();
            published.push_str(&format!(
                "{}  {name}\n",
                hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes))
            ));
        }
        std::fs::write(directory.join(CHECKSUMS), published).unwrap();
    }

    #[test]
    fn a_digest_is_read_out_of_the_list_the_release_publishes() {
        let published = "\
e6f96c54f861543a71f3bdaff1042f17d61944851738af5900632fdcfcd1707f  nibrunnerd-linux-x64
b6ada6ce51628084d1ba1d8b0b9b7a9785d183ab34e3824cf408b274a86ef221  vmlinux
";
        assert_eq!(
            digest_of(published, "vmlinux").as_deref(),
            Some("b6ada6ce51628084d1ba1d8b0b9b7a9785d183ab34e3824cf408b274a86ef221")
        );
        assert_eq!(digest_of(published, "manifest.json"), None);
    }

    // A name that is a suffix of another is not that other one, and taking it for one would install
    // a kernel under the name of a root filesystem.
    #[test]
    fn a_name_is_matched_whole_rather_than_by_ending_in_it() {
        let published = "aa  not-vmlinux\nbb  vmlinux\n";
        assert_eq!(digest_of(published, "vmlinux").as_deref(), Some("bb"));
    }

    #[test]
    fn an_artifact_that_does_not_hash_to_what_is_published_is_never_laid_down() {
        let from = tempfile::tempdir().unwrap();
        let into = tempfile::tempdir().unwrap();
        release(
            from.path(),
            &[("vmlinux", b"a kernel"), ("rootfs.ext4", b"a filesystem")],
        );
        // Published under one digest and then replaced, which is the shape of a release that was
        // tampered with between being signed for and being read.
        std::fs::write(from.path().join("vmlinux"), b"not that kernel").unwrap();
        std::fs::write(from.path().join("manifest.json"), b"{}").unwrap();

        let error = ensure(into.path(), from.path()).unwrap_err();
        assert!(error.message().contains("did not hash"), "{}", error.message());
        assert!(!into.path().join("vmlinux").exists());
    }

    #[test]
    fn a_release_missing_its_digests_is_refused_by_name() {
        let from = tempfile::tempdir().unwrap();
        let into = tempfile::tempdir().unwrap();
        let error = ensure(into.path(), from.path()).unwrap_err();
        assert!(error.message().contains(CHECKSUMS), "{}", error.message());
    }
}
