use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use protocol::ObjectKey;
use tokio::io::AsyncReadExt;

const UPLOAD_PART_SIZE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ExportStoreError {
    #[error("the bundle could not be handed over: {0}")]
    Transfer(String),
}

impl ExportStoreError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

#[async_trait]
pub trait ExportStore: Send + Sync {
    async fn upload(&self, bundle_path: &Path, object_key: &ObjectKey) -> Result<(), ExportStoreError>;
}

pub struct ObjectExportStore {
    store: Arc<dyn ObjectStore>,
    prefix: Option<String>,
}

impl ObjectExportStore {
    pub fn open(url: &str) -> Result<Self, ExportStoreError> {
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((bucket, prefix)) => (bucket, Some(prefix.trim_end_matches('/').to_string())),
                None => (rest, None),
            };
            let store = object_store::aws::AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .build()
                .map_err(|error| ExportStoreError::Transfer(error.to_string()))?;
            return Ok(Self {
                store: Arc::new(store),
                prefix: prefix.filter(|prefix| !prefix.is_empty()),
            });
        }
        let directory = std::path::PathBuf::from(url);
        crate::json_store::make_directory(&directory, 0o700)
            .map_err(|error| ExportStoreError::Transfer(error.to_string()))?;
        let store = object_store::local::LocalFileSystem::new_with_prefix(&directory)
            .map_err(|error| ExportStoreError::Transfer(error.to_string()))?;
        Ok(Self {
            store: Arc::new(store),
            prefix: None,
        })
    }

    fn path_for(&self, object_key: &ObjectKey) -> object_store::path::Path {
        match &self.prefix {
            None => object_store::path::Path::from(object_key.as_str()),
            Some(prefix) => object_store::path::Path::from(format!("{prefix}/{object_key}")),
        }
    }
}

#[async_trait]
impl ExportStore for ObjectExportStore {
    async fn upload(&self, bundle_path: &Path, object_key: &ObjectKey) -> Result<(), ExportStoreError> {
        let transfer = |error: &dyn std::fmt::Display| ExportStoreError::Transfer(error.to_string());
        let mut file = tokio::fs::File::open(bundle_path)
            .await
            .map_err(|error| transfer(&error))?;
        let mut upload = self
            .store
            .put_multipart(&self.path_for(object_key))
            .await
            .map_err(|error| transfer(&error))?;

        let mut part = vec![0u8; UPLOAD_PART_SIZE_BYTES];
        loop {
            let read = file.read(&mut part).await.map_err(|error| transfer(&error))?;
            if read == 0 {
                break;
            }
            upload
                .put_part(PutPayload::from(part[..read].to_vec()))
                .await
                .map_err(|error| transfer(&error))?;
        }
        upload.complete().await.map_err(|error| transfer(&error))?;
        Ok(())
    }
}

pub struct RecordingExportStore {
    uploads: std::sync::Mutex<Vec<(std::path::PathBuf, ObjectKey)>>,
    answer: Box<dyn Fn() -> Result<(), ExportStoreError> + Send + Sync>,
}

impl RecordingExportStore {
    pub fn accepting() -> Arc<Self> {
        Self::answering(|| Ok(()))
    }

    pub fn answering(answer: impl Fn() -> Result<(), ExportStoreError> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            uploads: std::sync::Mutex::new(Vec::new()),
            answer: Box::new(answer),
        })
    }

    pub fn uploads(&self) -> Vec<(std::path::PathBuf, ObjectKey)> {
        self.uploads.lock().expect("no panic holds this lock").clone()
    }
}

#[async_trait]
impl ExportStore for RecordingExportStore {
    async fn upload(&self, bundle_path: &Path, object_key: &ObjectKey) -> Result<(), ExportStoreError> {
        self.uploads
            .lock()
            .expect("no panic holds this lock")
            .push((bundle_path.to_path_buf(), object_key.clone()));
        (self.answer)()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_bundle_reaches_the_store_under_the_key_the_document_named() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("bundle.tar.gz");
        std::fs::write(&bundle, vec![7u8; UPLOAD_PART_SIZE_BYTES * 2 + 11]).unwrap();

        let destination = root.path().join("exports");
        let store = ObjectExportStore::open(&destination.display().to_string()).unwrap();
        let object_key = ObjectKey::parse("exports/exp-1/bundle.tar.gz").unwrap();
        store.upload(&bundle, &object_key).await.unwrap();

        let written = destination.join("exports/exp-1/bundle.tar.gz");
        assert_eq!(
            std::fs::metadata(&written).unwrap().len() as usize,
            UPLOAD_PART_SIZE_BYTES * 2 + 11
        );
    }

    #[tokio::test]
    async fn a_bundle_that_is_not_there_is_a_transfer_that_failed_rather_than_one_that_worked() {
        let root = tempfile::tempdir().unwrap();
        let store = ObjectExportStore::open(&root.path().join("exports").display().to_string()).unwrap();
        let object_key = ObjectKey::parse("exports/exp-1/bundle.tar.gz").unwrap();
        assert!(store
            .upload(&root.path().join("nothing.tar.gz"), &object_key)
            .await
            .is_err());
    }
}
