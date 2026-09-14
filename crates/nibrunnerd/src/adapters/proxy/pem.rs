//! The certificate files `[proxy.http.tls]` names, read whole and counted. `rustls_pemfile` takes
//! an END marker with the next BEGIN on the same line — what `cat` leaves between a file with no
//! trailing newline and the one after it — as the end of one certificate, and drops the one after
//! it without a word. A trust pool one certificate short fails every handshake with the missing
//! CA's certificate and says only `unknown ca`, so the markers are counted against what was read
//! and a file that is short is refused before anything is trusted from it.

use std::path::{Path, PathBuf};

use rustls::pki_types::CertificateDer;

const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";

#[derive(Debug, thiserror::Error)]
pub enum CertificateFileError {
    #[error("{} could not be read: {error}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("{} holds no certificate", path.display())]
    Empty { path: PathBuf },
    #[error(
        "{} marks {marked} certificates and {read} could be read, which is what a boundary without a newline does: concatenating a file that has no trailing newline puts the END of one certificate and the BEGIN of the next on one line, and the one after it is skipped",
        path.display()
    )]
    Short {
        path: PathBuf,
        marked: usize,
        read: usize,
    },
}

impl From<CertificateFileError> for std::io::Error {
    fn from(error: CertificateFileError) -> Self {
        let kind = match &error {
            CertificateFileError::Unreadable { error, .. } => error.kind(),
            CertificateFileError::Empty { .. } | CertificateFileError::Short { .. } => {
                std::io::ErrorKind::InvalidData
            }
        };
        std::io::Error::new(kind, error)
    }
}

pub fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, CertificateFileError> {
    let unreadable = |error| CertificateFileError::Unreadable {
        path: path.to_path_buf(),
        error,
    };
    let contents = std::fs::read(path).map_err(unreadable)?;
    let certificates: Vec<_> = rustls_pemfile::certs(&mut contents.as_slice())
        .collect::<Result<_, _>>()
        .map_err(unreadable)?;
    let marked = contents
        .windows(BEGIN.len())
        .filter(|window| *window == BEGIN)
        .count();
    if certificates.len() != marked {
        return Err(CertificateFileError::Short {
            path: path.to_path_buf(),
            marked,
            read: certificates.len(),
        });
    }
    if certificates.is_empty() {
        return Err(CertificateFileError::Empty {
            path: path.to_path_buf(),
        });
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The PEM layer decodes base64 and never looks at the DER, so a body of any bytes is a
    // certificate to it, and that is the layer that dropped one.
    const ONE: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    const ANOTHER: &str = "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";

    fn written(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("origin-pull-ca.pem");
        std::fs::write(&path, contents).unwrap();
        (directory, path)
    }

    #[test]
    fn a_well_formed_pool_loads_every_certificate_in_it() {
        let (_directory, path) = written(&format!("{ONE}{ANOTHER}"));
        assert_eq!(read_certificates(&path).unwrap().len(), 2);

        let (_directory, path) = written(ONE.trim_end());
        assert_eq!(
            read_certificates(&path).unwrap().len(),
            1,
            "a file with no trailing newline is whole on its own"
        );
    }

    #[test]
    fn a_pool_glued_at_a_boundary_is_refused_with_both_counts() {
        let (_directory, path) = written(&format!("{}{ANOTHER}", ONE.trim_end()));
        let error = read_certificates(&path).unwrap_err();
        assert!(matches!(
            error,
            CertificateFileError::Short {
                marked: 2,
                read: 1,
                ..
            }
        ));
        let said = error.to_string();
        assert!(said.starts_with(&path.display().to_string()), "{said}");
        assert!(
            said.contains("marks 2 certificates and 1 could be read"),
            "{said}"
        );
        assert!(said.contains("without a newline"), "{said}");
    }

    #[test]
    fn an_empty_file_is_refused() {
        let (_directory, path) = written("");
        let said = read_certificates(&path).unwrap_err().to_string();
        assert_eq!(said, format!("{} holds no certificate", path.display()));
    }
}
