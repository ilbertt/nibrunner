//! Laying ZeroFS down, and only that. It is fetched rather than carried: it is AGPL where the
//! hypervisor this binary does carry is Apache-2.0, and its release is an order of magnitude
//! larger than this daemon. Both of those are reasons to keep it a thing this host installs and
//! this binary merely runs — which is also what keeps its version a property of the host rather
//! than of the release that happened to be cut.

use std::io::Read;
use std::path::Path;

use super::InstallError;

pub const VERSION: &str = "v2.3.1";
const URL: &str = "https://github.com/Barre/ZeroFS/releases/download/v2.3.1/zerofs-pgo-multiplatform.tar.gz";
const SHA256: &str = "7a7d083f58677ccf5480850347fcf570e364baf48573bbd51f6f8d412972df3a";
const MEMBER: &str = "zerofs-linux-amd64-pgo";
const EXECUTABLE_MODE: u32 = 0o755;

/// Asked of the binary rather than of the path: a host that already runs this version is left
/// alone, and one carrying another is replaced.
pub fn installed(binary: &Path) -> bool {
    installed_version(binary).as_deref() == Some(VERSION)
}

pub async fn fetch(binary: &Path) -> Result<(), InstallError> {
    let archive = download().await?;
    let found = digest(&archive);
    if found != SHA256 {
        return Err(InstallError::Refused(format!(
            "the zerofs release did not hash to the version this build pins: expected {SHA256}, got {found}"
        )));
    }
    let extracted = member(&archive)
        .ok_or_else(|| InstallError::Refused(format!("the zerofs release does not hold {MEMBER}")))?;
    write_executable(binary, &extracted)
}

/// The version a host is actually running, which is the only thing that answers whether it needs
/// replacing — a path that exists says nothing about what is at the end of it.
fn installed_version(binary: &Path) -> Option<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()?;
    let said = String::from_utf8_lossy(&output.stdout);
    said.split_whitespace()
        .find(|word| {
            word.trim_start_matches('v')
                .starts_with(|c: char| c.is_ascii_digit())
        })
        .map(|version| format!("v{}", version.trim_start_matches('v')))
}

async fn download() -> Result<Vec<u8>, InstallError> {
    let transferred = |error: reqwest::Error| {
        InstallError::Refused(format!("zerofs {VERSION} could not be fetched: {error}"))
    };
    let response = reqwest::get(URL).await.map_err(transferred)?;
    let response = response.error_for_status().map_err(transferred)?;
    Ok(response.bytes().await.map_err(transferred)?.to_vec())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes))
}

/// The release carries a build per platform, so the one this host runs is picked by name rather
/// than by being the only thing in there.
fn member(archive: &[u8]) -> Option<Vec<u8>> {
    let mut opened = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in opened.entries().ok()? {
        let mut entry = entry.ok()?;
        let path = entry.path().ok()?.to_path_buf();
        if path.file_name().is_some_and(|name| name == MEMBER) {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary).ok()?;
            return Some(binary);
        }
    }
    None
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let unwritable = |error: std::io::Error| {
        InstallError::Refused(format!("{} could not be written: {error}", path.display()))
    };
    if let Some(parent) = path.parent() {
        crate::json_store::make_directory(parent, 0o755).map_err(unwritable)?;
    }
    // Staged and renamed, because the file being replaced may be the one a running ZeroFS was
    // started from, and a half-written binary at that path is worse than an old whole one.
    let staged = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&staged, bytes).map_err(unwritable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(EXECUTABLE_MODE))
            .map_err(unwritable)?;
    }
    std::fs::rename(&staged, path).map_err(unwritable)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_read_out_of_whatever_shape_the_binary_prints_it_in() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("zerofs");
        write_executable(&binary, b"#!/bin/sh\necho 'zerofs 2.3.1'\n").unwrap();
        assert_eq!(installed_version(&binary).as_deref(), Some("v2.3.1"));

        write_executable(&binary, b"#!/bin/sh\necho 'zerofs v2.3.1 (pgo)'\n").unwrap();
        assert_eq!(installed_version(&binary).as_deref(), Some("v2.3.1"));
    }

    #[test]
    fn a_binary_that_is_not_there_names_no_version_rather_than_failing() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(installed_version(&directory.path().join("absent")), None);
    }

    #[test]
    fn a_release_that_does_not_hash_to_the_pin_is_never_written() {
        assert_ne!(digest(b"not the release"), SHA256);
    }

    #[test]
    fn the_member_is_taken_out_of_a_release_that_carries_several() {
        let mut tarball = tar::Builder::new(Vec::new());
        for (name, body) in [("zerofs-linux-arm64-pgo", &b"arm"[..]), (MEMBER, &b"amd"[..])] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tarball.append_data(&mut header, name, body).unwrap();
        }
        let mut compressed = Vec::new();
        {
            use std::io::Write;
            let mut encoder = flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::fast());
            encoder.write_all(&tarball.into_inner().unwrap()).unwrap();
            encoder.finish().unwrap();
        }
        assert_eq!(member(&compressed).as_deref(), Some(&b"amd"[..]));
    }

    #[test]
    fn what_is_written_is_something_this_host_can_run() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("nested/zerofs");
        write_executable(&binary, b"#!/bin/sh\ntrue\n").unwrap();
        assert!(super::super::prerequisites::on_path("sh").is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&binary).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, EXECUTABLE_MODE);
        }
    }
}
