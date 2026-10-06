use std::sync::Arc;
use std::time::{Duration, Instant};

use nibrunnerd::adapters::vm::layers::ensure_layer_image;
use nibrunnerd::ports::{ArtifactStore, CommandRequest, CommandRunnerExt};
use nibrunnerd::test_support::mocks;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImagePin {
    name: String,
    tag: String,
    repository: String,
    digest: String,
    compressed_bytes: u64,
    paths: Vec<String>,
}

async fn verify_image(name: &str, tag: &str) {
    if !super::enabled() || !std::env::var("NIBRUNNER_REMOTE_OCI").is_ok_and(|value| value == "1") {
        return;
    }
    super::require_root();
    let pins: Vec<ImagePin> = serde_json::from_str(include_str!("remote_oci_images.json")).unwrap();
    let pin = pins
        .iter()
        .find(|pin| pin.name == name && pin.tag == tag)
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = mocks::artifacts_holding(Vec::new());
    let commands = super::commands();
    let layer = protocol::DesiredLayer::Oci {
        source: protocol::OciSource::Registry {
            repository: protocol::OciRepository::parse(&pin.repository).unwrap(),
            digest: protocol::Sha256Digest::parse(&pin.digest).unwrap(),
        },
    };
    let started = Instant::now();
    println!("Pulling {name}:{tag} at sha256:{} into a fresh cache", pin.digest);
    let image = tokio::time::timeout(
        Duration::from_secs(600),
        ensure_layer_image(&store, cache.path(), &layer, &commands),
    )
    .await
    .expect("registry pull and filesystem preparation finish within ten minutes")
    .unwrap();
    assert!(image.fetched_bytes >= pin.compressed_bytes);
    for path in &pin.paths {
        let metadata = super::debugfs(&image.path, &format!("stat {path}")).await;
        assert!(metadata.contains("Inode:"), "{name}:{tag} {path}: {metadata}");
    }
    commands
        .stdout_of(CommandRequest::new(&[
            "e2fsck",
            "-f",
            "-n",
            image.path.to_str().unwrap(),
        ]))
        .await
        .expect("the complete assembled ext4 filesystem is clean");
    let cached = ensure_layer_image(&store, cache.path(), &layer, &commands)
        .await
        .unwrap();
    assert_eq!(cached.path, image.path);
    assert_eq!(cached.fetched_bytes, 0);
    println!(
        "Verified {name}:{tag}: {} fetched bytes, {:.2}s, expected files present, ext4 clean, cache reused",
        image.fetched_bytes,
        started.elapsed().as_secs_f64()
    );
}

#[tokio::test]
async fn nginx_stable_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("nginx", "stable").await;
}

#[tokio::test]
async fn nginx_alpine_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("nginx", "stable-alpine").await;
}

#[tokio::test]
async fn node_bookworm_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("node", "22-bookworm-slim").await;
}

#[tokio::test]
async fn node_alpine_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("node", "22-alpine").await;
}

#[tokio::test]
async fn redis_bookworm_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("redis", "7-bookworm").await;
}

#[tokio::test]
async fn redis_alpine_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("redis", "7-alpine").await;
}

#[tokio::test]
async fn postgres_bookworm_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("postgres", "16-bookworm").await;
}

#[tokio::test]
async fn postgres_alpine_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("postgres", "16-alpine").await;
}

#[tokio::test]
async fn busybox_unpacks_from_docker_hub_into_a_clean_cached_filesystem() {
    verify_image("busybox", "1.37").await;
}
