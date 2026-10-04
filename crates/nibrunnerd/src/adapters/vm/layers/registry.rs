use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::LazyLock;
use std::time::Duration;

use futures::TryStreamExt;
use oci_client::client::ClientConfig;
use oci_client::manifest::{OciDescriptor, OciImageManifest, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use protocol::{OciRepository, Sha256Digest};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::ports::ArtifactError;

const MAXIMUM_TRANSFER_BYTES: u64 = 1024 * 1024 * 1024;
const MAXIMUM_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAXIMUM_LAYERS: usize = 128;
const MAXIMUM_INDEX_DEPTH: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_TIMEOUT: Duration = Duration::from_secs(300);
const MANIFEST_TYPES: &[&str] = &[
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
static PULLS: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(1));

pub(super) struct PulledImage {
    pub archive: Vec<u8>,
    pub fetched_bytes: u64,
}

fn failed(error: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Transfer(format!("the registry image could not be fetched: {error}"))
}

fn verified(bytes: &[u8], digest: &str) -> Result<(), ArtifactError> {
    let actual = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    if digest != actual {
        return Err(failed(format!("{digest} hashes to {actual}")));
    }
    Ok(())
}

fn sha256(digest: &str) -> Result<&str, ArtifactError> {
    let hash = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| failed("only SHA256 descriptors are supported"))?;
    Sha256Digest::parse(hash).map_err(failed)?;
    Ok(hash)
}

fn append(archive: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) -> Result<(), ArtifactError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, path, Cursor::new(bytes))
        .map_err(failed)
}

pub(super) async fn pull(
    repository: &OciRepository,
    digest: &Sha256Digest,
) -> Result<PulledImage, ArtifactError> {
    crate::install_crypto_provider();
    let _permit = PULLS
        .acquire()
        .await
        .expect("the registry pull semaphore is never closed");
    let client = Client::try_from(ClientConfig {
        connect_timeout: Some(CONNECT_TIMEOUT),
        read_timeout: Some(READ_TIMEOUT),
        ..ClientConfig::default()
    })
    .map_err(failed)?;
    let image: Reference = format!("{repository}@sha256:{digest}").parse().map_err(failed)?;
    tokio::time::timeout(PULL_TIMEOUT, pull_with(&client, &image))
        .await
        .map_err(|_| failed("the pull timed out"))?
}

async fn image_manifest(
    client: &Client,
    image: &Reference,
) -> Result<(OciImageManifest, Vec<u8>, u64), ArtifactError> {
    let mut reference = image.clone();
    let mut expected_size = None;
    let mut fetched_bytes = 0;
    for _ in 0..=MAXIMUM_INDEX_DEPTH {
        let (bytes, _) = client
            .pull_manifest_raw(&reference, &RegistryAuth::Anonymous, MANIFEST_TYPES)
            .await
            .map_err(failed)?;
        if bytes.len() > MAXIMUM_METADATA_BYTES
            || expected_size.is_some_and(|size| size != bytes.len() as i64)
        {
            return Err(failed(
                "the manifest exceeds 4 MiB or disagrees with its descriptor size",
            ));
        }
        let digest = reference.digest().expect("every registry reference is pinned");
        sha256(digest)?;
        verified(&bytes, digest)?;
        fetched_bytes += bytes.len() as u64;
        let manifest: OciManifest = serde_json::from_slice(&bytes).map_err(failed)?;
        if !MANIFEST_TYPES.contains(&manifest.content_type()) {
            return Err(failed("the manifest media type is unsupported"));
        }
        match manifest {
            OciManifest::Image(manifest) if manifest.schema_version == 2 => {
                return Ok((manifest, bytes.to_vec(), fetched_bytes))
            }
            OciManifest::ImageIndex(index) if index.schema_version == 2 => {
                let mut matching = index.manifests.iter().filter(|entry| {
                    entry.platform.as_ref().is_some_and(|platform| {
                        platform.os.to_string() == "linux"
                            && platform.architecture.to_string() == "amd64"
                            && platform
                                .variant
                                .as_deref()
                                .is_none_or(|variant| variant.is_empty() || variant == "v1")
                    })
                });
                let entry = matching
                    .next()
                    .ok_or_else(|| failed("the index has no Linux amd64 image"))?;
                if matching.next().is_some() {
                    return Err(failed("the index has more than one Linux amd64 image"));
                }
                sha256(&entry.digest)?;
                expected_size = Some(entry.size);
                reference = image.clone_with_digest(entry.digest.clone());
            }
            _ => return Err(failed("the manifest schema version is unsupported")),
        }
    }
    Err(failed("the image index is nested too deeply"))
}

async fn pull_with(client: &Client, image: &Reference) -> Result<PulledImage, ArtifactError> {
    let (manifest, bytes, mut fetched_bytes) = image_manifest(client, image).await?;
    if manifest.layers.len() > MAXIMUM_LAYERS {
        return Err(failed("the image has more than 128 layers"));
    }
    let manifest_digest = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
    let index = serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": [{
        "mediaType": manifest.media_type.as_deref().unwrap_or(MANIFEST_TYPES[0]),
        "digest": manifest_digest, "size": bytes.len(), "platform": {"os": "linux", "architecture": "amd64"}
    }]}))
    .map_err(failed)?;
    let mut archive = tar::Builder::new(Vec::new());
    append(&mut archive, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    append(&mut archive, "index.json", &index)?;
    append(
        &mut archive,
        &format!("blobs/sha256/{}", sha256(&manifest_digest)?),
        &bytes,
    )?;
    let mut held = BTreeSet::from([manifest_digest]);
    let mut remaining = MAXIMUM_TRANSFER_BYTES;
    for (index, descriptor) in std::iter::once(&manifest.config)
        .chain(&manifest.layers)
        .enumerate()
    {
        let hash = sha256(&descriptor.digest)?;
        let size = u64::try_from(descriptor.size).map_err(failed)?;
        if size > remaining || (index == 0 && size > MAXIMUM_METADATA_BYTES as u64) {
            return Err(failed(
                "image blobs exceed the download or configuration size limit",
            ));
        }
        if !held.insert(descriptor.digest.clone()) {
            continue;
        }
        let bytes = blob(client, image, descriptor).await?;
        remaining -= size;
        fetched_bytes += size;
        append(&mut archive, &format!("blobs/sha256/{hash}"), &bytes)?;
    }
    Ok(PulledImage {
        archive: archive.into_inner().map_err(failed)?,
        fetched_bytes,
    })
}

async fn blob(
    client: &Client,
    image: &Reference,
    descriptor: &OciDescriptor,
) -> Result<Vec<u8>, ArtifactError> {
    // Foreign-layer URLs are not needed for supported Linux images. Let the registry serve blobs.
    let mut descriptor = descriptor.clone();
    descriptor.urls = None;
    let mut stream = client
        .pull_blob_stream(image, &descriptor)
        .await
        .map_err(failed)?;
    let size = usize::try_from(descriptor.size).map_err(failed)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.try_next().await.map_err(failed)? {
        if chunk.len() > size.saturating_sub(bytes.len()) {
            return Err(failed("a blob exceeds its descriptor size"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != size {
        return Err(failed("a blob is shorter than its descriptor size"));
    }
    verified(&bytes, &descriptor.digest)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;
    use std::sync::Arc;

    use oci_client::client::ClientProtocol;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    struct Registry {
        image: Reference,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Registry {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Registry {
        async fn start(mutate: impl FnOnce(&mut BTreeMap<String, Vec<u8>>, &mut serde_json::Value)) -> Self {
            let archive = crate::test_support::oci::archive(b"a test program");
            let mut files = BTreeMap::new();
            for entry in tar::Archive::new(Cursor::new(archive)).entries().unwrap() {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                Read::read_to_end(&mut entry, &mut bytes).unwrap();
                files.insert(name, bytes);
            }
            let mut index: serde_json::Value = serde_json::from_slice(&files["index.json"]).unwrap();
            index["manifests"].as_array_mut().unwrap().push(json!({
                "mediaType": MANIFEST_TYPES[0], "digest": format!("sha256:{}", "f".repeat(64)),
                "size": 123, "platform": {"os": "linux", "architecture": "arm64"}
            }));
            mutate(&mut files, &mut index);
            let root = serde_json::to_vec(&index).unwrap();
            let digest = format!("sha256:{}", hex::encode(Sha256::digest(&root)));
            let mut responses = BTreeMap::new();
            responses.insert(format!("/v2/test/image/manifests/{digest}"), root);
            for (path, bytes) in files {
                if let Some(hash) = path.strip_prefix("blobs/sha256/") {
                    responses.insert(format!("/v2/test/image/manifests/sha256:{hash}"), bytes.clone());
                    responses.insert(format!("/v2/test/image/blobs/sha256:{hash}"), bytes);
                }
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let responses = Arc::new(responses);
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let responses = responses.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buffer = [0; 1024];
                        while !request.ends_with(b"\r\n\r\n") {
                            let count = stream.read(&mut buffer).await.unwrap();
                            if count == 0 {
                                return;
                            }
                            request.extend_from_slice(&buffer[..count]);
                        }
                        let request = String::from_utf8(request).unwrap();
                        let path = request
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .split('?')
                            .next()
                            .unwrap();
                        let (status, headers, body) = if path == "/v2/" {
                            ("401 Unauthorized", format!("WWW-Authenticate: Bearer realm=\"http://{address}/token\",service=\"test\"\r\n"), Vec::new())
                        } else if path == "/token" {
                            (
                                "200 OK",
                                String::new(),
                                br#"{"token":"moo","expires_in":3600}"#.to_vec(),
                            )
                        } else if request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer moo\r\n")
                        {
                            match responses.get(path) {
                                Some(body) => ("200 OK", String::new(), body.clone()),
                                None => ("404 Not Found", String::new(), Vec::new()),
                            }
                        } else {
                            ("401 Unauthorized", String::new(), Vec::new())
                        };
                        let head = format!("HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        stream.write_all(head.as_bytes()).await.unwrap();
                        stream.write_all(&body).await.unwrap();
                    });
                }
            });
            Self {
                image: Reference::with_digest(address.to_string(), "test/image".to_string(), digest),
                task,
            }
        }

        async fn pull(&self) -> Result<PulledImage, ArtifactError> {
            crate::install_crypto_provider();
            let client = Client::try_from(ClientConfig {
                protocol: ClientProtocol::Http,
                ..ClientConfig::default()
            })
            .unwrap();
            tokio::time::timeout(Duration::from_secs(10), pull_with(&client, &self.image))
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn a_bearer_challenge_and_multi_platform_index_yield_only_the_verified_amd64_filesystem() {
        let registry = Registry::start(|_, _| {}).await;
        let pulled = registry.pull().await.unwrap();
        assert!(pulled.fetched_bytes > 0);
        let flattened = oci_image::flatten(&pulled.archive).unwrap();
        let mut files = tar::Archive::new(Cursor::new(flattened.tar));
        let program = files
            .entries()
            .unwrap()
            .find_map(|entry| {
                let mut entry = entry.unwrap();
                if entry.path().unwrap().as_ref() != std::path::Path::new("app/tenant") {
                    return None;
                }
                let mut bytes = Vec::new();
                Read::read_to_end(&mut entry, &mut bytes).unwrap();
                Some(bytes)
            })
            .unwrap();
        assert_eq!(program, b"a test program");
    }

    #[tokio::test]
    async fn a_registry_cannot_substitute_different_bytes_for_a_pinned_manifest() {
        let registry = Registry::start(|files, index| {
            let digest = index["manifests"][0]["digest"]
                .as_str()
                .unwrap()
                .strip_prefix("sha256:")
                .unwrap();
            files.get_mut(&format!("blobs/sha256/{digest}")).unwrap()[0] = b'!';
        })
        .await;
        let error = registry.pull().await.err().unwrap();
        assert!(
            error.message().contains("digest") || error.message().contains("hashes"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_registry_cannot_substitute_different_bytes_for_a_manifest_blob() {
        let registry = Registry::start(|files, index| {
            let digest = index["manifests"][0]["digest"]
                .as_str()
                .unwrap()
                .strip_prefix("sha256:")
                .unwrap();
            let manifest: serde_json::Value =
                serde_json::from_slice(&files[&format!("blobs/sha256/{digest}")]).unwrap();
            let layer = manifest["layers"][0]["digest"]
                .as_str()
                .unwrap()
                .strip_prefix("sha256:")
                .unwrap();
            files.get_mut(&format!("blobs/sha256/{layer}")).unwrap()[0] ^= 1;
        })
        .await;
        let error = registry.pull().await.err().unwrap();
        assert!(
            error.message().contains("digest") || error.message().contains("hashes"),
            "{error}"
        );
    }
}
