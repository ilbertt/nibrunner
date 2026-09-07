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

    #[test]
    fn a_versions_file_this_host_cannot_read_is_not_read_as_no_versions_at_all() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("versions.json");
        std::fs::write(&path, "{ not json at all").unwrap();
        let error = read_host_versions(&path).unwrap_err();
        assert!(matches!(error, VersionsError::Unreadable(_)), "{error}");
        assert!(error.message().contains("does not hold the JSON"), "{error}");
    }

    #[test]
    fn a_versions_file_missing_a_field_is_a_bundle_that_names_no_versions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("versions.json");
        std::fs::write(&path, r#"{"agent":"sha","zerofs":"none"}"#).unwrap();
        assert!(read_host_versions(&path).is_err());
    }

    #[test]
    fn what_a_bundle_carries_is_named_by_the_bundle_and_the_agent_names_only_itself() {
        let compiled = compiled_versions("v1.16.1", "6.1.180-test");
        assert_eq!(compiled.firecracker, "v1.16.1");
        assert_eq!(compiled.guest_image, "6.1.180-test");
        assert_eq!(compiled.zerofs, "none");
        assert_eq!(compiled.agent, env!("CARGO_PKG_VERSION"));
        assert!(!compiled.agent.is_empty());
    }
}
