//! Reading and writing the daemon's own notes.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIR_MODE: u32 = 0o700;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path} could not be read: {source}")]
    Unreadable { path: PathBuf, source: std::io::Error },
    #[error("{path} could not be written: {source}")]
    Unwritable { path: PathBuf, source: std::io::Error },
    #[error("{path} does not hold the JSON this host wrote: {source}")]
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl StoreError {
    /// What a report carries to the control plane, and from there to whoever is looking.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

pub fn read_text(path: &Path) -> Result<Option<String>, StoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| StoreError::Malformed {
            path: path.to_path_buf(),
            source,
        })
}

/// Through a uniquely named sibling and a rename, which is atomic within a directory: a torn
/// write cannot leave an unparsable state file, and two writes in flight cannot collide.
pub fn write_text(path: &Path, value: &str, mode: u32) -> Result<(), StoreError> {
    let unwritable = |source: std::io::Error| StoreError::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        make_directory(parent, PRIVATE_DIR_MODE).map_err(unwritable)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, value).map_err(unwritable)?;
    set_mode(&temporary, mode).map_err(unwritable)?;
    std::fs::rename(&temporary, path).map_err(unwritable)?;
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let rendered = serde_json::to_string_pretty(value).map_err(|source| StoreError::Malformed {
        path: path.to_path_buf(),
        source,
    })?;
    write_text(path, &format!("{rendered}\n"), PRIVATE_FILE_MODE)
}

pub fn make_directory(path: &Path, mode: u32) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(path)?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_round_trips_and_a_missing_file_is_not_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("state.json");
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap(), None);
        write_json(&path, &serde_json::json!({ "a": 1 })).unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&path).unwrap(),
            Some(serde_json::json!({ "a": 1 }))
        );
        // Nothing is left beside it: a torn write would show up as a stray temporary.
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(siblings.len(), 1);
    }

    #[test]
    fn a_file_that_is_not_json_is_a_typed_failure_rather_than_a_guess() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, "not json").unwrap();
        let error = read_json::<serde_json::Value>(&path).unwrap_err();
        assert!(error.message().contains("does not hold the JSON"));
    }
}
