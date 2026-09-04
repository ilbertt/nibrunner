//! The guest boot contract, as constants. Ported from `apps/runtime/src/paths.h`.
//!
//! Firecracker assigns virtio-blk devices in the order its `drives` array declares them, so the
//! device names here are an agreement with the host and not a local choice — `crates/guest-contract
//! /src/firecracker.rs` is the other half of it. Every mount point except the ones on a tmpfs has
//! to already exist in the rootfs image: the root is read-only for the life of the VM, so nothing
//! can create a directory on it at boot.

pub const ARTIFACT_DEVICE: &str = "/dev/vdb";
pub const CONFIG_DEVICE: &str = "/dev/vdc";
pub const DATA_DEVICE: &str = "/dev/vdd";

pub const ARTIFACT_MOUNT: &str = "/mnt/artifact";
/// What the host packs the tenant's binary into the artifact image as.
pub const TENANT_BINARY: &str = "/mnt/artifact/server";

pub const CONFIG_MOUNT: &str = "/run/config";
pub const CONFIG_FILE: &str = "/run/config/instance.env";
pub const RESOLV_CONF: &str = "/run/resolv.conf";

/// The tenant's working directory is a tmpfs it does not own: the only path it can write is the
/// data filesystem mounted inside it.
pub const APP_DIR: &str = "/app";
pub const DATA_DIR: &str = "/app/data";
pub const TENANT_TMP_DIR: &str = "/tmp";

/// Debian's own numbering, and what the rootfs image names in `/etc/passwd`.
pub const TENANT_UID: u32 = 65534;
pub const TENANT_GID: u32 = 65534;
