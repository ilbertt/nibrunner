//! Where a tenant's binary comes from: a local directory for a single machine, an S3 bucket for a
//! fleet. One trait over `object_store`, so which of the two a host uses is a URL and not a code
//! path.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::{ObjectStore, ObjectStoreExt};
use protocol::ObjectKey;

use crate::services::{ArtifactError, ArtifactStore};

pub struct ObjectArtifactStore {
    store: Arc<dyn ObjectStore>,
    /// What the URL named beneath the bucket, prepended to every key. Absent for a directory,
    /// which is already a prefix.
    prefix: Option<String>,
}

impl ObjectArtifactStore {
    /// A URL, or a path. `s3://bucket/prefix` reaches AWS through the ordinary credential chain;
    /// anything else is a directory on this host, which is what a single machine wants and what
    /// makes `run-dev` need no account.
    pub fn open(url: &str) -> Result<Self, ArtifactError> {
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((bucket, prefix)) => (bucket, Some(prefix.trim_end_matches('/').to_string())),
                None => (rest, None),
            };
            let store = object_store::aws::AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .build()
                .map_err(|error| ArtifactError::Transfer(error.to_string()))?;
            return Ok(Self { store: Arc::new(store), prefix: prefix.filter(|prefix| !prefix.is_empty()) });
        }
        let directory = std::path::PathBuf::from(url);
        crate::json_store::make_directory(&directory, 0o700)
            .map_err(|error| ArtifactError::Transfer(error.to_string()))?;
        let store = object_store::local::LocalFileSystem::new_with_prefix(&directory)
            .map_err(|error| ArtifactError::Transfer(error.to_string()))?;
        Ok(Self { store: Arc::new(store), prefix: None })
    }

    fn path_for(&self, object_key: &ObjectKey) -> object_store::path::Path {
        match &self.prefix {
            None => object_store::path::Path::from(object_key.as_str()),
            Some(prefix) => object_store::path::Path::from(format!("{prefix}/{object_key}")),
        }
    }
}

#[async_trait]
impl ArtifactStore for ObjectArtifactStore {
    async fn read(&self, object_key: &ObjectKey) -> Result<Vec<u8>, ArtifactError> {
        let result = self
            .store
            .get(&self.path_for(object_key))
            .await
            .map_err(|error| ArtifactError::Transfer(error.to_string()))?;
        let bytes = result.bytes().await.map_err(|error| ArtifactError::Transfer(error.to_string()))?;
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_directory_is_a_bucket_a_single_machine_already_has() {
        let directory = tempfile::tempdir().unwrap();
        let store = ObjectArtifactStore::open(&directory.path().display().to_string()).unwrap();
        std::fs::create_dir_all(directory.path().join("artifacts")).unwrap();
        std::fs::write(directory.path().join("artifacts/one"), b"a binary").unwrap();
        let key = ObjectKey::parse("artifacts/one").unwrap();
        assert_eq!(store.read(&key).await.unwrap(), b"a binary");

        let missing = ObjectKey::parse("artifacts/absent").unwrap();
        assert!(matches!(store.read(&missing).await, Err(ArtifactError::Transfer(_))));
    }

    /// The prefix a bucket URL names is prepended to every key, so what the control plane sends
    /// stays the key it assigned rather than one this host has to rewrite.
    #[test]
    fn a_bucket_url_carries_its_prefix_into_every_key() {
        let key = ObjectKey::parse("artifacts/one").unwrap();
        let bare = ObjectArtifactStore { store: Arc::new(object_store::memory::InMemory::new()), prefix: None };
        assert_eq!(bare.path_for(&key).to_string(), "artifacts/one");
        let nested = ObjectArtifactStore {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: Some("hosts/host-1".into()),
        };
        assert_eq!(nested.path_for(&key).to_string(), "hosts/host-1/artifacts/one");
    }
}
