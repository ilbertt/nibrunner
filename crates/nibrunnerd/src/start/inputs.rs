use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::json_store::{self, StoreError};

const HASH_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InvocationId(uuid::Uuid);

impl InvocationId {
    pub(super) fn parse(shown: &str) -> Option<Self> {
        let shown = shown.trim();
        if shown.len() != uuid::fmt::Simple::LENGTH {
            return None;
        }
        uuid::Uuid::parse_str(shown)
            .ok()
            .filter(|id| !id.is_nil())
            .map(Self)
    }
}

pub(super) struct Inputs {
    paths: Vec<PathBuf>,
    fingerprint: String,
    receipt: PathBuf,
    applied: Option<String>,
}

impl Inputs {
    pub(super) fn read(paths: Vec<PathBuf>, receipt: PathBuf) -> Result<Self, StoreError> {
        let fingerprint = fingerprint(&paths)?;
        let applied = json_store::read_text(&receipt)?;
        Ok(Self {
            paths,
            fingerprint,
            receipt,
            applied,
        })
    }

    pub(super) fn pending(&self, invocation: Option<InvocationId>) -> bool {
        invocation.is_none_or(|id| self.applied.as_ref() != Some(&self.record(id)))
    }

    pub(super) fn invalidate(&self) -> Result<(), StoreError> {
        match std::fs::remove_file(&self.receipt) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Unwritable {
                path: self.receipt.clone(),
                source,
            }),
        }
    }

    pub(super) fn applied(&self, invocation: InvocationId) -> Result<(), StoreError> {
        if fingerprint(&self.paths)? != self.fingerprint {
            return Err(StoreError::Unwritable {
                path: self.receipt.clone(),
                source: std::io::Error::other("service inputs changed during activation; run start again"),
            });
        }
        json_store::write_text(&self.receipt, &self.record(invocation), 0o600)
    }

    fn record(&self, invocation: InvocationId) -> String {
        format!("{}\n{}", self.fingerprint, invocation.0.simple())
    }
}

fn fingerprint(paths: &[PathBuf]) -> Result<String, StoreError> {
    let mut inputs = Sha256::new();
    inputs.update(b"nibrunner-service-inputs-v1");
    for path in paths {
        let unreadable = |source| StoreError::Unreadable {
            path: path.clone(),
            source,
        };
        let mut file = std::fs::File::open(path).map_err(unreadable)?;
        let mut digest = Sha256::new();
        let mut buffer = [0; HASH_BUFFER_BYTES];
        loop {
            let read = file.read(&mut buffer).map_err(unreadable)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        let name = path.as_os_str().as_encoded_bytes();
        inputs.update((name.len() as u64).to_le_bytes());
        inputs.update(name);
        inputs.update(digest.finalize());
    }
    Ok(hex::encode(inputs.finalize()))
}

pub(super) fn receipt(state_dir: &Path, unit: &str) -> PathBuf {
    state_dir.join("applied-services").join(unit)
}
