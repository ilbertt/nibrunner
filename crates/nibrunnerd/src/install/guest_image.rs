//! The kernel and root filesystem every microVM on this host boots from.
//!
//! Fetched here rather than by whatever bootstrapped this binary, because where it goes is
//! `paths.guest_image_dir` — a thing the configuration says and a shell script would have to be
//! told twice. What the script knows is where a release is; what this knows is where files go.

use std::path::Path;

use super::InstallError;

/// The three the daemon checks before it boots anything, and the list its digests are published in.
const ARTIFACTS: [&str; 3] = ["vmlinux", "rootfs.ext4", "manifest.json"];
const CHECKSUMS: &str = "checksums.txt";
const READABLE_FILE_MODE: u32 = 0o644;

pub enum Laid {
    AlreadyThere(String),
    Fetched(String),
}

/// Idempotent by asking the same question the daemon asks on the way up: a directory that already
/// verifies against its own manifest is one nothing needs to be done to.
pub async fn ensure(directory: &Path, base: &str) -> Result<Laid, InstallError> {
    if let Ok(version) = super::prerequisites::verify(directory) {
        return Ok(Laid::AlreadyThere(version));
    }

    let published = fetch(base, CHECKSUMS).await?;
    let published = String::from_utf8(published)
        .map_err(|_| InstallError::Refused(format!("{base}/{CHECKSUMS} is not a list of digests")))?;

    crate::json_store::make_directory(directory, 0o755).map_err(|error| {
        InstallError::Refused(format!("{} could not be made: {error}", directory.display()))
    })?;
    for artifact in ARTIFACTS {
        let bytes = fetch(base, artifact).await?;
        let found = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        let expected = digest_of(&published, artifact).ok_or_else(|| {
            InstallError::Refused(format!("{base}/{CHECKSUMS} publishes no digest for {artifact}"))
        })?;
        if found != expected {
            return Err(InstallError::Refused(format!(
                "{artifact} did not hash to what {base}/{CHECKSUMS} publishes: expected {expected}, got {found}"
            )));
        }
        crate::json_store::write_text(&directory.join(artifact), "", READABLE_FILE_MODE)
            .and_then(|()| write_bytes(&directory.join(artifact), &bytes))
            .map_err(|error| InstallError::Refused(error.message()))?;
    }

    // The same check the daemon makes before it boots anything, run here so a directory that is
    // going to refuse at startup refuses now instead.
    let version = super::prerequisites::verify(directory).map_err(InstallError::Refused)?;
    Ok(Laid::Fetched(version))
}

/// `sha256sum` format: the digest, two spaces, the name.
fn digest_of(published: &str, artifact: &str) -> Option<String> {
    published
        .lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, name)| name.trim() == artifact)
        .map(|(digest, _)| digest.trim().to_string())
}

async fn fetch(base: &str, name: &str) -> Result<Vec<u8>, InstallError> {
    let url = format!("{}/{name}", base.trim_end_matches('/'));
    let transferred =
        |error: reqwest::Error| InstallError::Refused(format!("{url} could not be fetched: {error}"));
    let response = reqwest::get(&url).await.map_err(transferred)?;
    let response = response.error_for_status().map_err(transferred)?;
    Ok(response.bytes().await.map_err(transferred)?.to_vec())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), crate::json_store::StoreError> {
    std::fs::write(path, bytes).map_err(|source| crate::json_store::StoreError::Unwritable {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_read_out_of_the_list_the_release_publishes() {
        let published = "\
e6f96c54f861543a71f3bdaff1042f17d61944851738af5900632fdcfcd1707f  nibrunnerd-linux-x64
b6ada6ce51628084d1ba1d8b0b9b7a9785d183ab34e3824cf408b274a86ef221  vmlinux
127b5329c4db40d25f90a428f03859dbc8820e98f4e5425d457b21be52048308  rootfs.ext4
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
    fn a_url_is_joined_whether_or_not_the_base_ends_in_a_slash() {
        // The join is what `fetch` does with it; asserted here rather than by reaching the network.
        for base in ["https://example.test/release", "https://example.test/release/"] {
            assert_eq!(
                format!("{}/{}", base.trim_end_matches('/'), "vmlinux"),
                "https://example.test/release/vmlinux"
            );
        }
    }
}
