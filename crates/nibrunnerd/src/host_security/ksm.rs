use std::path::Path;

use super::SecurityError;

pub(super) fn check(directory: &Path) -> Result<(), SecurityError> {
    let present = directory
        .try_exists()
        .map_err(|error| SecurityError::KsmUnknown {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?;
    if !present {
        return Ok(());
    }
    match number(&directory.join("run"))? {
        0 | 2 => {}
        1 => return Err(SecurityError::KsmActive),
        status => {
            return Err(SecurityError::KsmUnknown {
                path: directory.join("run"),
                reason: format!("unexpected status {status}"),
            });
        }
    }
    let pages = number(&directory.join("pages_shared"))?;
    if pages != 0 {
        return Err(SecurityError::KsmShared { pages });
    }
    Ok(())
}

fn number(path: &Path) -> Result<u64, SecurityError> {
    let text = std::fs::read_to_string(path).map_err(|error| SecurityError::KsmUnknown {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    text.trim()
        .parse()
        .map_err(|error: std::num::ParseIntError| SecurityError::KsmUnknown {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(run: &str, pages: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("run"), run).unwrap();
        std::fs::write(directory.path().join("pages_shared"), pages).unwrap();
        directory
    }

    #[test]
    fn a_kernel_without_ksm_support_meets_the_requirement() {
        let directory = tempfile::tempdir().unwrap();
        assert!(check(&directory.path().join("ksm")).is_ok());
    }

    #[test]
    fn stopped_ksm_without_shared_pages_meets_the_requirement() {
        for run in ["0\n", "2\n"] {
            let directory = directory(run, "0\n");
            assert!(check(directory.path()).is_ok());
        }
    }

    #[test]
    fn active_ksm_is_refused_even_before_it_merges_any_pages() {
        let directory = directory("1\n", "0\n");
        assert!(matches!(check(directory.path()), Err(SecurityError::KsmActive)));
    }

    #[test]
    fn stopping_ksm_does_not_make_retained_shared_pages_safe() {
        for run in ["0", "2"] {
            let directory = directory(run, "42\n");
            assert!(matches!(
                check(directory.path()),
                Err(SecurityError::KsmShared { pages: 42 })
            ));
        }
    }

    #[test]
    fn missing_status_files_are_not_mistaken_for_absent_ksm_support() {
        for name in ["run", "pages_shared"] {
            let directory = directory("0", "0");
            std::fs::remove_file(directory.path().join(name)).unwrap();
            assert!(matches!(
                check(directory.path()),
                Err(SecurityError::KsmUnknown { .. })
            ));
        }
    }

    #[test]
    fn unreadable_ksm_status_is_refused() {
        let directory = directory("0", "0");
        let run = directory.path().join("run");
        std::fs::remove_file(&run).unwrap();
        std::fs::create_dir(run).unwrap();
        assert!(matches!(
            check(directory.path()),
            Err(SecurityError::KsmUnknown { .. })
        ));
    }

    #[test]
    fn malformed_ksm_status_is_refused() {
        for (run, pages) in [("3", "0"), ("", "0"), ("0", "-1"), ("0", "unknown")] {
            let directory = directory(run, pages);
            assert!(matches!(
                check(directory.path()),
                Err(SecurityError::KsmUnknown { .. })
            ));
        }
    }
}
