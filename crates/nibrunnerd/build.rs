//! Firecracker is carried inside this binary rather than fetched onto a host.
//!
//! A daemon that downloads its own hypervisor at runtime is one whose behaviour depends on what a
//! release page holds that day, and whose first boot needs the network. Embedding it pins the
//! version to the build: the tarball is the one the guest image was measured against, its sha256
//! is checked here, and a host runs what this binary was compiled with or nothing.
//!
//! The tarball is fetched at build time and cached, so a repeat build is offline. Where it cannot
//! be fetched the build still succeeds and the daemon says at startup that it carries no VMM —
//! which is what lets this workspace be built and tested on a machine that is not the host.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Pinned to what `infra/app-host/versions.json` in the nibrun repository adopts, and to what the
/// guest image's own manifest names as the Firecracker it was built against. Moving this is
/// moving the guest image with it.
const FIRECRACKER_VERSION: &str = "v1.16.1";
const FIRECRACKER_URL: &str = "https://github.com/firecracker-microvm/firecracker/releases/download/v1.16.1/firecracker-v1.16.1-x86_64.tgz";
const FIRECRACKER_SHA256: &str = "382a02a869e4d6d5cb14c40577f9545e8458021ea8b0b2d3fc10ec14d9c242e6";
/// The member inside the tarball, which is flat under one directory named for the release.
const FIRECRACKER_MEMBER: &str = "release-v1.16.1-x86_64/firecracker-v1.16.1-x86_64";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=NIBRUNNER_FIRECRACKER_BINARY");
    println!("cargo:rustc-env=NIBRUNNER_FIRECRACKER_VERSION={FIRECRACKER_VERSION}");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let embedded = out_dir.join("firecracker");

    match resolve(&out_dir) {
        Some(binary) => {
            std::fs::write(&embedded, binary).expect("the build directory is writable");
            println!("cargo:rustc-env=NIBRUNNER_FIRECRACKER_EMBEDDED=1");
        }
        None => {
            // An empty file rather than no file: `include_bytes!` needs a path that exists, and
            // the daemon reads the length to know whether it is carrying anything.
            std::fs::write(&embedded, []).expect("the build directory is writable");
            println!(
                "cargo:warning=firecracker {FIRECRACKER_VERSION} could not be fetched; this build carries no VMM and can boot nothing"
            );
        }
    }
    println!("cargo:rustc-env=NIBRUNNER_FIRECRACKER_PATH={}", embedded.display());
}

/// A binary named outright, then a cached tarball, then the network. The first is what a build
/// behind a proxy uses and what an integration lane pins.
fn resolve(out_dir: &Path) -> Option<Vec<u8>> {
    if let Ok(path) = std::env::var("NIBRUNNER_FIRECRACKER_BINARY") {
        return std::fs::read(path).ok();
    }
    let cached = cache_path();
    if let Some(cached) = &cached {
        if let Ok(bytes) = std::fs::read(cached) {
            if digest_of(&bytes) == FIRECRACKER_SHA256 {
                return extract(&bytes, out_dir);
            }
        }
    }
    let bytes = download()?;
    if digest_of(&bytes) != FIRECRACKER_SHA256 {
        println!("cargo:warning=the firecracker tarball did not hash to the version this build pins");
        return None;
    }
    if let Some(cached) = &cached {
        let _ = std::fs::create_dir_all(cached.parent()?);
        let _ = std::fs::write(cached, &bytes);
    }
    extract(&bytes, out_dir)
}

fn cache_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache/nibrunner").join(format!("firecracker-{FIRECRACKER_VERSION}.tgz")))
}

fn download() -> Option<Vec<u8>> {
    let mut body = Vec::new();
    ureq::get(FIRECRACKER_URL)
        .call()
        .ok()?
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .ok()?;
    Some(body)
}

fn digest_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn extract(tarball: &[u8], _out_dir: &Path) -> Option<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if entry.path().ok()?.to_string_lossy() == FIRECRACKER_MEMBER {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary).ok()?;
            return Some(binary);
        }
    }
    println!("cargo:warning=the firecracker tarball does not hold {FIRECRACKER_MEMBER}");
    None
}
