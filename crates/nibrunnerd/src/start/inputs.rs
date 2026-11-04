use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::json_store::{self, StoreError};

const HASH_BUFFER_BYTES: usize = 64 * 1024;

pub(super) struct Inputs {
    paths: Vec<PathBuf>,
    fingerprint: String,
    receipt: PathBuf,
    pub(super) pending: bool,
}

impl Inputs {
    pub(super) fn read(paths: Vec<PathBuf>, receipt: PathBuf) -> Result<Self, StoreError> {
        let fingerprint = fingerprint(&paths)?;
        let pending = json_store::read_text(&receipt)?.as_ref() != Some(&fingerprint);
        Ok(Self {
            paths,
            fingerprint,
            receipt,
            pending,
        })
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

    pub(super) fn applied(&self) -> Result<(), StoreError> {
        if fingerprint(&self.paths)? != self.fingerprint {
            return Err(StoreError::Unwritable {
                path: self.receipt.clone(),
                source: std::io::Error::other("service inputs changed during activation; run start again"),
            });
        }
        json_store::write_text(&self.receipt, &self.fingerprint, 0o600)
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
