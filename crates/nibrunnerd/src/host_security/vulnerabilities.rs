use std::io;
use std::path::Path;

struct Assessment {
    name: String,
    status: io::Result<String>,
}

pub(super) fn report(directory: &Path) {
    let assessments = match read(directory) {
        Ok(assessments) => assessments,
        Err(error) => {
            tracing::warn!(
                path = %directory.display(),
                %error,
                "host CPU vulnerability assessments are unavailable"
            );
            return;
        }
    };
    for assessment in assessments {
        match assessment.status {
            Ok(status) if needs_attention(&status) => tracing::warn!(
                cpu_vulnerability = assessment.name,
                status = status.trim(),
                "host CPU vulnerability assessment needs attention"
            ),
            Ok(status) => tracing::info!(
                cpu_vulnerability = assessment.name,
                status = status.trim(),
                "host CPU vulnerability assessment"
            ),
            Err(error) => tracing::warn!(
                cpu_vulnerability = assessment.name,
                %error,
                "host CPU vulnerability assessment could not be read"
            ),
        }
    }
}

fn read(directory: &Path) -> io::Result<Vec<Assessment>> {
    let mut assessments = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        assessments.push(Assessment {
            name: entry.file_name().to_string_lossy().into_owned(),
            status: std::fs::read_to_string(entry.path()),
        });
    }
    if assessments.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the kernel reported no CPU vulnerability assessments",
        ));
    }
    assessments.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(assessments)
}

fn needs_attention(status: &str) -> bool {
    let status = status.trim();
    let lowercase = status.to_ascii_lowercase();
    if lowercase.contains("vulnerable") || lowercase.contains("unknown") {
        return true;
    }
    !(status == "Not affected" || status.starts_with("Mitigation:") || status.starts_with("KVM: Mitigation:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaffected_or_mitigated_assessments_are_informational() {
        for status in [
            "Not affected\n",
            "Mitigation: PTI\n",
            "Mitigation: Clear CPU buffers; SMT disabled\n",
            "KVM: Mitigation: Split huge pages\n",
        ] {
            assert!(!needs_attention(status), "{status}");
        }
    }

    #[test]
    fn incomplete_mitigations_still_need_attention() {
        for status in [
            "Vulnerable\n",
            "KVM: Vulnerable\n",
            "Mitigation: Clear CPU buffers; SMT vulnerable\n",
            "Mitigation: Unknown\n",
        ] {
            assert!(needs_attention(status), "{status}");
        }
    }

    #[test]
    fn unrecognised_or_empty_assessments_need_attention() {
        for status in ["", "\n", "Unknown", "a future kernel assessment"] {
            assert!(needs_attention(status), "{status}");
        }
    }

    #[test]
    fn kernel_assessment_names_and_descriptions_are_preserved_in_sorted_order() {
        let directory = tempfile::tempdir().unwrap();
        let status = "Mitigation: Clear CPU buffers; SMT vulnerable\n";
        std::fs::write(directory.path().join("mds"), status).unwrap();
        std::fs::write(directory.path().join("a_future_issue"), "Unknown\n").unwrap();
        let assessments = read(directory.path()).unwrap();
        assert_eq!(assessments.len(), 2);
        assert_eq!(assessments[0].name, "a_future_issue");
        assert_eq!(assessments[1].name, "mds");
        assert_eq!(assessments[1].status.as_ref().unwrap(), status);
    }

    #[test]
    fn an_unreadable_assessment_does_not_hide_other_assessments() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("mds")).unwrap();
        std::fs::write(directory.path().join("spectre_v1"), "Not affected\n").unwrap();
        let assessments = read(directory.path()).unwrap();
        assert_eq!(assessments.len(), 2);
        assert!(assessments[0].status.is_err());
        assert_eq!(assessments[1].status.as_ref().unwrap(), "Not affected\n");
    }

    #[test]
    fn missing_or_empty_assessments_are_reported_as_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read(directory.path()).is_err());
        assert!(read(&directory.path().join("missing")).is_err());
        report(directory.path());
        report(&directory.path().join("missing"));
    }
}
