pub const ARTIFACT_DEVICE: &str = "/dev/vdb";
pub const CONFIG_DEVICE: &str = "/dev/vdc";
pub const DATA_DEVICE: &str = "/dev/vdd";

pub const ARTIFACT_MOUNT: &str = "/mnt/artifact";
pub const TENANT_BINARY: &str = "/mnt/artifact/server";

pub const CONFIG_MOUNT: &str = "/run/config";
pub const CONFIG_FILE: &str = "/run/config/instance.env";
pub const RESOLV_CONF: &str = "/run/resolv.conf";

pub const APP_DIR: &str = "/app";
pub const DATA_DIR: &str = "/app/data";
pub const TENANT_TMP_DIR: &str = "/tmp";

/// A rootfs artifact is the read-only lower layer; what the tenant writes lands in `upper` on the
/// data volume, and `work` is overlayfs's own scratch beside it, which no export should carry.
pub const OVERLAY_UPPER_DIR: &str = "upper";
pub const OVERLAY_WORK_DIR: &str = "work";
pub const ROOTFS_MOUNT: &str = "/mnt/root";
pub const ROOTFS_INIT: &str = "/sbin/init";

pub const TENANT_UID: u32 = 65534;
pub const TENANT_GID: u32 = 65534;
