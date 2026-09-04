//! What this host says it is running. Read off a file the deploy wrote rather than compiled in,
//! so a report cannot claim a version the binaries on disk no longer are.

use std::path::Path;

use protocol::HostVersions;

use crate::json_store::{read_json, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum VersionsError {
    #[error("the bundle names no versions at {path}")]
    Missing { path: String },
    #[error("{0}")]
    Unreadable(#[from] StoreError),
}

impl VersionsError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// What a host with no versions file reports: this binary's own version, and `none` for the
/// three components it did not fetch. The schema has no way to say "not applicable", so it says
/// `none` rather than omitting the field.
pub fn compiled_versions(firecracker: &str, guest_image: &str) -> HostVersions {
    HostVersions {
        agent: env!("CARGO_PKG_VERSION").to_string(),
        guest_image: guest_image.to_string(),
        zerofs: "none".to_string(),
        firecracker: firecracker.to_string(),
    }
}

pub fn read_host_versions(path: &Path) -> Result<HostVersions, VersionsError> {
    read_json(path)?.ok_or_else(|| VersionsError::Missing {
        path: path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_versions_file_is_read_and_a_missing_one_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("versions.json");
        assert!(read_host_versions(&path)
            .unwrap_err()
            .message()
            .contains("names no versions"));
        std::fs::write(
            &path,
            r#"{"agent":"sha","guestImage":"6.1.180-x","zerofs":"none","firecracker":"v1.16.1"}"#,
        )
        .unwrap();
        assert_eq!(read_host_versions(&path).unwrap().firecracker, "v1.16.1");
    }
}
