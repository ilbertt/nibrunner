use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIR_MODE: u32 = 0o700;
const TRAVERSABLE_DIR_MODE: u32 = 0o755;

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

/// A parent that is not there is made private, since what this writes is this host's own. One
/// that is there is somebody's — `/etc/zerofs`, made readable so the account ZeroFS runs as can
/// enter it — and putting a file in it is no reason to change who can.
pub fn write_text(path: &Path, value: &str, mode: u32) -> Result<(), StoreError> {
    let unwritable = |source: std::io::Error| StoreError::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
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

/// Whatever this makes on the way to `path` is something else's to cross — a service account
/// reaching its binary under /opt, or its cache under /data — so each of those is given a mode
/// outright rather than left to the umask of whichever shell ran this.
pub fn make_directory(path: &Path, mode: u32) -> Result<(), std::io::Error> {
    let missing: Vec<&Path> = path
        .ancestors()
        .take_while(|ancestor| !ancestor.exists())
        .collect();
    std::fs::create_dir_all(path)?;
    for made in missing.iter().skip(1) {
        set_mode(made, TRAVERSABLE_DIR_MODE)?;
    }
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

    #[test]
    fn a_file_holding_only_whitespace_is_read_as_nothing_rather_than_as_broken() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, "  \n\t ").unwrap();
        assert_eq!(read_text(&path).unwrap().as_deref(), Some(""));
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap(), None);
    }

    #[test]
    fn what_a_host_wrote_is_read_back_without_the_newline_it_was_written_with() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host-id");
        write_text(&path, "host-1\n", PRIVATE_FILE_MODE).unwrap();
        assert_eq!(read_text(&path).unwrap().as_deref(), Some("host-1"));
        assert_eq!(read_text(&directory.path().join("absent")).unwrap(), None);
    }

    #[test]
    fn a_path_that_cannot_be_read_is_named_rather_than_treated_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        let error = read_text(directory.path()).unwrap_err();
        assert!(matches!(error, StoreError::Unreadable { .. }), "{error}");
        assert!(error.message().contains("could not be read"));
    }

    #[test]
    fn a_document_that_has_nowhere_to_go_is_named_rather_than_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let occupied = directory.path().join("state");
        std::fs::write(&occupied, b"a file, not a directory").unwrap();
        let error = write_json(&occupied.join("nested.json"), &serde_json::json!({})).unwrap_err();
        assert!(matches!(error, StoreError::Unwritable { .. }), "{error}");
        assert!(error.message().contains("could not be written"));
    }

    #[test]
    fn a_value_no_json_can_hold_is_refused_before_anything_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let unrenderable = std::collections::BTreeMap::from([(vec![1u8, 2], 3)]);
        let error = write_json(&path, &unrenderable).unwrap_err();
        assert!(matches!(error, StoreError::Malformed { .. }), "{error}");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn what_this_host_keeps_is_readable_only_by_the_user_that_runs_it() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("private");
        let path = nested.join("state.json");
        write_json(&path, &serde_json::json!({ "a": 1 })).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), PRIVATE_FILE_MODE);
        assert_eq!(mode(&nested), PRIVATE_DIR_MODE);

        let socket = directory.path().join("readable");
        write_text(&socket, "anyone", 0o644).unwrap();
        assert_eq!(mode(&socket), 0o644);
    }

    // ZeroFS reads its config as its own account, and the directory `install` made readable for
    // it was being closed again by the very write that put the config there.
    #[cfg(unix)]
    #[test]
    fn a_file_put_in_a_directory_that_is_already_there_leaves_who_can_enter_it_alone() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let readable = directory.path().join("etc-zerofs");
        make_directory(&readable, 0o755).unwrap();
        write_text(&readable.join("config.toml"), "[cache]\n", 0o644).unwrap();
        assert_eq!(
            std::fs::metadata(&readable).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_is_already_there_is_still_brought_to_the_mode_asked_for() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("one").join("two");
        make_directory(&nested, 0o755).unwrap();
        make_directory(&nested, 0o700).unwrap();
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
