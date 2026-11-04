use std::path::{Path, PathBuf};

mod ksm;

const CPU_DIRECTORY: &str = "/sys/devices/system/cpu";
const KSM_DIRECTORY: &str = "/sys/kernel/mm/ksm";
const SECURITY_DOCS: &str = "https://nibrunner.dev/docs/guides/host-security";

#[derive(Debug, thiserror::Error)]
pub(crate) enum SecurityError {
    #[error("host SMT is active; disable it before starting nibrunner; see {SECURITY_DOCS}#host-smt")]
    SmtActive,
    #[error("host SMT status could not be verified from {path}: {reason}; see {SECURITY_DOCS}#host-smt")]
    SmtUnknown { path: PathBuf, reason: String },
    #[error("host KSM is active; disable it and unmerge retained pages before starting nibrunner; see {SECURITY_DOCS}#kernel-samepage-merging")]
    KsmActive,
    #[error("host KSM retains {pages} shared pages; unmerge them before starting nibrunner; see {SECURITY_DOCS}#kernel-samepage-merging")]
    KsmShared { pages: u64 },
    #[error("host KSM status could not be verified from {path}: {reason}; see {SECURITY_DOCS}#kernel-samepage-merging")]
    KsmUnknown { path: PathBuf, reason: String },
}

pub(crate) fn check() -> Result<(), SecurityError> {
    check_smt(Path::new(CPU_DIRECTORY))?;
    ksm::check(Path::new(KSM_DIRECTORY))
}

fn check_smt(cpu_directory: &Path) -> Result<(), SecurityError> {
    let path = cpu_directory.join("smt/active");
    let active = std::fs::read_to_string(&path).map_err(|error| SecurityError::SmtUnknown {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    match active.trim() {
        "0" => Ok(()),
        "1" => Err(SecurityError::SmtActive),
        status => Err(SecurityError::SmtUnknown {
            path,
            reason: format!("unexpected status {status:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_directory(active: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("smt")).unwrap();
        std::fs::write(directory.path().join("smt/active"), active).unwrap();
        directory
    }

    #[test]
    fn a_host_with_no_active_smt_siblings_meets_the_requirement() {
        let directory = cpu_directory("0\n");
        assert!(check_smt(directory.path()).is_ok());
    }

    #[test]
    fn active_smt_is_refused_with_remediation_instructions() {
        let directory = cpu_directory("1\n");
        let error = check_smt(directory.path()).unwrap_err();
        assert!(matches!(error, SecurityError::SmtActive));
        assert!(error.to_string().contains(SECURITY_DOCS));
    }

    #[test]
    fn absent_smt_status_is_not_evidence_that_smt_is_disabled() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            check_smt(directory.path()),
            Err(SecurityError::SmtUnknown { .. })
        ));
    }

    #[test]
    fn unreadable_smt_status_is_not_evidence_that_smt_is_disabled() {
        let directory = cpu_directory("0");
        let path = directory.path().join("smt/active");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(matches!(
            check_smt(directory.path()),
            Err(SecurityError::SmtUnknown { .. })
        ));
    }

    #[test]
    fn unexpected_smt_status_is_refused() {
        for active in ["", "2", "off", "0\n1", "unknown"] {
            let directory = cpu_directory(active);
            assert!(matches!(
                check_smt(directory.path()),
                Err(SecurityError::SmtUnknown { .. })
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_integration_host_meets_the_smt_requirement() {
        if std::env::var("NIBRUNNER_INTEGRATION").is_ok_and(|value| value == "1") {
            check().expect("disable host SMT before running integration tests");
        }
    }
}
